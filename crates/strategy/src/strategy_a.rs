//! Strategy A: Liquidity Shock Reversion.
//!
//! Detects sudden liquidity dislocations (spread widening, depth drops, volatility spikes)
//! and enters mean-reversion positions expecting price to revert. Uses fast decay
//! (seconds scale) suitable for capturing reversion within seconds to tens of seconds.
//!
//! Trigger: spread_z > 3 ∧ depth_drop < θ₁ ∧ vol_spike > θ₂ ∧ regime_kl < threshold
//! Feature vector: base 38-dim + 5 strategy-specific = 43 dimensions.
//! MAX_HOLD_TIME: seconds to tens of seconds (default 30s).

use std::collections::HashMap;

use fx_core::types::StrategyId;
use fx_events::projector::StateSnapshot;
use rand::Rng;

use crate::bayesian_lr::{QAction, QFunction, UpdateResult};
use crate::features::FeatureVector;
use crate::policy::Action;

/// Number of extra features Strategy A appends to the base FeatureVector.
pub const STRATEGY_A_EXTRA_DIM: usize = 5;

/// Total feature dimension for Strategy A's Q-function (38 + 5 = 43).
pub const STRATEGY_A_FEATURE_DIM: usize = FeatureVector::DIM + STRATEGY_A_EXTRA_DIM;

/// Strategy A configuration.
#[derive(Debug, Clone)]
pub struct StrategyAConfig {
    /// Trigger: spread_zscore must exceed this.
    pub spread_z_threshold: f64,
    /// Trigger: depth_change_rate must be below this (negative = depth drop).
    pub depth_drop_threshold: f64,
    /// Trigger: volatility_ratio must exceed this.
    pub vol_spike_threshold: f64,
    /// Trigger: regime KL divergence must be below this (known regime).
    pub regime_kl_threshold: f64,
    /// Maximum holding time in milliseconds.
    pub max_hold_time_ms: u64,
    /// Strategy A specific decay rate λ_A (seconds scale).
    pub decay_rate_a: f64,
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
    /// Decay rate for inflation when trade frequency recovers.
    pub inflation_decay_rate: f64,
    /// Maximum lot size per order.
    pub max_lot_size: u64,
    /// Minimum lot size (below this → Hold).
    pub min_lot_size: u64,
    /// Action consistency threshold.
    pub consistency_threshold: f64,
    /// Default lot size.
    pub default_lot_size: u64,
}

impl Default for StrategyAConfig {
    fn default() -> Self {
        Self {
            spread_z_threshold: 3.0,
            depth_drop_threshold: -0.2,
            vol_spike_threshold: 3.0,
            regime_kl_threshold: 1.0,
            max_hold_time_ms: 30_000,
            decay_rate_a: 0.001,
            lambda_reg: 0.01,
            halflife: 500,
            initial_sigma2: 0.01,
            optimistic_bias: 0.01,
            non_model_uncertainty_k: 0.1,
            latency_penalty_k: 0.001,
            min_trade_frequency: 0.02,
            trade_frequency_window: 500,
            hold_degeneration_inflation: 1.5,
            inflation_decay_rate: 0.99,
            max_lot_size: 1_000_000,
            min_lot_size: 1000,
            consistency_threshold: 0.05,
            default_lot_size: 100_000,
        }
    }
}

/// Episode state for Strategy A.
#[derive(Debug, Clone, PartialEq)]
pub enum EpisodeState {
    Idle,
    Active { entry_timestamp_ns: u64 },
}

/// Decision output from Strategy A.
#[derive(Debug, Clone)]
pub struct StrategyADecision {
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

/// Strategy A: Liquidity Shock Reversion.
///
/// Detects liquidity shocks via spread widening, depth drops, and volatility spikes,
/// then enters mean-reversion positions. Uses extended feature vector (43-dim) with
/// strategy-specific nonlinear and interaction terms. Episodes are bounded by MAX_HOLD_TIME.
pub struct StrategyA {
    config: StrategyAConfig,
    episode: EpisodeState,
    q_function: QFunction,
    trade_history: Vec<bool>,
    total_decisions: usize,
    current_inflation: f64,
}

impl StrategyA {
    pub fn new(config: StrategyAConfig) -> Self {
        let q_function = QFunction::new(
            STRATEGY_A_FEATURE_DIM,
            config.lambda_reg,
            config.halflife,
            config.initial_sigma2,
            config.optimistic_bias,
        );
        let trade_capacity = config.trade_frequency_window;
        Self {
            config,
            episode: EpisodeState::Idle,
            q_function,
            trade_history: Vec::with_capacity(trade_capacity),
            total_decisions: 0,
            current_inflation: 1.0,
        }
    }

    /// Check if trigger conditions are met.
    ///
    /// spread_z > threshold ∧ depth_drop < threshold ∧ vol_spike > threshold ∧ regime_kl < threshold
    pub fn is_triggered(&self, features: &FeatureVector, regime_kl: f64) -> bool {
        features.spread_zscore > self.config.spread_z_threshold
            && features.depth_change_rate < self.config.depth_drop_threshold
            && features.volatility_ratio > self.config.vol_spike_threshold
            && regime_kl < self.config.regime_kl_threshold
    }

    /// Extract Strategy A's extended feature vector (43-dim).
    ///
    /// Appends 5 strategy-specific features to base 38-dim:
    /// 1. spread_z × OBI
    /// 2. self_impact_A (amplified by depth change magnitude)
    /// 3. p_revert_A (emphasizing liquidity shock signals)
    /// 4. time_decay_A (seconds-scale decay)
    /// 5. depth_drop × realized_volatility (continuous)
    pub fn extract_features(&self, base: &FeatureVector) -> Vec<f64> {
        let mut phi = base.flattened();
        assert_eq!(phi.len(), FeatureVector::DIM);

        phi.push(base.spread_zscore * base.obi);
        phi.push(base.self_impact * (1.0 + base.depth_change_rate.abs().min(2.0)));
        phi.push(self.compute_p_revert_a(base));
        phi.push(self.compute_time_decay_a(base.holding_time_ms));
        phi.push(base.depth_change_rate * base.realized_volatility);

        assert_eq!(phi.len(), STRATEGY_A_FEATURE_DIM);
        phi
    }

    fn compute_p_revert_a(&self, base: &FeatureVector) -> f64 {
        let spread_signal = (base.spread_zscore.abs() / 3.0).min(1.0);
        let depth_signal = if base.depth_change_rate < -0.2 {
            ((-base.depth_change_rate - 0.2) / 0.8).min(1.0)
        } else {
            0.0
        };
        let vol_signal = if base.volatility_ratio > 1.0 {
            ((base.volatility_ratio - 1.0) / 2.0).min(1.0)
        } else {
            0.0
        };
        (spread_signal * 0.4 + depth_signal * 0.35 + vol_signal * 0.25).clamp(0.0, 1.0)
    }

    fn compute_time_decay_a(&self, holding_time_ms: f64) -> f64 {
        if holding_time_ms <= 0.0 {
            1.0
        } else {
            (-self.config.decay_rate_a * holding_time_ms).exp()
        }
    }

    pub fn start_episode(&mut self, timestamp_ns: u64) {
        self.episode = EpisodeState::Active {
            entry_timestamp_ns: timestamp_ns,
        };
    }

    pub fn end_episode(&mut self) {
        self.episode = EpisodeState::Idle;
    }

    pub fn should_end_episode(&self, now_ns: u64) -> bool {
        match &self.episode {
            EpisodeState::Active { entry_timestamp_ns } => {
                now_ns - *entry_timestamp_ns >= self.config.max_hold_time_ms * 1_000_000
            }
            EpisodeState::Idle => false,
        }
    }

    pub fn remaining_hold_time_ms(&self, now_ns: u64) -> u64 {
        match &self.episode {
            EpisodeState::Active { entry_timestamp_ns } => {
                let elapsed_ms = (now_ns - *entry_timestamp_ns) / 1_000_000;
                self.config.max_hold_time_ms.saturating_sub(elapsed_ms)
            }
            EpisodeState::Idle => 0,
        }
    }

    pub fn episode_state(&self) -> &EpisodeState {
        &self.episode
    }

    fn has_position(&self, state: &StateSnapshot) -> bool {
        state
            .positions
            .get(&StrategyId::A)
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
    /// 4. Extract extended features φ_A(s)
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
    ) -> StrategyADecision {
        self.total_decisions += 1;

        // Step 1: Episode timeout
        if self.should_end_episode(now_ns) {
            let pos_size = state
                .positions
                .get(&StrategyId::A)
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
            return StrategyADecision {
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
        if self.episode != EpisodeState::Idle && !self.has_position(state) {
            self.end_episode();
        }

        // Step 3: Trigger check (only when idle)
        let is_idle = self.episode == EpisodeState::Idle;
        let triggered = self.is_triggered(base_features, regime_kl);

        if is_idle && !triggered {
            self.record_trade(false);
            return StrategyADecision {
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
            if self.current_inflation < self.config.hold_degeneration_inflation {
                let ratio = self.config.hold_degeneration_inflation / self.current_inflation;
                self.q_function.inflate_covariance(ratio);
                self.current_inflation = self.config.hold_degeneration_inflation;
            }
        } else if self.current_inflation > 1.0 {
            self.current_inflation =
                1.0 + (self.current_inflation - 1.0) * self.config.inflation_decay_rate;
            if self.current_inflation < 1.001 {
                self.current_inflation = 1.0;
            }
        }

        let selected_q_action = match effective_action {
            Action::Buy(_) => QAction::Buy,
            Action::Sell(_) => QAction::Sell,
            Action::Hold => QAction::Hold,
        };

        StrategyADecision {
            action: effective_action,
            q_point: all_point_q[&selected_q_action],
            q_sampled: sampled_q_final[&selected_q_action],
            posterior_std: posterior_stds[&selected_q_action],
            triggered,
            episode_active: self.episode != EpisodeState::Idle,
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

    pub fn config(&self) -> &StrategyAConfig {
        &self.config
    }

    pub fn current_inflation(&self) -> f64 {
        self.current_inflation
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

    fn make_config() -> StrategyAConfig {
        StrategyAConfig::default()
    }

    fn make_zero_features() -> FeatureVector {
        FeatureVector::zero()
    }

    fn make_triggered_features() -> FeatureVector {
        let mut fv = FeatureVector::zero();
        fv.spread_zscore = 4.0;
        fv.depth_change_rate = -0.5;
        fv.volatility_ratio = 4.0;
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
            StrategyId::A,
            Position {
                strategy_id: StrategyId::A,
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
        let strategy = StrategyA::new(make_config());
        assert!(strategy.is_triggered(&make_triggered_features(), 0.5));
    }

    #[test]
    fn test_trigger_spread_z_below() {
        let strategy = StrategyA::new(make_config());
        let mut fv = make_triggered_features();
        fv.spread_zscore = 2.0;
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_depth_not_negative_enough() {
        let strategy = StrategyA::new(make_config());
        let mut fv = make_triggered_features();
        fv.depth_change_rate = -0.1;
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_vol_ratio_below() {
        let strategy = StrategyA::new(make_config());
        let mut fv = make_triggered_features();
        fv.volatility_ratio = 2.0;
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_regime_kl_above() {
        let strategy = StrategyA::new(make_config());
        assert!(!strategy.is_triggered(&make_triggered_features(), 1.5));
    }

    #[test]
    fn test_trigger_no_conditions_met() {
        let strategy = StrategyA::new(make_config());
        assert!(!strategy.is_triggered(&make_zero_features(), 0.5));
    }

    #[test]
    fn test_trigger_custom_thresholds() {
        let config = StrategyAConfig {
            spread_z_threshold: 5.0,
            depth_drop_threshold: -0.8,
            vol_spike_threshold: 5.0,
            regime_kl_threshold: 0.3,
            ..make_config()
        };
        let strategy = StrategyA::new(config);
        let fv = make_triggered_features(); // spread_z=4, depth=-0.5, vol=4
        assert!(!strategy.is_triggered(&fv, 0.2)); // spread_z 4 < 5
        assert!(!strategy.is_triggered(&fv, 0.5)); // regime_kl 0.5 > 0.3
    }

    // === Feature extraction tests ===

    #[test]
    fn test_extract_features_correct_dimension() {
        let strategy = StrategyA::new(make_config());
        let phi = strategy.extract_features(&make_zero_features());
        assert_eq!(phi.len(), STRATEGY_A_FEATURE_DIM);
        assert_eq!(phi.len(), 43);
    }

    #[test]
    fn test_extract_features_base_preserved() {
        let strategy = StrategyA::new(make_config());
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
    fn test_extract_features_spread_z_x_obi() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.spread_zscore = 3.5;
        base.obi = -0.4;
        let phi = strategy.extract_features(&base);
        let expected = 3.5 * -0.4;
        assert!(
            (phi[FeatureVector::DIM] - expected).abs() < 1e-15,
            "spread_z×OBI: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM]
        );
    }

    #[test]
    fn test_extract_features_self_impact_a() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.self_impact = 0.5;
        base.depth_change_rate = -0.5;
        let phi = strategy.extract_features(&base);
        // amplification = 1 + min(|-0.5|, 2.0) = 1.5
        let expected = 0.5 * 1.5;
        assert!(
            (phi[FeatureVector::DIM + 1] - expected).abs() < 1e-15,
            "self_impact_a: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 1]
        );
    }

    #[test]
    fn test_extract_features_self_impact_a_depth_clamped() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.self_impact = 0.5;
        base.depth_change_rate = -5.0; // |depth| = 5, clamped to 2.0
        let phi = strategy.extract_features(&base);
        let expected = 0.5 * 3.0;
        assert!(
            (phi[FeatureVector::DIM + 1] - expected).abs() < 1e-15,
            "self_impact_a clamped: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 1]
        );
    }

    #[test]
    fn test_extract_features_self_impact_a_no_depth_change() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.self_impact = 0.8;
        base.depth_change_rate = 0.0;
        let phi = strategy.extract_features(&base);
        let expected = 0.8 * 1.0;
        assert!(
            (phi[FeatureVector::DIM + 1] - expected).abs() < 1e-15,
            "self_impact_a no depth: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 1]
        );
    }

    #[test]
    fn test_extract_features_p_revert_a_zero() {
        let strategy = StrategyA::new(make_config());
        let phi = strategy.extract_features(&make_zero_features());
        let p = phi[FeatureVector::DIM + 2];
        assert!(
            p >= 0.0 && p <= 1.0,
            "p_revert_a should be in [0,1], got {}",
            p
        );
    }

    #[test]
    fn test_extract_features_p_revert_a_high() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.spread_zscore = 6.0;
        base.depth_change_rate = -0.8;
        base.volatility_ratio = 3.0;
        let phi = strategy.extract_features(&base);
        let p = phi[FeatureVector::DIM + 2];
        assert!(
            p > 0.5,
            "p_revert_a should be high for strong signals, got {}",
            p
        );
        assert!(p <= 1.0);
    }

    #[test]
    fn test_extract_features_p_revert_a_clamped_upper() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.spread_zscore = 100.0;
        base.depth_change_rate = -10.0;
        base.volatility_ratio = 100.0;
        let phi = strategy.extract_features(&base);
        let p = phi[FeatureVector::DIM + 2];
        assert!(p <= 1.0, "p_revert_a should not exceed 1.0, got {}", p);
    }

    #[test]
    fn test_extract_features_time_decay_a_at_zero() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 0.0;
        let phi = strategy.extract_features(&base);
        assert!(
            (phi[FeatureVector::DIM + 3] - 1.0).abs() < 1e-15,
            "time_decay_a at t=0 should be 1.0"
        );
    }

    #[test]
    fn test_extract_features_time_decay_a_decreases() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 5000.0;
        let phi = strategy.extract_features(&base);
        let td = phi[FeatureVector::DIM + 3];
        assert!(
            td < 1.0 && td > 0.0,
            "time_decay_a at 5s should be in (0,1), got {}",
            td
        );
    }

    #[test]
    fn test_extract_features_time_decay_a_slower_than_base() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 500.0; // 500ms
        let phi = strategy.extract_features(&base);
        let td_a = phi[FeatureVector::DIM + 3];
        // decay_rate_a = 0.001: exp(-0.001 * 500) = exp(-0.5) ≈ 0.607
        // base decay_rate = 0.01: exp(-0.01 * 500) = exp(-5) ≈ 0.0067
        // Strategy A decay should be much larger (slower decay)
        assert!(
            td_a > 0.5,
            "Strategy A decay at 500ms should still be > 0.5 (seconds scale), got {}",
            td_a
        );
    }

    #[test]
    fn test_extract_features_depth_drop_x_vol() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.depth_change_rate = -0.5;
        base.realized_volatility = 0.001;
        let phi = strategy.extract_features(&base);
        let expected = -0.5 * 0.001;
        assert!(
            (phi[FeatureVector::DIM + 4] - expected).abs() < 1e-15,
            "depth_drop×vol: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 4]
        );
    }

    #[test]
    fn test_extract_features_all_finite() {
        let strategy = StrategyA::new(make_config());
        let mut base = make_zero_features();
        base.spread_zscore = 10.0;
        base.obi = -0.99;
        base.depth_change_rate = -3.0;
        base.realized_volatility = 0.05;
        base.self_impact = 5.0;
        base.holding_time_ms = 60_000.0;
        let phi = strategy.extract_features(&base);
        for (i, &v) in phi.iter().enumerate() {
            assert!(v.is_finite(), "feature[{}] = {} is not finite", i, v);
        }
    }

    // === Episode management tests ===

    #[test]
    fn test_episode_initial_idle() {
        let strategy = StrategyA::new(make_config());
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);
    }

    #[test]
    fn test_start_episode() {
        let mut strategy = StrategyA::new(make_config());
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.episode_state(),
            &EpisodeState::Active {
                entry_timestamp_ns: NOW_NS
            }
        );
    }

    #[test]
    fn test_end_episode() {
        let mut strategy = StrategyA::new(make_config());
        strategy.start_episode(NOW_NS);
        strategy.end_episode();
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);
    }

    #[test]
    fn test_should_end_within_time() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(!strategy.should_end_episode(NOW_NS + 20_000_000_000));
    }

    #[test]
    fn test_should_end_at_boundary() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(strategy.should_end_episode(NOW_NS + 30_000_000_000));
    }

    #[test]
    fn test_should_end_past_time() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(strategy.should_end_episode(NOW_NS + 60_000_000_000));
    }

    #[test]
    fn test_should_end_idle_returns_false() {
        let strategy = StrategyA::new(make_config());
        assert!(!strategy.should_end_episode(NOW_NS + 999_999_999));
    }

    #[test]
    fn test_remaining_hold_time() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.remaining_hold_time_ms(NOW_NS + 10_000_000_000),
            20_000
        );
    }

    #[test]
    fn test_remaining_hold_time_idle() {
        let strategy = StrategyA::new(make_config());
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS), 0);
    }

    #[test]
    fn test_remaining_hold_time_expired() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS + 60_000_000_000), 0);
    }

    #[test]
    fn test_remaining_hold_time_at_zero() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS), 30_000);
    }

    // === Decision pipeline tests ===

    #[test]
    fn test_decide_idle_not_triggered_skips() {
        let mut strategy = StrategyA::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();
        let decision = strategy.decide(&make_zero_features(), &state, 0.5, 1.0, NOW_NS, &mut rng);
        assert!(matches!(decision.action, Action::Hold));
        assert!(!decision.triggered);
        assert!(!decision.episode_active);
        assert!(decision.skip_reason.is_some());
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);
    }

    #[test]
    fn test_decide_triggered_produces_decision() {
        let mut strategy = StrategyA::new(make_config());
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
        let mut strategy = StrategyA::new(make_config());
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
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
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
            NOW_NS + 31_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Sell(lot) if lot == 1000));
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);
    }

    #[test]
    fn test_decide_episode_timeout_short_force_close() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
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
            NOW_NS + 31_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Buy(lot) if lot == 500));
    }

    #[test]
    fn test_decide_episode_timeout_no_position_hold() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
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
            NOW_NS + 31_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Hold));
    }

    #[test]
    fn test_decide_position_closed_externally_syncs() {
        let mut strategy = StrategyA::new(make_config());
        strategy.start_episode(NOW_NS);
        let state = make_state(); // no position

        let mut rng = thread_rng();
        // Use non-triggered features so strategy stays Idle after sync
        let decision = strategy.decide(
            &make_zero_features(),
            &state,
            0.5,
            1.0,
            NOW_NS + 10_000_000_000,
            &mut rng,
        );
        // Episode should have synced to Idle (no position found)
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);
        assert!(!decision.triggered);
        assert!(matches!(decision.action, Action::Hold));
    }

    #[test]
    fn test_decide_active_episode_bypasses_trigger() {
        let mut strategy = StrategyA::new(make_config());
        strategy.start_episode(NOW_NS);
        let state = make_state_with_position(1000.0, NOW_NS);

        let mut rng = thread_rng();
        // Zero features don't meet trigger, but episode is active
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
        let mut strategy = StrategyA::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        // Force a directional action by using triggered features with zero latency
        // and checking that episode starts when action is directional
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
                    &EpisodeState::Active {
                        entry_timestamp_ns: NOW_NS
                    }
                );
                break;
            }
        }
        // Not guaranteed but very likely with optimistic bias
        if found_entry {
            strategy.end_episode(); // cleanup
        }
    }

    #[test]
    fn test_decide_global_position_blocks_buy() {
        let mut strategy = StrategyA::new(make_config());
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
        let mut strategy = StrategyA::new(make_config());
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
        let mut strategy = StrategyA::new(make_config());
        let mut state = make_state();
        state.lot_multiplier = 0.005; // below 0.01 threshold

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
        let strategy = StrategyA::new(make_config());
        assert_eq!(strategy.q_function().dim(), STRATEGY_A_FEATURE_DIM);
    }

    #[test]
    fn test_q_function_optimistic_bias() {
        let strategy = StrategyA::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        let q_buy = strategy.q_function().q_value(QAction::Buy, &phi);
        let q_hold = strategy.q_function().q_value(QAction::Hold, &phi);
        assert!(q_buy > q_hold, "Buy should have optimistic bias > Hold");
    }

    #[test]
    fn test_q_function_update() {
        let mut strategy = StrategyA::new(make_config());
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
        let mut strategy = StrategyA::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        assert_eq!(phi.len(), STRATEGY_A_FEATURE_DIM);

        for i in 0..10 {
            strategy.update(QAction::Buy, &phi, 0.1 * i as f64);
        }
        assert_eq!(
            strategy.q_function().model(QAction::Buy).n_observations(),
            10
        );
        // Sell model should be untouched
        assert_eq!(
            strategy.q_function().model(QAction::Sell).n_observations(),
            0
        );
    }

    // === Configuration tests ===

    #[test]
    fn test_config_defaults() {
        let config = StrategyAConfig::default();
        assert!((config.spread_z_threshold - 3.0).abs() < 1e-15);
        assert!((config.depth_drop_threshold - (-0.2)).abs() < 1e-15);
        assert!((config.vol_spike_threshold - 3.0).abs() < 1e-15);
        assert!((config.regime_kl_threshold - 1.0).abs() < 1e-15);
        assert_eq!(config.max_hold_time_ms, 30_000);
        assert!((config.decay_rate_a - 0.001).abs() < 1e-15);
        assert_eq!(config.default_lot_size, 100_000);
        assert_eq!(config.max_lot_size, 1_000_000);
        assert_eq!(config.min_lot_size, 1000);
    }

    #[test]
    fn test_extra_dim_constant() {
        assert_eq!(STRATEGY_A_EXTRA_DIM, 5);
        assert_eq!(STRATEGY_A_FEATURE_DIM, FeatureVector::DIM + 5);
    }

    // === Lot sizing tests ===

    #[test]
    fn test_lot_sizing_full_multiplier() {
        let strategy = StrategyA::new(make_config());
        assert_eq!(strategy.compute_lot_size(1.0), 100_000);
    }

    #[test]
    fn test_lot_sizing_half_multiplier() {
        let strategy = StrategyA::new(make_config());
        assert_eq!(strategy.compute_lot_size(0.5), 50_000);
    }

    #[test]
    fn test_lot_sizing_clamped_to_max() {
        let strategy = StrategyA::new(StrategyAConfig {
            max_lot_size: 500_000,
            ..make_config()
        });
        assert_eq!(strategy.compute_lot_size(10.0), 500_000);
    }

    #[test]
    fn test_lot_sizing_zero_multiplier() {
        let strategy = StrategyA::new(make_config());
        assert_eq!(strategy.compute_lot_size(0.0), 0);
    }

    // === Action consistency tests ===

    #[test]
    fn test_consistency_both_positive_close() {
        let strategy = StrategyA::new(make_config());
        assert!(strategy.check_action_consistency(1.0, 1.02));
    }

    #[test]
    fn test_consistency_far_apart() {
        let strategy = StrategyA::new(make_config());
        assert!(!strategy.check_action_consistency(1.0, 2.0));
    }

    #[test]
    fn test_consistency_one_negative() {
        let strategy = StrategyA::new(make_config());
        assert!(!strategy.check_action_consistency(-1.0, 1.0));
        assert!(!strategy.check_action_consistency(1.0, -1.0));
    }

    #[test]
    fn test_consistency_both_negative() {
        let strategy = StrategyA::new(make_config());
        assert!(!strategy.check_action_consistency(-1.0, -0.98));
    }

    // === Select action tests ===

    #[test]
    fn test_select_action_argmax_buy() {
        let strategy = StrategyA::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 5.0);
        q.insert(QAction::Sell, 3.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Buy);
    }

    #[test]
    fn test_select_action_argmax_sell() {
        let strategy = StrategyA::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 3.0);
        q.insert(QAction::Sell, 5.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Sell);
    }

    #[test]
    fn test_select_action_argmax_hold() {
        let strategy = StrategyA::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, -1.0);
        q.insert(QAction::Sell, -1.0);
        q.insert(QAction::Hold, 0.5);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Hold);
    }

    #[test]
    fn test_select_action_buy_blocked() {
        let strategy = StrategyA::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 10.0);
        q.insert(QAction::Sell, 5.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, false, true), QAction::Sell);
    }

    #[test]
    fn test_select_action_both_blocked() {
        let strategy = StrategyA::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 10.0);
        q.insert(QAction::Sell, 10.0);
        q.insert(QAction::Hold, -5.0);
        assert_eq!(strategy.select_action(&q, false, false), QAction::Hold);
    }

    // === Hold degeneration tests ===

    #[test]
    fn test_hold_degeneration_detected() {
        let config = StrategyAConfig {
            trade_frequency_window: 10,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyA::new(config);
        for _ in 0..10 {
            strategy.record_trade(false);
        }
        strategy.total_decisions = 10;
        assert!(strategy.check_hold_degeneration());
    }

    #[test]
    fn test_hold_degeneration_not_detected_with_trades() {
        let config = StrategyAConfig {
            trade_frequency_window: 10,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyA::new(config);
        for _ in 0..10 {
            strategy.record_trade(true);
        }
        strategy.total_decisions = 10;
        assert!(!strategy.check_hold_degeneration());
    }

    #[test]
    fn test_hold_degeneration_not_checked_early() {
        let config = StrategyAConfig {
            trade_frequency_window: 50,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyA::new(config);
        for _ in 0..50 {
            strategy.record_trade(false);
        }
        strategy.total_decisions = 10;
        assert!(!strategy.check_hold_degeneration());
    }

    // === Reset tests ===

    #[test]
    fn test_reset_trade_tracker() {
        let mut strategy = StrategyA::new(make_config());
        strategy.record_trade(true);
        strategy.total_decisions = 5;
        strategy.reset_trade_tracker();
        assert_eq!(strategy.total_decisions, 0);
    }

    #[test]
    fn test_q_function_reset() {
        let mut strategy = StrategyA::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        strategy.update(QAction::Buy, &phi, 1.0);
        strategy.q_function_mut().reset_all();
        assert_eq!(
            strategy.q_function().model(QAction::Buy).n_observations(),
            0
        );
    }

    // === Episode state with triggered features tests ===

    #[test]
    fn test_episode_lifecycle_full() {
        let mut strategy = StrategyA::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        // Initially idle
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);

        // Trigger met but not guaranteed to trade on first try due to TS
        // Force an entry by using high optimistic bias
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
                EpisodeState::Active { .. }
            ));
            strategy.end_episode();
        }
        assert_eq!(strategy.episode_state(), &EpisodeState::Idle);
    }

    #[test]
    fn test_decision_has_remaining_time_when_active() {
        let mut strategy = StrategyA::new(StrategyAConfig {
            max_hold_time_ms: 30_000,
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
            NOW_NS + 10_000_000_000,
            &mut rng,
        );
        assert_eq!(decision.remaining_hold_time_ms, 20_000);
    }

    #[test]
    fn test_decision_remaining_time_zero_when_idle() {
        let mut strategy = StrategyA::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        let decision = strategy.decide(&make_zero_features(), &state, 0.5, 1.0, NOW_NS, &mut rng);
        assert_eq!(decision.remaining_hold_time_ms, 0);
    }

    #[test]
    fn test_p_revert_a_signal_weights() {
        let strategy = StrategyA::new(make_config());

        // Pure spread signal
        let mut base = FeatureVector::zero();
        base.spread_zscore = 3.0;
        base.depth_change_rate = 0.0;
        base.volatility_ratio = 1.0;
        let p_spread_only = strategy.compute_p_revert_a(&base);

        // Spread + depth
        base.depth_change_rate = -1.0;
        let p_spread_depth = strategy.compute_p_revert_a(&base);
        assert!(
            p_spread_depth > p_spread_only,
            "Adding depth signal should increase p_revert_a: {} vs {}",
            p_spread_depth,
            p_spread_only
        );

        // Spread + depth + vol
        base.volatility_ratio = 3.0;
        let p_all = strategy.compute_p_revert_a(&base);
        assert!(
            p_all >= p_spread_depth,
            "Adding vol signal should not decrease p_revert_a: {} vs {}",
            p_all,
            p_spread_depth
        );
    }

    #[test]
    fn test_p_revert_a_depth_signal_scaled() {
        let strategy = StrategyA::new(make_config());

        let mut base = FeatureVector::zero();
        base.depth_change_rate = -0.6; // (-0.6 - 0.2) / 0.8 = 0.5
        let p_half = strategy.compute_p_revert_a(&base);

        base.depth_change_rate = -1.0; // (-1.0 - 0.2) / 0.8 = 1.0 (clamped)
        let p_full = strategy.compute_p_revert_a(&base);
        assert!(
            p_full > p_half,
            "Stronger depth drop should increase p_revert_a"
        );
    }

    #[test]
    fn test_p_revert_a_negative_depth_no_signal() {
        let strategy = StrategyA::new(make_config());
        let mut base = FeatureVector::zero();
        base.depth_change_rate = -0.1; // > -0.2, no depth signal
        base.spread_zscore = 0.0;
        base.volatility_ratio = 1.0;
        let p = strategy.compute_p_revert_a(&base);
        assert!(
            (p - 0.0).abs() < 1e-15,
            "No signals should give p_revert_a = 0, got {}",
            p
        );
    }
}
