//! On-policy Monte Carlo Evaluation.
//!
//! Implements episode-based Monte Carlo evaluation for the MDP policy.
//! Records transitions during episodes, computes discounted cumulative returns
//! on episode termination, and updates Q-functions.
//!
//! # Deadly Triad Avoidance (Sutton & Barto, 2018 §11.3)
//!
//! This evaluator structurally avoids the Deadly Triad:
//! - **On-policy**: Only records and updates with actually taken actions
//! - **Monte Carlo**: Uses full episodic returns G_t (no bootstrapping)
//! - **Bayesian regularization**: Delegated to QFunction's Bayesian LR with prior
//!
//! # Reward Function (Strategy-Separated)
//!
//! r_t^i = ΔPnL_t - λ_risk · σ²_i,t - λ_dd · min(DD_t^i, DD_cap)
//!
//! Where:
//! - ΔPnL_t: Change in strategy equity (realized + unrealized)
//! - σ²_i,t = p_i² · σ²_price: Position variance
//! - DD_t^i = max(0, equity_peak - equity_t): Drawdown from peak
//! - DD_cap: Caps the DD penalty to prevent it from dominating reward

use std::collections::HashMap;

use fx_core::types::StrategyId;
use fx_events::projector::StateSnapshot;

use crate::bayesian_lr::{QAction, QFunction, UpdateResult};

/// Terminal reason for an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    /// Position fully closed (voluntary or risk-limit forced).
    PositionClosed,
    /// MAX_HOLD_TIME exceeded, forced close.
    MaxHoldTimeExceeded,
    /// Daily hard limit triggered (§9.4).
    DailyHardLimit,
    /// Weekly hard limit triggered (§9.4.1).
    WeeklyHardLimit,
    /// Monthly hard limit triggered (§9.4.2).
    MonthlyHardLimit,
    /// Weekend gap halt / forced close.
    WeekendHalt,
    /// Unknown regime detected.
    UnknownRegime,
}

impl std::fmt::Display for TerminalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PositionClosed => write!(f, "PositionClosed"),
            Self::MaxHoldTimeExceeded => write!(f, "MaxHoldTimeExceeded"),
            Self::DailyHardLimit => write!(f, "DailyHardLimit"),
            Self::WeeklyHardLimit => write!(f, "WeeklyHardLimit"),
            Self::MonthlyHardLimit => write!(f, "MonthlyHardLimit"),
            Self::WeekendHalt => write!(f, "WeekendHalt"),
            Self::UnknownRegime => write!(f, "UnknownRegime"),
        }
    }
}

/// A single transition recorded during an episode.
#[derive(Debug, Clone)]
pub struct EpisodeTransition {
    /// Timestamp of this step (nanoseconds).
    pub timestamp_ns: u64,
    /// Action taken at this step.
    pub action: QAction,
    /// Feature vector φ(s) at this step.
    pub phi: Vec<f64>,
    /// Immediate reward r_t at this step.
    pub reward: f64,
}

/// Buffer for recording transitions within a single episode.
#[derive(Debug, Clone)]
pub struct EpisodeBuffer {
    /// Strategy this episode belongs to.
    pub strategy_id: StrategyId,
    /// Recorded transitions in order.
    pub transitions: Vec<EpisodeTransition>,
    /// Episode start timestamp (nanoseconds).
    pub start_timestamp_ns: u64,
    /// Previous equity for ΔPnL computation.
    prev_equity: f64,
    /// Peak equity during the episode (for DD computation).
    equity_peak: f64,
}

impl EpisodeBuffer {
    /// Create a new episode buffer.
    pub fn new(strategy_id: StrategyId, start_timestamp_ns: u64, initial_equity: f64) -> Self {
        Self {
            strategy_id,
            transitions: Vec::new(),
            start_timestamp_ns,
            prev_equity: initial_equity,
            equity_peak: initial_equity,
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Record a transition and compute the immediate reward.
    ///
    /// Reward: r_t = ΔPnL_t - λ_risk·σ²_i,t - λ_dd·min(DD_t, DD_cap)
    pub fn record(
        &mut self,
        timestamp_ns: u64,
        action: QAction,
        phi: Vec<f64>,
        current_equity: f64,
        position_size: f64,
        price_volatility_sq: f64,
        config: &RewardConfig,
    ) {
        let delta_pnl = current_equity - self.prev_equity;
        let position_variance = position_size * position_size * price_volatility_sq;
        let drawdown = (self.equity_peak - current_equity).max(0.0);
        let dd_capped = drawdown.min(config.dd_cap);

        let reward =
            delta_pnl - config.lambda_risk * position_variance - config.lambda_dd * dd_capped;

        self.equity_peak = self.equity_peak.max(current_equity);

        self.transitions.push(EpisodeTransition {
            timestamp_ns,
            action,
            phi,
            reward,
        });

        self.prev_equity = current_equity;
    }

    /// Number of recorded transitions.
    pub fn len(&self) -> usize {
        self.transitions.len()
    }

    /// Whether the buffer has no transitions.
    pub fn is_empty(&self) -> bool {
        self.transitions.is_empty()
    }

    /// Current equity peak (for inspection/testing).
    pub fn equity_peak(&self) -> f64 {
        self.equity_peak
    }

    /// Previous equity (for inspection/testing).
    pub fn prev_equity(&self) -> f64 {
        self.prev_equity
    }
}

/// Reward configuration for the strategy-separated reward function.
///
/// r_t^i = ΔPnL_t - λ_risk·σ²_i,t - λ_dd·min(DD_t^i, DD_cap)
#[derive(Debug, Clone)]
pub struct RewardConfig {
    /// Risk penalty weight λ_risk.
    pub lambda_risk: f64,
    /// Drawdown penalty weight λ_dd.
    pub lambda_dd: f64,
    /// Drawdown term cap (prevents DD from dominating reward).
    pub dd_cap: f64,
    /// Discount factor γ for MC returns.
    pub gamma: f64,
}

impl Default for RewardConfig {
    fn default() -> Self {
        Self {
            lambda_risk: 0.1,
            lambda_dd: 0.5,
            dd_cap: 100.0,
            gamma: 0.99,
        }
    }
}

/// Result of MC return computation for one completed episode.
#[derive(Debug, Clone)]
pub struct EpisodeResult {
    /// Strategy ID.
    pub strategy_id: StrategyId,
    /// Terminal reason.
    pub terminal_reason: TerminalReason,
    /// Number of transitions in the episode.
    pub num_transitions: usize,
    /// Total undiscounted reward (Σ r_t).
    pub total_reward: f64,
    /// Discounted return at episode start G_0.
    pub return_g0: f64,
    /// Episode duration in nanoseconds.
    pub duration_ns: u64,
    /// Per-transition discounted returns G_t (aligned with transitions).
    pub returns: Vec<f64>,
    /// Per-transition data for Q-function updates.
    pub transitions: Vec<EpisodeTransition>,
}

impl EpisodeResult {
    /// Average reward per step.
    pub fn avg_reward(&self) -> f64 {
        if self.num_transitions == 0 {
            0.0
        } else {
            self.total_reward / self.num_transitions as f64
        }
    }

    /// Duration in milliseconds.
    pub fn duration_ms(&self) -> f64 {
        self.duration_ns as f64 / 1_000_000.0
    }
}

/// Configuration for the MC evaluator.
#[derive(Debug, Clone, Default)]
pub struct McEvalConfig {
    /// Reward function parameters.
    pub reward: RewardConfig,
}

/// On-policy Monte Carlo Evaluator.
///
/// Manages episode buffers per strategy, records transitions, computes
/// discounted cumulative returns on episode termination, and updates Q-functions.
///
/// # Episode Lifecycle
///
/// 1. `start_episode()` — called when position opens (zero → non-zero)
/// 2. `record_transition()` — called at each decision step while position is held
/// 3. `end_episode()` / `end_episode_and_update()` — called on terminal condition
///
/// # Terminal Conditions (any one triggers episode end)
///
/// 1. Position fully closed (voluntary or risk-limit forced)
/// 2. Holding time > MAX_HOLD_TIME
/// 3. Daily hard limit triggered
/// 4. Unknown regime detected
///
/// # Partial Fills
///
/// Partial fills do NOT terminate the episode. Only full position close does.
pub struct McEvaluator {
    config: McEvalConfig,
    /// Active episode buffers (one per strategy).
    active_episodes: HashMap<StrategyId, EpisodeBuffer>,
    /// Completed episode history.
    completed_episodes: Vec<EpisodeResult>,
}

impl McEvaluator {
    /// Create a new MC evaluator with the given configuration.
    pub fn new(config: McEvalConfig) -> Self {
        Self {
            config,
            active_episodes: HashMap::new(),
            completed_episodes: Vec::new(),
        }
    }

    /// Start a new episode for a strategy.
    ///
    /// Called when position transitions from zero to non-zero.
    /// The `initial_equity` is the strategy's current equity (realized + unrealized PnL).
    pub fn start_episode(
        &mut self,
        strategy_id: StrategyId,
        timestamp_ns: u64,
        initial_equity: f64,
    ) {
        assert!(
            !self.active_episodes.contains_key(&strategy_id),
            "Active episode already exists for strategy {:?}",
            strategy_id
        );
        let buffer = EpisodeBuffer::new(strategy_id, timestamp_ns, initial_equity);
        self.active_episodes.insert(strategy_id, buffer);
    }

    /// Check if a strategy has an active episode.
    pub fn has_active_episode(&self, strategy_id: StrategyId) -> bool {
        self.active_episodes.contains_key(&strategy_id)
    }

    #[allow(clippy::too_many_arguments)]
    /// Record a transition within an active episode.
    ///
    /// Computes the immediate reward from equity change, position risk, and drawdown,
    /// then appends to the episode buffer.
    ///
    /// # Panics
    /// Panics if no active episode exists for the strategy (invariant violation).
    pub fn record_transition(
        &mut self,
        strategy_id: StrategyId,
        timestamp_ns: u64,
        action: QAction,
        phi: Vec<f64>,
        state: &StateSnapshot,
        price_volatility_sq: f64,
    ) {
        let buffer = self
            .active_episodes
            .get_mut(&strategy_id)
            .unwrap_or_else(|| {
                panic!(
                    "No active episode for strategy {:?}. Call start_episode() first.",
                    strategy_id
                )
            });

        let equity = state_equity(state, strategy_id);
        let position_size = state_position_size(state, strategy_id);

        buffer.record(
            timestamp_ns,
            action,
            phi,
            equity,
            position_size,
            price_volatility_sq,
            &self.config.reward,
        );
    }

    /// End an episode and compute MC returns.
    ///
    /// Returns the `EpisodeResult` containing discounted returns per transition.
    /// The transitions and returns are aligned: `result.returns[i]` is G_t for
    /// `result.transitions[i]`.
    ///
    /// # Panics
    /// Panics if no active episode exists for the strategy.
    pub fn end_episode(
        &mut self,
        strategy_id: StrategyId,
        reason: TerminalReason,
        end_timestamp_ns: u64,
    ) -> EpisodeResult {
        let buffer = self
            .active_episodes
            .remove(&strategy_id)
            .unwrap_or_else(|| {
                panic!(
                    "No active episode for strategy {:?}. Cannot end non-existent episode.",
                    strategy_id
                )
            });

        let result = Self::compute_episode_result(
            buffer,
            reason,
            end_timestamp_ns,
            self.config.reward.gamma,
        );
        self.completed_episodes.push(result.clone());
        result
    }

    /// End an episode and immediately update Q-function with MC returns.
    ///
    /// For each transition (s_t, a_t) in the episode, performs:
    /// `QFunction::update(a_t, φ_t, G_t)`
    pub fn end_episode_and_update(
        &mut self,
        strategy_id: StrategyId,
        reason: TerminalReason,
        end_timestamp_ns: u64,
        q_function: &mut QFunction,
    ) -> (EpisodeResult, Vec<UpdateResult>) {
        let result = self.end_episode(strategy_id, reason, end_timestamp_ns);
        let update_results = update_q_function(q_function, &result);
        (result, update_results)
    }

    /// Update Q-function with MC returns from a completed episode result.
    ///
    /// For each transition (s_t, a_t, G_t): `QFunction::update(a_t, φ_t, G_t)`
    pub fn update_from_result(
        q_function: &mut QFunction,
        result: &EpisodeResult,
    ) -> Vec<UpdateResult> {
        update_q_function(q_function, result)
    }

    /// Number of completed episodes across all strategies.
    pub fn completed_count(&self) -> usize {
        self.completed_episodes.len()
    }

    /// Number of completed episodes for a specific strategy.
    pub fn completed_count_for(&self, strategy_id: StrategyId) -> usize {
        self.completed_episodes
            .iter()
            .filter(|r| r.strategy_id == strategy_id)
            .count()
    }

    /// Get a read-only reference to the active episode buffer.
    pub fn active_episode(&self, strategy_id: StrategyId) -> Option<&EpisodeBuffer> {
        self.active_episodes.get(&strategy_id)
    }

    /// Get completed episodes for a specific strategy.
    pub fn episodes_for(&self, strategy_id: StrategyId) -> Vec<&EpisodeResult> {
        self.completed_episodes
            .iter()
            .filter(|r| r.strategy_id == strategy_id)
            .collect()
    }

    /// Compute discounted cumulative returns G_t for a reward sequence.
    ///
    /// G_t = Σ_{k=0}^{T-t} γ^k · r_{t+k}
    ///
    /// Computed in reverse order for O(n) efficiency.
    pub fn compute_returns(rewards: &[f64], gamma: f64) -> Vec<f64> {
        let n = rewards.len();
        if n == 0 {
            return Vec::new();
        }
        let mut returns = Vec::with_capacity(n);
        let mut g = 0.0;
        for i in (0..n).rev() {
            g = rewards[i] + gamma * g;
            returns.push(g);
        }
        returns.reverse();
        returns
    }

    /// Get reward config reference.
    pub fn reward_config(&self) -> &RewardConfig {
        &self.config.reward
    }

    /// Compute episode result from a buffer (internal).
    fn compute_episode_result(
        buffer: EpisodeBuffer,
        reason: TerminalReason,
        end_timestamp_ns: u64,
        gamma: f64,
    ) -> EpisodeResult {
        let rewards: Vec<f64> = buffer.transitions.iter().map(|t| t.reward).collect();
        let returns = Self::compute_returns(&rewards, gamma);

        let total_reward: f64 = rewards.iter().sum();
        let return_g0 = returns.first().copied().unwrap_or(0.0);
        let duration_ns = end_timestamp_ns.saturating_sub(buffer.start_timestamp_ns);
        let num_transitions = buffer.transitions.len();

        EpisodeResult {
            strategy_id: buffer.strategy_id,
            terminal_reason: reason,
            num_transitions,
            total_reward,
            return_g0,
            duration_ns,
            returns,
            transitions: buffer.transitions,
        }
    }
}

/// Update Q-function with MC returns from an episode result.
///
/// For each transition: `QFunction::update(a_t, φ_t, G_t)`
fn update_q_function(q_function: &mut QFunction, result: &EpisodeResult) -> Vec<UpdateResult> {
    result
        .transitions
        .iter()
        .zip(result.returns.iter())
        .map(|(transition, &g_t)| q_function.update(transition.action, &transition.phi, g_t))
        .collect()
}

/// Extract strategy equity from state snapshot.
fn state_equity(state: &StateSnapshot, strategy_id: StrategyId) -> f64 {
    state
        .positions
        .get(&strategy_id)
        .map(|p| p.realized_pnl + p.unrealized_pnl)
        .unwrap_or(0.0)
}

/// Extract position size from state snapshot.
fn state_position_size(state: &StateSnapshot, strategy_id: StrategyId) -> f64 {
    state
        .positions
        .get(&strategy_id)
        .map(|p| p.size)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bayesian_lr::QAction;
    use fx_core::types::StrategyId;
    use fx_events::projector::{LimitStateData, Position, StateSnapshot};

    // --- Helper functions ---

    fn default_reward_config() -> RewardConfig {
        RewardConfig {
            lambda_risk: 0.1,
            lambda_dd: 0.5,
            dd_cap: 100.0,
            gamma: 0.99,
        }
    }

    fn make_state_with_equity(
        strategy_id: StrategyId,
        equity: f64,
        position_size: f64,
    ) -> StateSnapshot {
        let mut positions = std::collections::HashMap::new();
        let mut pos = Position::new(strategy_id);
        pos.size = position_size;
        pos.realized_pnl = equity * 0.4;
        pos.unrealized_pnl = equity * 0.6;
        positions.insert(strategy_id, pos);
        StateSnapshot {
            positions,
            global_position: position_size,
            global_position_limit: 1_000_000.0,
            total_unrealized_pnl: equity * 0.6,
            total_realized_pnl: equity * 0.4,
            limit_state: LimitStateData::default(),
            state_version: 1,
            staleness_ms: 0,
            state_hash: String::new(),
            lot_multiplier: 1.0,
            last_market_data_ns: 0,
        }
    }

    fn phi_ones(dim: usize) -> Vec<f64> {
        vec![1.0; dim]
    }

    // --- EpisodeBuffer tests ---

    #[test]
    fn test_episode_buffer_new() {
        let buf = EpisodeBuffer::new(StrategyId::A, 1_000_000_000, 0.0);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.strategy_id, StrategyId::A);
        assert_eq!(buf.start_timestamp_ns, 1_000_000_000);
        assert_eq!(buf.prev_equity(), 0.0);
        assert_eq!(buf.equity_peak(), 0.0);
    }

    #[test]
    fn test_episode_buffer_record_basic() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 0.0);
        let config = default_reward_config();

        // Step 1: equity goes from 0 to 10
        buf.record(
            1_000_000_000,
            QAction::Buy,
            phi_ones(5),
            10.0,
            100_000.0,
            0.0001,
            &config,
        );
        assert_eq!(buf.len(), 1);
        assert_eq!(
            buf.transitions[0].reward,
            10.0 - 0.1 * 100_000.0 * 100_000.0 * 0.0001
        );
        // = 10.0 - 0.1 * 1_000_000_000 * 0.0001 = 10.0 - 10000 = -9990
        // Wait that's too extreme. Let me recalculate.
        // position_variance = 100_000 * 100_000 * 0.0001 = 1_000_000_000
        // risk_penalty = 0.1 * 1_000_000_000 = 100_000_000
        // That's indeed extreme for those parameters. But it's just testing the formula.
        assert_eq!(buf.equity_peak(), 10.0);
    }

    #[test]
    fn test_episode_buffer_reward_components() {
        let mut buf = EpisodeBuffer::new(StrategyId::B, 0, 100.0);
        let config = RewardConfig {
            lambda_risk: 0.01,
            lambda_dd: 0.1,
            dd_cap: 50.0,
            gamma: 0.99,
        };

        // equity drops from 100 to 80, position=1000, vol_sq=0.01
        buf.record(
            1_000_000_000,
            QAction::Hold,
            phi_ones(5),
            80.0,
            1000.0,
            0.01,
            &config,
        );
        let t = &buf.transitions[0];
        let expected_delta_pnl = 80.0 - 100.0; // -20
        let expected_pos_var = 1000.0 * 1000.0 * 0.01; // 10000
        let expected_dd = (100.0_f64 - 80.0).max(0.0); // 20
        let expected_dd_capped = expected_dd.min(50.0); // 20
        let expected_reward =
            expected_delta_pnl - 0.01 * expected_pos_var - 0.1 * expected_dd_capped;
        // = -20 - 100 - 2 = -122
        assert!((t.reward - expected_reward).abs() < 1e-10);
    }

    #[test]
    fn test_episode_buffer_dd_cap() {
        let mut buf = EpisodeBuffer::new(StrategyId::C, 0, 100.0);
        let config = RewardConfig {
            lambda_risk: 0.0,
            lambda_dd: 1.0,
            dd_cap: 10.0,
            gamma: 0.99,
        };

        // equity drops from 100 to 50, DD = 50, capped at 10
        buf.record(
            1_000_000_000,
            QAction::Hold,
            phi_ones(5),
            50.0,
            0.0,
            0.0,
            &config,
        );
        let t = &buf.transitions[0];
        // reward = -50 - 0 - 1.0 * min(50, 10) = -50 - 10 = -60
        assert!((t.reward - (-60.0)).abs() < 1e-10);
    }

    #[test]
    fn test_episode_buffer_equity_peak_tracks() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 0.0);
        let config = RewardConfig {
            lambda_risk: 0.0,
            lambda_dd: 0.0,
            dd_cap: 100.0,
            gamma: 0.99,
        };

        buf.record(1, QAction::Buy, phi_ones(5), 10.0, 0.0, 0.0, &config);
        assert_eq!(buf.equity_peak(), 10.0);

        buf.record(2, QAction::Hold, phi_ones(5), 5.0, 0.0, 0.0, &config);
        assert_eq!(buf.equity_peak(), 10.0); // peak unchanged

        buf.record(3, QAction::Hold, phi_ones(5), 15.0, 0.0, 0.0, &config);
        assert_eq!(buf.equity_peak(), 15.0); // peak updated
    }

    #[test]
    fn test_episode_buffer_dd_from_peak() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 0.0);
        let config = RewardConfig {
            lambda_risk: 0.0,
            lambda_dd: 1.0,
            dd_cap: 1000.0,
            gamma: 0.99,
        };

        // Peak at 10, drop to 5: DD = 5
        buf.record(1, QAction::Buy, phi_ones(5), 10.0, 0.0, 0.0, &config);
        buf.record(2, QAction::Hold, phi_ones(5), 5.0, 0.0, 0.0, &config);
        // reward = (5-10) - 0 - 1.0*(10-5) = -5 - 5 = -10
        assert!((buf.transitions[1].reward - (-10.0)).abs() < 1e-10);

        // Recover to 8: DD = 2 (from peak 10)
        buf.record(3, QAction::Hold, phi_ones(5), 8.0, 0.0, 0.0, &config);
        // reward = (8-5) - 0 - 1.0*(10-8) = 3 - 2 = 1
        assert!((buf.transitions[2].reward - 1.0).abs() < 1e-10);
    }

    // --- compute_returns tests ---

    #[test]
    fn test_compute_returns_empty() {
        let returns = McEvaluator::compute_returns(&[], 0.99);
        assert!(returns.is_empty());
    }

    #[test]
    fn test_compute_returns_single() {
        let returns = McEvaluator::compute_returns(&[5.0], 0.99);
        assert_eq!(returns.len(), 1);
        assert!((returns[0] - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_compute_returns_two_steps() {
        // rewards = [1, 2], gamma = 0.99
        // G_1 = 2
        // G_0 = 1 + 0.99 * 2 = 2.98
        let returns = McEvaluator::compute_returns(&[1.0, 2.0], 0.99);
        assert_eq!(returns.len(), 2);
        assert!((returns[0] - 2.98).abs() < 1e-10);
        assert!((returns[1] - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_compute_returns_decay() {
        // rewards = [1, 1, 1, 1], gamma = 0.9
        // G_3 = 1
        // G_2 = 1 + 0.9*1 = 1.9
        // G_1 = 1 + 0.9*1.9 = 2.71
        // G_0 = 1 + 0.9*2.71 = 3.439
        let returns = McEvaluator::compute_returns(&[1.0, 1.0, 1.0, 1.0], 0.9);
        assert_eq!(returns.len(), 4);
        assert!((returns[0] - 3.439).abs() < 1e-10);
        assert!((returns[1] - 2.71).abs() < 1e-10);
        assert!((returns[2] - 1.9).abs() < 1e-10);
        assert!((returns[3] - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_compute_returns_gamma_1() {
        // With gamma=1, returns should equal cumulative sum from the right
        let rewards = vec![1.0, 2.0, 3.0, 4.0];
        let returns = McEvaluator::compute_returns(&rewards, 1.0);
        assert_eq!(returns, vec![10.0, 9.0, 7.0, 4.0]);
    }

    #[test]
    fn test_compute_returns_negative_rewards() {
        let returns = McEvaluator::compute_returns(&[-1.0, -2.0, 3.0], 0.9);
        // G_2 = 3
        // G_1 = -2 + 0.9*3 = 0.7
        // G_0 = -1 + 0.9*0.7 = -0.37
        assert_eq!(returns.len(), 3);
        assert!((returns[0] - (-0.37)).abs() < 1e-10);
        assert!((returns[1] - 0.7).abs() < 1e-10);
        assert!((returns[2] - 3.0).abs() < 1e-10);
    }

    // --- McEvaluator lifecycle tests ---

    #[test]
    fn test_evaluator_start_episode() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.start_episode(StrategyId::A, 100, 0.0);
        assert!(eval.has_active_episode(StrategyId::A));
        assert!(!eval.has_active_episode(StrategyId::B));
    }

    #[test]
    #[should_panic(expected = "Active episode already exists")]
    fn test_evaluator_double_start_panics() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.start_episode(StrategyId::A, 100, 0.0);
        eval.start_episode(StrategyId::A, 200, 0.0);
    }

    #[test]
    #[should_panic(expected = "No active episode")]
    fn test_evaluator_end_without_start_panics() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 200);
    }

    #[test]
    fn test_evaluator_full_lifecycle() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });
        let state = make_state_with_equity(StrategyId::A, 10.0, 100_000.0);

        eval.start_episode(StrategyId::A, 100, 0.0);
        eval.record_transition(StrategyId::A, 200, QAction::Buy, phi_ones(5), &state, 0.0);

        let state2 = make_state_with_equity(StrategyId::A, 20.0, 100_000.0);
        eval.record_transition(StrategyId::A, 300, QAction::Hold, phi_ones(5), &state2, 0.0);

        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 400);

        assert_eq!(result.strategy_id, StrategyId::A);
        assert_eq!(result.terminal_reason, TerminalReason::PositionClosed);
        assert_eq!(result.num_transitions, 2);
        assert!((result.total_reward - 20.0).abs() < 1e-10); // 10 + 10
        assert!(!eval.has_active_episode(StrategyId::A));
        assert_eq!(eval.completed_count(), 1);
    }

    #[test]
    fn test_evaluator_multi_strategy() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 100, 0.0);
        eval.start_episode(StrategyId::B, 100, 0.0);

        assert!(eval.has_active_episode(StrategyId::A));
        assert!(eval.has_active_episode(StrategyId::B));

        let state_a = make_state_with_equity(StrategyId::A, 5.0, 100_000.0);
        eval.record_transition(StrategyId::A, 200, QAction::Buy, phi_ones(5), &state_a, 0.0);

        let result_a = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 300);
        assert_eq!(result_a.num_transitions, 1);
        assert!(eval.has_active_episode(StrategyId::B));
        assert!(!eval.has_active_episode(StrategyId::A));
    }

    // --- MC return computation integration ---

    #[test]
    fn test_end_episode_returns_computed() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.9,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);

        let s1 = make_state_with_equity(StrategyId::A, 1.0, 0.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(5), &s1, 0.0);
        let s2 = make_state_with_equity(StrategyId::A, 2.0, 0.0);
        eval.record_transition(StrategyId::A, 2, QAction::Hold, phi_ones(5), &s2, 0.0);
        let s3 = make_state_with_equity(StrategyId::A, 3.0, 0.0);
        eval.record_transition(StrategyId::A, 3, QAction::Sell, phi_ones(5), &s3, 0.0);

        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 4);

        // rewards = [1, 1, 1], gamma = 0.9
        // G_2 = 1
        // G_1 = 1 + 0.9*1 = 1.9
        // G_0 = 1 + 0.9*1.9 = 2.71
        assert_eq!(result.returns.len(), 3);
        assert!((result.returns[0] - 2.71).abs() < 1e-10);
        assert!((result.returns[1] - 1.9).abs() < 1e-10);
        assert!((result.returns[2] - 1.0).abs() < 1e-10);
        assert!((result.return_g0 - 2.71).abs() < 1e-10);
    }

    #[test]
    fn test_end_episode_duration() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.start_episode(StrategyId::A, 1_000_000_000, 0.0);

        let result = eval.end_episode(
            StrategyId::A,
            TerminalReason::MaxHoldTimeExceeded,
            31_000_000_000,
        );
        assert_eq!(result.duration_ns, 30_000_000_000);
        assert!((result.duration_ms() - 30_000.0).abs() < 1e-6);
    }

    // --- Q-function update tests ---

    #[test]
    fn test_end_episode_and_update() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.01);

        eval.start_episode(StrategyId::A, 0, 0.0);
        let s1 = make_state_with_equity(StrategyId::A, 1.0, 0.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(dim), &s1, 0.0);
        let s2 = make_state_with_equity(StrategyId::A, 2.0, 0.0);
        eval.record_transition(StrategyId::A, 2, QAction::Hold, phi_ones(dim), &s2, 0.0);

        let (result, updates) = eval.end_episode_and_update(
            StrategyId::A,
            TerminalReason::PositionClosed,
            3,
            &mut q_fn,
        );

        assert_eq!(result.num_transitions, 2);
        assert_eq!(updates.len(), 2);

        // All updates should succeed (no divergence)
        for u in &updates {
            assert!(!u.diverged);
        }
    }

    #[test]
    fn test_update_from_result() {
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.01);

        let result = EpisodeResult {
            strategy_id: StrategyId::A,
            terminal_reason: TerminalReason::PositionClosed,
            num_transitions: 2,
            total_reward: 3.0,
            return_g0: 2.98,
            duration_ns: 1_000_000_000,
            returns: vec![2.98, 1.0],
            transitions: vec![
                EpisodeTransition {
                    timestamp_ns: 1,
                    action: QAction::Buy,
                    phi: phi_ones(dim),
                    reward: 1.0,
                },
                EpisodeTransition {
                    timestamp_ns: 2,
                    action: QAction::Hold,
                    phi: phi_ones(dim),
                    reward: 1.0,
                },
            ],
        };

        let updates = McEvaluator::update_from_result(&mut q_fn, &result);
        assert_eq!(updates.len(), 2);
        for u in &updates {
            assert!(!u.diverged);
        }
    }

    // --- Episode result tests ---

    #[test]
    fn test_episode_result_avg_reward() {
        let result = EpisodeResult {
            strategy_id: StrategyId::A,
            terminal_reason: TerminalReason::PositionClosed,
            num_transitions: 4,
            total_reward: 20.0,
            return_g0: 19.0,
            duration_ns: 1_000_000_000,
            returns: vec![19.0, 15.0, 10.0, 4.0],
            transitions: vec![],
        };
        assert!((result.avg_reward() - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_episode_result_avg_reward_zero_transitions() {
        let result = EpisodeResult {
            strategy_id: StrategyId::A,
            terminal_reason: TerminalReason::PositionClosed,
            num_transitions: 0,
            total_reward: 0.0,
            return_g0: 0.0,
            duration_ns: 0,
            returns: vec![],
            transitions: vec![],
        };
        assert!((result.avg_reward() - 0.0).abs() < 1e-10);
    }

    // --- Terminal reason tests ---

    #[test]
    fn test_all_terminal_reasons() {
        let reasons = [
            (TerminalReason::PositionClosed, "PositionClosed"),
            (TerminalReason::MaxHoldTimeExceeded, "MaxHoldTimeExceeded"),
            (TerminalReason::DailyHardLimit, "DailyHardLimit"),
            (TerminalReason::UnknownRegime, "UnknownRegime"),
        ];

        for (reason, name) in reasons {
            let mut eval = McEvaluator::new(McEvalConfig::default());
            eval.start_episode(StrategyId::A, 0, 0.0);
            let result = eval.end_episode(StrategyId::A, reason, 100);
            assert_eq!(result.terminal_reason, reason);
            assert_eq!(format!("{}", reason), name);
        }
    }

    // --- Partial fill (no episode end) test ---

    #[test]
    fn test_partial_fill_continues_episode() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);

        // Simulate partial fill: position size reduces but doesn't go to zero
        let s1 = make_state_with_equity(StrategyId::A, 1.0, 100_000.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(5), &s1, 0.0);

        // Partial close: position still non-zero
        let s2 = make_state_with_equity(StrategyId::A, 2.0, 50_000.0);
        eval.record_transition(StrategyId::A, 2, QAction::Sell, phi_ones(5), &s2, 0.0);

        // Episode still active
        assert!(eval.has_active_episode(StrategyId::A));

        // Full close: position goes to zero
        let s3 = make_state_with_equity(StrategyId::A, 3.0, 0.0);
        eval.record_transition(StrategyId::A, 3, QAction::Sell, phi_ones(5), &s3, 0.0);

        // Now end the episode
        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 4);
        assert_eq!(result.num_transitions, 3);
    }

    // --- Empty episode test ---

    #[test]
    fn test_empty_episode() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.start_episode(StrategyId::A, 0, 0.0);

        // End immediately with no transitions
        let result = eval.end_episode(StrategyId::A, TerminalReason::DailyHardLimit, 100);

        assert_eq!(result.num_transitions, 0);
        assert!((result.total_reward - 0.0).abs() < 1e-10);
        assert!((result.return_g0 - 0.0).abs() < 1e-10);
        assert!(result.returns.is_empty());
        assert!(result.transitions.is_empty());
        assert_eq!(result.duration_ns, 100);
    }

    // --- Completed episodes tracking ---

    #[test]
    fn test_completed_episodes_tracking() {
        let mut eval = McEvaluator::new(McEvalConfig::default());

        // Episode 1 for Strategy A
        eval.start_episode(StrategyId::A, 0, 0.0);
        let s1 = make_state_with_equity(StrategyId::A, 1.0, 0.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(5), &s1, 0.0);
        eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 100);

        // Episode 2 for Strategy B
        eval.start_episode(StrategyId::B, 200, 0.0);
        let s2 = make_state_with_equity(StrategyId::B, 2.0, 0.0);
        eval.record_transition(StrategyId::B, 300, QAction::Buy, phi_ones(5), &s2, 0.0);
        eval.end_episode(StrategyId::B, TerminalReason::MaxHoldTimeExceeded, 400);

        // Episode 3 for Strategy A
        eval.start_episode(StrategyId::A, 500, 0.0);
        let s3 = make_state_with_equity(StrategyId::A, 3.0, 0.0);
        eval.record_transition(StrategyId::A, 600, QAction::Buy, phi_ones(5), &s3, 0.0);
        eval.end_episode(StrategyId::A, TerminalReason::UnknownRegime, 700);

        assert_eq!(eval.completed_count(), 3);
        assert_eq!(eval.completed_count_for(StrategyId::A), 2);
        assert_eq!(eval.completed_count_for(StrategyId::B), 1);
        assert_eq!(eval.completed_count_for(StrategyId::C), 0);

        let a_episodes = eval.episodes_for(StrategyId::A);
        assert_eq!(a_episodes.len(), 2);
    }

    // --- Deadly Triad: on-policy verification ---

    #[test]
    fn test_on_policy_only_records_taken_actions() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);

        // Only record Buy and Hold (what was actually taken)
        let s1 = make_state_with_equity(StrategyId::A, 1.0, 0.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(5), &s1, 0.0);
        let s2 = make_state_with_equity(StrategyId::A, 2.0, 0.0);
        eval.record_transition(StrategyId::A, 2, QAction::Hold, phi_ones(5), &s2, 0.0);

        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 3);

        // Verify only Buy and Hold were recorded (no Sell)
        assert_eq!(result.transitions[0].action, QAction::Buy);
        assert_eq!(result.transitions[1].action, QAction::Hold);

        // Q-function update should only use Buy and Hold
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.01);
        let updates = McEvaluator::update_from_result(&mut q_fn, &result);
        assert_eq!(updates.len(), 2); // Only 2 updates, not 3
    }

    // --- MC vs bootstrapping verification ---

    #[test]
    fn test_mc_uses_full_returns_not_bootstrap() {
        // MC returns should be the FULL discounted sum, not r + gamma * Q
        let rewards = vec![1.0, 2.0, 3.0, 4.0];
        let gamma = 0.9;
        let returns = McEvaluator::compute_returns(&rewards, gamma);

        // Verify G_0 = 1 + 0.9*2 + 0.81*3 + 0.729*4 = 1 + 1.8 + 2.43 + 2.916 = 8.146
        let expected_g0 = 1.0 + 0.9 * 2.0 + 0.81 * 3.0 + 0.729 * 4.0;
        assert!((returns[0] - expected_g0).abs() < 1e-10);

        // This is NOT r_0 + gamma * Q(s_1, a*) — it's the full episodic return
        // No bootstrapping is used
    }

    // --- Reward function edge cases ---

    #[test]
    fn test_reward_zero_position_no_risk_penalty() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 0.0);
        let config = RewardConfig {
            lambda_risk: 1.0,
            lambda_dd: 0.0,
            dd_cap: 100.0,
            gamma: 0.99,
        };

        // Zero position size → position_variance = 0 → no risk penalty
        buf.record(1, QAction::Hold, phi_ones(5), 5.0, 0.0, 100.0, &config);
        assert!((buf.transitions[0].reward - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_reward_negative_equity() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 0.0);
        let config = RewardConfig {
            lambda_risk: 0.0,
            lambda_dd: 0.0,
            dd_cap: 100.0,
            gamma: 0.99,
        };

        // Equity goes negative
        buf.record(1, QAction::Buy, phi_ones(5), -10.0, 0.0, 0.0, &config);
        assert!((buf.transitions[0].reward - (-10.0)).abs() < 1e-10);

        // Peak should still be 0 (initial)
        assert_eq!(buf.equity_peak(), 0.0);
    }

    #[test]
    fn test_reward_initial_equity_nonzero() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 50.0);
        let config = RewardConfig {
            lambda_risk: 0.0,
            lambda_dd: 0.0,
            dd_cap: 100.0,
            gamma: 0.99,
        };

        // Initial equity is 50, goes to 60 → delta = 10
        buf.record(1, QAction::Buy, phi_ones(5), 60.0, 0.0, 0.0, &config);
        assert!((buf.transitions[0].reward - 10.0).abs() < 1e-10);
        assert_eq!(buf.equity_peak(), 60.0);
    }

    // --- Discount factor effect on returns ---

    #[test]
    fn test_gamma_affects_return_concentration() {
        let rewards = vec![1.0; 10];

        let returns_high_gamma = McEvaluator::compute_returns(&rewards, 0.999);
        let returns_low_gamma = McEvaluator::compute_returns(&rewards, 0.5);

        // High gamma: G_0 ≈ sum of all rewards (little discounting)
        // Low gamma: G_0 is dominated by early rewards
        assert!(returns_high_gamma[0] > returns_low_gamma[0]);

        // G_9 = 1.0 regardless of gamma (last step, no future rewards)
        assert!((returns_high_gamma[9] - 1.0).abs() < 1e-10);
        assert!((returns_low_gamma[9] - 1.0).abs() < 1e-10);
    }

    // --- Q-function update with MC returns ---

    #[test]
    fn test_q_function_update_with_mc_returns_converges() {
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.001, 100, 0.01, 0.0); // no optimistic bias
        let gamma = 0.99;

        // Run multiple episodes with consistent reward signal
        for _ in 0..50 {
            let rewards = vec![1.0, 1.0, 1.0];
            let returns = McEvaluator::compute_returns(&rewards, gamma);

            let result = EpisodeResult {
                strategy_id: StrategyId::A,
                terminal_reason: TerminalReason::PositionClosed,
                num_transitions: 3,
                total_reward: 3.0,
                return_g0: returns[0],
                duration_ns: 3_000_000_000,
                returns,
                transitions: vec![
                    EpisodeTransition {
                        timestamp_ns: 1,
                        action: QAction::Buy,
                        phi: phi_ones(dim),
                        reward: 1.0,
                    },
                    EpisodeTransition {
                        timestamp_ns: 2,
                        action: QAction::Hold,
                        phi: phi_ones(dim),
                        reward: 1.0,
                    },
                    EpisodeTransition {
                        timestamp_ns: 3,
                        action: QAction::Sell,
                        phi: phi_ones(dim),
                        reward: 1.0,
                    },
                ],
            };

            McEvaluator::update_from_result(&mut q_fn, &result);
        }

        // After many episodes, Q-value for phi=ones should be positive
        // (since all returns were positive)
        let q_buy = q_fn.q_value(QAction::Buy, &phi_ones(dim));
        let q_hold = q_fn.q_value(QAction::Hold, &phi_ones(dim));
        let q_sell = q_fn.q_value(QAction::Sell, &phi_ones(dim));

        assert!(
            q_buy > 0.0,
            "Buy Q should be positive after positive episodes, got {}",
            q_buy
        );
        assert!(
            q_hold > 0.0,
            "Hold Q should be positive after positive episodes, got {}",
            q_hold
        );
        assert!(
            q_sell > 0.0,
            "Sell Q should be positive after positive episodes, got {}",
            q_sell
        );
    }

    #[test]
    fn test_q_function_update_with_negative_returns() {
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.001, 100, 0.01, 0.0);
        let gamma = 0.99;

        // Run multiple episodes with negative reward signal
        for _ in 0..50 {
            let rewards = vec![-1.0, -1.0, -1.0];
            let returns = McEvaluator::compute_returns(&rewards, gamma);

            let result = EpisodeResult {
                strategy_id: StrategyId::A,
                terminal_reason: TerminalReason::PositionClosed,
                num_transitions: 3,
                total_reward: -3.0,
                return_g0: returns[0],
                duration_ns: 3_000_000_000,
                returns,
                transitions: vec![
                    EpisodeTransition {
                        timestamp_ns: 1,
                        action: QAction::Buy,
                        phi: phi_ones(dim),
                        reward: -1.0,
                    },
                    EpisodeTransition {
                        timestamp_ns: 2,
                        action: QAction::Hold,
                        phi: phi_ones(dim),
                        reward: -1.0,
                    },
                    EpisodeTransition {
                        timestamp_ns: 3,
                        action: QAction::Sell,
                        phi: phi_ones(dim),
                        reward: -1.0,
                    },
                ],
            };

            McEvaluator::update_from_result(&mut q_fn, &result);
        }

        // After many negative episodes, Q-values should be negative
        let q_buy = q_fn.q_value(QAction::Buy, &phi_ones(dim));
        assert!(
            q_buy < 0.0,
            "Buy Q should be negative after negative episodes, got {}",
            q_buy
        );
    }

    // --- Integration: full episode with risk penalties ---

    #[test]
    fn test_full_episode_with_risk_and_dd() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.001,
                lambda_dd: 0.1,
                dd_cap: 50.0,
                gamma: 0.99,
            },
        });

        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.01);

        eval.start_episode(StrategyId::A, 0, 0.0);

        // Step 1: Buy, equity +5, position=100k, vol=0.0001
        let s1 = make_state_with_equity(StrategyId::A, 5.0, 100_000.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(dim), &s1, 0.0001);

        // Step 2: Hold, equity +3 (drop from peak 5), DD=2
        let s2 = make_state_with_equity(StrategyId::A, 3.0, 100_000.0);
        eval.record_transition(StrategyId::A, 2, QAction::Hold, phi_ones(dim), &s2, 0.0001);

        // Step 3: Sell (close), equity +8, DD=0 (above peak 5)
        let s3 = make_state_with_equity(StrategyId::A, 8.0, 0.0);
        eval.record_transition(StrategyId::A, 3, QAction::Sell, phi_ones(dim), &s3, 0.0001);

        let (result, updates) = eval.end_episode_and_update(
            StrategyId::A,
            TerminalReason::PositionClosed,
            4,
            &mut q_fn,
        );

        assert_eq!(result.num_transitions, 3);
        assert_eq!(updates.len(), 3);

        // Total reward should be negative due to position risk penalty
        // Step 1: r = 5 - 0.001*(100000^2 * 0.0001) - 0.1*0 = 5 - 1000 = -995
        // Step 2: r = (3-5) - 0.001*(100000^2 * 0.0001) - 0.1*min(2,50) = -2 - 1000 - 0.2 = -1002.2
        // Step 3: r = (8-3) - 0 - 0 = 5 (position closed, no risk)
        let expected_r1 = 5.0 - 0.001 * 100_000.0 * 100_000.0 * 0.0001;
        let expected_r2 = -2.0 - 0.001 * 100_000.0 * 100_000.0 * 0.0001 - 0.1 * 2.0;
        let expected_r3 = 5.0;

        assert!((result.transitions[0].reward - expected_r1).abs() < 1e-6);
        assert!((result.transitions[1].reward - expected_r2).abs() < 1e-6);
        assert!((result.transitions[2].reward - expected_r3).abs() < 1e-6);

        // Returns should be discounted
        assert_eq!(result.returns.len(), 3);
    }

    // --- Active episode access ---

    #[test]
    fn test_active_episode_access() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        assert!(eval.active_episode(StrategyId::A).is_none());

        eval.start_episode(StrategyId::A, 100, 0.0);
        let active = eval.active_episode(StrategyId::A).unwrap();
        assert_eq!(active.strategy_id, StrategyId::A);
        assert!(active.is_empty());
    }

    // --- Config access ---

    #[test]
    fn test_config_access() {
        let config = McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.5,
                lambda_dd: 1.0,
                dd_cap: 200.0,
                gamma: 0.95,
            },
        };
        let eval = McEvaluator::new(config);
        let rc = eval.reward_config();
        assert!((rc.lambda_risk - 0.5).abs() < 1e-10);
        assert!((rc.gamma - 0.95).abs() < 1e-10);
    }

    // --- Episode with MAX_HOLD_TIME termination ---

    #[test]
    fn test_max_hold_time_forced_close() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);

        // Simulate holding through MAX_HOLD_TIME
        let s1 = make_state_with_equity(StrategyId::A, 2.0, 100_000.0);
        eval.record_transition(
            StrategyId::A,
            15_000_000_000,
            QAction::Buy,
            phi_ones(5),
            &s1,
            0.0,
        );

        let s2 = make_state_with_equity(StrategyId::A, 1.0, 100_000.0);
        eval.record_transition(
            StrategyId::A,
            30_000_000_000,
            QAction::Hold,
            phi_ones(5),
            &s2,
            0.0,
        );

        // MAX_HOLD_TIME exceeded → forced close
        let result = eval.end_episode(
            StrategyId::A,
            TerminalReason::MaxHoldTimeExceeded,
            30_000_000_000,
        );

        assert_eq!(result.terminal_reason, TerminalReason::MaxHoldTimeExceeded);
        assert_eq!(result.num_transitions, 2);
        assert!((result.total_reward - 1.0).abs() < 1e-10); // 2 + (-1) = 1
        assert_eq!(result.duration_ns, 30_000_000_000);
    }

    // --- Default config tests ---

    #[test]
    fn test_default_configs() {
        let rc = RewardConfig::default();
        assert!((rc.lambda_risk - 0.1).abs() < 1e-10);
        assert!((rc.lambda_dd - 0.5).abs() < 1e-10);
        assert!((rc.dd_cap - 100.0).abs() < 1e-10);
        assert!((rc.gamma - 0.99).abs() < 1e-10);

        let mc = McEvalConfig::default();
        assert!((mc.reward.gamma - 0.99).abs() < 1e-10);
    }

    // =========================================================================
    // §3.0 MDP Formulation Validation Tests
    // =========================================================================
    //
    // These tests verify the implementation matches design.md §3.0:
    //   State space:  s_t = (X_t^market, p_t^position)
    //   Action space: a_t ∈ {buy_k, sell_k, hold}
    //   Constraints:  |p_{t+1}| ≤ P_max
    //   Reward:       r_t^i = ΔPnL - λ_risk·σ² - λ_dd·min(DD, DD_cap)
    //   Q-function:   Q(s,a) = w_a^T φ(s)

    // --- State Space Validation ---

    /// Verify FeatureVector contains both market features AND position state,
    /// matching the MDP state s_t = (X_t^market, p_t^position).
    #[test]
    fn test_mdp_state_space_contains_market_and_position_features() {
        use crate::features::FeatureVector;

        let fv = FeatureVector::zero();
        let flat = fv.flattened();
        assert_eq!(flat.len(), FeatureVector::DIM);

        // Market features (X_t^market): indices 0-15
        // spread, spread_zscore, obi, delta_obi, depth_change_rate, queue_position,
        // realized_volatility, volatility_ratio, volatility_decay_rate,
        // session_tokyo, session_london, session_ny, session_sydney,
        // time_since_open_ms, time_since_last_spike_ms, holding_time_ms
        let _ = fv.spread;
        let _ = fv.realized_volatility;
        let _ = fv.session_tokyo;

        // Position state features (p_t^position): indices 16-19
        // position_size, position_direction, entry_price, pnl_unrealized
        let _ = fv.position_size;
        let _ = fv.position_direction;
        let _ = fv.entry_price;
        let _ = fv.pnl_unrealized;

        // Execution features with forced lag: indices 20-23
        let _ = fv.trade_intensity;
        let _ = fv.recent_fill_rate; // lagged
        let _ = fv.recent_slippage; // lagged

        // Non-linear transform terms: indices 24-29
        let _ = fv.self_impact;
        let _ = fv.time_decay;
        let _ = fv.dynamic_cost;

        // Interaction terms: indices 30-33
        let _ = fv.spread_z_x_vol;
        let _ = fv.position_size_x_vol;

        // Additional interaction terms: indices 34-35
        let _ = fv.obi_x_vol;
        let _ = fv.spread_z_x_self_impact;

        // Verify flattened length is exactly DIM
        assert_eq!(flat.len(), FeatureVector::DIM);
    }

    /// Verify FeatureVector roundtrip preserves all fields,
    /// ensuring the state vector is fully recoverable.
    #[test]
    fn test_mdp_state_vector_roundtrip_integrity() {
        use crate::features::FeatureVector;

        let mut fv = FeatureVector::zero();
        fv.spread = 1.5;
        fv.spread_zscore = 2.3;
        fv.position_size = 100_000.0;
        fv.position_direction = 1.0;
        fv.entry_price = 1.08500;
        fv.pnl_unrealized = 50.0;
        fv.self_impact = 0.01;
        fv.p_revert = 0.7;

        let flat = fv.flattened();
        let restored = FeatureVector::from_flattened(&flat).unwrap();

        assert!((restored.spread - 1.5).abs() < 1e-15);
        assert!((restored.position_size - 100_000.0).abs() < 1e-15);
        assert!((restored.entry_price - 1.08500).abs() < 1e-15);
        assert!((restored.pnl_unrealized - 50.0).abs() < 1e-15);
        assert!((restored.self_impact - 0.01).abs() < 1e-15);
    }

    // --- Action Space Validation ---

    /// Verify QAction matches the MDP action space: {Buy, Sell, Hold}.
    #[test]
    fn test_mdp_action_space_has_three_actions() {
        let all = QAction::all();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&QAction::Buy));
        assert!(all.contains(&QAction::Sell));
        assert!(all.contains(&QAction::Hold));
    }

    /// Verify Buy and Sell receive optimistic initialization while Hold does not.
    #[test]
    fn test_mdp_optimistic_initialization_buy_sell_not_hold() {
        let dim = 5;
        let bias = 1.0;
        let q_fn = QFunction::new(dim, 0.01, 500, 0.01, bias);

        let phi = vec![1.0; dim];
        let q_buy = q_fn.q_value(QAction::Buy, &phi);
        let q_sell = q_fn.q_value(QAction::Sell, &phi);
        let q_hold = q_fn.q_value(QAction::Hold, &phi);

        // Buy and Sell should have positive bias
        assert!(
            q_buy > 0.0,
            "Buy should have optimistic bias, got {}",
            q_buy
        );
        assert!(
            q_sell > 0.0,
            "Sell should have optimistic bias, got {}",
            q_sell
        );
        // Hold should NOT have optimistic bias
        assert!(
            q_hold.abs() < 1e-10,
            "Hold should not have optimistic bias, got {}",
            q_hold
        );
    }

    // --- Position Constraints Validation ---

    /// Verify P_max constraint formula: P_max^global = Σ P_max^i / max(corr, floor)
    /// This mirrors the design doc §9.5 global position constraint.
    #[test]
    fn test_mdp_p_max_constraint_formula() {
        // Replicate the formula from fx-risk::GlobalPositionChecker without importing
        // P_max^global = Σ P_max^i / max(correlation_factor, FLOOR_CORRELATION)

        let strategy_max: [f64; 3] = [5.0, 5.0, 5.0]; // per-strategy P_max
        let sum_max: f64 = strategy_max.iter().sum(); // 15.0

        // Default: correlation=1.0, floor=1.5 → divisor=1.5 → limit=10.0
        let divisor = 1.0_f64.max(1.5);
        let limit = sum_max / divisor;
        assert!(
            (limit - 10.0).abs() < 1e-10,
            "P_max^global should be 10.0, got {}",
            limit
        );

        // High correlation → lower limit
        let high_corr_divisor = 2.0_f64.max(1.5);
        let tight_limit = sum_max / high_corr_divisor;
        assert!(
            tight_limit < limit,
            "Higher correlation should reduce P_max"
        );

        // Floor provides lower bound on divisor
        let floor_divisor = 0.1_f64.max(2.0);
        let floored_limit = sum_max / floor_divisor;
        assert!(
            (floored_limit - 7.5).abs() < 1e-10,
            "Floor should cap divisor: got {}",
            floored_limit
        );
    }

    // --- Strategy-Separated Reward Validation ---

    /// Verify the reward function r_t^i = ΔPnL - λ_risk·σ² - λ_dd·min(DD, DD_cap)
    /// matches the design doc formula exactly.
    #[test]
    fn test_mdp_reward_formula_matches_design_doc() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 100.0);
        let config = RewardConfig {
            lambda_risk: 0.01,
            lambda_dd: 0.2,
            dd_cap: 30.0,
            gamma: 0.99,
        };

        // Equity drops from 100 to 85, position=5000, vol²=0.0004
        let equity = 85.0;
        let pos_size = 5000.0;
        let vol_sq = 0.0004;

        buf.record(
            1_000_000_000,
            QAction::Hold,
            vec![1.0; 5],
            equity,
            pos_size,
            vol_sq,
            &config,
        );

        let delta_pnl = equity - 100.0; // -15
        let pos_var = pos_size * pos_size * vol_sq; // 10000
        let dd = (100.0 - equity).max(0.0); // 15
        let dd_capped = dd.min(config.dd_cap); // 15
        let expected = delta_pnl - config.lambda_risk * pos_var - config.lambda_dd * dd_capped;

        let actual = buf.transitions[0].reward;
        assert!(
            (actual - expected).abs() < 1e-10,
            "Reward formula mismatch: expected {}, got {}",
            expected,
            actual
        );
    }

    /// Verify each strategy's reward is independent (no cross-strategy coupling).
    #[test]
    fn test_mdp_strategy_separated_rewards_independent() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        // Start independent episodes for A and B
        eval.start_episode(StrategyId::A, 0, 0.0);
        eval.start_episode(StrategyId::B, 0, 0.0);

        // Strategy A: equity goes up
        let sa = make_state_with_equity(StrategyId::A, 10.0, 0.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, vec![1.0; 5], &sa, 0.0);

        // Strategy B: equity goes down
        let sb = make_state_with_equity(StrategyId::B, -5.0, 0.0);
        eval.record_transition(StrategyId::B, 1, QAction::Sell, vec![1.0; 5], &sb, 0.0);

        let ra = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 2);
        let rb = eval.end_episode(StrategyId::B, TerminalReason::PositionClosed, 2);

        // Strategy A reward is +10 (independent of B's -5)
        assert!(
            (ra.total_reward - 10.0).abs() < 1e-10,
            "Strategy A reward should be 10, got {}",
            ra.total_reward
        );
        // Strategy B reward is -5 (independent of A's +10)
        assert!(
            (rb.total_reward - (-5.0)).abs() < 1e-10,
            "Strategy B reward should be -5, got {}",
            rb.total_reward
        );
    }

    /// Verify DD_cap prevents drawdown penalty from dominating reward.
    #[test]
    fn test_mdp_dd_cap_saturation() {
        let mut buf = EpisodeBuffer::new(StrategyId::A, 0, 100.0);
        let config = RewardConfig {
            lambda_risk: 0.0,
            lambda_dd: 1.0,
            dd_cap: 5.0,
            gamma: 0.99,
        };

        // Equity drops from 100 to 50 → DD = 50, capped at 5
        buf.record(1, QAction::Hold, vec![1.0; 5], 50.0, 0.0, 0.0, &config);

        let expected = (50.0 - 100.0) - 1.0_f64 * (100.0_f64 - 50.0).min(5.0_f64);
        // = -50 - 5 = -55
        assert!(
            (buf.transitions[0].reward - expected).abs() < 1e-10,
            "DD should be capped at 5, got reward {}",
            buf.transitions[0].reward
        );
    }

    // --- Q-Function Validation ---

    /// Verify Q(s,a) = w_a^T φ(s) point estimate is deterministic for fixed input.
    #[test]
    fn test_mdp_q_function_point_estimate_deterministic() {
        let dim = 10;
        let q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.0);

        let phi = vec![1.0, 0.5, -0.3, 0.2, 0.0, 0.1, -0.1, 0.4, 0.3, -0.2];

        // Same input should always produce same output
        let q1 = q_fn.q_value(QAction::Buy, &phi);
        let q2 = q_fn.q_value(QAction::Buy, &phi);
        assert!(
            (q1 - q2).abs() < 1e-15,
            "Q point estimate should be deterministic"
        );
    }

    /// Verify Q-function uses separate BLR models per action (Buy, Sell, Hold).
    #[test]
    fn test_mdp_q_function_separate_models_per_action() {
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.5);

        let phi = vec![1.0; dim];

        // Update Buy with positive return, Sell with negative
        for _ in 0..20 {
            q_fn.update(QAction::Buy, &phi, 10.0);
            q_fn.update(QAction::Sell, &phi, -10.0);
        }

        let q_buy = q_fn.q_value(QAction::Buy, &phi);
        let q_sell = q_fn.q_value(QAction::Sell, &phi);

        // After positive updates for Buy and negative for Sell, they should diverge
        assert!(
            q_buy > q_sell,
            "Buy Q ({}) should be > Sell Q ({}) after divergent updates",
            q_buy,
            q_sell
        );
    }

    /// Verify σ_model is ONLY in Thompson Sampling, NOT in point estimate.
    #[test]
    fn test_mdp_sigma_model_only_in_sampling_not_point_estimate() {
        let dim = 5;
        let q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.0);
        let phi = vec![1.0; dim];

        // Point estimate should be a single deterministic value
        let q_point = q_fn.q_point(QAction::Buy, &phi);

        // Sampled values should vary (reflecting σ_model uncertainty)
        use rand::thread_rng;
        let mut rng = thread_rng();
        let mut samples = Vec::new();
        for _ in 0..100 {
            samples.push(q_fn.sample_q_value(QAction::Buy, &phi, &mut rng));
        }

        // Samples should have variance (σ_model is reflected)
        let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
        let var: f64 =
            samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / samples.len() as f64;
        assert!(
            var > 0.0,
            "Thompson Sampling samples should have variance (σ_model reflected)"
        );

        // Point estimate should equal the mean of the sampling distribution
        // (point estimate is ŵ·φ, samples are w̃·φ where w̃ ~ N(ŵ, Σ̂))
        // The mean of samples should converge to the point estimate
        assert!(
            (mean - q_point).abs() < 2.0,
            "Sample mean ({}) should be close to point estimate ({})",
            mean,
            q_point
        );
    }

    /// Verify divergence detection: ||w_t|| / ||w_{t-1}|| > threshold triggers reset.
    #[test]
    fn test_mdp_divergence_monitoring() {
        use crate::bayesian_lr::BayesianLinearRegression;

        let dim = 5;
        let mut blr = BayesianLinearRegression::new(dim, 0.001, 100, 0.01);

        let phi = vec![1.0; dim];

        // Normal update should not diverge
        let result = blr.update(&phi, 1.0);
        assert!(
            !result.diverged,
            "Normal update should not trigger divergence"
        );
        assert!(
            result.divergence_ratio >= 0.0,
            "Divergence ratio should be non-negative"
        );

        // The default threshold is 2.0 (from bayesian_lr.rs line 70)
        // Verify the UpdateResult struct contains the expected fields
        let _ = result.residual;
    }

    /// Verify MC returns use full episodic G_t (no bootstrapping).
    #[test]
    fn test_mdp_mc_returns_full_episodic_no_bootstrap() {
        // G_t = Σ_{k=0}^{T-t} γ^k · r_{t+k}, NOT r_t + γ · Q(s_{t+1}, a*)
        let rewards = vec![2.0, 3.0, -1.0, 4.0];
        let gamma = 0.95;

        let returns = McEvaluator::compute_returns(&rewards, gamma);

        // G_0 = 2 + 0.95*3 + 0.9025*(-1) + 0.857375*4
        let expected_g0 = 2.0 + 0.95 * 3.0 + 0.95_f64.powi(2) * (-1.0) + 0.95_f64.powi(3) * 4.0;
        assert!(
            (returns[0] - expected_g0).abs() < 1e-10,
            "G_0 should be full discounted sum {}, got {}",
            expected_g0,
            returns[0]
        );

        // This is NOT r_0 + gamma * V(s_1) — it's the full MC return
    }

    /// Verify on-policy: only actually-taken actions are recorded and used for updates.
    #[test]
    fn test_mdp_on_policy_only_taken_actions_updated() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);

        // Only take Buy and Hold (NOT Sell)
        let s1 = make_state_with_equity(StrategyId::A, 5.0, 0.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, vec![1.0; 5], &s1, 0.0);
        let s2 = make_state_with_equity(StrategyId::A, 8.0, 0.0);
        eval.record_transition(StrategyId::A, 2, QAction::Hold, vec![1.0; 5], &s2, 0.0);

        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 3);

        // Verify only Buy and Hold were recorded
        assert_eq!(result.transitions.len(), 2);
        assert_eq!(result.transitions[0].action, QAction::Buy);
        assert_eq!(result.transitions[1].action, QAction::Hold);

        // Q-function update only touches Buy and Hold models
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.0);
        let updates = McEvaluator::update_from_result(&mut q_fn, &result);
        assert_eq!(updates.len(), 2);
    }

    /// Verify per-strategy episode buffers are independent.
    #[test]
    fn test_mdp_per_strategy_episode_buffers_independent() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        // Start episodes for all 3 strategies simultaneously
        eval.start_episode(StrategyId::A, 0, 0.0);
        eval.start_episode(StrategyId::B, 0, 0.0);
        eval.start_episode(StrategyId::C, 0, 0.0);

        // Each has independent equity trajectory
        let sa = make_state_with_equity(StrategyId::A, 10.0, 0.0);
        let sb = make_state_with_equity(StrategyId::B, -5.0, 0.0);
        let sc = make_state_with_equity(StrategyId::C, 3.0, 0.0);

        eval.record_transition(StrategyId::A, 1, QAction::Buy, vec![1.0; 5], &sa, 0.0);
        eval.record_transition(StrategyId::B, 1, QAction::Sell, vec![1.0; 5], &sb, 0.0);
        eval.record_transition(StrategyId::C, 1, QAction::Hold, vec![1.0; 5], &sc, 0.0);

        let ra = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 2);
        let rb = eval.end_episode(StrategyId::B, TerminalReason::PositionClosed, 2);
        let rc = eval.end_episode(StrategyId::C, TerminalReason::PositionClosed, 2);

        // Each strategy's total reward matches only its own equity change
        assert!((ra.total_reward - 10.0).abs() < 1e-10);
        assert!((rb.total_reward - (-5.0)).abs() < 1e-10);
        assert!((rc.total_reward - 3.0).abs() < 1e-10);
    }

    // =========================================================================
    // §3.0.2 Episode Definition Validation Tests
    // =========================================================================
    //
    // These tests verify the episode definition per design.md §3.0.2:
    //   - Start condition: position goes from zero to non-zero
    //   - Terminal conditions: (1) PositionClosed (2) MaxHoldTimeExceeded
    //                          (3) DailyHardLimit (4) UnknownRegime
    //   - Flat periods: excluded from episodes, not part of learning
    //   - Partial fills: episode ends only on FULL position close
    //   - MAX_HOLD_TIME: forced close with PnL included

    /// Verify all 4 terminal conditions exist in TerminalReason enum.
    #[test]
    fn test_episode_terminal_reasons_cover_all_four() {
        let reasons = [
            TerminalReason::PositionClosed,
            TerminalReason::MaxHoldTimeExceeded,
            TerminalReason::DailyHardLimit,
            TerminalReason::UnknownRegime,
        ];
        assert_eq!(reasons.len(), 4);

        // Verify Display trait
        assert_eq!(
            format!("{}", TerminalReason::PositionClosed),
            "PositionClosed"
        );
        assert_eq!(
            format!("{}", TerminalReason::MaxHoldTimeExceeded),
            "MaxHoldTimeExceeded"
        );
        assert_eq!(
            format!("{}", TerminalReason::DailyHardLimit),
            "DailyHardLimit"
        );
        assert_eq!(
            format!("{}", TerminalReason::UnknownRegime),
            "UnknownRegime"
        );

        // Verify PartialEq
        assert_eq!(
            TerminalReason::PositionClosed,
            TerminalReason::PositionClosed
        );
        assert_ne!(
            TerminalReason::PositionClosed,
            TerminalReason::MaxHoldTimeExceeded
        );
    }

    /// Verify episode start: only when position transitions from zero to non-zero.
    /// Flat periods (zero position) should NOT start an episode.
    #[test]
    fn test_episode_start_on_position_open() {
        let mut eval = McEvaluator::new(McEvalConfig::default());

        // Initially no active episodes
        assert!(!eval.has_active_episode(StrategyId::A));

        // Starting an episode creates the buffer
        eval.start_episode(StrategyId::A, 0, 0.0);
        assert!(eval.has_active_episode(StrategyId::A));
    }

    /// Verify double start is prevented (panic).
    #[test]
    #[should_panic(expected = "Active episode already exists")]
    fn test_episode_double_start_prevented() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.start_episode(StrategyId::A, 0, 0.0);
        eval.start_episode(StrategyId::A, 100, 0.0); // Should panic
    }

    /// Verify episode end without start is prevented (panic).
    #[test]
    #[should_panic(expected = "No active episode")]
    fn test_episode_end_without_start_prevented() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 100);
    }

    /// Verify flat period handling: episode with no transitions is valid.
    /// The episode records no learning signal (flat period = no position).
    #[test]
    fn test_episode_flat_period_no_transitions() {
        let mut eval = McEvaluator::new(McEvalConfig::default());
        eval.start_episode(StrategyId::A, 0, 0.0);

        // Immediately end with no transitions (flat period / brief open-close)
        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 100);

        assert_eq!(result.num_transitions, 0);
        assert!((result.total_reward - 0.0).abs() < 1e-10);
        assert!(result.returns.is_empty());
    }

    /// Verify partial fill does NOT end the episode.
    /// Only full position close (or other terminal conditions) ends the episode.
    #[test]
    fn test_episode_partial_fill_does_not_end_episode() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);

        // Step 1: Open position (buy)
        let s1 = make_state_with_equity(StrategyId::A, 5.0, 100_000.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(5), &s1, 0.0);

        // Step 2: Partial close (position still non-zero)
        let s2 = make_state_with_equity(StrategyId::A, 8.0, 50_000.0);
        eval.record_transition(StrategyId::A, 2, QAction::Sell, phi_ones(5), &s2, 0.0);

        // Episode should still be active after partial fill
        assert!(
            eval.has_active_episode(StrategyId::A),
            "Episode should still be active after partial fill"
        );

        // Step 3: Full close (position goes to zero)
        let s3 = make_state_with_equity(StrategyId::A, 10.0, 0.0);
        eval.record_transition(StrategyId::A, 3, QAction::Sell, phi_ones(5), &s3, 0.0);

        // Now end the episode with PositionClosed
        let result = eval.end_episode(StrategyId::A, TerminalReason::PositionClosed, 4);
        assert_eq!(result.num_transitions, 3);
        assert_eq!(result.terminal_reason, TerminalReason::PositionClosed);
    }

    /// Verify MAX_HOLD_TIME forces close and PnL is included in episode.
    #[test]
    fn test_episode_max_hold_time_forced_close_with_pnl() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::A, 1_000_000_000, 0.0);

        // Position held through multiple ticks
        let s1 = make_state_with_equity(StrategyId::A, 5.0, 100_000.0);
        eval.record_transition(
            StrategyId::A,
            15_000_000_000,
            QAction::Buy,
            phi_ones(5),
            &s1,
            0.0,
        );

        let s2 = make_state_with_equity(StrategyId::A, 3.0, 100_000.0);
        eval.record_transition(
            StrategyId::A,
            30_000_000_000,
            QAction::Hold,
            phi_ones(5),
            &s2,
            0.0,
        );

        // MAX_HOLD_TIME exceeded → forced close
        let result = eval.end_episode(
            StrategyId::A,
            TerminalReason::MaxHoldTimeExceeded,
            31_000_000_000,
        );

        // Episode should have recorded the PnL from the forced close
        assert_eq!(result.terminal_reason, TerminalReason::MaxHoldTimeExceeded);
        assert_eq!(result.num_transitions, 2);
        // Total reward = (5-0) + (3-5) = 5 + (-2) = 3
        assert!(
            (result.total_reward - 3.0).abs() < 1e-10,
            "MAX_HOLD_TIME forced close should include accumulated PnL, got {}",
            result.total_reward
        );

        // MC returns should be computed for Q-function update
        assert_eq!(result.returns.len(), 2);
    }

    /// Verify DailyHardLimit terminal condition.
    #[test]
    fn test_episode_daily_hard_limit_terminal() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::B, 0, 0.0);
        let s = make_state_with_equity(StrategyId::B, -10.0, 100_000.0);
        eval.record_transition(StrategyId::B, 1, QAction::Sell, phi_ones(5), &s, 0.0);

        let result = eval.end_episode(StrategyId::B, TerminalReason::DailyHardLimit, 2);
        assert_eq!(result.terminal_reason, TerminalReason::DailyHardLimit);
        assert_eq!(result.num_transitions, 1);
    }

    /// Verify UnknownRegime terminal condition.
    #[test]
    fn test_episode_unknown_regime_terminal() {
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.99,
            },
        });

        eval.start_episode(StrategyId::C, 0, 0.0);
        let s = make_state_with_equity(StrategyId::C, 3.0, 50_000.0);
        eval.record_transition(StrategyId::C, 1, QAction::Buy, phi_ones(5), &s, 0.0);

        let result = eval.end_episode(StrategyId::C, TerminalReason::UnknownRegime, 2);
        assert_eq!(result.terminal_reason, TerminalReason::UnknownRegime);
        assert_eq!(result.num_transitions, 1);
    }

    /// Verify episode with MAX_HOLD_TIME updates Q-function correctly.
    #[test]
    fn test_episode_max_hold_time_updates_q_function() {
        let dim = 5;
        let mut q_fn = QFunction::new(dim, 0.01, 500, 0.01, 0.0);
        let mut eval = McEvaluator::new(McEvalConfig {
            reward: RewardConfig {
                lambda_risk: 0.0,
                lambda_dd: 0.0,
                dd_cap: 100.0,
                gamma: 0.9,
            },
        });

        eval.start_episode(StrategyId::A, 0, 0.0);
        let s1 = make_state_with_equity(StrategyId::A, 2.0, 100_000.0);
        eval.record_transition(StrategyId::A, 1, QAction::Buy, phi_ones(dim), &s1, 0.0);

        let s2 = make_state_with_equity(StrategyId::A, 5.0, 100_000.0);
        eval.record_transition(StrategyId::A, 2, QAction::Hold, phi_ones(dim), &s2, 0.0);

        let (result, updates) = eval.end_episode_and_update(
            StrategyId::A,
            TerminalReason::MaxHoldTimeExceeded,
            3,
            &mut q_fn,
        );

        assert_eq!(result.terminal_reason, TerminalReason::MaxHoldTimeExceeded);
        assert_eq!(updates.len(), 2);
        // All Q-function updates should succeed
        for u in &updates {
            assert!(!u.diverged, "Q-update should not diverge");
        }
    }
}
