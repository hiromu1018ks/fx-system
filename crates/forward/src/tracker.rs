use std::collections::VecDeque;

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
    pub execution_drift_mean: f64,
    pub execution_drift_std: f64,
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
            execution_drift_mean: 0.0,
            execution_drift_std: 0.0,
        }
    }
}

/// Real-time performance tracker with rolling calculations.
pub struct PerformanceTracker {
    snapshot: PerformanceSnapshot,
    pnl_history: VecDeque<f64>,
    equity_curve: VecDeque<f64>,
    rolling_window: usize,
    peak_equity: f64,
    execution_drifts: VecDeque<f64>,
}

impl PerformanceTracker {
    pub fn new() -> Self {
        Self {
            snapshot: PerformanceSnapshot::default(),
            pnl_history: VecDeque::new(),
            equity_curve: VecDeque::new(),
            rolling_window: 100,
            peak_equity: 0.0,
            execution_drifts: VecDeque::new(),
        }
    }

    /// Create tracker with a custom rolling window size.
    pub fn with_window(rolling_window: usize) -> Self {
        Self {
            rolling_window,
            ..Self::new()
        }
    }

    /// Update tracker with latest PnL values.
    pub fn update(&mut self, timestamp_ns: u64, realized_pnl: f64, unrealized_pnl: f64) {
        self.snapshot.timestamp_ns = timestamp_ns;
        self.snapshot.realized_pnl = realized_pnl;
        self.snapshot.unrealized_pnl = unrealized_pnl;
        self.snapshot.cumulative_pnl = realized_pnl + unrealized_pnl;

        // Track equity curve for drawdown
        self.equity_curve.push_back(self.snapshot.cumulative_pnl);
        if self.equity_curve.len() > self.rolling_window {
            self.equity_curve.pop_front();
        }

        // Track peak and drawdown
        if self.snapshot.cumulative_pnl > self.peak_equity {
            self.peak_equity = self.snapshot.cumulative_pnl;
        }
        let dd = self.peak_equity - self.snapshot.cumulative_pnl;
        if dd > self.snapshot.max_drawdown {
            self.snapshot.max_drawdown = dd;
        }

        // Compute rolling Sharpe from equity changes
        self.compute_rolling_sharpe();
    }

    /// Record a completed trade.
    pub fn record_trade(&mut self, pnl: f64) {
        self.snapshot.total_trades += 1;
        if pnl > 0.0 {
            self.snapshot.winning_trades += 1;
        }

        self.pnl_history.push_back(pnl);
        if self.pnl_history.len() > self.rolling_window {
            self.pnl_history.pop_front();
        }

        if self.snapshot.total_trades > 0 {
            self.snapshot.win_rate =
                self.snapshot.winning_trades as f64 / self.snapshot.total_trades as f64;
        }

        self.compute_rolling_sharpe();
    }

    /// Record execution drift (expected vs actual fill difference).
    pub fn record_execution_drift(&mut self, drift: f64) {
        self.execution_drifts.push_back(drift);
        if self.execution_drifts.len() > self.rolling_window {
            self.execution_drifts.pop_front();
        }

        if self.execution_drifts.is_empty() {
            return;
        }

        let n = self.execution_drifts.len() as f64;
        let mean = self.execution_drifts.iter().sum::<f64>() / n;
        let variance = if n > 1.0 {
            self.execution_drifts
                .iter()
                .map(|d| (d - mean).powi(2))
                .sum::<f64>()
                / (n - 1.0)
        } else {
            0.0
        };

        self.snapshot.execution_drift_mean = mean;
        self.snapshot.execution_drift_std = variance.sqrt();
    }

    fn compute_rolling_sharpe(&mut self) {
        if self.pnl_history.len() < 2 {
            return;
        }

        let returns: Vec<f64> = self.pnl_history.iter().copied().collect();
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;

        let variance = if n > 1.0 {
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };

        let std = variance.sqrt();
        if std > f64::EPSILON {
            // Annualize assuming ~252 trading days * ~6.5 hours * ~3600 ticks = ~5.9M ticks/year
            // For simplicity, use sqrt(252) annualization (daily-level Sharpe)
            self.snapshot.rolling_sharpe = (mean / std) * 252.0_f64.sqrt();
        } else {
            self.snapshot.rolling_sharpe = 0.0;
        }
    }

    pub fn snapshot(&self) -> &PerformanceSnapshot {
        &self.snapshot
    }

    pub fn total_trades(&self) -> u64 {
        self.snapshot.total_trades
    }

    pub fn cumulative_pnl(&self) -> f64 {
        self.snapshot.cumulative_pnl
    }

    pub fn max_drawdown(&self) -> f64 {
        self.snapshot.max_drawdown
    }

    pub fn rolling_sharpe(&self) -> f64 {
        self.snapshot.rolling_sharpe
    }
}

impl Default for PerformanceTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_state() {
        let tracker = PerformanceTracker::new();
        let snap = tracker.snapshot();
        assert_eq!(snap.cumulative_pnl, 0.0);
        assert_eq!(snap.total_trades, 0);
        assert_eq!(snap.max_drawdown, 0.0);
        assert_eq!(snap.rolling_sharpe, 0.0);
    }

    #[test]
    fn test_update_pnl() {
        let mut tracker = PerformanceTracker::new();
        tracker.update(1000, 10.0, 5.0);
        assert_eq!(tracker.snapshot().cumulative_pnl, 15.0);
        assert_eq!(tracker.snapshot().realized_pnl, 10.0);
        assert_eq!(tracker.snapshot().unrealized_pnl, 5.0);
    }

    #[test]
    fn test_drawdown_tracking() {
        let mut tracker = PerformanceTracker::new();
        tracker.update(1000, 100.0, 0.0); // Peak
        tracker.update(2000, 80.0, 0.0); // Drawdown 20
        assert_eq!(tracker.max_drawdown(), 20.0);
        tracker.update(3000, 120.0, 0.0); // New peak
        assert_eq!(tracker.max_drawdown(), 20.0); // Max DD unchanged
    }

    #[test]
    fn test_record_trade() {
        let mut tracker = PerformanceTracker::new();
        tracker.record_trade(10.0);
        tracker.record_trade(-5.0);
        tracker.record_trade(8.0);

        assert_eq!(tracker.total_trades(), 3);
        assert!((tracker.snapshot().win_rate - 0.6667).abs() < 0.01);
    }

    #[test]
    fn test_rolling_sharpe() {
        let mut tracker = PerformanceTracker::new();
        // Feed a series of trades with some variance
        for i in 0..50 {
            let pnl = 1.0 + (i as f64 * 0.1).sin(); // Variable positive returns
            tracker.record_trade(pnl);
            tracker.update(1000 + i as u64 * 100, i as f64, 0.0);
        }
        // Should have a positive Sharpe with mostly positive returns
        assert!(tracker.rolling_sharpe() > 0.0);
    }

    #[test]
    fn test_execution_drift() {
        let mut tracker = PerformanceTracker::new();
        tracker.record_execution_drift(0.001);
        tracker.record_execution_drift(0.002);
        tracker.record_execution_drift(-0.001);

        let mean = tracker.snapshot().execution_drift_mean;
        let std = tracker.snapshot().execution_drift_std;
        assert!(mean > 0.0);
        assert!(std > 0.0);
    }

    #[test]
    fn test_window_eviction() {
        let mut tracker = PerformanceTracker::with_window(5);
        for i in 0..10 {
            tracker.record_trade(i as f64);
        }
        assert_eq!(tracker.total_trades(), 10); // Total count unaffected
        assert_eq!(tracker.pnl_history.len(), 5); // Window is capped
    }
}
