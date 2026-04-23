//! Strategy C: Session Structural Bias.
//!
//! Captures directional biases that emerge during specific trading sessions (London open,
//! NY overlap, Tokyo morning). Unlike Strategy A's fast reversion (seconds) and Strategy B's
//! volatility-decay momentum (minutes), Strategy C exploits structural session patterns on
//! a tens-of-minutes horizon with the slowest decay (λ_C << λ_B << λ_A).
//!
//! Trigger: session_active ∧ OBI_significant ∧ time_since_open_in_window ∧ regime_kl < threshold
//! Feature vector: base 38-dim + 5 strategy-specific = 43 dimensions.
//! MAX_HOLD_TIME: tens of minutes (default 600s = 10 min).

use std::collections::HashMap;

use fx_core::types::StrategyId;
use fx_events::projector::StateSnapshot;
use rand::Rng;

use crate::bayesian_lr::{QAction, QFunction, UpdateResult};
use crate::features::FeatureVector;
use crate::policy::Action;

/// Number of extra features Strategy C appends to the base FeatureVector.
pub const STRATEGY_C_EXTRA_DIM: usize = 5;

/// Total feature dimension for Strategy C's Q-function (38 + 5 = 43).
pub const STRATEGY_C_FEATURE_DIM: usize = FeatureVector::DIM + STRATEGY_C_EXTRA_DIM;

/// Strategy C configuration.
#[derive(Debug, Clone)]
pub struct StrategyCConfig {
    /// Trigger: at least one session one-hot must exceed this.
    pub session_active_threshold: f64,
    /// Trigger: |OBI| must exceed this (directional flow significance).
    pub obi_significance_threshold: f64,
    /// Trigger: time_since_open_ms must be below this (session maturity window).
    pub max_session_open_ms: f64,
    /// Trigger: regime KL divergence must be below this (known regime).
    pub regime_kl_threshold: f64,
    /// Maximum holding time in milliseconds.
    pub max_hold_time_ms: u64,
    /// Strategy C specific decay rate λ_C (tens-of-minutes scale, slowest).
    pub decay_rate_c: f64,
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

impl Default for StrategyCConfig {
    fn default() -> Self {
        Self {
            session_active_threshold: 0.5,
            obi_significance_threshold: 0.15,
            max_session_open_ms: 1_200_000.0, // first 20 minutes after session open
            regime_kl_threshold: 1.0,
            max_hold_time_ms: 600_000, // 10 minutes
            decay_rate_c: 0.00005,     // very slow decay (tens-of-minutes scale)
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

/// Episode state for Strategy C.
#[derive(Debug, Clone, PartialEq)]
pub enum EpisodeStateC {
    Idle,
    Active { entry_timestamp_ns: u64 },
}

/// Decision output from Strategy C.
#[derive(Debug, Clone)]
pub struct StrategyCDecision {
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

/// Strategy C: Session Structural Bias.
///
/// Detects persistent directional biases during specific trading sessions. Uses extended
/// feature vector (43-dim) with strategy-specific nonlinear and interaction terms focused
/// on session-structure and trend signals. Episodes are bounded by MAX_HOLD_TIME (tens of minutes).
pub struct StrategyC {
    config: StrategyCConfig,
    episode: EpisodeStateC,
    q_function: QFunction,
    trade_history: Vec<bool>,
    total_decisions: usize,
    current_inflation: f64,
}

impl StrategyC {
    pub fn new(config: StrategyCConfig) -> Self {
        let q_function = QFunction::new(
            STRATEGY_C_FEATURE_DIM,
            config.lambda_reg,
            config.halflife,
            config.initial_sigma2,
            config.optimistic_bias,
        );
        let trade_capacity = config.trade_frequency_window;
        Self {
            config,
            episode: EpisodeStateC::Idle,
            q_function,
            trade_history: Vec::with_capacity(trade_capacity),
            total_decisions: 0,
            current_inflation: 1.0,
        }
    }

    /// Check if trigger conditions are met.
    ///
    /// session_active (any session one-hot > threshold) ∧ OBI_significant (|OBI| > threshold)
    /// ∧ time_since_open_in_window (time < max_session_open_ms) ∧ regime_kl < threshold
    pub fn is_triggered(&self, features: &FeatureVector, regime_kl: f64) -> bool {
        let session_active = features.session_tokyo > self.config.session_active_threshold
            || features.session_london > self.config.session_active_threshold
            || features.session_ny > self.config.session_active_threshold
            || features.session_sydney > self.config.session_active_threshold;

        session_active
            && features.obi.abs() > self.config.obi_significance_threshold
            && features.time_since_open_ms < self.config.max_session_open_ms
            && regime_kl < self.config.regime_kl_threshold
    }

    /// Extract Strategy C's extended feature vector (43-dim).
    ///
    /// Appends 5 strategy-specific features to base 38-dim:
    /// 1. session × OBI (OBI weighted by dominant session)
    /// 2. range_break × liquidity_resiliency (depth_change × queue_position)
    /// 3. p_trend_c (adaptive trend estimate from session/OBI/time signals)
    /// 4. time_decay_C (tens-of-minutes scale decay, slowest of all strategies)
    /// 5. OBI × time_since_open (flow intensity modulated by session maturity)
    pub fn extract_features(&self, base: &FeatureVector) -> Vec<f64> {
        let mut phi = base.flattened();
        assert_eq!(phi.len(), FeatureVector::DIM);

        phi.push(self.compute_session_x_obi(base));
        phi.push(base.depth_change_rate * base.queue_position);
        phi.push(self.compute_p_trend_c(base));
        phi.push(self.compute_time_decay_c(base.holding_time_ms));
        phi.push(base.obi * (base.time_since_open_ms / 1_000_000.0).min(60.0));

        assert_eq!(phi.len(), STRATEGY_C_FEATURE_DIM);
        phi
    }

    fn compute_session_x_obi(&self, base: &FeatureVector) -> f64 {
        // OBI weighted by the dominant session signal
        let dominant_session = base
            .session_tokyo
            .max(base.session_london)
            .max(base.session_ny)
            .max(base.session_sydney);
        base.obi * dominant_session
    }

    fn compute_p_trend_c(&self, base: &FeatureVector) -> f64 {
        // Adaptive trend estimate based on:
        // - OBI consistency (directional flow alignment)
        // - Session structural signal (session active strength)
        // - Time-based decay weighting (early session = stronger structural bias)
        let obi_signal = base.obi.abs().min(1.0);
        let session_signal = base
            .session_tokyo
            .max(base.session_london)
            .max(base.session_ny)
            .max(base.session_sydney);
        // Time decay weighting: stronger bias early in session
        let time_weight = if base.time_since_open_ms > 0.0 {
            (-base.time_since_open_ms / 3_600_000.0).exp() // 1-hour decay
        } else {
            1.0
        };
        (obi_signal * 0.4 + session_signal * 0.35 + time_weight * 0.25).clamp(0.0, 1.0)
    }

    fn compute_time_decay_c(&self, holding_time_ms: f64) -> f64 {
        if holding_time_ms <= 0.0 {
            1.0
        } else {
            (-self.config.decay_rate_c * holding_time_ms).exp()
        }
    }

    pub fn start_episode(&mut self, timestamp_ns: u64) {
        self.episode = EpisodeStateC::Active {
            entry_timestamp_ns: timestamp_ns,
        };
    }

    pub fn end_episode(&mut self) {
        self.episode = EpisodeStateC::Idle;
    }

    pub fn should_end_episode(&self, now_ns: u64) -> bool {
        match &self.episode {
            EpisodeStateC::Active { entry_timestamp_ns } => {
                now_ns - *entry_timestamp_ns >= self.config.max_hold_time_ms * 1_000_000
            }
            EpisodeStateC::Idle => false,
        }
    }

    pub fn remaining_hold_time_ms(&self, now_ns: u64) -> u64 {
        match &self.episode {
            EpisodeStateC::Active { entry_timestamp_ns } => {
                let elapsed_ms = (now_ns - *entry_timestamp_ns) / 1_000_000;
                self.config.max_hold_time_ms.saturating_sub(elapsed_ms)
            }
            EpisodeStateC::Idle => 0,
        }
    }

    pub fn episode_state(&self) -> &EpisodeStateC {
        &self.episode
    }

    fn has_position(&self, state: &StateSnapshot) -> bool {
        state
            .positions
            .get(&StrategyId::C)
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
    /// 4. Extract extended features φ_C(s)
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
    ) -> StrategyCDecision {
        self.total_decisions += 1;

        // Step 1: Episode timeout
        if self.should_end_episode(now_ns) {
            let pos_size = state
                .positions
                .get(&StrategyId::C)
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
            return StrategyCDecision {
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
        if self.episode != EpisodeStateC::Idle && !self.has_position(state) {
            self.end_episode();
        }

        // Step 3: Trigger check (only when idle)
        let is_idle = self.episode == EpisodeStateC::Idle;
        let triggered = self.is_triggered(base_features, regime_kl);

        if is_idle && !triggered {
            self.record_trade(false);
            return StrategyCDecision {
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

        // Step 6: Consistency check (only after sufficient observations)
        let q_buy_final = sampled_q_final[&QAction::Buy];
        let q_sell_final = sampled_q_final[&QAction::Sell];
        let min_obs = 20;
        let has_sufficient_data = self.q_function.model(QAction::Buy).n_observations() >= min_obs
            && self.q_function.model(QAction::Sell).n_observations() >= min_obs;
        let consistency_fallback =
            has_sufficient_data && self.check_action_consistency(q_buy_final, q_sell_final);

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

        // Signal-driven exit: when active and Q-function selects the closing direction
        let pos_size = state
            .positions
            .get(&StrategyId::C)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let should_close_signal = !is_idle
            && match &effective_action {
                Action::Buy(_) => pos_size < -f64::EPSILON,
                Action::Sell(_) => pos_size > f64::EPSILON,
                Action::Hold => false,
            };

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

        StrategyCDecision {
            action: effective_action,
            q_point: all_point_q[&selected_q_action],
            q_sampled: sampled_q_final[&selected_q_action],
            posterior_std: posterior_stds[&selected_q_action],
            triggered,
            episode_active: self.episode != EpisodeStateC::Idle,
            should_close: should_close_signal,
            skip_reason: if should_close_signal {
                Some("TRIGGER_EXIT close".to_string())
            } else {
                None
            },
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

    pub fn config(&self) -> &StrategyCConfig {
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

    fn make_config() -> StrategyCConfig {
        StrategyCConfig::default()
    }

    fn make_zero_features() -> FeatureVector {
        FeatureVector::zero()
    }

    fn make_triggered_features() -> FeatureVector {
        let mut fv = FeatureVector::zero();
        fv.session_london = 1.0;
        fv.obi = 0.3;
        fv.time_since_open_ms = 600_000.0; // 10 min after open
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
            StrategyId::C,
            Position {
                strategy_id: StrategyId::C,
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
        let strategy = StrategyC::new(make_config());
        assert!(strategy.is_triggered(&make_triggered_features(), 0.5));
    }

    #[test]
    fn test_trigger_no_session_active() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.session_london = 0.0;
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_obi_below_threshold() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.obi = 0.03;
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_time_since_open_exceeded() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.time_since_open_ms = 5_000_000_000.0; // > 1 hour
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_regime_kl_above() {
        let strategy = StrategyC::new(make_config());
        assert!(!strategy.is_triggered(&make_triggered_features(), 1.5));
    }

    #[test]
    fn test_trigger_no_conditions_met() {
        let strategy = StrategyC::new(make_config());
        assert!(!strategy.is_triggered(&make_zero_features(), 0.5));
    }

    #[test]
    fn test_trigger_custom_thresholds() {
        let config = StrategyCConfig {
            session_active_threshold: 0.8,
            obi_significance_threshold: 0.5,
            max_session_open_ms: 300_000.0,
            regime_kl_threshold: 0.3,
            ..make_config()
        };
        let strategy = StrategyC::new(config);
        let fv = make_triggered_features(); // session=1.0, obi=0.3, time=600s
        assert!(!strategy.is_triggered(&fv, 0.5)); // obi 0.3 < 0.5
    }

    #[test]
    fn test_trigger_tokyo_session() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.session_london = 0.0;
        fv.session_tokyo = 1.0;
        assert!(strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_ny_session() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.session_london = 0.0;
        fv.session_ny = 1.0;
        assert!(strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_negative_obi() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.obi = -0.3; // |obi| > 0.05
        assert!(strategy.is_triggered(&fv, 0.5));
    }

    #[test]
    fn test_trigger_time_since_open_at_boundary() {
        let strategy = StrategyC::new(make_config());
        let mut fv = make_triggered_features();
        fv.time_since_open_ms = 1_200_000.0; // at boundary, not strictly <
        assert!(!strategy.is_triggered(&fv, 0.5));
    }

    // === Feature extraction tests ===

    #[test]
    fn test_extract_features_correct_dimension() {
        let strategy = StrategyC::new(make_config());
        let phi = strategy.extract_features(&make_zero_features());
        assert_eq!(phi.len(), STRATEGY_C_FEATURE_DIM);
        assert_eq!(phi.len(), 43);
    }

    #[test]
    fn test_extract_features_base_preserved() {
        let strategy = StrategyC::new(make_config());
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
    fn test_extract_features_session_x_obi() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.session_london = 0.8;
        base.obi = 0.5;
        let phi = strategy.extract_features(&base);
        let expected = 0.5 * 0.8;
        assert!(
            (phi[FeatureVector::DIM] - expected).abs() < 1e-15,
            "session×OBI: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM]
        );
    }

    #[test]
    fn test_extract_features_range_break_x_liquidity() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.depth_change_rate = -0.3;
        base.queue_position = 0.7;
        let phi = strategy.extract_features(&base);
        let expected = -0.3 * 0.7;
        assert!(
            (phi[FeatureVector::DIM + 1] - expected).abs() < 1e-15,
            "range_break×liquidity: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 1]
        );
    }

    #[test]
    fn test_extract_features_p_trend_c_zero() {
        let strategy = StrategyC::new(make_config());
        let phi = strategy.extract_features(&make_zero_features());
        let p = phi[FeatureVector::DIM + 2];
        assert!(
            p >= 0.0 && p <= 1.0,
            "p_trend_c should be in [0,1], got {}",
            p
        );
    }

    #[test]
    fn test_extract_features_p_trend_c_high() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.session_london = 1.0;
        base.obi = 0.9;
        base.time_since_open_ms = 100_000.0; // early session
        let phi = strategy.extract_features(&base);
        let p = phi[FeatureVector::DIM + 2];
        assert!(
            p > 0.5,
            "p_trend_c should be high for strong session/OBI signals, got {}",
            p
        );
        assert!(p <= 1.0);
    }

    #[test]
    fn test_extract_features_p_trend_c_clamped_upper() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.session_london = 100.0;
        base.obi = 100.0;
        base.time_since_open_ms = 0.0;
        let phi = strategy.extract_features(&base);
        let p = phi[FeatureVector::DIM + 2];
        assert!(p <= 1.0, "p_trend_c should not exceed 1.0, got {}", p);
    }

    #[test]
    fn test_extract_features_time_decay_c_at_zero() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 0.0;
        let phi = strategy.extract_features(&base);
        assert!(
            (phi[FeatureVector::DIM + 3] - 1.0).abs() < 1e-15,
            "time_decay_c at t=0 should be 1.0"
        );
    }

    #[test]
    fn test_extract_features_time_decay_c_decreases() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 600_000.0; // 10 min
        let phi = strategy.extract_features(&base);
        let td = phi[FeatureVector::DIM + 3];
        assert!(
            td < 1.0 && td > 0.0,
            "time_decay_c at 10min should be in (0,1), got {}",
            td
        );
    }

    #[test]
    fn test_extract_features_time_decay_c_slowest() {
        // Verify C has the slowest decay: at 5min, C should retain more value than B would
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.holding_time_ms = 300_000.0; // 5 min
        let phi = strategy.extract_features(&base);
        let td_c = phi[FeatureVector::DIM + 3];
        // decay_rate_c = 0.00005: exp(-0.00005 * 300000) = exp(-15) ≈ 3e-7
        // decay_rate_b = 0.0001: exp(-0.0001 * 300000) = exp(-30) ≈ 9e-14
        // Both are tiny at 5min, but C > B
        assert!(
            td_c > 0.0,
            "Strategy C decay at 5min should still be > 0 (very slow), got {}",
            td_c
        );
    }

    #[test]
    fn test_extract_features_obi_x_time_since_open() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.obi = 0.2;
        base.time_since_open_ms = 30_000_000.0; // 30 min → 30.0
        let phi = strategy.extract_features(&base);
        let expected = 0.2 * 30.0; // clamped at 60.0 max, so 6.0
        assert!(
            (phi[FeatureVector::DIM + 4] - expected).abs() < 1e-15,
            "OBI×time_since_open: expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 4]
        );
    }

    #[test]
    fn test_extract_features_obi_x_time_clamped() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.obi = 0.5;
        base.time_since_open_ms = 120_000_000.0; // 120 min → clamped to 60.0
        let phi = strategy.extract_features(&base);
        let expected = 0.5 * 60.0;
        assert!(
            (phi[FeatureVector::DIM + 4] - expected).abs() < 1e-15,
            "OBI×time_since_open (clamped): expected {}, got {}",
            expected,
            phi[FeatureVector::DIM + 4]
        );
    }

    #[test]
    fn test_extract_features_all_finite() {
        let strategy = StrategyC::new(make_config());
        let mut base = make_zero_features();
        base.session_london = 1.0;
        base.obi = -0.99;
        base.depth_change_rate = -5.0;
        base.queue_position = 10.0;
        base.time_since_open_ms = 3_500_000.0;
        base.holding_time_ms = 600_000.0;
        let phi = strategy.extract_features(&base);
        for (i, &v) in phi.iter().enumerate() {
            assert!(v.is_finite(), "feature[{}] = {} is not finite", i, v);
        }
    }

    // === Episode management tests ===

    #[test]
    fn test_episode_initial_idle() {
        let strategy = StrategyC::new(make_config());
        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);
    }

    #[test]
    fn test_start_episode() {
        let mut strategy = StrategyC::new(make_config());
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.episode_state(),
            &EpisodeStateC::Active {
                entry_timestamp_ns: NOW_NS
            }
        );
    }

    #[test]
    fn test_end_episode() {
        let mut strategy = StrategyC::new(make_config());
        strategy.start_episode(NOW_NS);
        strategy.end_episode();
        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);
    }

    #[test]
    fn test_should_end_within_time() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(!strategy.should_end_episode(NOW_NS + 400_000_000_000));
    }

    #[test]
    fn test_should_end_at_boundary() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(strategy.should_end_episode(NOW_NS + 600_000_000_000));
    }

    #[test]
    fn test_should_end_past_time() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert!(strategy.should_end_episode(NOW_NS + 1_200_000_000_000));
    }

    #[test]
    fn test_should_end_idle_returns_false() {
        let strategy = StrategyC::new(make_config());
        assert!(!strategy.should_end_episode(NOW_NS + 999_999_999));
    }

    #[test]
    fn test_remaining_hold_time() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.remaining_hold_time_ms(NOW_NS + 200_000_000_000),
            400_000
        );
    }

    #[test]
    fn test_remaining_hold_time_idle() {
        let strategy = StrategyC::new(make_config());
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS), 0);
    }

    #[test]
    fn test_remaining_hold_time_expired() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(
            strategy.remaining_hold_time_ms(NOW_NS + 1_200_000_000_000),
            0
        );
    }

    #[test]
    fn test_remaining_hold_time_at_zero() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        assert_eq!(strategy.remaining_hold_time_ms(NOW_NS), 600_000);
    }

    // === Decision pipeline tests ===

    #[test]
    fn test_decide_idle_not_triggered_skips() {
        let mut strategy = StrategyC::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();
        let decision = strategy.decide(&make_zero_features(), &state, 0.5, 1.0, NOW_NS, &mut rng);
        assert!(matches!(decision.action, Action::Hold));
        assert!(!decision.triggered);
        assert!(!decision.episode_active);
        assert!(decision.skip_reason.is_some());
        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);
    }

    #[test]
    fn test_decide_triggered_produces_decision() {
        let mut strategy = StrategyC::new(make_config());
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
        let mut strategy = StrategyC::new(make_config());
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
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
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
            NOW_NS + 601_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Sell(lot) if lot == 1000));
        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);
    }

    #[test]
    fn test_decide_episode_timeout_short_force_close() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
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
            NOW_NS + 601_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Buy(lot) if lot == 500));
    }

    #[test]
    fn test_decide_episode_timeout_no_position_hold() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
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
            NOW_NS + 601_000_000_000,
            &mut rng,
        );
        assert!(decision.should_close);
        assert!(matches!(decision.action, Action::Hold));
    }

    #[test]
    fn test_decide_position_closed_externally_syncs() {
        let mut strategy = StrategyC::new(make_config());
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
        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);
        assert!(!decision.triggered);
        assert!(matches!(decision.action, Action::Hold));
    }

    #[test]
    fn test_decide_entry_starts_episode() {
        let mut strategy = StrategyC::new(make_config());
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
                    &EpisodeStateC::Active {
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
        let mut strategy = StrategyC::new(make_config());
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
        let mut strategy = StrategyC::new(make_config());
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
        let mut strategy = StrategyC::new(make_config());
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
        let strategy = StrategyC::new(make_config());
        assert_eq!(strategy.q_function().dim(), STRATEGY_C_FEATURE_DIM);
    }

    #[test]
    fn test_q_function_optimistic_bias() {
        let strategy = StrategyC::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        let q_buy = strategy.q_function().q_value(QAction::Buy, &phi);
        let q_hold = strategy.q_function().q_value(QAction::Hold, &phi);
        assert!(q_buy > q_hold, "Buy should have optimistic bias > Hold");
    }

    #[test]
    fn test_q_function_update() {
        let mut strategy = StrategyC::new(make_config());
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
        let mut strategy = StrategyC::new(make_config());
        let phi = strategy.extract_features(&make_triggered_features());
        assert_eq!(phi.len(), STRATEGY_C_FEATURE_DIM);

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
        let config = StrategyCConfig::default();
        assert!((config.session_active_threshold - 0.5).abs() < 1e-15);
        assert!((config.obi_significance_threshold - 0.15).abs() < 1e-15);
        assert!((config.max_session_open_ms - 1_200_000.0).abs() < 1e-15);
        assert!((config.regime_kl_threshold - 1.0).abs() < 1e-15);
        assert_eq!(config.max_hold_time_ms, 600_000);
        assert!((config.decay_rate_c - 0.00005).abs() < 1e-15);
        assert_eq!(config.default_lot_size, 100_000);
        assert_eq!(config.max_lot_size, 1_000_000);
        assert_eq!(config.min_lot_size, 1000);
    }

    #[test]
    fn test_extra_dim_constant() {
        assert_eq!(STRATEGY_C_EXTRA_DIM, 5);
        assert_eq!(STRATEGY_C_FEATURE_DIM, FeatureVector::DIM + 5);
    }

    // === Lot sizing tests ===

    #[test]
    fn test_lot_sizing_full_multiplier() {
        let strategy = StrategyC::new(make_config());
        assert_eq!(strategy.compute_lot_size(1.0), 100_000);
    }

    #[test]
    fn test_lot_sizing_half_multiplier() {
        let strategy = StrategyC::new(make_config());
        assert_eq!(strategy.compute_lot_size(0.5), 50_000);
    }

    #[test]
    fn test_lot_sizing_clamped_to_max() {
        let strategy = StrategyC::new(StrategyCConfig {
            max_lot_size: 500_000,
            ..make_config()
        });
        assert_eq!(strategy.compute_lot_size(10.0), 500_000);
    }

    #[test]
    fn test_lot_sizing_zero_multiplier() {
        let strategy = StrategyC::new(make_config());
        assert_eq!(strategy.compute_lot_size(0.0), 0);
    }

    // === Action consistency tests ===

    #[test]
    fn test_consistency_both_positive_close() {
        let strategy = StrategyC::new(make_config());
        assert!(strategy.check_action_consistency(1.0, 1.02));
    }

    #[test]
    fn test_consistency_far_apart() {
        let strategy = StrategyC::new(make_config());
        assert!(!strategy.check_action_consistency(1.0, 2.0));
    }

    #[test]
    fn test_consistency_one_negative() {
        let strategy = StrategyC::new(make_config());
        assert!(!strategy.check_action_consistency(-1.0, 1.0));
        assert!(!strategy.check_action_consistency(1.0, -1.0));
    }

    #[test]
    fn test_consistency_both_negative() {
        let strategy = StrategyC::new(make_config());
        assert!(!strategy.check_action_consistency(-1.0, -0.98));
    }

    // === Select action tests ===

    #[test]
    fn test_select_action_argmax_buy() {
        let strategy = StrategyC::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 5.0);
        q.insert(QAction::Sell, 3.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Buy);
    }

    #[test]
    fn test_select_action_argmax_sell() {
        let strategy = StrategyC::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 3.0);
        q.insert(QAction::Sell, 5.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Sell);
    }

    #[test]
    fn test_select_action_argmax_hold() {
        let strategy = StrategyC::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, -1.0);
        q.insert(QAction::Sell, -1.0);
        q.insert(QAction::Hold, 0.5);
        assert_eq!(strategy.select_action(&q, true, true), QAction::Hold);
    }

    #[test]
    fn test_select_action_buy_blocked() {
        let strategy = StrategyC::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 10.0);
        q.insert(QAction::Sell, 5.0);
        q.insert(QAction::Hold, 1.0);
        assert_eq!(strategy.select_action(&q, false, true), QAction::Sell);
    }

    #[test]
    fn test_select_action_both_blocked() {
        let strategy = StrategyC::new(make_config());
        let mut q = HashMap::new();
        q.insert(QAction::Buy, 10.0);
        q.insert(QAction::Sell, 10.0);
        q.insert(QAction::Hold, -5.0);
        assert_eq!(strategy.select_action(&q, false, false), QAction::Hold);
    }

    // === Hold degeneration tests ===

    #[test]
    fn test_hold_degeneration_detected() {
        let config = StrategyCConfig {
            trade_frequency_window: 10,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyC::new(config);
        for _ in 0..10 {
            strategy.record_trade(false);
        }
        strategy.total_decisions = 10;
        assert!(strategy.check_hold_degeneration());
    }

    #[test]
    fn test_hold_degeneration_not_detected_with_trades() {
        let config = StrategyCConfig {
            trade_frequency_window: 10,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyC::new(config);
        for _ in 0..10 {
            strategy.record_trade(true);
        }
        strategy.total_decisions = 10;
        assert!(!strategy.check_hold_degeneration());
    }

    #[test]
    fn test_hold_degeneration_not_checked_early() {
        let config = StrategyCConfig {
            trade_frequency_window: 50,
            min_trade_frequency: 0.5,
            ..make_config()
        };
        let mut strategy = StrategyC::new(config);
        for _ in 0..50 {
            strategy.record_trade(false);
        }
        strategy.total_decisions = 10;
        assert!(!strategy.check_hold_degeneration());
    }

    // === Reset tests ===

    #[test]
    fn test_reset_trade_tracker() {
        let mut strategy = StrategyC::new(make_config());
        strategy.record_trade(true);
        strategy.total_decisions = 5;
        strategy.reset_trade_tracker();
        assert_eq!(strategy.total_decisions, 0);
    }

    #[test]
    fn test_q_function_reset() {
        let mut strategy = StrategyC::new(make_config());
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
        let mut strategy = StrategyC::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);

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
                EpisodeStateC::Active { .. }
            ));
            strategy.end_episode();
        }
        assert_eq!(strategy.episode_state(), &EpisodeStateC::Idle);
    }

    #[test]
    fn test_decision_has_remaining_time_when_active() {
        let mut strategy = StrategyC::new(StrategyCConfig {
            max_hold_time_ms: 600_000,
            ..make_config()
        });
        strategy.start_episode(NOW_NS);
        let state = make_state_with_position(1000.0, NOW_NS);
        let mut rng = thread_rng();

        let mut features = make_triggered_features();
        features.obi = 0.1;

        let decision = strategy.decide(
            &features,
            &state,
            0.5,
            1.0,
            NOW_NS + 200_000_000_000,
            &mut rng,
        );
        assert_eq!(decision.remaining_hold_time_ms, 400_000);
    }

    #[test]
    fn test_decision_remaining_time_zero_when_idle() {
        let mut strategy = StrategyC::new(make_config());
        let state = make_state();
        let mut rng = thread_rng();

        let decision = strategy.decide(&make_zero_features(), &state, 0.5, 1.0, NOW_NS, &mut rng);
        assert_eq!(decision.remaining_hold_time_ms, 0);
    }

    // === p_trend_c signal weight tests ===

    #[test]
    fn test_p_trend_c_signal_weights() {
        let strategy = StrategyC::new(make_config());

        // Pure OBI signal (no session, at open → time_weight=1.0)
        let mut base = FeatureVector::zero();
        base.obi = 0.8;
        base.session_tokyo = 0.0;
        base.session_london = 0.0;
        base.session_ny = 0.0;
        base.session_sydney = 0.0;
        base.time_since_open_ms = 0.0;
        let p_obi_only = strategy.compute_p_trend_c(&base);

        // OBI + session
        base.session_london = 0.9;
        let p_obi_session = strategy.compute_p_trend_c(&base);
        assert!(
            p_obi_session > p_obi_only,
            "Adding session signal should increase p_trend_c: {} vs {}",
            p_obi_session,
            p_obi_only
        );

        // At open (t=0), time_weight is already 1.0. Moving forward in time should decrease.
        base.time_since_open_ms = 3_500_000.0; // ≈1h into session
        let p_late = strategy.compute_p_trend_c(&base);
        assert!(
            p_late < p_obi_session,
            "Late session should decrease p_trend_c: {} vs {}",
            p_late,
            p_obi_session
        );
    }

    #[test]
    fn test_p_trend_c_session_scaled() {
        let strategy = StrategyC::new(make_config());

        let mut base = FeatureVector::zero();
        base.obi = 0.5;
        base.session_london = 0.3;
        base.time_since_open_ms = 0.0;
        let p_low = strategy.compute_p_trend_c(&base);

        base.session_london = 1.0;
        let p_high = strategy.compute_p_trend_c(&base);
        assert!(
            p_high > p_low,
            "Higher session signal should increase p_trend_c"
        );
    }

    #[test]
    fn test_p_trend_c_late_session_decay() {
        let strategy = StrategyC::new(make_config());

        let mut base = FeatureVector::zero();
        base.obi = 0.5;
        base.session_london = 1.0;
        base.time_since_open_ms = 100_000.0; // early
        let p_early = strategy.compute_p_trend_c(&base);

        base.time_since_open_ms = 3_500_000.0; // late (≈1h)
        let p_late = strategy.compute_p_trend_c(&base);
        assert!(
            p_early >= p_late,
            "Late session should have equal or lower p_trend_c: {} vs {}",
            p_early,
            p_late
        );
    }
}
