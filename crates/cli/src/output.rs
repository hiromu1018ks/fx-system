use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use fx_backtest::engine::BacktestResult;
use fx_forward::runner::ForwardTestResult;

pub fn write_backtest_result(result: &BacktestResult, dir: &Path) -> Result<()> {
    let json_path = dir.join("backtest_result.json");
    let output = BacktestResultJson::from_result(result);
    let json = serde_json::to_string_pretty(&output)
        .context("Failed to serialize backtest result to JSON")?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("Failed to write {}", json_path.display()))?;

    let trades_path = dir.join("trades.csv");
    write_trades_csv(&result.trades, &trades_path)?;

    Ok(())
}

#[allow(dead_code)]
pub fn write_forward_result(result: &ForwardTestResult, dir: &Path) -> Result<()> {
    let json_path = dir.join("forward_result.json");
    let output = ForwardResultJson::from_result(result);
    let json = serde_json::to_string_pretty(&output)
        .context("Failed to serialize forward test result to JSON")?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("Failed to write {}", json_path.display()))?;
    Ok(())
}

#[derive(Serialize)]
struct BacktestResultJson {
    total_ticks: u64,
    total_decision_ticks: u64,
    total_trades: usize,
    wall_time_ms: u64,
    total_pnl: f64,
    realized_pnl: f64,
    win_rate: f64,
    max_drawdown: f64,
    sharpe_ratio: f64,
    sortino_ratio: f64,
    profit_factor: f64,
    avg_trade_duration_ns: u64,
    execution_fill_rate: f64,
    execution_avg_slippage: f64,
    strategies: Vec<String>,
}

impl BacktestResultJson {
    fn from_result(r: &BacktestResult) -> Self {
        Self {
            total_ticks: r.total_ticks,
            total_decision_ticks: r.total_decision_ticks,
            total_trades: r.trades.len(),
            wall_time_ms: r.wall_time_ms,
            total_pnl: r.summary.total_pnl,
            realized_pnl: r.summary.realized_pnl,
            win_rate: r.summary.win_rate,
            max_drawdown: r.summary.max_drawdown,
            sharpe_ratio: r.summary.sharpe_ratio,
            sortino_ratio: r.summary.sortino_ratio,
            profit_factor: r.summary.profit_factor,
            avg_trade_duration_ns: r.summary.avg_trade_duration_ns,
            execution_fill_rate: r.execution_stats.overall_fill_rate,
            execution_avg_slippage: r.execution_stats.avg_slippage,
            strategies: r
                .config
                .enabled_strategies
                .iter()
                .map(|s| format!("{:?}", s))
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[allow(dead_code)]
struct ForwardResultJson {
    total_ticks: u64,
    total_decisions: u64,
    total_trades: u64,
    duration_secs: f64,
    final_pnl: f64,
    strategies_used: Vec<String>,
}

#[allow(dead_code)]
impl ForwardResultJson {
    fn from_result(r: &ForwardTestResult) -> Self {
        Self {
            total_ticks: r.total_ticks,
            total_decisions: r.total_decisions,
            total_trades: r.total_trades,
            duration_secs: r.duration_secs,
            final_pnl: r.final_pnl,
            strategies_used: r.strategies_used.clone(),
        }
    }
}

#[derive(Serialize)]
struct TradeCsvRow {
    timestamp_ns: u64,
    strategy: String,
    direction: String,
    lots: f64,
    fill_price: f64,
    slippage: f64,
    pnl: f64,
    fill_probability: f64,
    latency_ms: f64,
    close_reason: Option<String>,
}

impl TradeCsvRow {
    fn from_record(r: &fx_backtest::stats::TradeRecord) -> Self {
        Self {
            timestamp_ns: r.timestamp_ns,
            strategy: format!("{:?}", r.strategy_id),
            direction: format!("{:?}", r.direction),
            lots: r.lots,
            fill_price: r.fill_price,
            slippage: r.slippage,
            pnl: r.pnl,
            fill_probability: r.fill_probability,
            latency_ms: r.latency_ms,
            close_reason: r.close_reason.clone(),
        }
    }
}

fn write_trades_csv(trades: &[fx_backtest::stats::TradeRecord], path: &Path) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path)
        .with_context(|| format!("Failed to create trades CSV at {}", path.display()))?;
    for trade in trades {
        let row = TradeCsvRow::from_record(trade);
        wtr.serialize(&row)
            .with_context(|| format!("Failed to write trade record to {}", path.display()))?;
    }
    wtr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_backtest::engine::{BacktestConfig, BacktestDecision};
    use fx_backtest::stats::{ExecutionStats, TradeSummary};
    use fx_core::types::StrategyId;

    fn make_trade_summary() -> TradeSummary {
        TradeSummary {
            total_pnl: 100.0,
            realized_pnl: 80.0,
            total_trades: 10,
            winning_trades: 6,
            losing_trades: 4,
            win_rate: 0.6,
            avg_win: 25.0,
            avg_loss: -12.5,
            profit_factor: 3.0,
            max_drawdown: -50.0,
            max_drawdown_duration_ns: 5_000_000_000,
            sharpe_ratio: 1.2,
            sortino_ratio: 1.5,
            avg_slippage: 0.001,
            avg_fill_probability: 0.95,
            avg_latency_ms: 0.5,
            max_consecutive_wins: 3,
            max_consecutive_losses: 2,
            avg_trade_duration_ns: 30_000_000_000,
        }
    }

    fn make_backtest_result() -> BacktestResult {
        BacktestResult {
            config: BacktestConfig::default(),
            trades: vec![],
            decisions: vec![BacktestDecision {
                timestamp_ns: 1000,
                strategy_id: StrategyId::A,
                direction: Some(fx_core::types::Direction::Buy),
                lots: 100_000,
                triggered: true,
                skip_reason: None,
            }],
            total_ticks: 100,
            total_decision_ticks: 50,
            wall_time_ms: 42,
            summary: make_trade_summary(),
            execution_stats: ExecutionStats::empty(),
            execution_events: vec![],
        }
    }

    #[test]
    fn test_backtest_result_json_serializes() {
        let result = make_backtest_result();
        let output = BacktestResultJson::from_result(&result);
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("total_ticks"));
        assert!(json.contains("total_pnl"));
    }

    #[test]
    fn test_write_backtest_result_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = make_backtest_result();
        write_backtest_result(&result, dir.path()).unwrap();

        assert!(dir.path().join("backtest_result.json").exists());
        assert!(dir.path().join("trades.csv").exists());

        let content = std::fs::read_to_string(dir.path().join("backtest_result.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["total_ticks"], 100);
        assert_eq!(parsed["total_trades"], 0);
    }

    #[test]
    fn test_forward_result_json_serializes() {
        let result = ForwardTestResult {
            total_ticks: 200,
            total_decisions: 80,
            total_trades: 15,
            duration_secs: 60.0,
            final_pnl: 250.0,
            strategies_used: vec!["A".to_string(), "C".to_string()],
        };
        let output = ForwardResultJson::from_result(&result);
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("total_ticks"));
        assert!(json.contains("final_pnl"));
    }

    #[test]
    fn test_write_forward_result_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = ForwardTestResult {
            total_ticks: 200,
            total_decisions: 80,
            total_trades: 15,
            duration_secs: 60.0,
            final_pnl: 250.0,
            strategies_used: vec!["A".to_string()],
        };
        write_forward_result(&result, dir.path()).unwrap();

        assert!(dir.path().join("forward_result.json").exists());
        let content = std::fs::read_to_string(dir.path().join("forward_result.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["total_ticks"], 200);
        assert_eq!(parsed["final_pnl"], 250.0);
    }
}
