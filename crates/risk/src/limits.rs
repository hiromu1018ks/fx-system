use thiserror::Error;

#[derive(Error, Debug)]
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
}

pub type Result<T> = std::result::Result<T, RiskError>;
