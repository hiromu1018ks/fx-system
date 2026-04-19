//! Strategy B: Volatility Decay Momentum.
//!
//! Captures momentum that emerges as volatility decays from a spike. Unlike Strategy A's
//! fast mean-reversion (seconds), Strategy B rides momentum on a minutes-to-tens-of-minutes
//! horizon with much slower decay (λ_B << λ_A).
//!
//! Trigger: vol_spike_recent ∧ vol_decaying ∧ OBI_aligned ∧ regime_kl < threshold
//! Feature vector: base 34-dim + 5 strategy-specific = 39 dimensions.
//! MAX_HOLD_TIME: minutes to tens of minutes (default 300s = 5 min).

use std::collections::HashMap;

use fx_core::types::StrategyId;
use fx_events::projector::StateSnapshot;
use rand::Rng;

use crate::bayesian_lr::{QAction, QFunction, UpdateResult};
use crate::features::FeatureVector;
use crate::policy::Action;

/// Number of extra features Strategy B appends to the base FeatureVector.
pub const STRATEGY_B_EXTRA_DIM: usize = 5;

/// Total feature dimension for Strategy B's Q-function (34 + 5 = 39).
pub const STRATEGY_B_FEATURE_DIM: usize = FeatureVector::DIM + STRATEGY_B_EXTRA_DIM;

/// Strategy B configuration.
#[derive(Debug, Clone)]
pub struct StrategyBConfig {
    /// Trigger: volatility_ratio must have exceeded this recently (spike detected).
    pub vol_spike_threshold: f64,
    /// Trigger: volatility must be decaying (volatility_decay_rate < this, negative).
    pub vol_decaying_threshold: f64,
    /// Trigger: OBI must exceed this in absolute value (directional alignment).
    pub obi_alignment_threshold: f64,
    /// Trigger: regime KL divergence must be below this (known regime).
    pub regime_kl_threshold: f64,
    /// Maximum holding time in milliseconds (5 minutes default).
    pub max_hold_time_ms: u64,
    /// Strategy B specific decay rate λ_B (minutes scale, much slower than λ_A).
    pub decay_rate_b: f64,
    /// Q-function regularization.
    pub lambda_reg: f64,
    /// Q-function EMA halflife for noise variance.
    pub halflife: usize,
    /// Initial noise variance.
    pub initial_sigma2: f64,
    /// Optimistic initialization bias for Buy/Sell.
    pub optimistic_bias: f64,
    /// Non-model uncertainty penalty coefficient.
    pub non_model_uncertainty_k: f64,
    /// Latency penalty coefficient.
    pub latency_penalty_k: f64,
    /// Minimum trade frequency threshold.
    pub min_trade_frequency: f64,
    /// Trade frequency monitoring window (decisions).
    pub trade_frequency_window: usize,
    /// Covariance inflation factor for hold degeneration.
    pub hold_degeneration_inflation: f64,
    /// Maximum lot size per order.
    pub max_lot_size: u64,
    /// Minimum lot size (below this → Hold).
    pub min_lot_size: u64,
    /// Action consistency threshold.
    pub consistency_threshold: f64,
    /// Default lot size.
    pub default_lot_size: u64,
}

impl Default for StrategyBConfig {
    fn default() -> Self {
        Self {
            vol_spike_threshold: 2.0,
            vol_decaying_threshold: 0.0,
            obi_alignment_threshold: 0.1,
            regime_kl_threshold: 1.0,
            max_hold_time_ms: 300_000,
            decay_rate_b: 0.0001,
            lambda_reg: 0.01,
            halflife: 500,
            initial_sigma2: 0.01,
            optimistic_bias: 0.01,
            non_model_uncertainty_k: 0.1,
            latency_penalty_k: 0.001,
            min_trade_frequency: 0.02,
            trade_frequency_window: 500,
            hold_degeneration_inflation: 1.5,
            max_lot_size: 1_000_000,
            min_lot_size: 1000,
            consistency_threshold: 0.05,
            default_lot_size: 100_000,
        }
    }
}

/// Episode state for Strategy B.
#[derive(Debug, Clone, PartialEq)]
pub enum EpisodeStateB {
    Idle,
    Active { entry_timestamp_ns: u64 },
}

/// Decision output from Strategy B.
#[derive(Debug, Clone)]
pub struct StrategyBDecision {
    pub action: Action,
    pub q_point: f64,
    pub q_sampled: f64,
    pub posterior_std: f64,
    pub triggered: bool,
    pub episode_active: bool,
    pub should_close: bool,
    pub skip_reason: Option<String>,
    pub remaining_hold_time_ms: u64,
    pub hold_degeneration_detected: bool,
    pub consistency_fallback: bool,
}

/// Strategy B: Volatility Decay Momentum.
///
/// Detects momentum continuation after volatility spikes begin to decay. Uses extended
/// feature vector (39-dim) with strategy-specific nonlinear and interaction terms focused
/// on momentum signals. Episodes are bounded by MAX_HOLD_TIME (minutes scale).
pub struct StrategyB {
    config: StrategyBConfig,
    episode: EpisodeStateB,
    q_function: QFunction,
    trade_history: Vec<bool>,
    total_decisions: usize,
}

impl StrategyB {
    pub fn new(config: StrategyBConfig) -> Self {
        let q_function = QFunction::new(
            STRATEGY_B_FEATURE_DIM,
            config.lambda_reg,
            config.halflife,
            config.initial_sigma2,
            config.optimistic_bias,
        );
        let trade_capacity = config.trade_frequency_window;
        Self {
            config,
            episode: EpisodeStateB::Idle,
            q_function,
            trade_history: Vec::with_capacity(trade_capacity),
            total_decisions: 0,
        }
    }

    /// Check if trigger conditions are met.
    ///
    /// vol_spike_recent (volatility_ratio > threshold) ∧ vol_decaying (decay_rate < 0)
    /// ∧ OBI_aligned (|OBI| > threshold) ∧ regime_kl < threshold
    pub fn is_triggered(&self, features: &FeatureVector, regime_kl: f64) -> bool {
        features.volatility_ratio > self.config.vol_spike_threshold
            && features.volatility_decay_rate < self.config.vol_decaying_threshold
            && features.obi.abs() > self.config.obi_alignment_threshold
            && regime_kl < self.config.regime_kl_threshold
    }

    /// Extract Strategy B's extended feature vector (39-dim).
    ///
    /// Appends 5 strategy-specific features to base 34-dim:
    /// 1. rv_spike × trend (realized_vol × OBI direction)
    /// 2. OFI × intensity (delta_obi × trade_intensity)
    /// 3. p_continue_B (emphasizing momentum continuation signals)
    /// 4. time_decay_B (minutes-scale decay, much slower than A)
    /// 5. vol_ratio × signed_volume (volatility-momentum interaction)
    pub fn extract_features(&self, base: &FeatureVector) -> Vec<f64> {
        let mut phi = base.flattened();
        assert_eq!(phi.len(), FeatureVector::DIM);

        phi.push(base.realized_volatility * base.obi);
        phi.push(base.delta_obi * base.trade_intensity);
        phi.push(self.compute_p_continue_b(base));
        phi.push(self.compute_time_decay_b(base.holding_time_ms));
        phi.push(base.volatility_ratio * base.signed_volume);

        assert_eq!(phi.len(), STRATEGY_B_FEATURE_DIM);
        phi
    }

    fn compute_p_continue_b(&self, base: &FeatureVector) -> f64 {
        // Momentum continuation signal based on:
        // - Vol decaying (negative decay_rate → momentum emerging)
        // - OBI alignment (directional flow)
        // - Trade intensity (participation)
        let vol_decay_signal = if base.volatility_decay_rate < 0.0 {
            (-base.volatility_decay_rate).min(1.0)
        } else {
            0.0
        };
        let obi_signal = base.obi.abs().min(1.0);
        let intensity_signal = (base.trade_intensity / 10.0).min(1.0);
        (vol_decay_signal * 0.4 + obi_signal * 0.35 + intensity_signal * 0.25).clamp(0.0, 1.0)
    }

    fn compute_time_decay_b(&self, holding_time_ms: f64) -> f64 {
        if holding_time_ms <= 0.0 {
            1.0
        } else {
            (-self.config.decay_rate_b * holding_time_ms).exp()
        }
    }

    pub fn start_episode(&mut self, timestamp_ns: u64) {
        self.episode = EpisodeStateB::Active {
            entry_timestamp_ns: timestamp_ns,
        };
    }

    pub fn end_episode(&mut self) {
        self.episode = EpisodeStateB::Idle;
    }

    pub fn should_end_episode(&self, now_ns: u64) -> bool {
        match &self.episode {
            EpisodeStateB::Active { entry_timestamp_ns } => {
                now_ns - *entry_timestamp_ns >= self.config.max_hold_time_ms * 1_000_000
            }
            EpisodeStateB::Idle => false,
        }
    }

    pub fn remaining_hold_time_ms(&self, now_ns: u64) -> u64 {
        match &self.episode {
            EpisodeStateB::Active { entry_timestamp_ns } => {
                let elapsed_ms = (now_ns - *entry_timestamp_ns) / 1_000_000;
                self.config.max_hold_time_ms.saturating_sub(elapsed_ms)
            }
            EpisodeStateB::Idle => 0,
        }
    }

    pub fn episode_state(&self) -> &EpisodeStateB {
        &self.episode
    }

    fn has_position(&self, state: &StateSnapshot) -> bool {
        state
            .positions
            .get(&StrategyId::B)
            .map(|p| p.size.abs() > f64::EPSILON)
            .unwrap_or(false)
    }

    fn record_trade(&mut self, traded: bool) {
        self.trade_history.push(traded);
        if self.trade_history.len() > self.config.trade_frequency_window {
            self.trade_history.remove(0);
        }
    }

    fn trade_frequency(&self) -> f64 {
        if self.trade_history.is_empty() {
            return 0.0;
        }
        self.trade_history.iter().filter(|&&x| x).count() as f64 / self.trade_history.len() as f64
    }

    fn check_hold_degeneration(&self) -> bool {
        if self.total_decisions < self.config.trade_frequency_window {
            return false;
        }
        self.trade_frequency() < self.config.min_trade_frequency
    }

    fn check_action_consistency(&self, q_buy: f64, q_sell: f64) -> bool {
        if q_buy <= 0.0 || q_sell <= 0.0 {
            return false;
        }
        let max_q = q_buy.max(q_sell);
        let diff = (q_buy - q_sell).abs();
        diff / (max_q.abs() + 1e-15) < self.config.consistency_threshold
    }

    fn compute_lot_size(&self, lot_multiplier: f64) -> u64 {
        let effective = (self.config.default_lot_size as f64 * lot_multiplier) as u64;
        effective.clamp(0, self.config.max_lot_size)
    }

    fn select_action(
        &self,
        sampled_q: &HashMap<QAction, f64>,
        buy_allowed: bool,
        sell_allowed: bool,
    ) -> QAction {
        let q_buy = sampled_q[&QAction::Buy];
        let q_sell = sampled_q[&QAction::Sell];
        let q_hold = sampled_q[&QAction::Hold];

        if !buy_allowed && !sell_allowed {
            return QAction::Hold;
        }
        if !buy_allowed {
            return if q_sell > q_hold {
                QAction::Sell
            } else {
                QAction::Hold
            };
        }
        if !sell_allowed {
            return if q_buy > q_hold {
                QAction::Buy
            } else {
                QAction::Hold
            };
        }

        if q_buy >= q_sell && q_buy >= q_hold {
            QAction::Buy
        } else if q_sell >= q_buy && q_sell >= q_hold {
            QAction::Sell
        } else {
            QAction::Hold
        }
    }

    /// Make a trading decision.
    ///
    /// Pipeline:
    /// 1. Episode timeout → force close
    /// 2. Sync episode with position state
    /// 3. Idle + not triggered → skip
    /// 4. Extract extended features φ_B(s)
    /// 5. Thompson Sampling + penalties
    /// 6. Consistency + global constraints
    /// 7. Lot sizing + hold degeneration monitoring
    pub fn decide(
        &mut self,
        base_features: &FeatureVector,
        state: &StateSnapshot,
        regime_kl: f64,
        latency_ms: f64,
        now_ns: u64,
        rng: &mut impl Rng,
    ) -> StrategyBDecision {
        self.total_decisions += 1;

        // Step 1: Episode timeout
        if self.should_end_episode(now_ns) {
            let pos_size = state
                .positions
                .get(&StrategyId::B)
                .map(|p| p.size)
                .unwrap_or(0.0);
            let close_action = if pos_size > f64::EPSILON {
                Action::Sell(pos_size.abs() as u64)
            } else if pos_size < -f64::EPSILON {
                Action::Buy(pos_size.abs() as u64)
            } else {
                Action::Hold
            };
            self.end_episode();
            return StrategyBDecision {
                action: close_action,
                q_point: 0.0,
                q_sampled: 0.0,
                posterior_std: 0.0,
                triggered: false,
                episode_active: false,
                should_close: true,
                skip_reason: Some("MAX_HOLD_TIME exceeded".to_string()),
                remaining_hold_time_ms: 0,
                hold_degeneration_detected: false,
                consistency_fallback: false,
            };
        }

        // Step 2: Sync episode with position
        if self.episode != EpisodeStateB::Idle && !self.has_position(state) {
            self.end_episode();
        }

        // Step 3: Trigger check (only when idle)
        let is_idle = self.episode == EpisodeStateB::Idle;
        let triggered = self.is_triggered(base_features, regime_kl);

        if is_idle && !triggered {
            self.record_trade(false);
            return StrategyBDecision {
                action: Action::Hold,
                q_point: 0.0,
                q_sampled: 0.0,
                posterior_std: 0.0,
                triggered: false,
                episode_active: false,
                should_close: false,
                skip_reason: Some("trigger conditions not met".to_string()),
                remaining_hold_time_ms: 0,
                hold_degeneration_detected: false,
                consistency_fallback: false,
            };
        }

        // Step 4: Extract extended features
        let phi = self.extract_features(base_features);

        // Step 5: Thompson Sampling
        let mut sampled_q_raw: HashMap<QAction, f64> = HashMap::new();
        for &action in QAction::all() {
            let q = self.q_function.sample_q_value(action, &phi, rng);
            sampled_q_raw.insert(action, q);
        }

        let self_impact = base_features.self_impact;
        let dynamic_cost = base_features.dynamic_cost;
        let sigma_noise = self.q_function.model(QAction::Buy).noise_variance().sqrt();
        let non_model_penalty = self.config.non_model_uncertainty_k * sigma_noise;
        let latency_penalty = self.config.latency_penalty_k * latency_ms;

        let mut sampled_q_final: HashMap<QAction, f64> = HashMap::new();
        for &action in QAction::all() {
            let q_raw = sampled_q_raw[&action];
            let penalty = if action == QAction::Hold {
                0.0
            } else {
                self_impact + dynamic_cost + non_model_penalty + latency_penalty
            };
            sampled_q_final.insert(action, q_raw - penalty);
        }

        // Point estimates for monitoring
        let all_point_q = self.q_function.q_values(&phi);
        let posterior_stds = self.q_function.posterior_stds(&phi);

        // Step 6: Consistency check
        let q_buy_final = sampled_q_final[&QAction::Buy];
        let q_sell_final = sampled_q_final[&QAction::Sell];
        let consistency_fallback = self.check_action_consistency(q_buy_final, q_sell_final);

        // Global position constraints
        let buy_allowed = state.global_position + 1.0 <= state.global_position_limit;
        let sell_allowed = state.global_position - 1.0 >= -state.global_position_limit;

        // Step 7: Select action
        let selected = if consistency_fallback {
            QAction::Hold
        } else {
            self.select_action(&sampled_q_final, buy_allowed, sell_allowed)
        };

        // Lot sizing
        let lot_multiplier = state.lot_multiplier;
        let effective_action = if lot_multiplier < 0.01 {
            Action::Hold
        } else {
            match selected {
                QAction::Buy => {
                    let lot = self.compute_lot_size(lot_multiplier);
                    if lot < self.config.min_lot_size {
                        Action::Hold
                    } else {
                        Action::Buy(lot)
                    }
                }
                QAction::Sell => {
                    let lot = self.compute_lot_size(lot_multiplier);
                    if lot < self.config.min_lot_size {
                        Action::Hold
                    } else {
                        Action::Sell(lot)
                    }
                }
                QAction::Hold => Action::Hold,
            }
        };

        let traded = matches!(effective_action, Action::Buy(_) | Action::Sell(_));
        self.record_trade(traded);

        // Start episode on entry from idle
        if is_idle && traded {
            self.start_episode(now_ns);
        }

        // Hold degeneration monitoring
        let hold_degeneration_detected = self.check_hold_degeneration();
        if hold_degeneration_detected {
            self.q_function
                .inflate_covariance(self.config.hold_degeneration_inflation);
        }

        let selected_q_action = match effective_action {
            Action::Buy(_) => QAction::Buy,
            Action::Sell(_) => QAction::Sell,
            Action::Hold => QAction::Hold,
        };

        StrategyBDecision {
            action: effective_action,
            q_point: all_point_q[&selected_q_action],
            q_sampled: sampled_q_final[&selected_q_action],
            posterior_std: posterior_stds[&selected_q_action],
            triggered,
            episode_active: self.episode != EpisodeStateB::Idle,
            should_close: false,
            skip_reason: None,
            remaining_hold_time_ms: self.remaining_hold_time_ms(now_ns),
            hold_degeneration_detected,
            consistency_fallback,
        }
    }

    /// On-policy Q-function update.
    pub fn update(&mut self, action: QAction, phi: &[f64], target: f64) -> UpdateResult {
        self.q_function.update(action, phi, target)
    }

    pub fn q_function(&self) -> &QFunction {
        &self.q_function
    }

    pub fn q_function_mut(&mut self) -> &mut QFunction {
        &mut self.q_function
    }

    pub fn config(&self) -> &StrategyBConfig {
        &self.config
    }

    pub fn reset_trade_tracker(&mut self) {
        self.trade_history.clear();
        self.total_decisions = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use fx_events::projector::{LimitStateData, Position};
    use rand::thread_rng;

    use super::*;

    fn make_config() -> StrategyBConfig {
        StrategyBConfig::default()
    }

    fn make_zero_features() -> FeatureVector {
        FeatureVector::zero()
    }

    fn make_triggered_features() -> FeatureVector {
        let mut fv = FeatureVector::zero();
        fv.volatility_ratio = 3.0;
        fv.volatility_decay_rate = -0.5;
        fv.obi = 0.5;
        fv
    }

    fn make_state() -> StateSnapshot {
        StateSnapshot {
            positions: HashMap::new(),
            global_position: 0.0,
            global_position_limit: 10.0,
            total_unrealized_pnl: 0.0,
            total_realized_pnl: 0.0,
            limit_state: LimitStateData::default(),
            state_version: 0,
            staleness_ms: 0,
            state_hash: String::new(),
            lot_multiplier: 1.0,
            last_market_data_ns: 1_000_000_000,
        }
    }

    fn make_state_with_position(size: f64, entry_ns: u64) -> StateSnapshot {
        let mut positions = HashMap::new();
        positions.insert(
            StrategyId::B,
            Position {
                strategy_id: StrategyId::B,
                size,
                entry_price: 110.0,
                unrealized_pnl: 5.0,
                realized_pnl: 0.0,
                entry_timestamp_ns: entry_ns,
            },
        );
        StateSnapshot {
            positions,
            global_position: size,
            global_position_limit: 10.0,
            total_unrealized_pnl: 5.0,
            total_realized_pnl: 0.0,
            limit_state: LimitStateData::default(),
            state_version: 1,
            staleness_ms: 0,
            state_hash: String::new(),
            lot_multiplier: 1.0,
            last_market_data_ns: entry_ns,
        }
    }

    const NOW_NS: u64 = 1_000_000_000_000;

    // === Trigger condition tests ===

    #[test]
    fn test_trigger_all_conditions_met() {
        let strategy = StrategyB::new(make_config());
        assert!(strategy.is_triggered(&make_triggered_features(), 0.5));
    }

    #[test]
    fn test_trigger_vol_ratio_below() {
        let strategy = StrategyB::new(make_config());
        let mut fv = make_triggered_features();
        fv.volatility_ratio = 1.5;
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_vol_not_decaying() {
        let strategy = StrategyB::new(make_config());
        let mut fv = make_triggered_features();
        fv.volatility_decay_rate = 0.1; // positive = vol increasing, not decaying
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_obi_below_threshold() {
        let strategy = StrategyB::new(make_config());
        let mut fv = make_triggered_features();
        fv.obi = 0.05; // below default 0.1
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_regime_kl_above() {
        let strategy = StrategyB::new(make_config());
        assert!(!strategy.is_triggered(&make_triggered_features(), 1.5));
    }

    #[test]
    fn test_trigger_no_conditions_met() {
        let strategy = StrategyB::new(make_config());
        assert!(!strategy.is_triggered(&make_zero_features(), 0.5));
    }

    #[test]
    fn test_trigger_custom_thresholds() {
        let config = StrategyBConfig {
            vol_spike_threshold: 4.0,
            vol_decaying_threshold: -1.0,
            obi_alignment_threshold: 0.3,
            regime_kl_threshold: 0.3,
            ..make_config()
        };
        let strategy = StrategyB::new(config);
        let fv = make_triggered_features(); // vol_ratio=3, decay=-0.5, obi=0.5
        assert!(!strategy.is_triggered(&fv, 0.5)); // vol_ratio 3 < 4
    }

    #[test]
    fn test_trigger_negative_obi_aligned() {
        let strategy = StrategyB::new(make_config());
        let mut fv = make_triggered_features();
        fv.obi = -0.5; // negative OBI, |obi| > 0.1
        assert!(strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_vol_decaying_at_boundary() {
        let strategy = StrategyB::new(make_config());
        let mut fv = make_triggered_features();
        fv.volatility_decay_rate = 0.0; // at boundary, not strictly < 0
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    // === Feature extraction tests ===

    #[test]
    fn test_extract_features_correct_dimension() {
        let strategy = StrategyB::new(make_config());
        let phi = strategy.extract_features(&make_zero_features());
        assert_eq!(phi.len(), STRATEGY_B_FEATURE_DIM);
        assert_eq!(phi.len(), 39);
    }

    #[test]
    fn test_extract_features_base_preserved() {
        let strategy = StrategyB::new(make_config());
        let base = make_zero_features();
        let phi = strategy.extract_features(&base);
        let base_flat = base.flattened();
        for i in 0..FeatureVector::DIM {
            assert!(
                (phi[i] - base_flat[i]).abs() < 1e-15,
                "base[{}] mismatch: {} vs {}",
                i,
                phi[i],
                base_flat[i]
            );
        }
    }

    #[test]
    fn test_extract_features_rv_spike_x_trend() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.realized_volatility = 0.002;
        base.obi = 0.3;
        let phi = strategy.extract_features(&base);
        let expected = 0.002 * 0.3;
        assert!(
            (phi[FeatureVector::DIM] - expected).abs() < 1e-15,
            "rv_spike×trend: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM]
        );
    }

    #[test]
    fn test_extract_features_ofi_x_intensity() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.delta_obi = 0.1;
        base.trade_intensity = 5.0;
        let phi = strategy.extract_features(&base);
        let expected = 0.1 * 5.0;
        assert!(
            (phi[FeatureVector::DIM + 1] - expected).abs() < 1e-15,
            "OFI×intensity: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 1]
        );
    }

    #[test]
    fn test_extract_features_p_continue_b_zero() {
        let strategy = StrategyB::new(make_config());
        let phi = strategy.extract_features(&make_zero_features());
        let p = phi[FeatureVector::DIM + 2];
        assert!(
            p >= 0.0 && p <= 1.0,
            "p_continue_b should be in [0,1], got {}",
            p
        );
    }

    #[test]
    fn test_extract_features_p_continue_b_high() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.volatility_decay_rate = -0.8;
        base.obi = 0.9;
        base.trade_intensity = 10.0;
        let phi = strategy.extract_features(&base);
        let p = phi[FeatureVector::DIM + 2];
        assert!(
            p > 0.5,
            "p_continue_b should be high for strong momentum signals, got {}",
            p
        );
        assert!(p <= 1.0);
    }

    #[test]
    fn test_extract_features_p_continue_b_clamped_upper() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.volatility_decay_rate = -100.0;
        base.obi = 100.0;
        base.trade_intensity = 1000.0;
        let phi = strategy.extract_features(&base);
        let p = phi[FeatureVector::DIM + 2];
        assert!(p <= 1.0, "p_continue_b should not exceed 1.0, got {}", p);
    }

    #[test]
    fn test_extract_features_time_decay_b_at_zero() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 0.0;
        let phi = strategy.extract_features(&base);
        assert!(
            (phi[FeatureVector::DIM + 3] - 1.0).abs() < 1e-15,
            "time_decay_b at t=0 should be 1.0"
        );
    }

    #[test]
    fn test_extract_features_time_decay_b_decreases() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 300_000.0; // 5 min
        let phi = strategy.extract_features(&base);
        let td = phi[FeatureVector::DIM + 3];
        assert!(
            td < 1.0 && td > 0.0,
            "time_decay_b at 5min should be in (0,1), got {}",
            td
        );
    }

    #[test]
    fn test_extract_features_time_decay_b_slower_than_a() {
        // Compare with Strategy A's decay_rate_a = 0.001
        // Strategy B: decay_rate_b = 0.0001 (10x slower)
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 5000.0;
        let phi = strategy.extract_features(&base);
        let td_b = phi[FeatureVector::DIM + 3];
        // decay_rate_b = 0.0001: exp(-0.0001 * 5000) = exp(-0.5) ≈ 0.607
        // decay_rate_a = 0.001: exp(-0.001 * 5000) = exp(-5) ≈ 0.0067
        assert!(
            td_b > 0.5,
            "Strategy B decay at 5s should be > 0.5 (minutes scale), got {}",
            td_b
        );
    }

    #[test]
    fn test_extract_features_vol_ratio_x_signed_volume() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.volatility_ratio = 3.0;
        base.signed_volume = 1000.0;
        let phi = strategy.extract_features(&base);
        let expected = 3.0 * 1000.0;
        assert!(
            (phi[FeatureVector::DIM + 4] - expected).abs() < 1e-15,
            "vol_ratio×signed_volume: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 4]
        );
    }

    #[test]
    fn test_extract_features_all_finite() {
        let strategy = StrategyB::new(make_config());
        let mut base = make_zero_features();
        base.realized_volatility = 0.05;
        base.obi = -0.99;
        base.delta_obi = 10.0;
        base.trade_intensity = 1000.0;
        base.volatility_ratio = 50.0;
        base.signed_volume = -50000.0;
        base.holding_time_ms = 600_000.0;
        base.volatility_decay_rate = -5.0;
        let phi = strategy.extract_features(&base);
        for (i, &v) in phi.iter().enumerate() {
            assert!(v.is_finite(), "feature[{}] = {} is not finite", i, v);
        }
    }

    // === Episode management tests ===

    #[test]
    fn test_episode_initial_idle() {
        let strategy = StrategyB::new(make_config());
        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);
    }

    #[test]
    fn test_start_episode() {
        let mut strategy = StrategyB::new(make_config());
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.episode_state(),
            &EpisodeStateB::Active {
                entry_timestamp_ns: NOW_NS
            }
        );
    }

    #[test]
    fn test_end_episode() {
        let mut strategy = StrategyB::new(make_config());
        strategy.start_episode(NOW_NS);
        strategy.end_episode();
        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);
    }

    #[test]
    fn test_should_end_within_time() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(!strategy.should_end_episode(NOW_NS + 200_000_000_000));
    }

    #[test]
    fn test_should_end_at_boundary() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(strategy.should_end_episode(NOW_NS + 300_000_000_000));
    }

    #[test]
    fn test_should_end_past_time() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(strategy.should_end_episode(NOW_NS + 600_000_000_000));
    }

    #[test]
    fn test_should_end_idle_returns_false() {
        let strategy = StrategyB::new(make_config());
        assert!(!strategy.should_end_episode(NOW_NS + 999_999_999));
    }

    #[test]
    fn test_remaining_hold_time() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.remaining_hold_time_ms(NOW_NS + 100_000_000_000),
            200_000
        );
    }

    #[test]
    fn test_remaining_hold_time_idle() {
        let strategy = StrategyB::new(make_config());
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS), 0);
    }

    #[test]
    fn test_remaining_hold_time_expired() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS + 600_000_000_000), 0);
    }

    #[test]
    fn test_remaining_hold_time_at_zero() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS), 300_000);
    }

    // === Decision pipeline tests ===

    #[test]
    fn test_decide_idle_not_triggered_skips() {
        let mut strategy = StrategyB::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();
        let decision = strategy.decide(&make_zero_features(), &state, 0.5, 1.0, NOW_NS, &mut rng);
        assert!(matches!(decision.action, Action::Hold));
        assert!(!decision.triggered);
        assert!(!decision.episode_active);
        assert!(decision.skip_reason.is_some());
        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);
    }

    #[test]
    fn test_decide_triggered_produces_decision() {
        let mut strategy = StrategyB::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();
        let decision = strategy.decide(
            &make_triggered_features(),
            &state,
            0.5,
            1.0,
            NOW_NS,
            &mut rng,
        );
        assert!(decision.triggered);
        assert!(decision.skip_reason.is_none());
    }

    #[test]
    fn test_decide_triggered_can_explore() {
        let mut strategy = StrategyB::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();
        let mut directional_count = 0;
        for _ in 0..50 {
            let decision = strategy.decide(
                &make_triggered_features(),
                &state,
                0.5,
                0.0,
                NOW_NS,
                &mut rng,
            );
            match decision.action {
                Action::Buy(_) | Action::Sell(_) => directional_count += 1,
                Action::Hold => {}
            }
        }
        assert!(
            directional_count > 0,
            "Optimistic bias should produce some directional trades"
        );
    }

    #[test]
    fn test_decide_episode_timeout_long_force_close() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        let state = make_state_with_position(1000.0, NOW_NS);
        strategy.start_episode(NOW_NS);

        let mut rng = thread_rng();
        let decision = strategy.decide(
            &make_triggered_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 301_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Sell(lot) if lot == 1000));
        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);
    }

    #[test]
    fn test_decide_episode_timeout_short_force_close() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        let state = make_state_with_position(-500.0, NOW_NS);
        strategy.start_episode(NOW_NS);

        let mut rng = thread_rng();
        let decision = strategy.decide(
            &make_triggered_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 301_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Buy(lot) if lot == 500));
    }

    #[test]
    fn test_decide_episode_timeout_no_position_hold() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        let state = make_state();

        let mut rng = thread_rng();
        let decision = strategy.decide(
            &make_triggered_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 301_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Hold));
    }

    #[test]
    fn test_decide_position_closed_externally_syncs() {
        let mut strategy = StrategyB::new(make_config());
        strategy.start_episode(NOW_NS);
        let state = make_state();

        let mut rng = thread_rng();
        let decision = strategy.decide(
            &make_zero_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 10_000_000_000,
            &mut rng,
        );
        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);
        assert!(!decision.triggered);
        assert!(matches!(decision.action, Action::Hold));
    }

    #[test]
    fn test_decide_active_episode_bypasses_trigger() {
        let mut strategy = StrategyB::new(make_config());
        strategy.start_episode(NOW_NS);
        let state = make_state_with_position(1000.0, NOW_NS);

        let mut rng = thread_rng();
        let decision = strategy.decide(
            &make_zero_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 10_000_000_000,
            &mut rng,
        );
        assert!(decision.episode_active);
        assert!(decision.skip_reason.is_none());
    }

    #[test]
    fn test_decide_entry_starts_episode() {
        let mut strategy = StrategyB::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        let mut found_entry = false;
        for _ in 0..100 {
            let decision = strategy.decide(
                &make_triggered_features(),
                &state,
                0.5,
                0.0,
                NOW_NS,
                &mut rng,
            );
            if matches!(decision.action, Action::Buy(_) | Action::Sell(_)) {
                found_entry = true;
                assert_eq!(
                    strategy.episode_state(),
                    &EpisodeStateB::Active {
                        entry_timestamp_ns: NOW_NS
                    }
                );
                break;
            }
        }
        if found_entry {
            strategy.end_episode();
        }
    }

    #[test]
    fn test_decide_global_position_blocks_buy() {
        let mut strategy = StrategyB::new(make_config());
        let mut state = make_state();
        state.global_position = 10.0;
        state.global_position_limit = 10.0;

        let mut rng = thread_rng();
        for _ in 0..30 {
            let decision = strategy.decide(
                &make_triggered_features(),
                &state,
                0.5,
                0.0,
                NOW_NS,
                &mut rng,
            );
            if let Action::Buy(_) = decision.action {
                panic!("Buy should be blocked at global limit");
            }
        }
    }

    #[test]
    fn test_decide_global_position_blocks_sell() {
        let mut strategy = StrategyB::new(make_config());
        let mut state = make_state();
        state.global_position = -10.0;
        state.global_position_limit = 10.0;

        let mut rng = thread_rng();
        for _ in 0..30 {
            let decision = strategy.decide(
                &make_triggered_features(),
                &state,
                0.5,
                0.0,
                NOW_NS,
                &mut rng,
            );
            if let Action::Sell(_) = decision.action {
                panic!("Sell should be blocked at negative global limit");
            }
        }
    }

    #[test]
    fn test_decide_low_lot_multiplier_forces_hold() {
        let mut strategy = StrategyB::new(make_config());
        let mut state = make_state();
        state.lot_multiplier = 0.005;

        let mut rng = thread_rng();
        for _ in 0..20 {
            let decision = strategy.decide(
                &make_triggered_features(),
                &state,
                0.5,
                0.0,
                NOW_NS,
                &mut rng,
            );
            assert!(
                matches!(decision.action, Action::Hold),
                "Low lot_multiplier should force hold"
            );
        }
    }

    // === Q-function tests ===

    #[test]
    fn test_q_function_dimension() {
        let strategy = StrategyB::new(make_config());
        assert_eq!(strategy.q_function().dim(), STRATEGY_B_FEATURE_DIM);
    }

    #[test]
    fn test_q_function_optimistic_bias() {
        let strategy = StrategyB::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        let q_buy = strategy.q_function().q_value(QAction::Buy, &phi);
        let q_hold = strategy.q_function().q_value(QAction::Hold, &phi);
        assert!(q_buy > q_hold, "Buy should have optimistic bias > Hold");
    }

    #[test]
    fn test_q_function_update() {
        let mut strategy = StrategyB::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        let result = strategy.update(QAction::Buy, &phi, 1.0);
        assert!(!result.diverged);
        assert_eq!(
            strategy.q_function().model(QAction::Buy).n_observations(),
            1
        );
    }

    #[test]
    fn test_q_function_update_with_extended_features() {
        let mut strategy = StrategyB::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        assert_eq!(phi.len(), STRATEGY_B_FEATURE_DIM);

        for i in 0..10 {
            strategy.update(QAction::Buy, &phi, 0.1 * i as f64);
        }
        assert_eq!(
            strategy.q_function().model(QAction::Buy).n_observations(),
            10
        );
        assert_eq!(
            strategy.q_function().model(QAction::Sell).n_observations(),
            0
        );
    }

    // === Configuration tests ===

    #[test]
    fn test_config_defaults() {
        let config = StrategyBConfig::default();
        assert!((config.vol_spike_threshold - 2.0).abs() < 1e-15);
        assert!((config.vol_decaying_threshold - 0.0).abs() < 1e-15);
        assert!((config.obi_alignment_threshold - 0.1).abs() < 1e-15);
        assert!((config.regime_kl_threshold - 1.0).abs() < 1e-15);
        assert_eq!(config.max_hold_time_ms, 300_000);
        assert!((config.decay_rate_b - 0.0001).abs() < 1e-15);
        assert_eq!(config.default_lot_size, 100_000);
        assert_eq!(config.max_lot_size, 1_000_000);
        assert_eq!(config.min_lot_size, 1000);
    }

    #[test]
    fn test_extra_dim_constant() {
        assert_eq!(STRATEGY_B_EXTRA_DIM, 5);
        assert_eq!(STRATEGY_B_FEATURE_DIM, FeatureVector::DIM + 5);
    }

    // === Lot sizing tests ===

    #[test]
    fn test_lot_sizing_full_multiplier() {
        let strategy = StrategyB::new(make_config());
        assert_eq!(strategy.compute_lot_size(1.0), 100_000);
    }

    #[test]
    fn test_lot_sizing_half_multiplier() {
        let strategy = StrategyB::new(make_config());
        assert_eq!(strategy.compute_lot_size(0.5), 50_000);
    }

    #[test]
    fn test_lot_sizing_clamped_to_max() {
        let strategy = StrategyB::new(StrategyBConfig {
            max_lot_size: 500_000,
            ..make_config()
        });
        assert_eq!(strategy.compute_lot_size(10.0), 500_000);
    }

    #[test]
    fn test_lot_sizing_zero_multiplier() {
        let strategy = StrategyB::new(make_config());
        assert_eq!(strategy.compute_lot_size(0.0), 0);
    }

    // === Action consistency tests ===

    #[test]
    fn test_consistency_both_positive_close() {
        let strategy = StrategyB::new(make_config());
        assert!(strategy.check_action_consistency(1.0, 1.02));
    }

    #[test]
    fn test_consistency_far_apart() {
        let strategy = StrategyB::new(make_config());
        assert!(!strategy.check_action_consistency(1.0, 2.0));
    }

    #[test]
    fn test_consistency_one_negative() {
        let strategy = StrategyB::new(make_config());
        assert!(!strategy.check_action_consistency(-1.0, 1.0));
        assert!(!strategy.check_action_consistency(1.0, -1.0));
    }

    #[test]
    fn test_consistency_both_negative() {
        let strategy = StrategyB::new(make_config());
        assert!(!strategy.check_action_consistency(-1.0, -0.98));
    }

    // === Select action tests ===

    #[test]
    fn test_select_action_argmax_buy() {
        let strategy = StrategyB::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 5.0);
        q.insert(QAction::Sell, 3.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Buy);
    }

    #[test]
    fn test_select_action_argmax_sell() {
        let strategy = StrategyB::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 3.0);
        q.insert(QAction::Sell, 5.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Sell);
    }

    #[test]
    fn test_select_action_argmax_hold() {
        let strategy = StrategyB::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, -1.0);
        q.insert(QAction::Sell, -1.0);
        q.insert(QAction::Hold, 0.5);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Hold);
    }

    #[test]
    fn test_select_action_buy_blocked() {
        let strategy = StrategyB::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 10.0);
        q.insert(QAction::Sell, 5.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, false, true), QAction::Sell);
    }

    #[test]
    fn test_select_action_both_blocked() {
        let strategy = StrategyB::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 10.0);
        q.insert(QAction::Sell, 10.0);
        q.insert(QAction::Hold, -5.0);
        assert_eq!(strategy.select_action(&q, false, false), QAction::Hold);
    }

    // === Hold degeneration tests ===

    #[test]
    fn test_hold_degeneration_detected() {
        let config = StrategyBConfig {
            trade_frequency_window: 10,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyB::new(config);
        for _ in 0..10 {
            strategy.record_trade(false);
        }
        strategy.total_decisions = 10;
        assert!(strategy.check_hold_degeneration());
    }

    #[test]
    fn test_hold_degeneration_not_detected_with_trades() {
        let config = StrategyBConfig {
            trade_frequency_window: 10,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyB::new(config);
        for _ in 0..10 {
            strategy.record_trade(true);
        }
        strategy.total_decisions = 10;
        assert!(!strategy.check_hold_degeneration());
    }

    #[test]
    fn test_hold_degeneration_not_checked_early() {
        let config = StrategyBConfig {
            trade_frequency_window: 50,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyB::new(config);
        for _ in 0..50 {
            strategy.record_trade(false);
        }
        strategy.total_decisions = 10;
        assert!(!strategy.check_hold_degeneration());
    }

    // === Reset tests ===

    #[test]
    fn test_reset_trade_tracker() {
        let mut strategy = StrategyB::new(make_config());
        strategy.record_trade(true);
        strategy.total_decisions = 5;
        strategy.reset_trade_tracker();
        assert_eq!(strategy.total_decisions, 0);
    }

    #[test]
    fn test_q_function_reset() {
        let mut strategy = StrategyB::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        strategy.update(QAction::Buy, &phi, 1.0);
        strategy.q_function_mut().reset_all();
        assert_eq!(
            strategy.q_function().model(QAction::Buy).n_observations(),
            0
        );
    }

    // === Episode lifecycle tests ===

    #[test]
    fn test_episode_lifecycle_full() {
        let mut strategy = StrategyB::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);

        let mut entered = false;
        for _ in 0..200 {
            let decision = strategy.decide(
                &make_triggered_features(),
                &state,
                0.5,
                0.0,
                NOW_NS,
                &mut rng,
            );
            if matches!(decision.action, Action::Buy(_) | Action::Sell(_)) {
                entered = true;
                break;
            }
        }
        if entered {
            assert!(matches!(
                strategy.episode_state(),
                EpisodeStateB::Active { .. }
            ));
            strategy.end_episode();
        }
        assert_eq!(strategy.episode_state(), &EpisodeStateB::Idle);
    }

    #[test]
    fn test_decision_has_remaining_time_when_active() {
        let mut strategy = StrategyB::new(StrategyBConfig {
            max_hold_time_ms: 300_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        let state = make_state_with_position(1000.0, NOW_NS);
        let mut rng = thread_rng();

        let decision = strategy.decide(
            &make_zero_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 100_000_000_000,
            &mut rng,
        );
        assert_eq!(decision.remaining_hold_time_ms, 200_000);
    }

    #[test]
    fn test_decision_remaining_time_zero_when_idle() {
        let mut strategy = StrategyB::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        let decision = strategy.decide(&make_zero_features(), &state, 0.5, 1.0, NOW_NS, &mut rng);
        assert_eq!(decision.remaining_hold_time_ms, 0);
    }

    // === p_continue_b signal weight tests ===

    #[test]
    fn test_p_continue_b_signal_weights() {
        let strategy = StrategyB::new(make_config());

        // Pure vol decay signal
        let mut base = FeatureVector::zero();
        base.volatility_decay_rate = -0.5;
        base.obi = 0.0;
        base.trade_intensity = 0.0;
        let p_vol_only = strategy.compute_p_continue_b(&base);

        // Vol decay + OBI
        base.obi = 0.8;
        let p_vol_obi = strategy.compute_p_continue_b(&base);
        assert!(
            p_vol_obi > p_vol_only,
            "Adding OBI signal should increase p_continue_b: {} vs {}",
            p_vol_obi,
            p_vol_only
        );

        // Vol decay + OBI + intensity
        base.trade_intensity = 10.0;
        let p_all = strategy.compute_p_continue_b(&base);
        assert!(
            p_all >= p_vol_obi,
            "Adding intensity signal should not decrease p_continue_b: {} vs {}",
            p_all,
            p_vol_obi
        );
    }

    #[test]
    fn test_p_continue_b_intensity_scaled() {
        let strategy = StrategyB::new(make_config());

        let mut base = FeatureVector::zero();
        base.volatility_decay_rate = -0.5;
        base.obi = 0.5;
        base.trade_intensity = 3.0; // 3/10 = 0.3
        let p_low = strategy.compute_p_continue_b(&base);

        base.trade_intensity = 10.0; // 10/10 = 1.0 (clamped)
        let p_high = strategy.compute_p_continue_b(&base);
        assert!(
            p_high > p_low,
            "Higher trade intensity should increase p_continue_b"
        );
    }

    #[test]
    fn test_p_continue_b_no_decay_no_signal() {
        let strategy = StrategyB::new(make_config());
        let mut base = FeatureVector::zero();
        base.volatility_decay_rate = 0.1; // positive = no decay
        base.obi = 0.0;
        base.trade_intensity = 0.0;
        let p = strategy.compute_p_continue_b(&base);
        assert!(
            (p - 0.0).abs() < 1e-15,
            "No momentum signals should give p_continue_b = 0, got {}",
            p
        );
    }
}
