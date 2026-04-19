use serde::{Deserialize, Serialize};

/// Alert types for the forward test risk monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertType {
    RiskLimit,
    ExecutionDrift,
    KillSwitch,
    SharpeDegradation,
    StrategyCulled,
    FeedAnomaly,
}

/// Alert severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

/// An alert generated during forward testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub alert_type: AlertType,
    pub severity: AlertSeverity,
    pub message: String,
    pub timestamp_ns: u64,
}
