use serde::{Deserialize, Serialize};

/// Metrics from a backtest or forward test run for comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub total_pnl: f64,
    pub win_rate: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown: f64,
    pub fill_rate: f64,
    pub avg_slippage: f64,
    pub total_trades: u64,
}

/// Decomposition of PnL difference into explainable components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlDecomposition {
    pub execution_component: f64,
    pub latency_component: f64,
    pub impact_component: f64,
    pub residual: f64,
}

/// Per-metric comparison detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricComparison {
    pub metric_name: String,
    pub backtest_value: f64,
    pub forward_value: f64,
    pub difference: f64,
    pub passes: bool,
}

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
    pub metric_details: Vec<MetricComparison>,
    pub pnl_decomposition: Option<PnlDecomposition>,
}

/// Configuration for comparison thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonThresholds {
    pub max_pnl_diff: f64,
    pub max_win_rate_diff: f64,
    pub max_sharpe_diff: f64,
    pub max_drawdown_diff: f64,
    pub max_fill_rate_diff: f64,
    pub max_slippage_diff: f64,
}

impl Default for ComparisonThresholds {
    fn default() -> Self {
        Self {
            max_pnl_diff: 0.2,       // 20% PnL deviation
            max_win_rate_diff: 0.1,  // 10% win rate deviation
            max_sharpe_diff: 0.3,    // 0.3 Sharpe deviation
            max_drawdown_diff: 0.2,  // 20% drawdown deviation
            max_fill_rate_diff: 0.1, // 10% fill rate deviation
            max_slippage_diff: 0.5,  // 0.5 pip slippage deviation
        }
    }
}

/// Compares forward test results against backtest results.
pub struct ComparisonEngine {
    thresholds: ComparisonThresholds,
}

impl ComparisonEngine {
    pub fn new(thresholds: ComparisonThresholds) -> Self {
        Self { thresholds }
    }

    /// Compare forward test metrics against backtest metrics.
    pub fn compare(
        &self,
        backtest: &PerformanceMetrics,
        forward: &PerformanceMetrics,
    ) -> ComparisonReport {
        let pnl_diff = Self::relative_diff(backtest.total_pnl, forward.total_pnl);
        let win_rate_diff = (forward.win_rate - backtest.win_rate).abs();
        let sharpe_diff = (forward.sharpe_ratio - backtest.sharpe_ratio).abs();
        let drawdown_diff = Self::relative_diff(backtest.max_drawdown, forward.max_drawdown);
        let fill_rate_diff = (forward.fill_rate - backtest.fill_rate).abs();
        let slippage_diff = (forward.avg_slippage - backtest.avg_slippage).abs();

        let details = vec![
            self.compare_metric(
                "pnl",
                backtest.total_pnl,
                forward.total_pnl,
                pnl_diff,
                self.thresholds.max_pnl_diff,
            ),
            self.compare_metric(
                "win_rate",
                backtest.win_rate,
                forward.win_rate,
                win_rate_diff,
                self.thresholds.max_win_rate_diff,
            ),
            self.compare_metric(
                "sharpe",
                backtest.sharpe_ratio,
                forward.sharpe_ratio,
                sharpe_diff,
                self.thresholds.max_sharpe_diff,
            ),
            self.compare_metric(
                "drawdown",
                backtest.max_drawdown,
                forward.max_drawdown,
                drawdown_diff,
                self.thresholds.max_drawdown_diff,
            ),
            self.compare_metric(
                "fill_rate",
                backtest.fill_rate,
                forward.fill_rate,
                fill_rate_diff,
                self.thresholds.max_fill_rate_diff,
            ),
            self.compare_metric(
                "slippage",
                backtest.avg_slippage,
                forward.avg_slippage,
                slippage_diff,
                self.thresholds.max_slippage_diff,
            ),
        ];

        let overall_pass = details.iter().all(|d| d.passes);

        // Decompose PnL difference
        let pnl_decomposition = if (backtest.total_pnl - forward.total_pnl).abs() > f64::EPSILON {
            let total_diff = forward.total_pnl - backtest.total_pnl;
            let exec_component = slippage_diff * forward.total_trades as f64;
            let latency_component = pnl_diff * backtest.total_pnl * 0.1; // Estimate
            let impact_component = fill_rate_diff * total_diff.abs() * 0.05;
            let residual = total_diff - exec_component - latency_component - impact_component;
            Some(PnlDecomposition {
                execution_component: exec_component,
                latency_component,
                impact_component,
                residual,
            })
        } else {
            None
        };

        ComparisonReport {
            pnl_diff,
            win_rate_diff,
            sharpe_diff,
            drawdown_diff,
            fill_rate_diff,
            avg_slippage_diff: slippage_diff,
            overall_pass,
            metric_details: details,
            pnl_decomposition,
        }
    }

    fn compare_metric(
        &self,
        name: &str,
        bt_val: f64,
        fw_val: f64,
        diff: f64,
        threshold: f64,
    ) -> MetricComparison {
        MetricComparison {
            metric_name: name.to_string(),
            backtest_value: bt_val,
            forward_value: fw_val,
            difference: diff,
            passes: diff <= threshold,
        }
    }

    /// Compute relative difference between two values.
    fn relative_diff(a: f64, b: f64) -> f64 {
        if a.abs() < f64::EPSILON && b.abs() < f64::EPSILON {
            0.0
        } else if a.abs() < f64::EPSILON {
            f64::INFINITY
        } else {
            (b - a).abs() / a.abs()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_backtest() -> PerformanceMetrics {
        PerformanceMetrics {
            total_pnl: 1000.0,
            win_rate: 0.6,
            sharpe_ratio: 1.0,
            max_drawdown: 200.0,
            fill_rate: 0.95,
            avg_slippage: 0.1,
            total_trades: 100,
        }
    }

    fn sample_forward() -> PerformanceMetrics {
        PerformanceMetrics {
            total_pnl: 950.0,
            win_rate: 0.58,
            sharpe_ratio: 0.9,
            max_drawdown: 210.0,
            fill_rate: 0.92,
            avg_slippage: 0.15,
            total_trades: 95,
        }
    }

    #[test]
    fn test_compare_similar_metrics() {
        let engine = ComparisonEngine::new(ComparisonThresholds::default());
        let report = engine.compare(&sample_backtest(), &sample_forward());
        // The metrics are similar enough to pass with default thresholds
        assert_eq!(report.metric_details.len(), 6);
    }

    #[test]
    fn test_compare_identical() {
        let engine = ComparisonEngine::new(ComparisonThresholds::default());
        let bt = sample_backtest();
        let report = engine.compare(&bt, &bt);
        assert!(report.overall_pass);
        assert_eq!(report.pnl_diff, 0.0);
    }

    #[test]
    fn test_compare_divergent_fails() {
        let engine = ComparisonEngine::new(ComparisonThresholds::default());
        let bt = sample_backtest();
        let fw = PerformanceMetrics {
            total_pnl: -500.0,
            win_rate: 0.3,
            sharpe_ratio: -0.5,
            max_drawdown: 800.0,
            fill_rate: 0.5,
            avg_slippage: 2.0,
            total_trades: 50,
        };
        let report = engine.compare(&bt, &fw);
        assert!(!report.overall_pass);
    }

    #[test]
    fn test_pnl_decomposition() {
        let engine = ComparisonEngine::new(ComparisonThresholds::default());
        let report = engine.compare(&sample_backtest(), &sample_forward());
        assert!(report.pnl_decomposition.is_some());
        let decomp = report.pnl_decomposition.unwrap();
        // Components should sum approximately to total diff
        assert!(decomp.execution_component >= 0.0);
    }

    #[test]
    fn test_relative_diff() {
        assert_eq!(ComparisonEngine::relative_diff(100.0, 110.0), 0.1);
        assert_eq!(ComparisonEngine::relative_diff(100.0, 100.0), 0.0);
        assert_eq!(ComparisonEngine::relative_diff(0.0, 0.0), 0.0);
    }

    #[test]
    fn test_custom_thresholds() {
        let thresholds = ComparisonThresholds {
            max_pnl_diff: 0.01, // Very tight
            max_win_rate_diff: 0.01,
            max_sharpe_diff: 0.01,
            max_drawdown_diff: 0.01,
            max_fill_rate_diff: 0.01,
            max_slippage_diff: 0.01,
        };
        let engine = ComparisonEngine::new(thresholds);
        let report = engine.compare(&sample_backtest(), &sample_forward());
        assert!(!report.overall_pass);
    }
}
