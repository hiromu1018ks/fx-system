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

    pub fn update(&mut self, timestamp_ns: u64, realized_pnl: f64, unrealized_pnl: f64) {
        self.snapshot.timestamp_ns = timestamp_ns;
        self.snapshot.realized_pnl = realized_pnl;
        self.snapshot.unrealized_pnl = unrealized_pnl;
        self.snapshot.cumulative_pnl = realized_pnl + unrealized_pnl;

        let peak = self.snapshot.cumulative_pnl.max(0.0);
        let dd = peak - self.snapshot.cumulative_pnl;
        if dd > self.snapshot.max_drawdown {
            self.snapshot.max_drawdown = dd;
        }
    }
}

impl Default for PerformanceTracker {
    fn default() -> Self {
        Self::new()
    }
}
