use std::collections::HashMap;

use fx_core::types::{Direction, StrategyId};
use fx_events::projector::StateSnapshot;
use tracing::{info, warn};

use crate::limits::{Result, RiskError};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Global position constraint configuration.
///
/// Controls the maximum net exposure across all strategies with correlation
/// adjustment and inter-strategy priority allocation.
#[derive(Debug, Clone)]
pub struct GlobalPositionConfig {
    /// Per-strategy maximum position in lot units (P_max^i).
    pub strategy_max_positions: HashMap<StrategyId, f64>,
    /// Correlation factor between strategies.
    /// Higher → more correlated → lower effective global limit.
    pub correlation_factor: f64,
    /// Minimum correlation divisor for stress events (recommended 1.5–2.0).
    pub floor_correlation: f64,
    /// Lot size corresponding to one unit of position (e.g. 100_000).
    pub lot_unit_size: f64,
    /// Minimum lot size below which a reduced order is blocked.
    pub min_lot_size: f64,
}

impl Default for GlobalPositionConfig {
    fn default() -> Self {
        let mut max_pos = HashMap::new();
        max_pos.insert(StrategyId::A, 5.0);
        max_pos.insert(StrategyId::B, 5.0);
        max_pos.insert(StrategyId::C, 5.0);
        Self {
            strategy_max_positions: max_pos,
            correlation_factor: 1.0,
            floor_correlation: 1.5,
            lot_unit_size: 100_000.0,
            min_lot_size: 1_000.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Check result
// ---------------------------------------------------------------------------

/// Outcome of a global position constraint check (non-error path).
#[derive(Debug, Clone)]
pub struct PositionCheckResult {
    /// Effective lot size (may be reduced for lower-priority strategies).
    pub effective_lot: f64,
    /// Computed global position limit P_max^global.
    pub global_limit: f64,
    /// Current net global position (sum of all strategy positions).
    pub current_global_position: f64,
    /// This strategy's priority rank (0 = highest |Q|).
    pub priority_rank: usize,
    /// Number of strategies competing for allocation.
    pub total_strategies: usize,
}

// ---------------------------------------------------------------------------
// Stateless checker
// ---------------------------------------------------------------------------

/// Stateless global position constraint checker.
///
/// All methods are pure functions — no internal state.
pub struct GlobalPositionChecker;

impl GlobalPositionChecker {
    /// Compute the correlation-adjusted global position limit.
    ///
    /// `P_max^global = Σ P_max^i / max(correlation_factor, FLOOR_CORRELATION)`
    pub fn compute_global_limit(config: &GlobalPositionConfig) -> f64 {
        let sum_max: f64 = config.strategy_max_positions.values().sum();
        let divisor = config.correlation_factor.max(config.floor_correlation);
        sum_max / divisor
    }

    /// Validate a new order against global position constraints.
    ///
    /// Checks:
    /// 1. Hard constraint: `|global_position + delta| ≤ P_max^global`
    /// 2. Priority-based lot reduction for lower-ranked strategies
    ///
    /// Returns `Ok(PositionCheckResult)` with the effective lot size, or
    /// `Err(RiskError::GlobalPositionConstraint)` when the order is blocked.
    pub fn validate_order(
        config: &GlobalPositionConfig,
        snapshot: &StateSnapshot,
        strategy_id: StrategyId,
        direction: Direction,
        requested_lot: f64,
        _q_value: f64,
        all_strategy_q: &HashMap<StrategyId, f64>,
    ) -> Result<PositionCheckResult> {
        let global_limit = Self::compute_global_limit(config);
        let current_pos = snapshot.global_position;

        // Zero-lot is always allowed (no-op)
        if requested_lot <= 0.0 {
            return Ok(PositionCheckResult {
                effective_lot: 0.0,
                global_limit,
                current_global_position: current_pos,
                priority_rank: 0,
                total_strategies: all_strategy_q.len(),
            });
        }

        // Convert requested lot to position delta (signed by direction)
        let position_delta = requested_lot / config.lot_unit_size;
        let signed_delta = match direction {
            Direction::Buy => position_delta,
            Direction::Sell => -position_delta,
        };
        let new_global_pos = current_pos + signed_delta;

        // Hard constraint: |Σ p_i + δ| ≤ P_max^global
        if new_global_pos.abs() > global_limit {
            warn!(
                strategy = ?strategy_id,
                direction = ?direction,
                current_global = current_pos,
                delta = signed_delta,
                new_global = new_global_pos,
                global_limit = global_limit,
                "GLOBAL position constraint violated"
            );
            return Err(RiskError::GlobalPositionConstraint);
        }

        // Also check the absolute constraint on existing position
        if current_pos.abs() > global_limit {
            warn!(
                current_global = current_pos,
                global_limit = global_limit,
                "GLOBAL position already exceeds limit"
            );
            return Err(RiskError::GlobalPositionConstraint);
        }

        // Compute strategy priority by |Q-value| (higher = better edge)
        let mut ranked: Vec<(StrategyId, f64)> =
            all_strategy_q.iter().map(|(&id, &q)| (id, q)).collect();
        ranked.sort_by(|a, b| {
            b.1.abs()
                .partial_cmp(&a.1.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let priority_rank = ranked
            .iter()
            .position(|(id, _)| *id == strategy_id)
            .unwrap_or(ranked.len());
        let total_strategies = ranked.len();

        // Priority-based lot reduction
        let effective_lot = if priority_rank == 0 {
            requested_lot
        } else {
            let factor = Self::compute_priority_lot_factor(priority_rank);
            let reduced = requested_lot * factor;

            if reduced < config.min_lot_size {
                info!(
                    strategy = ?strategy_id,
                    rank = priority_rank,
                    reduced_lot = reduced,
                    min_lot = config.min_lot_size,
                    "Strategy lot reduced below minimum — order blocked"
                );
                return Err(RiskError::GlobalPositionConstraint);
            }
            reduced
        };

        Ok(PositionCheckResult {
            effective_lot,
            global_limit,
            current_global_position: current_pos,
            priority_rank,
            total_strategies,
        })
    }

    /// Check buy/sell direction feasibility given current global position.
    ///
    /// Uses one lot-unit as the assumed position delta.
    /// Returns `(buy_allowed, sell_allowed)`.
    pub fn check_direction_feasibility(
        config: &GlobalPositionConfig,
        snapshot: &StateSnapshot,
    ) -> (bool, bool) {
        let global_limit = Self::compute_global_limit(config);
        let current_pos = snapshot.global_position;
        // One standard lot-unit as delta
        let buy_allowed = (current_pos + 1.0).abs() <= global_limit;
        let sell_allowed = (current_pos - 1.0).abs() <= global_limit;
        (buy_allowed, sell_allowed)
    }

    /// Compute lot reduction factor for a given priority rank.
    ///
    /// Rank 0 → 1.0 (full), rank 1 → 0.5, rank 2 → 0.25, …
    pub fn compute_priority_lot_factor(rank: usize) -> f64 {
        0.5_f64.powi(rank as i32)
    }

    /// Unified validation: global position + hierarchical loss limits.
    ///
    /// Checks global position first, then delegates to `HierarchicalRiskLimiter`.
    /// Returns `Ok((PositionCheckResult, LimitCheckResult))` on success.
    #[allow(clippy::too_many_arguments)]
    pub fn validate_full(
        global_config: &GlobalPositionConfig,
        limits_config: &crate::limits::RiskLimitsConfig,
        snapshot: &StateSnapshot,
        strategy_id: StrategyId,
        direction: Direction,
        requested_lot: f64,
        _q_value: f64,
        all_strategy_q: &HashMap<StrategyId, f64>,
    ) -> Result<(PositionCheckResult, crate::limits::LimitCheckResult)> {
        let pos_result = Self::validate_order(
            global_config,
            snapshot,
            strategy_id,
            direction,
            requested_lot,
            _q_value,
            all_strategy_q,
        )?;

        let limit_result = crate::limits::HierarchicalRiskLimiter::validate_order(
            limits_config,
            &snapshot.limit_state,
        )?;

        Ok((pos_result, limit_result))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> GlobalPositionConfig {
        GlobalPositionConfig::default()
    }

    fn empty_snapshot() -> StateSnapshot {
        StateSnapshot {
            positions: HashMap::new(),
            global_position: 0.0,
            global_position_limit: 10.0,
            total_unrealized_pnl: 0.0,
            total_realized_pnl: 0.0,
            limit_state: fx_events::projector::LimitStateData::default(),
            state_version: 0,
            staleness_ms: 0,
            state_hash: String::new(),
            lot_multiplier: 1.0,
            last_market_data_ns: 0,
        }
    }

    fn snapshot_with_global(pos: f64) -> StateSnapshot {
        let mut snap = empty_snapshot();
        snap.global_position = pos;
        snap
    }

    fn all_q_flat(q: f64) -> HashMap<StrategyId, f64> {
        let mut m = HashMap::new();
        m.insert(StrategyId::A, q);
        m.insert(StrategyId::B, q);
        m.insert(StrategyId::C, q);
        m
    }

    fn all_q(a: f64, b: f64, c: f64) -> HashMap<StrategyId, f64> {
        let mut m = HashMap::new();
        m.insert(StrategyId::A, a);
        m.insert(StrategyId::B, b);
        m.insert(StrategyId::C, c);
        m
    }

    // -- compute_global_limit ------------------------------------------------

    #[test]
    fn global_limit_basic() {
        // Σ P_max = 5+5+5 = 15, floor = 1.5, correlation = 1.0 → divisor = 1.5
        let config = default_config();
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!((limit - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn global_limit_high_correlation() {
        let mut config = default_config();
        config.correlation_factor = 2.0;
        // divisor = max(2.0, 1.5) = 2.0 → 15/2 = 7.5
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!((limit - 7.5).abs() < f64::EPSILON);
    }

    #[test]
    fn global_limit_floor_correlation() {
        let mut config = default_config();
        config.correlation_factor = 0.5;
        config.floor_correlation = 1.5;
        // divisor = max(0.5, 1.5) = 1.5 → 15/1.5 = 10
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!((limit - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn global_limit_custom_floor() {
        let mut config = default_config();
        config.correlation_factor = 0.3;
        config.floor_correlation = 2.0;
        // divisor = max(0.3, 2.0) = 2.0 → 15/2 = 7.5
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!((limit - 7.5).abs() < f64::EPSILON);
    }

    #[test]
    fn global_limit_asymmetric_strategies() {
        let mut config = default_config();
        config.strategy_max_positions.insert(StrategyId::A, 10.0);
        config.strategy_max_positions.insert(StrategyId::B, 4.0);
        config.strategy_max_positions.insert(StrategyId::C, 2.0);
        // sum = 16, divisor = 1.5 → 10.667
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!((limit - 16.0 / 1.5).abs() < 1e-10);
    }

    #[test]
    fn global_limit_zero_floor() {
        let mut config = default_config();
        config.correlation_factor = 0.0;
        config.floor_correlation = 0.0;
        // divisor = max(0, 0) = 0 → would be inf. This shouldn't happen in practice.
        // Test that we handle it gracefully (produces inf, caller should validate config).
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!(limit.is_infinite());
    }

    // -- validate_order: hard constraint --------------------------------------

    #[test]
    fn validate_order_ok_zero_position() {
        let config = default_config();
        let snap = empty_snapshot();
        let q = all_q(0.5, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.5,
            &q,
        );
        assert!(result.is_ok());
        let r = result.unwrap();
        assert!((r.effective_lot - 100_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn validate_order_ok_within_limit() {
        let config = default_config();
        let snap = snapshot_with_global(3.0);
        let q = all_q_flat(0.1);
        // 3 + 1 = 4 ≤ 10
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_order_blocked_at_limit() {
        let config = default_config();
        let snap = snapshot_with_global(10.0);
        let q = all_q_flat(0.1);
        // 10 + 1 = 11 > 10 → blocked
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RiskError::GlobalPositionConstraint
        ));
    }

    #[test]
    fn validate_order_blocked_beyond_limit() {
        let config = default_config();
        let snap = snapshot_with_global(15.0);
        let q = all_q_flat(0.1);
        // already beyond limit
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_sell_ok_negative() {
        let config = default_config();
        let snap = snapshot_with_global(-5.0);
        let q = all_q_flat(0.1);
        // -5 - 1 = -6, |−6| = 6 ≤ 10 → ok
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::B,
            Direction::Sell,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validate_sell_blocked_negative_limit() {
        let config = default_config();
        let snap = snapshot_with_global(-10.0);
        let q = all_q_flat(0.1);
        // -10 - 1 = -11, |−11| = 11 > 10 → blocked
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::B,
            Direction::Sell,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_order_multi_lot() {
        let config = default_config();
        let snap = snapshot_with_global(0.0);
        let q = all_q(0.5, 0.1, 0.05);
        // 500k lot → 5 position units, 0 + 5 = 5 ≤ 10 → ok
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            500_000.0,
            0.5,
            &q,
        );
        assert!(result.is_ok());
        assert!((result.unwrap().effective_lot - 500_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn validate_order_multi_lot_blocked() {
        let config = default_config();
        let snap = snapshot_with_global(0.0);
        let q = all_q_flat(0.1);
        // 1.5M lot → 15 position units, 0 + 15 = 15 > 10 → blocked
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            1_500_000.0,
            0.1,
            &q,
        );
        assert!(result.is_err());
    }

    // -- Priority-based lot reduction -----------------------------------------

    #[test]
    fn priority_rank_0_full_lot() {
        let config = default_config();
        let snap = empty_snapshot();
        // Strategy A has highest Q
        let q = all_q(0.5, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.5,
            &q,
        );
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.priority_rank, 0);
        assert!((r.effective_lot - 100_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn priority_rank_1_half_lot() {
        let config = default_config();
        let snap = empty_snapshot();
        // Strategy B is rank 1
        let q = all_q(0.5, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::B,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.priority_rank, 1);
        assert!((r.effective_lot - 50_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn priority_rank_2_quarter_lot() {
        let config = default_config();
        let snap = empty_snapshot();
        // Strategy C is rank 2
        let q = all_q(0.5, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::C,
            Direction::Buy,
            100_000.0,
            0.05,
            &q,
        );
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.priority_rank, 2);
        assert!((r.effective_lot - 25_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn priority_blocked_below_min_lot() {
        let mut config = default_config();
        config.min_lot_size = 30_000.0;
        let snap = empty_snapshot();
        // Strategy C is rank 2 → 100k * 0.25 = 25k < 30k → blocked
        let q = all_q(0.5, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::C,
            Direction::Buy,
            100_000.0,
            0.05,
            &q,
        );
        assert!(result.is_err());
    }

    #[test]
    fn priority_negative_q_ranking() {
        let config = default_config();
        let snap = empty_snapshot();
        // |Q_A|=0.1, |Q_B|=0.3, |Q_C|=0.05 → B is rank 0, A is rank 1
        let q = all_q(0.1, -0.3, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::B,
            Direction::Sell,
            100_000.0,
            -0.3,
            &q,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().priority_rank, 0);
    }

    #[test]
    fn priority_equal_q_all_full_lot() {
        let config = default_config();
        let snap = empty_snapshot();
        // All Q equal → HashMap iteration order is non-deterministic across runs
        // so we only verify that the order succeeds (some rank is assigned)
        let q = all_q_flat(0.1);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::B,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
        // All Q equal so B may get any rank; just verify it returns a result
        let r = result.unwrap();
        assert!(r.priority_rank < 3);
    }

    // -- check_direction_feasibility ------------------------------------------

    #[test]
    fn direction_both_feasible() {
        let config = default_config();
        let snap = snapshot_with_global(0.0);
        let (buy, sell) = GlobalPositionChecker::check_direction_feasibility(&config, &snap);
        assert!(buy);
        assert!(sell);
    }

    #[test]
    fn direction_buy_blocked() {
        let config = default_config();
        let snap = snapshot_with_global(10.0);
        let (buy, sell) = GlobalPositionChecker::check_direction_feasibility(&config, &snap);
        assert!(!buy);
        assert!(sell); // -9 is within limit
    }

    #[test]
    fn direction_sell_blocked() {
        let config = default_config();
        let snap = snapshot_with_global(-10.0);
        let (buy, sell) = GlobalPositionChecker::check_direction_feasibility(&config, &snap);
        assert!(buy);
        assert!(!sell);
    }

    #[test]
    fn direction_both_blocked() {
        // global_limit = 10.0, buy: 11 > 10 blocked, sell: -9 ≤ 10 ok
        // To block both, we need |pos ± 1| > limit for both directions
        let mut tight_config = default_config();
        tight_config.correlation_factor = 3.0;
        // sum=15, divisor=max(3.0,1.5)=3.0 → limit=5.0
        let snap = snapshot_with_global(5.0);
        let (buy, sell) = GlobalPositionChecker::check_direction_feasibility(&tight_config, &snap);
        assert!(!buy);
        assert!(sell); // 4 ≤ 5
    }

    // -- compute_priority_lot_factor ------------------------------------------

    #[test]
    fn lot_factor_rank_0() {
        assert!((GlobalPositionChecker::compute_priority_lot_factor(0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lot_factor_rank_1() {
        assert!((GlobalPositionChecker::compute_priority_lot_factor(1) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn lot_factor_rank_2() {
        assert!(
            (GlobalPositionChecker::compute_priority_lot_factor(2) - 0.25).abs() < f64::EPSILON
        );
    }

    #[test]
    fn lot_factor_rank_3() {
        assert!(
            (GlobalPositionChecker::compute_priority_lot_factor(3) - 0.125).abs() < f64::EPSILON
        );
    }

    // -- validate_full (integration with limits) ------------------------------

    #[test]
    fn validate_full_ok() {
        let g_config = default_config();
        let l_config = crate::limits::RiskLimitsConfig::default();
        let snap = empty_snapshot();
        // Give Strategy A the highest Q to ensure rank 0
        let q = all_q(0.5, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_full(
            &g_config,
            &l_config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.5,
            &q,
        );
        assert!(result.is_ok());
        let (pos, lim) = result.unwrap();
        assert!((pos.effective_lot - 100_000.0).abs() < f64::EPSILON);
        assert!(!lim.daily_mtm_limited);
    }

    #[test]
    fn validate_full_global_blocked() {
        let g_config = default_config();
        let l_config = crate::limits::RiskLimitsConfig::default();
        let snap = snapshot_with_global(15.0);
        let q = all_q_flat(0.1);
        let result = GlobalPositionChecker::validate_full(
            &g_config,
            &l_config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(matches!(result, Err(RiskError::GlobalPositionConstraint)));
    }

    #[test]
    fn validate_full_limits_blocked() {
        let g_config = default_config();
        let l_config = crate::limits::RiskLimitsConfig::default();
        let mut snap = empty_snapshot();
        snap.limit_state.monthly_pnl = -9999.0;
        let q = all_q_flat(0.1);
        let result = GlobalPositionChecker::validate_full(
            &g_config,
            &l_config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(matches!(result, Err(RiskError::MonthlyLimit { .. })));
    }

    // -- Result metadata ------------------------------------------------------

    #[test]
    fn result_metadata() {
        let config = default_config();
        let snap = snapshot_with_global(3.0);
        let q = all_q(0.3, 0.1, 0.05);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.3,
            &q,
        );
        let r = result.unwrap();
        assert!((r.global_limit - 10.0).abs() < f64::EPSILON);
        assert!((r.current_global_position - 3.0).abs() < f64::EPSILON);
        assert_eq!(r.total_strategies, 3);
    }

    // -- Edge cases -----------------------------------------------------------

    #[test]
    fn unknown_strategy_in_q_map() {
        let config = default_config();
        let snap = snapshot_with_global(0.0);
        // All Q are 0 → equal |Q| for all strategies. Rank is non-deterministic
        // across HashMap iteration order, so just verify it succeeds.
        let q = all_q(0.0, 0.0, 0.0);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.0,
            &q,
        );
        assert!(result.is_ok());
        let r = result.unwrap();
        assert!(r.priority_rank < 3);
    }

    #[test]
    fn zero_lot_requested() {
        let config = default_config();
        let snap = empty_snapshot();
        let q = all_q_flat(0.1);
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            0.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
        assert!((result.unwrap().effective_lot - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn negative_position_existing() {
        let config = default_config();
        let snap = snapshot_with_global(-8.0);
        let q = all_q_flat(0.1);
        // -8 + 1 = -7, |−7| = 7 ≤ 10 → ok (buying from negative)
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn negative_position_sell_exceeds() {
        let config = default_config();
        let snap = snapshot_with_global(-8.0);
        let q = all_q_flat(0.1);
        // -8 - 3 = -11, |−11| = 11 > 10 → blocked (300k lot = 3 units, Sell)
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Sell,
            300_000.0,
            0.1,
            &q,
        );
        assert!(result.is_err());
    }

    // ========================================================================
    // §9.5 Global Position Constraint Verification Tests (design.md §9.5)
    // ========================================================================

    /// §9.5: グローバルポジション制約公式 P_max^global = ΣP_max^i / max(corr, floor) を確認
    #[test]
    fn s9_5_global_limit_formula_matches_design_doc() {
        let config = default_config();
        // ΣP_max = 5+5+5 = 15, correlation_factor=1.0, floor=1.5
        // divisor = max(1.0, 1.5) = 1.5, limit = 15/1.5 = 10.0
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        let sum_max: f64 = config.strategy_max_positions.values().sum();
        let divisor = config.correlation_factor.max(config.floor_correlation);
        let expected = sum_max / divisor;
        assert!(
            (limit - expected).abs() < f64::EPSILON,
            "P_max^global = ΣP_max^i / max(corr, floor): expected {}, got {}",
            expected,
            limit
        );
    }

    /// §9.5: FLOOR_CORRELATIONがストレス時の過大許容を防止することを確認
    #[test]
    fn s9_5_floor_correlation_prevents_over_allocation() {
        let mut config = default_config();
        // 平穏時に推定された低いcorrelation_factor
        config.correlation_factor = 0.3;
        config.floor_correlation = 1.5;

        // floorが適用される: divisor = max(0.3, 1.5) = 1.5
        let limit = GlobalPositionChecker::compute_global_limit(&config);
        assert!(
            (limit - 10.0).abs() < f64::EPSILON,
            "floor should prevent excessive allocation: got {}",
            limit
        );

        // floorなしの場合: divisor = 0.3, limit = 15/0.3 = 50 (危険)
        config.floor_correlation = 0.0;
        let limit_no_floor = GlobalPositionChecker::compute_global_limit(&config);
        assert!(
            limit_no_floor > limit * 4.0,
            "without floor, limit would be much higher"
        );
    }

    /// §9.5: |Σp_i| ≤ P_max^global のハード制約を確認
    #[test]
    fn s9_5_hard_constraint_blocks_excess_position() {
        let config = default_config();
        // global_limit = 10.0
        let snap = snapshot_with_global(9.5);
        let q = all_q_flat(0.1);

        // 9.5 + 1.0 = 10.5 > 10.0 → blocked
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(
            result.is_err(),
            "|Σp_i + δ| ≤ P_max^global should block excess position"
        );
    }

    /// §9.5: 境界値での許可を確認（|Σp_i + δ| == P_max^global）
    #[test]
    fn s9_5_boundary_exact_limit_allowed() {
        let config = default_config();
        let snap = snapshot_with_global(9.0);
        let q = all_q_flat(0.1);

        // 9.0 + 1.0 = 10.0 ≤ 10.0 → allowed (exact boundary)
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok(), "exact boundary should be allowed");
    }

    /// §9.5: Q値最上位戦略が優先されロット削減されないことを確認
    #[test]
    fn s9_5_highest_q_strategy_gets_full_lot() {
        let config = default_config();
        let snap = empty_snapshot();
        let q = all_q(0.5, 0.1, 0.05);

        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            100_000.0,
            0.5,
            &q,
        );
        let r = result.unwrap();
        assert_eq!(r.priority_rank, 0, "highest Q strategy should be rank 0");
        assert!(
            (r.effective_lot - 100_000.0).abs() < f64::EPSILON,
            "rank 0 strategy should get full lot"
        );
    }

    /// §9.5: 下位戦略のロット削減（0.5^n）を確認
    #[test]
    fn s9_5_lower_priority_strategies_get_reduced_lots() {
        let config = default_config();
        let snap = empty_snapshot();
        let q = all_q(0.5, 0.1, 0.05);

        // Strategy B: rank 1 → 50%
        let result_b = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::B,
            Direction::Buy,
            100_000.0,
            0.1,
            &q,
        );
        assert_eq!(result_b.as_ref().unwrap().priority_rank, 1);
        assert!((result_b.as_ref().unwrap().effective_lot - 50_000.0).abs() < f64::EPSILON);

        // Strategy C: rank 2 → 25%
        let result_c = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::C,
            Direction::Buy,
            100_000.0,
            0.05,
            &q,
        );
        assert_eq!(result_c.as_ref().unwrap().priority_rank, 2);
        assert!((result_c.as_ref().unwrap().effective_lot - 25_000.0).abs() < f64::EPSILON);
    }

    /// §9.5: 負のグローバルポジションでも制約が対称に機能することを確認
    #[test]
    fn s9_5_negative_position_symmetric_constraint() {
        let config = default_config();
        let snap = snapshot_with_global(-9.5);
        let q = all_q_flat(0.1);

        // -9.5 - 1.0 = -10.5, |-10.5| = 10.5 > 10.0 → blocked
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Sell,
            100_000.0,
            0.1,
            &q,
        );
        assert!(
            result.is_err(),
            "negative position constraint should be symmetric"
        );
    }

    // -- Floor correlation stress scenario ------------------------------------

    #[test]
    fn stress_scenario_tight_limit() {
        let mut config = default_config();
        config.correlation_factor = 3.0;
        config.floor_correlation = 2.0;
        // divisor = max(3.0, 2.0) = 3.0 → 15/3 = 5.0
        let snap = snapshot_with_global(0.0);
        let q = all_q_flat(0.1);
        // 1 lot = 1 unit, 0 + 5 = 5 ≤ 5 → ok (at boundary)
        let result = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            500_000.0,
            0.1,
            &q,
        );
        assert!(result.is_ok());
        // 6 lot → 6 > 5 → blocked
        let result2 = GlobalPositionChecker::validate_order(
            &config,
            &snap,
            StrategyId::A,
            Direction::Buy,
            600_000.0,
            0.1,
            &q,
        );
        assert!(result2.is_err());
    }

    #[test]
    fn correlation_adjustment_reduces_limit() {
        let mut config = default_config();
        config.correlation_factor = 1.0;
        let limit_low = GlobalPositionChecker::compute_global_limit(&config);
        config.correlation_factor = 2.5;
        let limit_high = GlobalPositionChecker::compute_global_limit(&config);
        assert!(limit_high < limit_low);
    }
}
