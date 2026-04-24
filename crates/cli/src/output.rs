use anyhow::{Context, Result};
use fx_core::types::StrategyId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use fx_backtest::engine::BacktestResult;
use fx_forward::runner::ForwardTestResult;
use fx_strategy::features::FeatureVector;

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

/// Write a bridge-ready JSON with full trade-level data for Python validation pipeline.
#[allow(dead_code)]
pub fn write_backtest_result_for_bridge(result: &BacktestResult, dir: &Path) -> Result<()> {
    let json_path = dir.join("backtest_result.json");
    let output = BacktestBridgeJson::from_result(result);
    let json = serde_json::to_string_pretty(&output)
        .context("Failed to serialize bridge backtest result to JSON")?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("Failed to write {}", json_path.display()))?;

    let trades_path = dir.join("trades.csv");
    write_trades_csv(&result.trades, &trades_path)?;

    Ok(())
}

pub fn write_forward_result(result: &ForwardTestResult, dir: &Path) -> Result<()> {
    let json_path = dir.join("forward_result.json");
    let output = ForwardResultJson::from_result(result);
    let json = serde_json::to_string_pretty(&output)
        .context("Failed to serialize forward test result to JSON")?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("Failed to write {}", json_path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Bridge-ready JSON (full trade-level data for Python validation pipeline)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct BacktestBridgeJson {
    summary: BridgeSummary,
    trades: Vec<BridgeTrade>,
    returns: Vec<f64>,
    risk_metric_returns: Vec<f64>,
    risk_metric_return_basis: String,
    strategy_breakdown: Vec<BridgeStrategyBreakdown>,
    strategy_funnel: Vec<StrategyFunnelRow>,
    decision_diagnostics: DecisionDiagnostics,
    num_features: usize,
    execution_stats: BridgeExecutionStats,
}

#[derive(Serialize)]
struct BridgeSummary {
    total_ticks: u64,
    total_decision_ticks: u64,
    total_trades: usize,
    close_trades: usize,
    wall_time_ms: u64,
    total_pnl: f64,
    realized_pnl: f64,
    win_rate: f64,
    max_drawdown: f64,
    sharpe_ratio: f64,
    sortino_ratio: f64,
    profit_factor: f64,
    avg_trade_duration_ns: u64,
}

#[derive(Serialize)]
struct BridgeTrade {
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

#[derive(Serialize)]
struct BridgeStrategyBreakdown {
    strategy: String,
    total_trades: u64,
    total_pnl: f64,
    win_rate: f64,
    avg_pnl: f64,
}

#[derive(Serialize)]
struct BridgeExecutionStats {
    overall_fill_rate: f64,
    avg_slippage: f64,
    total_fills: u64,
    total_rejections: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionDiagnostics {
    pub total_recorded_decisions: u64,
    pub triggered_decisions: u64,
    pub entry_attempts: u64,
    pub filled_entries: u64,
    pub hold_events: u64,
    pub close_trades: u64,
    pub skip_reasons: Vec<ReasonCount>,
    pub skip_reasons_by_strategy: Vec<StrategySkipReasons>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategySkipReasons {
    pub strategy: String,
    pub reasons: Vec<ReasonCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasonCount {
    pub reason: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategyFunnelRow {
    pub strategy: String,
    pub evaluated: u64,
    pub triggered: u64,
    pub idle_triggered: u64,
    pub decide_called: u64,
    pub order_attempted: u64,
    pub risk_passed: u64,
    pub filled: u64,
    pub closed: u64,
}

#[derive(Debug, Clone)]
pub struct RiskMetricSummary {
    pub returns: Vec<f64>,
    pub basis: &'static str,
}

pub fn backtest_decision_diagnostics(result: &BacktestResult) -> DecisionDiagnostics {
    let total_recorded_decisions = result.decisions.len() as u64;
    let triggered_decisions = result.decisions.iter().filter(|d| d.triggered).count() as u64;
    let entry_attempts = result
        .decisions
        .iter()
        .filter(|d| d.direction.is_some())
        .count() as u64;
    let filled_entries = result
        .decisions
        .iter()
        .filter(|d| d.direction.is_some() && d.skip_reason.is_none())
        .count() as u64;
    let hold_events = result
        .decisions
        .iter()
        .filter(|d| d.direction.is_none())
        .count() as u64;
    let close_trades = result
        .trades
        .iter()
        .filter(|t| t.close_reason.is_some())
        .count() as u64;

    let mut skip_reason_counts: BTreeMap<String, u64> = BTreeMap::new();
    for reason in result
        .decisions
        .iter()
        .filter_map(|d| d.skip_reason.as_ref())
        .cloned()
    {
        *skip_reason_counts.entry(reason).or_insert(0) += 1;
    }
    let mut skip_reasons: Vec<ReasonCount> = skip_reason_counts
        .into_iter()
        .map(|(reason, count)| ReasonCount { reason, count })
        .collect();
    skip_reasons.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));

    let mut skip_reasons_by_strategy: Vec<StrategySkipReasons> = StrategyId::all()
        .iter()
        .copied()
        .map(|sid| {
            let mut counts: BTreeMap<String, u64> = BTreeMap::new();
            for reason in result
                .decisions
                .iter()
                .filter(|d| d.strategy_id == sid)
                .filter_map(|d| d.skip_reason.as_ref())
                .cloned()
            {
                *counts.entry(reason).or_insert(0) += 1;
            }
            let mut reasons: Vec<ReasonCount> = counts
                .into_iter()
                .map(|(reason, count)| ReasonCount { reason, count })
                .collect();
            reasons.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
            StrategySkipReasons {
                strategy: format!("{:?}", sid),
                reasons,
            }
        })
        .collect();

    skip_reasons_by_strategy
        .retain(|s| !s.reasons.is_empty());

    DecisionDiagnostics {
        total_recorded_decisions,
        triggered_decisions,
        entry_attempts,
        filled_entries,
        hold_events,
        close_trades,
        skip_reasons,
        skip_reasons_by_strategy,
    }
}

pub fn backtest_risk_metric_summary(result: &BacktestResult) -> RiskMetricSummary {
    let close_returns: Vec<f64> = result
        .trades
        .iter()
        .filter(|t| t.close_reason.is_some())
        .map(|t| t.pnl)
        .collect();
    if close_returns.is_empty() {
        RiskMetricSummary {
            returns: result.trades.iter().map(|t| t.pnl).collect(),
            basis: "fill_pnl",
        }
    } else {
        RiskMetricSummary {
            returns: close_returns,
            basis: "close_trade_pnl",
        }
    }
}

pub fn backtest_strategy_funnel(result: &BacktestResult) -> Vec<StrategyFunnelRow> {
    StrategyId::all()
        .iter()
        .copied()
        .map(|sid| StrategyFunnelRow {
            strategy: format!("{:?}", sid),
            evaluated: *result.trigger_diagnostics.evaluated.get(&sid).unwrap_or(&0),
            triggered: *result.trigger_diagnostics.triggered.get(&sid).unwrap_or(&0),
            idle_triggered: *result
                .trigger_diagnostics
                .idle_triggered
                .get(&sid)
                .unwrap_or(&0),
            decide_called: *result
                .trigger_diagnostics
                .decide_called
                .get(&sid)
                .unwrap_or(&0),
            order_attempted: *result
                .trigger_diagnostics
                .order_attempted
                .get(&sid)
                .unwrap_or(&0),
            risk_passed: *result
                .trigger_diagnostics
                .risk_passed
                .get(&sid)
                .unwrap_or(&0),
            filled: *result.trigger_diagnostics.filled.get(&sid).unwrap_or(&0),
            closed: *result.trigger_diagnostics.closed.get(&sid).unwrap_or(&0),
        })
        .collect()
}

impl BacktestBridgeJson {
    fn from_result(r: &BacktestResult) -> Self {
        let trades: Vec<BridgeTrade> = r
            .trades
            .iter()
            .map(|t| BridgeTrade {
                timestamp_ns: t.timestamp_ns,
                strategy: format!("{:?}", t.strategy_id),
                direction: format!("{:?}", t.direction),
                lots: t.lots,
                fill_price: t.fill_price,
                slippage: t.slippage,
                pnl: t.pnl,
                fill_probability: t.fill_probability,
                latency_ms: t.latency_ms,
                close_reason: t.close_reason.clone(),
            })
            .collect();

        let returns: Vec<f64> = r.trades.iter().map(|t| t.pnl).collect();
        let risk_metric = backtest_risk_metric_summary(r);
        let decision_diagnostics = backtest_decision_diagnostics(r);
        let strategy_funnel = backtest_strategy_funnel(r);

        let mut breakdown = fx_backtest::stats::compute_strategy_breakdown(&r.trades);
        breakdown.sort_by_key(|entry| entry.strategy_id.stable_index());
        let strategy_breakdown: Vec<BridgeStrategyBreakdown> = breakdown
            .iter()
            .map(|b| BridgeStrategyBreakdown {
                strategy: format!("{:?}", b.strategy_id),
                total_trades: b.total_trades,
                total_pnl: b.total_pnl,
                win_rate: b.win_rate,
                avg_pnl: b.avg_pnl,
            })
            .collect();

        Self {
            summary: BridgeSummary {
                total_ticks: r.total_ticks,
                total_decision_ticks: r.total_decision_ticks,
                total_trades: r.trades.len(),
                close_trades: decision_diagnostics.close_trades as usize,
                wall_time_ms: r.wall_time_ms,
                total_pnl: r.summary.total_pnl,
                realized_pnl: r.summary.realized_pnl,
                win_rate: r.summary.win_rate,
                max_drawdown: r.summary.max_drawdown,
                sharpe_ratio: r.summary.sharpe_ratio,
                sortino_ratio: r.summary.sortino_ratio,
                profit_factor: r.summary.profit_factor,
                avg_trade_duration_ns: r.summary.avg_trade_duration_ns,
            },
            trades,
            returns,
            risk_metric_returns: risk_metric.returns,
            risk_metric_return_basis: risk_metric.basis.to_string(),
            strategy_breakdown,
            strategy_funnel,
            decision_diagnostics,
            num_features: FeatureVector::DIM,
            execution_stats: BridgeExecutionStats {
                overall_fill_rate: r.execution_stats.overall_fill_rate,
                avg_slippage: r.execution_stats.avg_slippage,
                total_fills: r.execution_stats.total_fills,
                total_rejections: r.execution_stats.total_rejections,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Python validation result reader
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidationCheckResult {
    pub name: String,
    pub passed: bool,
    pub details: String,
    pub value: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidationResult {
    pub all_passed: bool,
    pub n_passed: usize,
    pub n_failed: usize,
    pub checks: Vec<ValidationCheckResult>,
}

impl ValidationResult {
    pub fn from_json_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content).context("Failed to parse validation result JSON")
    }

    pub fn print_summary(&self) {
        let status = if self.all_passed { "PASSED" } else { "FAILED" };
        println!("\nValidation Result: {status}");
        println!(
            "  Checks: {}/{} passed",
            self.n_passed,
            self.n_passed + self.n_failed
        );
        for check in &self.checks {
            let mark = if check.passed { "PASS" } else { "FAIL" };
            println!("  [{mark}] {}: {}", check.name, check.details);
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct BridgeBacktestData {
    pub summary: BridgeSummaryRead,
    pub returns: Vec<f64>,
    pub num_features: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct BridgeSummaryRead {
    pub total_pnl: f64,
    pub sharpe_ratio: f64,
    pub total_trades: usize,
}

#[allow(dead_code)]
impl BridgeBacktestData {
    pub fn from_json_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content).context("Failed to parse backtest result JSON")
    }
}

// ---------------------------------------------------------------------------
// Simple summary JSON (original format)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct BacktestResultJson {
    total_ticks: u64,
    total_decision_ticks: u64,
    total_trades: usize,
    close_trades: usize,
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
    summary: BridgeSummary,
    trades: Vec<BridgeTrade>,
    returns: Vec<f64>,
    risk_metric_returns: Vec<f64>,
    risk_metric_return_basis: String,
    strategy_breakdown: Vec<BridgeStrategyBreakdown>,
    strategy_funnel: Vec<StrategyFunnelRow>,
    decision_diagnostics: DecisionDiagnostics,
    num_features: usize,
    execution_stats: BridgeExecutionStats,
}

impl BacktestResultJson {
    fn from_result(r: &BacktestResult) -> Self {
        let bridge = BacktestBridgeJson::from_result(r);
        Self {
            total_ticks: r.total_ticks,
            total_decision_ticks: r.total_decision_ticks,
            total_trades: r.trades.len(),
            close_trades: bridge.summary.close_trades,
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
            strategies: StrategyId::all()
                .iter()
                .copied()
                .filter(|sid| r.config.enabled_strategies.contains(sid))
                .map(|sid| format!("{:?}", sid))
                .collect(),
            summary: bridge.summary,
            trades: bridge.trades,
            returns: bridge.returns,
            risk_metric_returns: bridge.risk_metric_returns,
            risk_metric_return_basis: bridge.risk_metric_return_basis,
            strategy_breakdown: bridge.strategy_breakdown,
            strategy_funnel: bridge.strategy_funnel,
            decision_diagnostics: bridge.decision_diagnostics,
            num_features: bridge.num_features,
            execution_stats: bridge.execution_stats,
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
            strategy_events_published: 0,
            state_snapshots_published: 0,
            observability_ticks: 0,
            trigger_diagnostics: fx_backtest::engine::TriggerDiagnostics::default(),
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
            strategy_events_published: 0,
            state_snapshots_published: 0,
            duration_secs: 60.0,
            final_pnl: 250.0,
            strategies_used: vec!["A".to_string(), "C".to_string()],
            strategy_funnels: std::collections::HashMap::new(),
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
            strategy_events_published: 0,
            state_snapshots_published: 0,
            duration_secs: 60.0,
            final_pnl: 250.0,
            strategies_used: vec!["A".to_string()],
            strategy_funnels: std::collections::HashMap::new(),
        };
        write_forward_result(&result, dir.path()).unwrap();

        assert!(dir.path().join("forward_result.json").exists());
        let content = std::fs::read_to_string(dir.path().join("forward_result.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["total_ticks"], 200);
        assert_eq!(parsed["final_pnl"], 250.0);
    }

    fn make_backtest_result_with_trades() -> BacktestResult {
        use fx_backtest::stats::TradeRecord;
        use fx_core::types::Direction;

        BacktestResult {
            config: BacktestConfig::default(),
            trades: vec![
                TradeRecord {
                    timestamp_ns: 1000,
                    strategy_id: StrategyId::A,
                    direction: Direction::Buy,
                    lots: 1000.0,
                    fill_price: 110.05,
                    slippage: 0.01,
                    pnl: 5.0,
                    fill_probability: 0.95,
                    latency_ms: 0.5,
                    close_reason: None,
                },
                TradeRecord {
                    timestamp_ns: 2000,
                    strategy_id: StrategyId::A,
                    direction: Direction::Sell,
                    lots: 1000.0,
                    fill_price: 110.10,
                    slippage: 0.02,
                    pnl: -3.0,
                    fill_probability: 0.90,
                    latency_ms: 0.8,
                    close_reason: Some("max_hold_time".to_string()),
                },
                TradeRecord {
                    timestamp_ns: 3000,
                    strategy_id: StrategyId::B,
                    direction: Direction::Buy,
                    lots: 2000.0,
                    fill_price: 110.15,
                    slippage: 0.015,
                    pnl: 8.0,
                    fill_probability: 0.88,
                    latency_ms: 0.3,
                    close_reason: None,
                },
            ],
            decisions: vec![],
            total_ticks: 100,
            total_decision_ticks: 50,
            wall_time_ms: 42,
            summary: make_trade_summary(),
            execution_stats: ExecutionStats::empty(),
            execution_events: vec![],
            strategy_events_published: 0,
            state_snapshots_published: 0,
            observability_ticks: 0,
            trigger_diagnostics: fx_backtest::engine::TriggerDiagnostics::default(),
        }
    }

    #[test]
    fn test_bridge_json_includes_trades_and_returns() {
        let result = make_backtest_result_with_trades();
        let dir = tempfile::tempdir().unwrap();
        write_backtest_result_for_bridge(&result, dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join("backtest_result.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Verify structure
        assert!(parsed["summary"].is_object());
        assert!(parsed["trades"].is_array());
        assert!(parsed["returns"].is_array());
        assert!(parsed["strategy_breakdown"].is_array());
        assert!(parsed["strategy_funnel"].is_array());
        assert!(parsed["execution_stats"].is_object());

        // Verify trades
        let trades = parsed["trades"].as_array().unwrap();
        assert_eq!(trades.len(), 3);
        assert_eq!(trades[0]["pnl"], 5.0);
        assert_eq!(trades[1]["pnl"], -3.0);
        assert_eq!(trades[2]["strategy"], "B");

        // Verify returns
        let returns = parsed["returns"].as_array().unwrap();
        assert_eq!(returns.len(), 3);
        assert_eq!(returns[0], 5.0);
        assert_eq!(returns[1], -3.0);
        assert_eq!(returns[2], 8.0);

        // Verify strategy breakdown
        let breakdown = parsed["strategy_breakdown"].as_array().unwrap();
        assert!(!breakdown.is_empty());

        // Verify num_features
        assert_eq!(parsed["num_features"], FeatureVector::DIM as u64);

        // Verify trades.csv also written
        assert!(dir.path().join("trades.csv").exists());
    }

    #[test]
    fn test_bridge_json_strategy_breakdown_aggregation() {
        let result = make_backtest_result_with_trades();
        let bridge = BacktestBridgeJson::from_result(&result);
        assert_eq!(bridge.strategy_breakdown.len(), 2); // A and B
        assert_eq!(bridge.returns, vec![5.0, -3.0, 8.0]);
    }

    #[test]
    fn test_validation_result_reads_json() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("validation_result.json");
        let json_content = r#"{
            "all_passed": true,
            "n_passed": 3,
            "n_failed": 0,
            "checks": [
                {"name": "Sharpe Ceiling", "passed": true, "details": "ok", "value": 1.2},
                {"name": "DSR", "passed": true, "details": "ok", "value": 0.98},
                {"name": "CPCV", "passed": true, "details": "ok", "value": 0.5}
            ]
        }"#;
        std::fs::write(&json_path, json_content).unwrap();

        let result = ValidationResult::from_json_file(&json_path).unwrap();
        assert!(result.all_passed);
        assert_eq!(result.n_passed, 3);
        assert_eq!(result.n_failed, 0);
        assert_eq!(result.checks.len(), 3);
        assert_eq!(result.checks[0].name, "Sharpe Ceiling");
        assert_eq!(result.checks[2].value, 0.5);
    }

    #[test]
    fn test_validation_result_reads_failed() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("validation_result.json");
        let json_content = r#"{
            "all_passed": false,
            "n_passed": 1,
            "n_failed": 2,
            "checks": [
                {"name": "Sharpe Ceiling", "passed": true, "details": "ok", "value": 1.0},
                {"name": "DSR", "passed": false, "details": "too low", "value": 0.3},
                {"name": "PBO", "passed": false, "details": "overfit", "value": 0.5}
            ]
        }"#;
        std::fs::write(&json_path, json_content).unwrap();

        let result = ValidationResult::from_json_file(&json_path).unwrap();
        assert!(!result.all_passed);
        assert_eq!(result.n_passed, 1);
        assert_eq!(result.n_failed, 2);
    }

    #[test]
    fn test_validation_result_missing_file_errors() {
        let result = ValidationResult::from_json_file(Path::new("/nonexistent/file.json"));
        assert!(result.is_err());
    }

    #[test]
    fn test_validation_result_invalid_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("bad.json");
        std::fs::write(&json_path, "not json").unwrap();
        let result = ValidationResult::from_json_file(&json_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_bridge_backtest_data_reads_json() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("backtest.json");
        let json_content = r#"{
            "summary": {"total_pnl": 100.0, "sharpe_ratio": 1.2, "total_trades": 10},
            "returns": [1.0, -0.5, 2.0],
            "num_features": 30
        }"#;
        std::fs::write(&json_path, json_content).unwrap();

        let data = BridgeBacktestData::from_json_file(&json_path).unwrap();
        assert_eq!(data.summary.total_pnl, 100.0);
        assert_eq!(data.returns.len(), 3);
        assert_eq!(data.num_features, 30);
    }
}
