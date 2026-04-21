use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::comparison::ComparisonReport;
use crate::config::ReportFormat;
use crate::runner::ForwardTestResult;
use crate::tracker::PerformanceSnapshot;

/// Session summary report combining all forward test data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    pub test_result: ForwardTestResult,
    pub performance: PerformanceSnapshot,
    pub comparison: Option<ComparisonReport>,
    pub generated_at_ns: u64,
}

/// Trade record for CSV output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub trade_id: u64,
    pub timestamp_ns: u64,
    pub symbol: String,
    pub side: String,
    pub lots: f64,
    pub fill_price: f64,
    pub slippage: f64,
    pub pnl: f64,
    pub strategy: String,
}

/// Report generator for forward test results.
pub struct ReportGenerator {
    output_dir: String,
    format: ReportFormat,
}

impl ReportGenerator {
    pub fn new(output_dir: String, format: ReportFormat) -> Self {
        Self { output_dir, format }
    }

    /// Generate all reports from session data.
    pub fn generate(&self, report: &SessionReport) -> Result<()> {
        fs::create_dir_all(&self.output_dir)
            .with_context(|| format!("Failed to create output dir: {}", self.output_dir))?;

        match self.format {
            ReportFormat::Json => self.write_json(report)?,
            ReportFormat::Csv => self.write_csv(report)?,
            ReportFormat::Both => {
                self.write_json(report)?;
                self.write_csv(report)?;
            }
        }
        Ok(())
    }

    fn write_json(&self, report: &SessionReport) -> Result<()> {
        let path = Path::new(&self.output_dir).join("session_report.json");
        let json =
            serde_json::to_string_pretty(report).context("Failed to serialize session report")?;
        fs::write(&path, json)
            .with_context(|| format!("Failed to write JSON report: {}", path.display()))?;
        Ok(())
    }

    fn write_csv(&self, report: &SessionReport) -> Result<()> {
        let path = Path::new(&self.output_dir).join("performance_summary.csv");
        let mut csv = String::from("metric,value\n");
        csv.push_str(&format!("total_ticks,{}\n", report.test_result.total_ticks));
        csv.push_str(&format!(
            "total_decisions,{}\n",
            report.test_result.total_decisions
        ));
        csv.push_str(&format!(
            "total_trades,{}\n",
            report.test_result.total_trades
        ));
        csv.push_str(&format!(
            "duration_secs,{:.3}\n",
            report.test_result.duration_secs
        ));
        csv.push_str(&format!("final_pnl,{:.6}\n", report.test_result.final_pnl));
        csv.push_str(&format!(
            "realized_pnl,{:.6}\n",
            report.performance.realized_pnl
        ));
        csv.push_str(&format!(
            "unrealized_pnl,{:.6}\n",
            report.performance.unrealized_pnl
        ));
        csv.push_str(&format!(
            "rolling_sharpe,{:.6}\n",
            report.performance.rolling_sharpe
        ));
        csv.push_str(&format!(
            "max_drawdown,{:.6}\n",
            report.performance.max_drawdown
        ));
        csv.push_str(&format!("win_rate,{:.4}\n", report.performance.win_rate));

        fs::write(&path, csv)
            .with_context(|| format!("Failed to write CSV report: {}", path.display()))?;
        Ok(())
    }

    /// Write trade records to CSV.
    pub fn write_trades_csv(&self, trades: &[TradeRecord]) -> Result<()> {
        let path = Path::new(&self.output_dir).join("trades.csv");
        let mut csv = String::from(
            "trade_id,timestamp_ns,symbol,side,lots,fill_price,slippage,pnl,strategy\n",
        );
        for t in trades {
            csv.push_str(&format!(
                "{},{},{},{},{:.4},{:.6},{:.6},{:.6},{}\n",
                t.trade_id,
                t.timestamp_ns,
                t.symbol,
                t.side,
                t.lots,
                t.fill_price,
                t.slippage,
                t.pnl,
                t.strategy
            ));
        }
        fs::write(&path, csv)
            .with_context(|| format!("Failed to write trades CSV: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::PerformanceSnapshot;

    fn sample_report() -> SessionReport {
        SessionReport {
            test_result: ForwardTestResult {
                total_ticks: 1000,
                total_decisions: 500,
                total_trades: 100,
                strategy_events_published: 0,
                state_snapshots_published: 0,
                duration_secs: 60.0,
                final_pnl: 50.0,
                strategies_used: vec!["A".to_string()],
            },
            performance: PerformanceSnapshot::default(),
            comparison: None,
            generated_at_ns: 1_000_000,
        }
    }

    #[test]
    fn test_generate_json() {
        let dir = tempfile::tempdir().unwrap();
        let gen =
            ReportGenerator::new(dir.path().to_str().unwrap().to_string(), ReportFormat::Json);
        let report = sample_report();
        gen.generate(&report).unwrap();

        let json_path = dir.path().join("session_report.json");
        assert!(json_path.exists());
        let content = fs::read_to_string(json_path).unwrap();
        assert!(content.contains("total_ticks"));
    }

    #[test]
    fn test_generate_csv() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ReportGenerator::new(dir.path().to_str().unwrap().to_string(), ReportFormat::Csv);
        let report = sample_report();
        gen.generate(&report).unwrap();

        let csv_path = dir.path().join("performance_summary.csv");
        assert!(csv_path.exists());
        let content = fs::read_to_string(csv_path).unwrap();
        assert!(content.contains("total_ticks,1000"));
        assert!(content.contains("final_pnl,50"));
    }

    #[test]
    fn test_generate_both() {
        let dir = tempfile::tempdir().unwrap();
        let gen =
            ReportGenerator::new(dir.path().to_str().unwrap().to_string(), ReportFormat::Both);
        gen.generate(&sample_report()).unwrap();

        assert!(dir.path().join("session_report.json").exists());
        assert!(dir.path().join("performance_summary.csv").exists());
    }

    #[test]
    fn test_write_trades_csv() {
        let dir = tempfile::tempdir().unwrap();
        let gen =
            ReportGenerator::new(dir.path().to_str().unwrap().to_string(), ReportFormat::Json);
        let trades = vec![TradeRecord {
            trade_id: 1,
            timestamp_ns: 1000,
            symbol: "EUR/USD".to_string(),
            side: "buy".to_string(),
            lots: 1.0,
            fill_price: 1.1000,
            slippage: 0.0001,
            pnl: 5.0,
            strategy: "A".to_string(),
        }];
        gen.write_trades_csv(&trades).unwrap();

        let path = dir.path().join("trades.csv");
        assert!(path.exists());
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("EUR/USD"));
    }
}
