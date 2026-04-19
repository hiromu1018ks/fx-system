use serde::{Deserialize, Serialize};

/// Snapshot of performance metrics at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceSnapshot {
    pub timestamp_ns: u64,
    pub cumulative_pnl: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub rolling_sharpe: f64,
    pub max_drawdown: f64,
    pub win_rate: f64,
    pub total_trades: u64,
    pub winning_trades: u64,
}

impl Default for PerformanceSnapshot {
    fn default() -> Self {
        Self {
            timestamp_ns: 0,
            cumulative_pnl: 0.0,
            realized_pnl: 0.0,
            unrealized_pnl: 0.0,
            rolling_sharpe: 0.0,
            max_drawdown: 0.0,
            win_rate: 0.0,
            total_trades: 0,
            winning_trades: 0,
        }
    }
}

/// Real-time performance tracker.
pub struct PerformanceTracker {
    snapshot: PerformanceSnapshot,
}

impl PerformanceTracker {
    pub fn new() -> Self {
        Self {
            snapshot: PerformanceSnapshot::default(),
        }
    }

    pub fn snapshot(&self) -> &PerformanceSnapshot {
        &self.snapshot
    }
}

impl Default for PerformanceTracker {
    fn default() -> Self {
        Self::new()
    }
}
