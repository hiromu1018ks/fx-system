//! Integration tests for the forward test full pipeline.
//!
//! Tests the complete flow:
//!   RecordedDataFeed → ForwardTestRunner → PaperExecution → PerformanceTracker
//!
//! plus alert system, comparison engine, reproducibility, and report generation.

use std::collections::HashMap;
use fx_events::event::GenericEvent;
use fx_forward::alert::{
    Alert, AlertEvaluator, AlertSeverity, AlertSystem, AlertType, LogAlertChannel,
};
use fx_forward::comparison::{ComparisonEngine, ComparisonThresholds, PerformanceMetrics};
use fx_forward::config::{
    AlertChannelConfig, AlertConfig, ForwardRiskConfig, ForwardTestConfig, ReportConfig,
    ReportFormat,
};
use fx_forward::feed::{DataSourceConfig, RecordedDataFeed, VecEventStore};
use fx_forward::report::{ReportGenerator, SessionReport, TradeRecord};
use fx_forward::runner::ForwardTestRunner;
use fx_forward::tracker::{PerformanceSnapshot, PerformanceTracker};
use prost::Message as _;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const NS_BASE: u64 = 1_000_000_000_000_000;

fn make_market_event(
    timestamp_ns: u64,
    bid: f64,
    ask: f64,
    bid_size: f64,
    ask_size: f64,
) -> GenericEvent {
    use fx_core::types::{EventTier, StreamId};
    use fx_events::header::EventHeader;
    use fx_events::proto;

    let payload = proto::MarketEventPayload {
        header: None,
        symbol: "USD/JPY".to_string(),
        bid,
        ask,
        bid_size,
        ask_size,
        timestamp_ns,
        bid_levels: vec![],
        ask_levels: vec![],
        latency_ms: 0.0,
    }
    .encode_to_vec();

    let header = EventHeader {
        timestamp_ns,
        stream_id: StreamId::Market,
        sequence_id: 0,
        tier: EventTier::Tier3Raw,
        ..EventHeader::new(StreamId::Market, 0, EventTier::Tier3Raw)
    };

    GenericEvent::new(header, payload)
}

fn generate_market_events(count: usize, interval_ms: u64, base_price: f64) -> Vec<GenericEvent> {
    let mut events = Vec::with_capacity(count);
    for i in 0..count {
        let ts = NS_BASE + (i as u64) * interval_ms * 1_000_000;
        let noise = ((i % 7) as f64 - 3.0) * 0.001;
        let mid = base_price + noise;
        let half_spread = 0.0005;
        events.push(make_market_event(
            ts,
            mid - half_spread,
            mid + half_spread,
            1e6,
            1e6,
        ));
    }
    events
}

fn make_forward_config(strategies: &[&str]) -> ForwardTestConfig {
    ForwardTestConfig {
        enabled_strategies: strategies.iter().map(|s| s.to_string()).collect(),
        data_source: DataSourceConfig::Recorded {
            event_store_path: String::new(),
            speed: 0.0,
            start_time: None,
            end_time: None,
        },
        duration: None,
        alert_config: AlertConfig {
            channels: vec![AlertChannelConfig::Log],
            risk_limit_threshold: 0.8,
            execution_drift_threshold: 2.0,
            sharpe_degradation_threshold: 0.3,
        },
        report_config: ReportConfig {
            output_dir: "./reports".to_string(),
            format: ReportFormat::Both,
            interval: None,
        },
        risk_config: ForwardRiskConfig {
            max_position_lots: 10.0,
            max_daily_loss_mtm: 500.0,
            max_daily_loss_realized: 1_000.0,
            max_weekly_loss: 2_500.0,
            max_monthly_loss: 5_000.0,
            daily_mtm_lot_fraction: 0.25,
            daily_mtm_q_threshold: 0.01,
            max_drawdown: 1000.0,
        },
        comparison_config: None,
        regime_config: fx_strategy::regime::RegimeConfig::default(),
    }
}

// ---------------------------------------------------------------------------
// 1. Full Pipeline: RecordedDataFeed → Runner → PaperExecution → Tracker
// ---------------------------------------------------------------------------

#[test]
fn test_full_pipeline_recorded_feed_to_runner() {
    let events = generate_market_events(200, 100, 110.0);
    let store = VecEventStore::new(events);
    let feed = RecordedDataFeed::new(store, 0.0, None, None);
    let config = make_forward_config(&["A", "B", "C"]);
    let mut runner = ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 200);
    assert!(result.duration_secs >= 0.0);
    assert!(result.final_pnl.is_finite());
    // All three strategies should be listed
    assert!(result.strategies_used.contains(&"A".to_string()));
    assert!(result.strategies_used.contains(&"B".to_string()));
    assert!(result.strategies_used.contains(&"C".to_string()));
}

// ---------------------------------------------------------------------------
// 2. Strategy Individual and Combination Tests
// ---------------------------------------------------------------------------

#[test]
fn test_strategy_a_only() {
    let events = generate_market_events(100, 100, 110.0);
    let store = VecEventStore::new(events);
    let feed = RecordedDataFeed::new(store, 0.0, None, None);
    let config = make_forward_config(&["A"]);
    let mut runner = ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 100);
    assert_eq!(result.strategies_used.len(), 1);
    assert_eq!(result.strategies_used[0], "A");
}

#[test]
fn test_strategy_subset_bc() {
    let events = generate_market_events(100, 100, 110.0);
    let store = VecEventStore::new(events);
    let feed = RecordedDataFeed::new(store, 0.0, None, None);
    let config = make_forward_config(&["B", "C"]);
    let mut runner = ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.strategies_used.len(), 2);
    assert!(result.strategies_used.contains(&"B".to_string()));
    assert!(result.strategies_used.contains(&"C".to_string()));
    assert!(!result.strategies_used.contains(&"A".to_string()));
}

// ---------------------------------------------------------------------------
// 3. Alert System: Log Channel and Webhook Mock
// ---------------------------------------------------------------------------

#[test]
fn test_alert_evaluator_risk_limit() {
    let mut evaluator = AlertEvaluator::new(Duration::from_millis(100), 0.8, 2.0, 0.3);

    // Below threshold — no alert
    let alert = evaluator.evaluate_risk_limit(50.0, 500.0, NS_BASE);
    assert!(alert.is_none());

    // Above threshold — alert fires
    let alert = evaluator.evaluate_risk_limit(400.0, 500.0, NS_BASE);
    assert!(alert.is_some());
    let a = alert.unwrap();
    assert!(matches!(a.alert_type, AlertType::RiskLimit));
    assert!(matches!(
        a.severity,
        AlertSeverity::Warning | AlertSeverity::Critical
    ));
}

#[test]
fn test_alert_evaluator_execution_drift() {
    let mut evaluator = AlertEvaluator::new(Duration::from_millis(100), 0.8, 2.0, 0.3);

    // Below threshold — no alert
    let alert = evaluator.evaluate_execution_drift(0.001, 0.01, NS_BASE);
    assert!(alert.is_none());

    // Above threshold — alert fires
    let alert = evaluator.evaluate_execution_drift(3.0, 1.0, NS_BASE);
    assert!(alert.is_some());
    let a = alert.unwrap();
    assert!(matches!(a.alert_type, AlertType::ExecutionDrift));
}

#[test]
fn test_alert_evaluator_sharpe_degradation() {
    let mut evaluator = AlertEvaluator::new(Duration::from_millis(100), 0.8, 2.0, 0.3);

    // Small degradation — no alert
    let alert = evaluator.evaluate_sharpe_degradation(0.9, 1.0, NS_BASE);
    assert!(alert.is_none());

    // Large degradation — alert fires
    let alert = evaluator.evaluate_sharpe_degradation(0.5, 1.0, NS_BASE);
    assert!(alert.is_some());
    let a = alert.unwrap();
    assert!(matches!(a.alert_type, AlertType::SharpeDegradation));
}

#[test]
fn test_alert_system_log_channel() {
    let log_channel = LogAlertChannel;
    let evaluator = AlertEvaluator::new(Duration::from_millis(100), 0.8, 2.0, 0.3);
    let system = AlertSystem::new(vec![Box::new(log_channel)], evaluator);

    let alert = Alert {
        alert_type: AlertType::RiskLimit,
        severity: AlertSeverity::Warning,
        message: "Test alert".to_string(),
        timestamp_ns: NS_BASE,
    };

    // Should not panic
    system.send(&alert);
}

#[test]
fn test_alert_system_webhook_channel_creation() {
    // Verify WebhookAlertChannel can be created (actual HTTP call not tested)
    let channel =
        fx_forward::alert::WebhookAlertChannel::new("http://localhost:9999/alert".to_string());
    // Channel is created — the actual send would fail without a server
    // but construction should succeed
    let _ = channel;
}

// ---------------------------------------------------------------------------
// 4. ComparisonEngine: Forward vs Backtest Results
// ---------------------------------------------------------------------------

#[test]
fn test_comparison_engine_identical_results() {
    let thresholds = ComparisonThresholds::default();
    let engine = ComparisonEngine::new(thresholds);

    let metrics = PerformanceMetrics {
        total_pnl: 100.0,
        win_rate: 0.55,
        sharpe_ratio: 1.0,
        max_drawdown: -50.0,
        fill_rate: 0.95,
        avg_slippage: 0.0001,
        total_trades: 100,
    };

    let report = engine.compare(&metrics, &metrics);

    assert!(report.overall_pass);
    assert!((report.pnl_diff).abs() < 1e-10);
    assert!((report.win_rate_diff).abs() < 1e-10);
}

#[test]
fn test_comparison_engine_detects_divergence() {
    let thresholds = ComparisonThresholds {
        max_pnl_diff: 0.2,
        max_win_rate_diff: 0.1,
        max_sharpe_diff: 0.3,
        max_drawdown_diff: 0.2,
        max_fill_rate_diff: 0.1,
        max_slippage_diff: 0.5,
    };
    let engine = ComparisonEngine::new(thresholds);

    let backtest = PerformanceMetrics {
        total_pnl: 100.0,
        win_rate: 0.55,
        sharpe_ratio: 1.0,
        max_drawdown: -50.0,
        fill_rate: 0.95,
        avg_slippage: 0.0001,
        total_trades: 100,
    };

    let forward = PerformanceMetrics {
        total_pnl: -200.0, // Large divergence
        win_rate: 0.3,     // Large divergence
        sharpe_ratio: 0.3,
        max_drawdown: -150.0,
        fill_rate: 0.7,
        avg_slippage: 0.001,
        total_trades: 100,
    };

    let report = engine.compare(&backtest, &forward);

    assert!(!report.overall_pass);
    assert!(report.pnl_diff.abs() > 0.1);
    assert!(report.win_rate_diff.abs() > 0.1);
}

#[test]
fn test_comparison_engine_metric_details() {
    let engine = ComparisonEngine::new(ComparisonThresholds::default());

    let backtest = PerformanceMetrics {
        total_pnl: 100.0,
        win_rate: 0.55,
        sharpe_ratio: 1.0,
        max_drawdown: -50.0,
        fill_rate: 0.95,
        avg_slippage: 0.0001,
        total_trades: 100,
    };
    let forward = PerformanceMetrics {
        total_pnl: 80.0,
        win_rate: 0.50,
        sharpe_ratio: 0.8,
        max_drawdown: -60.0,
        fill_rate: 0.90,
        avg_slippage: 0.0002,
        total_trades: 100,
    };

    let report = engine.compare(&backtest, &forward);

    // Metric details should cover all comparison dimensions
    assert!(!report.metric_details.is_empty());
    let metric_names: Vec<_> = report
        .metric_details
        .iter()
        .map(|d| d.metric_name.clone())
        .collect();
    assert!(metric_names.len() >= 6);
}

// ---------------------------------------------------------------------------
// 5. Duration Management: Graceful Shutdown
// ---------------------------------------------------------------------------

#[test]
fn test_duration_config_none_runs_all_ticks() {
    // With duration: None, runner should process all available ticks
    let events = generate_market_events(100, 100, 110.0);
    let store = VecEventStore::new(events);
    let feed = RecordedDataFeed::new(store, 0.0, None, None);
    let mut config = make_forward_config(&["A"]);
    config.duration = None;
    let mut runner = ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 100);
}

#[test]
fn test_duration_config_limited() {
    // With a very short duration, runner should still complete
    let events = generate_market_events(100, 100, 110.0);
    let store = VecEventStore::new(events);
    let feed = RecordedDataFeed::new(store, 0.0, None, None);
    let mut config = make_forward_config(&["A"]);
    config.duration = Some(Duration::from_secs(60));
    let mut runner = ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    // Should complete without error (max speed, 100 ticks at 100ms = 10s < 60s)
    assert_eq!(result.total_ticks, 100);
    assert!(result.duration_secs >= 0.0);
}

// ---------------------------------------------------------------------------
// 6. Reproducibility: Same Seed + Same Data = Same Result
// ---------------------------------------------------------------------------

#[test]
fn test_reproducibility_same_seed_same_data() {
    let events = generate_market_events(200, 100, 110.0);
    let config = make_forward_config(&["A", "B", "C"]);

    let store1 = VecEventStore::new(events.clone());
    let feed1 = RecordedDataFeed::new(store1, 0.0, None, None);
    let mut runner1 = ForwardTestRunner::new(feed1, config.clone());
    let rt1 = tokio::runtime::Runtime::new().unwrap();
    let result1 = rt1.block_on(runner1.run(99)).unwrap();

    let store2 = VecEventStore::new(events);
    let feed2 = RecordedDataFeed::new(store2, 0.0, None, None);
    let mut runner2 = ForwardTestRunner::new(feed2, config);
    let rt2 = tokio::runtime::Runtime::new().unwrap();
    let result2 = rt2.block_on(runner2.run(99)).unwrap();

    assert_eq!(result1.total_ticks, result2.total_ticks);
    assert_eq!(result1.total_decisions, result2.total_decisions);
    assert_eq!(result1.total_trades, result2.total_trades);
    assert!(
        (result1.final_pnl - result2.final_pnl).abs() < 1e-10,
        "Same seed should produce identical PnL: {} vs {}",
        result1.final_pnl,
        result2.final_pnl
    );
}

// ---------------------------------------------------------------------------
// 7. Report Output: JSON/CSV
// ---------------------------------------------------------------------------

#[test]
fn test_report_generator_json_output() {
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path();

    let report = SessionReport {
        test_result: fx_forward::runner::ForwardTestResult {
            total_ticks: 100,
            total_decisions: 50,
            total_trades: 10,
            strategy_events_published: 0,
            state_snapshots_published: 0,
            duration_secs: 10.0,
            final_pnl: 50.0,
            strategies_used: vec!["A".to_string(), "B".to_string()],
            strategy_funnels: HashMap::new(),
        },
        performance: PerformanceSnapshot::default(),
        comparison: None,
        generated_at_ns: NS_BASE,
    };

    let generator = ReportGenerator::new(report_path.display().to_string(), ReportFormat::Json);
    let result = generator.generate(&report);

    assert!(
        result.is_ok(),
        "JSON report generation failed: {:?}",
        result.err()
    );

    // Verify JSON file was created
    let json_path = report_path.join("session_report.json");
    assert!(json_path.exists(), "JSON report file should exist");
    let content = std::fs::read_to_string(&json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["test_result"]["total_ticks"], 100);
    assert_eq!(parsed["test_result"]["final_pnl"], 50.0);
}

#[test]
fn test_report_generator_csv_output() {
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path();

    let report = SessionReport {
        test_result: fx_forward::runner::ForwardTestResult {
            total_ticks: 100,
            total_decisions: 50,
            total_trades: 10,
            strategy_events_published: 0,
            state_snapshots_published: 0,
            duration_secs: 10.0,
            final_pnl: 50.0,
            strategies_used: vec!["A".to_string()],
            strategy_funnels: HashMap::new(),
        },
        performance: PerformanceSnapshot::default(),
        comparison: None,
        generated_at_ns: NS_BASE,
    };

    let generator = ReportGenerator::new(report_path.display().to_string(), ReportFormat::Csv);
    let result = generator.generate(&report);

    assert!(
        result.is_ok(),
        "CSV report generation failed: {:?}",
        result.err()
    );

    // Verify CSV file was created
    let csv_path = report_path.join("performance_summary.csv");
    assert!(csv_path.exists(), "CSV report file should exist");
}

#[test]
fn test_report_generator_trades_csv() {
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path();

    let trades = vec![
        TradeRecord {
            trade_id: 1,
            timestamp_ns: NS_BASE,
            symbol: "USD/JPY".to_string(),
            side: "Buy".to_string(),
            lots: 100_000.0,
            fill_price: 110.005,
            slippage: 0.0001,
            pnl: 5.0,
            strategy: "A".to_string(),
        },
        TradeRecord {
            trade_id: 2,
            timestamp_ns: NS_BASE + 100_000_000,
            symbol: "USD/JPY".to_string(),
            side: "Sell".to_string(),
            lots: 100_000.0,
            fill_price: 110.010,
            slippage: 0.0002,
            pnl: -3.0,
            strategy: "B".to_string(),
        },
    ];

    let generator = ReportGenerator::new(report_path.display().to_string(), ReportFormat::Csv);
    let result = generator.write_trades_csv(&trades);

    assert!(
        result.is_ok(),
        "Trades CSV generation failed: {:?}",
        result.err()
    );

    let csv_path = report_path.join("trades.csv");
    assert!(csv_path.exists(), "Trades CSV file should exist");
    let content = std::fs::read_to_string(&csv_path).unwrap();
    // Verify CSV has header
    assert!(content.contains("trade_id") || content.contains("symbol"));
    // Verify data rows
    assert!(content.contains("USD/JPY"));
}

// ---------------------------------------------------------------------------
// 8. PerformanceTracker API
// ---------------------------------------------------------------------------

#[test]
fn test_performance_tracker_update_and_snapshot() {
    let mut tracker = PerformanceTracker::new();

    tracker.update(NS_BASE, 10.0, 5.0);
    tracker.record_trade(10.0);
    tracker.update(NS_BASE + 100_000_000, 20.0, 8.0);
    tracker.record_trade(-5.0);

    let snapshot = tracker.snapshot();
    assert!(snapshot.cumulative_pnl.is_finite());
    assert!(snapshot.max_drawdown <= 0.0);
    assert_eq!(snapshot.total_trades, 2);
    assert!(snapshot.win_rate >= 0.0 && snapshot.win_rate <= 1.0);
}

#[test]
fn test_performance_tracker_rolling_sharpe() {
    let mut tracker = PerformanceTracker::with_window(20);

    // Record a series of trades
    for i in 0..30 {
        let pnl = if i % 3 == 0 { 5.0 } else { -2.0 };
        tracker.record_trade(pnl);
        tracker.update(NS_BASE + (i as u64) * 100_000_000, pnl, 0.0);
    }

    let snapshot = tracker.snapshot();
    // Rolling Sharpe should be finite (could be negative)
    assert!(snapshot.rolling_sharpe.is_finite());
}
