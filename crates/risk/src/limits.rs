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
}

pub type Result<T> = std::result::Result<T, RiskError>;
