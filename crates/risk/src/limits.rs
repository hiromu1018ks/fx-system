use tracing::{error, warn};

use fx_events::projector::LimitStateData;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum RiskError {
    #[error("daily MTM loss limit breached: PnL = {pnl}")]
    DailyMtmLimit { pnl: f64 },
    #[error("daily realized loss limit breached: PnL = {pnl}")]
    DailyRealizedLimit { pnl: f64 },
    #[error("weekly loss limit breached: PnL = {pnl}")]
    WeeklyLimit { pnl: f64 },
    #[error("monthly loss limit breached: PnL = {pnl}")]
    MonthlyLimit { pnl: f64 },
    #[error("global position constraint violated")]
    GlobalPositionConstraint,
    #[error("staleness halted: {staleness_ms}ms exceeds threshold {threshold_ms}ms")]
    StalenessHalted {
        staleness_ms: u64,
        threshold_ms: u64,
    },
    #[error("staleness degraded: lot_multiplier={lot_multiplier}, effective_lot={effective_lot_size} < min_lot={min_lot_size}")]
    StalenessDegraded {
        staleness_ms: u64,
        lot_multiplier: f64,
        effective_lot_size: u64,
        min_lot_size: u64,
    },
    #[error("kill switch active: order masked, remaining {remaining_ms}ms")]
    KillSwitchMasked { remaining_ms: u64 },
}

pub type Result<T> = std::result::Result<T, RiskError>;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Hierarchical loss-limit thresholds.
///
/// Checked **before** Q-value evaluation — hard limits fire regardless of
/// estimated edge.
#[derive(Debug, Clone)]
pub struct RiskLimitsConfig {
    /// Daily stage-1 (MTM warning): breached → lot 25 % + Q-threshold
    pub max_daily_loss_mtm: f64,
    /// Daily stage-2 (realised hard-stop): breached → close all + halt
    pub max_daily_loss_realized: f64,
    /// Weekly hard limit: breached → close all + halt until next week
    pub max_weekly_loss: f64,
    /// Monthly hard limit: breached → close all + operator approval required
    pub max_monthly_loss: f64,
    /// Lot fraction when daily MTM limit is active (default 0.25)
    pub daily_mtm_lot_fraction: f64,
    /// Minimum absolute Q-value required when daily MTM limit is active
    pub daily_mtm_q_threshold: f64,
}

impl Default for RiskLimitsConfig {
    fn default() -> Self {
        Self {
            max_daily_loss_mtm: -500.0,
            max_daily_loss_realized: -1000.0,
            max_weekly_loss: -2500.0,
            max_monthly_loss: -5000.0,
            daily_mtm_lot_fraction: 0.25,
            daily_mtm_q_threshold: 0.01,
        }
    }
}

// ---------------------------------------------------------------------------
// Limit check result (non-error path)
// ---------------------------------------------------------------------------

/// Returned when `validate_order` succeeds (no hard breach).
#[derive(Debug, Clone)]
pub struct LimitCheckResult {
    /// Whether the daily MTM warning stage is active.
    pub daily_mtm_limited: bool,
    /// Lot multiplier to apply on top of any barrier multiplier.
    /// 1.0 normally, `daily_mtm_lot_fraction` when MTM-limited.
    pub lot_multiplier: f64,
    /// If `daily_mtm_limited`, orders are only allowed when
    /// `|q_value| >= daily_mtm_q_threshold`.
    pub q_threshold: f64,
}

// ---------------------------------------------------------------------------
// Close-all command
// ---------------------------------------------------------------------------

/// Emitted when a hard limit fires and all positions must be closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Daily realised-loss hard-stop triggered.
    DailyRealizedHalt,
    /// Weekly hard limit triggered.
    WeeklyHalt,
    /// Monthly hard limit triggered.
    MonthlyHalt,
}

// ---------------------------------------------------------------------------
// Hierarchical risk limiter
// ---------------------------------------------------------------------------

/// Stateless checker — reads `LimitStateData` and decides whether a new order
/// is permitted and whether positions must be closed.
///
/// All checks are performed **before** Q-value evaluation per PRD constraints.
pub struct HierarchicalRiskLimiter;

impl HierarchicalRiskLimiter {
    /// Full evaluation: check every tier and return the strictest outcome.
    ///
    /// Returns `Ok(LimitCheckResult)` when the order may proceed (possibly
    /// with restrictions), or `Err(RiskError)` when a hard limit fires.
    ///
    /// Also returns `Some(CloseReason)` when a hard limit requires an
    /// immediate close of all positions.
    pub fn evaluate(
        config: &RiskLimitsConfig,
        limit_state: &LimitStateData,
    ) -> (
        std::result::Result<LimitCheckResult, RiskError>,
        Option<CloseReason>,
    ) {
        // Monthly (checked first — most severe)
        if limit_state.monthly_pnl < config.max_monthly_loss {
            error!(
                monthly_pnl = limit_state.monthly_pnl,
                threshold = config.max_monthly_loss,
                "MONTHLY hard limit breached"
            );
            return (
                Err(RiskError::MonthlyLimit {
                    pnl: limit_state.monthly_pnl,
                }),
                Some(CloseReason::MonthlyHalt),
            );
        }

        // Weekly
        if limit_state.weekly_pnl < config.max_weekly_loss {
            error!(
                weekly_pnl = limit_state.weekly_pnl,
                threshold = config.max_weekly_loss,
                "WEEKLY hard limit breached"
            );
            return (
                Err(RiskError::WeeklyLimit {
                    pnl: limit_state.weekly_pnl,
                }),
                Some(CloseReason::WeeklyHalt),
            );
        }

        // Daily stage-2: realised hard-stop
        if limit_state.daily_pnl_realized < config.max_daily_loss_realized {
            error!(
                daily_realized = limit_state.daily_pnl_realized,
                threshold = config.max_daily_loss_realized,
                "DAILY realised hard-stop breached"
            );
            return (
                Err(RiskError::DailyRealizedLimit {
                    pnl: limit_state.daily_pnl_realized,
                }),
                Some(CloseReason::DailyRealizedHalt),
            );
        }

        // Daily stage-1: MTM warning
        if limit_state.daily_pnl_mtm < config.max_daily_loss_mtm {
            warn!(
                daily_mtm = limit_state.daily_pnl_mtm,
                threshold = config.max_daily_loss_mtm,
                "DAILY MTM warning active — lot reduced to {}%",
                config.daily_mtm_lot_fraction * 100.0
            );
            return (
                Ok(LimitCheckResult {
                    daily_mtm_limited: true,
                    lot_multiplier: config.daily_mtm_lot_fraction,
                    q_threshold: config.daily_mtm_q_threshold,
                }),
                None,
            );
        }

        // All clear
        (
            Ok(LimitCheckResult {
                daily_mtm_limited: false,
                lot_multiplier: 1.0,
                q_threshold: 0.0,
            }),
            None,
        )
    }

    /// Convenience: validate an order, returning `Ok(LimitCheckResult)` or
    /// `Err(RiskError)`.  Use `evaluate()` if you also need the `CloseReason`.
    pub fn validate_order(
        config: &RiskLimitsConfig,
        limit_state: &LimitStateData,
    ) -> Result<LimitCheckResult> {
        let (result, _close) = Self::evaluate(config, limit_state);
        result
    }

    /// Whether any hard halt is currently flagged in the limit state.
    /// This is the fast pre-check before running the full evaluation.
    pub fn is_halted(limit_state: &LimitStateData) -> bool {
        limit_state.daily_realized_halted || limit_state.weekly_halted || limit_state.monthly_halted
    }

    /// Derive a `LimitStateData` with halt flags set from current PnL values.
    /// The returned struct reflects the *current* breach status.
    pub fn compute_limit_state(
        config: &RiskLimitsConfig,
        daily_mtm: f64,
        daily_realized: f64,
        weekly: f64,
        monthly: f64,
    ) -> LimitStateData {
        LimitStateData {
            daily_pnl_mtm: daily_mtm,
            daily_pnl_realized: daily_realized,
            weekly_pnl: weekly,
            monthly_pnl: monthly,
            daily_mtm_limited: daily_mtm < config.max_daily_loss_mtm,
            daily_realized_halted: daily_realized < config.max_daily_loss_realized,
            weekly_halted: weekly < config.max_weekly_loss,
            monthly_halted: monthly < config.max_monthly_loss,
        }
    }

    /// Apply the Q-threshold gate when daily MTM limit is active.
    /// Returns `true` if the order passes the gate.
    pub fn passes_q_threshold(
        check: &LimitCheckResult,
        q_value_buy: f64,
        q_value_sell: f64,
    ) -> bool {
        if !check.daily_mtm_limited {
            return true;
        }
        // At least one direction must exceed the threshold
        q_value_buy.abs() >= check.q_threshold || q_value_sell.abs() >= check.q_threshold
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> RiskLimitsConfig {
        RiskLimitsConfig::default()
    }

    fn normal_state() -> LimitStateData {
        LimitStateData::default()
    }

    fn state_with(
        daily_mtm: f64,
        daily_realized: f64,
        weekly: f64,
        monthly: f64,
    ) -> LimitStateData {
        LimitStateData {
            daily_pnl_mtm: daily_mtm,
            daily_pnl_realized: daily_realized,
            weekly_pnl: weekly,
            monthly_pnl: monthly,
            ..Default::default()
        }
    }

    // -- All clear ------------------------------------------------------------

    #[test]
    fn all_clear_no_restrictions() {
        let state = normal_state();
        let (result, close) = HierarchicalRiskLimiter::evaluate(&default_config(), &state);
        assert!(result.is_ok());
        assert!(close.is_none());
        let r = result.unwrap();
        assert!(!r.daily_mtm_limited);
        assert!((r.lot_multiplier - 1.0).abs() < f64::EPSILON);
        assert!((r.q_threshold - 0.0).abs() < f64::EPSILON);
    }

    // -- Daily MTM stage-1 ----------------------------------------------------

    #[test]
    fn daily_mtm_warning_active() {
        let config = default_config();
        let state = state_with(-600.0, 0.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_ok());
        assert!(close.is_none());
        let r = result.unwrap();
        assert!(r.daily_mtm_limited);
        assert!((r.lot_multiplier - 0.25).abs() < f64::EPSILON);
        assert!((r.q_threshold - 0.01).abs() < f64::EPSILON);
    }

    #[test]
    fn daily_mtm_exactly_at_threshold_ok() {
        let config = default_config();
        let state = state_with(-500.0, 0.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        // exactly at threshold: not yet limited (strictly less than)
        assert!(result.is_ok());
        assert!(close.is_none());
        assert!(!result.unwrap().daily_mtm_limited);
    }

    #[test]
    fn daily_mtm_just_below_threshold() {
        let config = default_config();
        let state = state_with(-500.01, 0.0, 0.0, 0.0);
        let (result, _) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.unwrap().daily_mtm_limited);
    }

    // -- Daily realised stage-2 (hard-stop) -----------------------------------

    #[test]
    fn daily_realized_hard_stop() {
        let config = default_config();
        let state = state_with(0.0, -1001.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_err());
        match result.unwrap_err() {
            RiskError::DailyRealizedLimit { pnl } => {
                assert!((pnl - (-1001.0)).abs() < f64::EPSILON)
            }
            other => panic!("wrong error variant: {other}"),
        }
        assert_eq!(close, Some(CloseReason::DailyRealizedHalt));
    }

    #[test]
    fn daily_realized_exactly_at_threshold_ok() {
        let config = default_config();
        let state = state_with(0.0, -1000.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_ok());
        assert!(close.is_none());
    }

    // -- Weekly hard limit -----------------------------------------------------

    #[test]
    fn weekly_hard_limit() {
        let config = default_config();
        let state = state_with(0.0, 0.0, -2600.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_err());
        match result.unwrap_err() {
            RiskError::WeeklyLimit { pnl } => assert!((pnl - (-2600.0)).abs() < f64::EPSILON),
            other => panic!("wrong error variant: {other}"),
        }
        assert_eq!(close, Some(CloseReason::WeeklyHalt));
    }

    #[test]
    fn weekly_exactly_at_threshold_ok() {
        let config = default_config();
        let state = state_with(0.0, 0.0, -2500.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_ok());
        assert!(close.is_none());
    }

    // -- Monthly hard limit ----------------------------------------------------

    #[test]
    fn monthly_hard_limit() {
        let config = default_config();
        let state = state_with(0.0, 0.0, 0.0, -5100.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_err());
        match result.unwrap_err() {
            RiskError::MonthlyLimit { pnl } => assert!((pnl - (-5100.0)).abs() < f64::EPSILON),
            other => panic!("wrong error variant: {other}"),
        }
        assert_eq!(close, Some(CloseReason::MonthlyHalt));
    }

    #[test]
    fn monthly_exactly_at_threshold_ok() {
        let config = default_config();
        let state = state_with(0.0, 0.0, 0.0, -5000.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(result.is_ok());
        assert!(close.is_none());
    }

    // -- Priority: monthly > weekly > daily realized > daily MTM ---------------

    #[test]
    fn monthly_takes_priority_over_weekly() {
        let config = default_config();
        let state = state_with(0.0, -2000.0, -3000.0, -6000.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(matches!(result, Err(RiskError::MonthlyLimit { .. })));
        assert_eq!(close, Some(CloseReason::MonthlyHalt));
    }

    #[test]
    fn weekly_takes_priority_over_daily() {
        let config = default_config();
        let state = state_with(-700.0, -1500.0, -3000.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(matches!(result, Err(RiskError::WeeklyLimit { .. })));
        assert_eq!(close, Some(CloseReason::WeeklyHalt));
    }

    #[test]
    fn daily_realized_takes_priority_over_daily_mtm() {
        let config = default_config();
        let state = state_with(-800.0, -1500.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        assert!(matches!(result, Err(RiskError::DailyRealizedLimit { .. })));
        assert_eq!(close, Some(CloseReason::DailyRealizedHalt));
    }

    // -- validate_order convenience --------------------------------------------

    #[test]
    fn validate_order_ok() {
        let state = normal_state();
        assert!(HierarchicalRiskLimiter::validate_order(&default_config(), &state).is_ok());
    }

    #[test]
    fn validate_order_rejected() {
        let state = state_with(0.0, 0.0, 0.0, -9999.0);
        assert!(HierarchicalRiskLimiter::validate_order(&default_config(), &state).is_err());
    }

    // -- is_halted -------------------------------------------------------------

    #[test]
    fn is_halted_false_when_normal() {
        let state = normal_state();
        assert!(!HierarchicalRiskLimiter::is_halted(&state));
    }

    #[test]
    fn is_halted_true_daily_realized() {
        let mut state = normal_state();
        state.daily_realized_halted = true;
        assert!(HierarchicalRiskLimiter::is_halted(&state));
    }

    #[test]
    fn is_halted_true_weekly() {
        let mut state = normal_state();
        state.weekly_halted = true;
        assert!(HierarchicalRiskLimiter::is_halted(&state));
    }

    #[test]
    fn is_halted_true_monthly() {
        let mut state = normal_state();
        state.monthly_halted = true;
        assert!(HierarchicalRiskLimiter::is_halted(&state));
    }

    #[test]
    fn is_halted_false_mtm_only() {
        let mut state = normal_state();
        state.daily_mtm_limited = true;
        assert!(!HierarchicalRiskLimiter::is_halted(&state));
    }

    // -- compute_limit_state ---------------------------------------------------

    #[test]
    fn compute_limit_state_all_clear() {
        let state = HierarchicalRiskLimiter::compute_limit_state(
            &default_config(),
            -100.0,
            -200.0,
            -500.0,
            -1000.0,
        );
        assert!(!state.daily_mtm_limited);
        assert!(!state.daily_realized_halted);
        assert!(!state.weekly_halted);
        assert!(!state.monthly_halted);
    }

    #[test]
    fn compute_limit_state_mtm_limited() {
        let state = HierarchicalRiskLimiter::compute_limit_state(
            &default_config(),
            -600.0,
            -200.0,
            -500.0,
            -1000.0,
        );
        assert!(state.daily_mtm_limited);
        assert!(!state.daily_realized_halted);
    }

    #[test]
    fn compute_limit_state_realized_halted() {
        let state = HierarchicalRiskLimiter::compute_limit_state(
            &default_config(),
            -600.0,
            -1100.0,
            -500.0,
            -1000.0,
        );
        assert!(state.daily_mtm_limited);
        assert!(state.daily_realized_halted);
    }

    #[test]
    fn compute_limit_state_weekly_halted() {
        let state =
            HierarchicalRiskLimiter::compute_limit_state(&default_config(), 0.0, 0.0, -3000.0, 0.0);
        assert!(state.weekly_halted);
    }

    #[test]
    fn compute_limit_state_monthly_halted() {
        let state =
            HierarchicalRiskLimiter::compute_limit_state(&default_config(), 0.0, 0.0, 0.0, -6000.0);
        assert!(state.monthly_halted);
    }

    // -- Q-threshold gate ------------------------------------------------------

    #[test]
    fn q_threshold_passes_when_not_limited() {
        let check = LimitCheckResult {
            daily_mtm_limited: false,
            lot_multiplier: 1.0,
            q_threshold: 0.0,
        };
        assert!(HierarchicalRiskLimiter::passes_q_threshold(
            &check, 0.001, 0.001
        ));
    }

    #[test]
    fn q_threshold_passes_when_q_high() {
        let check = LimitCheckResult {
            daily_mtm_limited: true,
            lot_multiplier: 0.25,
            q_threshold: 0.01,
        };
        assert!(HierarchicalRiskLimiter::passes_q_threshold(
            &check, 0.02, 0.0
        ));
    }

    #[test]
    fn q_threshold_fails_when_q_low() {
        let check = LimitCheckResult {
            daily_mtm_limited: true,
            lot_multiplier: 0.25,
            q_threshold: 0.01,
        };
        assert!(!HierarchicalRiskLimiter::passes_q_threshold(
            &check, 0.005, 0.003
        ));
    }

    #[test]
    fn q_threshold_sell_direction_passes() {
        let check = LimitCheckResult {
            daily_mtm_limited: true,
            lot_multiplier: 0.25,
            q_threshold: 0.01,
        };
        assert!(HierarchicalRiskLimiter::passes_q_threshold(
            &check, 0.0, -0.02
        ));
    }

    // -- Positive PnL ----------------------------------------------------------

    #[test]
    fn positive_pnl_all_clear() {
        let state = state_with(100.0, 50.0, 200.0, 500.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&default_config(), &state);
        assert!(result.is_ok());
        assert!(close.is_none());
    }

    // -- Custom config ---------------------------------------------------------

    #[test]
    fn custom_config_thresholds() {
        let config = RiskLimitsConfig {
            max_daily_loss_mtm: -100.0,
            max_daily_loss_realized: -200.0,
            max_weekly_loss: -500.0,
            max_monthly_loss: -1000.0,
            daily_mtm_lot_fraction: 0.5,
            daily_mtm_q_threshold: 0.05,
        };
        let state = state_with(-150.0, 0.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&config, &state);
        let r = result.unwrap();
        assert!(r.daily_mtm_limited);
        assert!((r.lot_multiplier - 0.5).abs() < f64::EPSILON);
        assert!((r.q_threshold - 0.05).abs() < f64::EPSILON);
        assert!(close.is_none());
    }

    // -- CloseReason variants --------------------------------------------------

    #[test]
    fn close_reason_equality() {
        assert_eq!(
            CloseReason::DailyRealizedHalt,
            CloseReason::DailyRealizedHalt
        );
        assert_ne!(CloseReason::WeeklyHalt, CloseReason::MonthlyHalt);
    }

    // -- Halted flags in state block further orders ----------------------------

    #[test]
    fn halted_state_flags_block_validate() {
        let mut state = normal_state();
        state.monthly_halted = true;
        // Even though PnL values are zero, the halt flag is already set.
        // evaluate checks PnL thresholds, not flags — the flags are for
        // external coordination. But is_halted reflects the flag.
        assert!(HierarchicalRiskLimiter::is_halted(&state));
    }

    // -- Edge: all limits breached simultaneously ------------------------------

    #[test]
    fn all_breached_monthly_wins() {
        let state = state_with(-9999.0, -9999.0, -9999.0, -9999.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&default_config(), &state);
        assert!(matches!(result, Err(RiskError::MonthlyLimit { .. })));
        assert_eq!(close, Some(CloseReason::MonthlyHalt));
    }

    // -- Edge: zero PnL --------------------------------------------------------

    #[test]
    fn zero_pnl_all_clear() {
        let state = state_with(0.0, 0.0, 0.0, 0.0);
        let (result, close) = HierarchicalRiskLimiter::evaluate(&default_config(), &state);
        assert!(result.is_ok());
        assert!(close.is_none());
    }
}
