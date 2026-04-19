use serde::{Deserialize, Serialize};

/// Result of comparing forward test against backtest results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub pnl_diff: f64,
    pub win_rate_diff: f64,
    pub sharpe_diff: f64,
    pub drawdown_diff: f64,
    pub fill_rate_diff: f64,
    pub avg_slippage_diff: f64,
    pub overall_pass: bool,
}
