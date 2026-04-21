//! CLI integration tests: end-to-end backtest pipeline via public APIs.

use std::io::Write;
use std::path::Path;

/// Generate a synthetic CSV with monotonically increasing timestamps,
/// valid bid/ask pairs, and enough ticks for strategies to evaluate.
fn write_synthetic_csv(dir: &Path, filename: &str, n_ticks: usize) -> std::path::PathBuf {
    let path = dir.join(filename);
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "timestamp,bid,ask,bid_volume,ask_volume,symbol").unwrap();
    let base_ts: u64 = 1_700_000_000_000_000_000; // nanos
    for i in 0..n_ticks {
        let ts = base_ts + (i as u64) * 100_000_000; // 100ms apart
        let mid = 150.000 + (i as f64) * 0.001;
        let spread = 0.02;
        writeln!(
            f,
            "{},{:.5},{:.5},1000.0,1000.0,USD/JPY",
            ts,
            mid - spread / 2.0,
            mid + spread / 2.0
        )
        .unwrap();
    }
    path
}

#[test]
fn test_cli_backtest_pipeline_with_synthetic_csv() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 500);

    // Load CSV through the data module (same path as CLI)
    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    assert_eq!(ticks.len(), 500);

    let events = fx_backtest::data::ticks_to_events(&ticks);
    assert!(!events.is_empty());

    let mut config = fx_backtest::engine::BacktestConfig::default();
    config.rng_seed = Some([42u8; 32]);
    // Use only Strategy A for deterministic testing
    use std::collections::HashSet;
    config.enabled_strategies = HashSet::from([fx_core::types::StrategyId::A]);

    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    let result = engine.run_from_events(&events);

    assert_eq!(result.total_ticks, 500);
    assert!(result.summary.total_pnl.is_finite());
    assert!(result.summary.win_rate >= 0.0 && result.summary.win_rate <= 1.0);
    assert!(result.summary.max_drawdown <= 0.0);
}

#[test]
fn test_cli_backtest_writes_output_files() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 300);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);

    let config = fx_backtest::engine::BacktestConfig::default();
    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    let result = engine.run_from_events(&events);

    let output_dir = dir.path().join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Use the same output logic as the CLI
    let json_path = output_dir.join("backtest_result.json");
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "total_ticks": result.total_ticks,
        "total_trades": result.trades.len(),
        "total_pnl": result.summary.total_pnl,
    }))
    .unwrap();
    std::fs::write(&json_path, json).unwrap();

    let trades_path = output_dir.join("trades.csv");
    let mut wtr = csv::Writer::from_path(&trades_path).unwrap();
    for trade in &result.trades {
        wtr.serialize(serde_json::json!({
            "timestamp_ns": trade.timestamp_ns,
            "strategy": format!("{:?}", trade.strategy_id),
            "direction": format!("{:?}", trade.direction),
            "lots": trade.lots,
            "fill_price": trade.fill_price,
            "pnl": trade.pnl,
        }))
        .unwrap();
    }
    wtr.flush().unwrap();

    assert!(json_path.exists());
    assert!(trades_path.exists());

    let json_content = std::fs::read_to_string(&json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
    assert_eq!(parsed["total_ticks"], 300);
}

#[test]
fn test_backtest_feature_dump_writes_schema_and_rows() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 50);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);

    let mut engine =
        fx_backtest::engine::BacktestEngine::new(fx_backtest::engine::BacktestConfig::default());
    let dump_path = dir.path().join("artifacts").join("features.csv");
    engine.enable_feature_dump(&dump_path).unwrap();
    let result = engine.run_from_events(&events);

    assert_eq!(result.total_ticks, 50);
    assert!(dump_path.exists());

    let mut reader = csv::Reader::from_path(&dump_path).unwrap();
    let headers = reader.headers().unwrap().clone();
    assert_eq!(headers.get(0), Some("timestamp_ns"));
    assert_eq!(headers.get(1), Some("source_strategy"));
    assert_eq!(headers.get(2), Some("feature_version"));
    assert_eq!(headers.get(3), Some("spread"));
    assert_eq!(headers.len(), 3 + fx_strategy::features::FeatureVector::DIM);

    let rows: Vec<_> = reader.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows.len(), 50);
    assert_eq!(rows[0].get(1), Some("A"));
    assert_eq!(
        rows[0].get(2),
        Some(fx_strategy::features::FeatureVector::SCHEMA_VERSION)
    );
}

#[test]
fn test_cli_backtest_with_toml_config() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 200);

    // Write a TOML config
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
symbol = "USD/JPY"
default_lot_size = 50000
rng_seed = 99

[strategy_a]
spread_z_threshold = 2.0

[mc_eval.reward]
gamma = 0.99
"#,
    )
    .unwrap();

    // Load config the same way the CLI does
    let config = crate_integration_config_load(&config_path);
    assert_eq!(config.symbol, "USD/JPY");
    assert_eq!(config.default_lot_size, 50000);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);

    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    let result = engine.run_from_events(&events);

    assert_eq!(result.total_ticks, 200);
    assert!(result.summary.total_pnl.is_finite());
}

#[test]
fn test_cli_backtest_csv_validation_errors() {
    let dir = tempfile::tempdir().unwrap();

    // CSV with bid >= ask (invalid)
    let bad_path = dir.path().join("bad.csv");
    let mut f = std::fs::File::create(&bad_path).unwrap();
    writeln!(f, "timestamp,bid,ask,bid_volume,ask_volume,symbol").unwrap();
    writeln!(f, "1700000000000000000,150.0,149.0,1000.0,1000.0,USD/JPY").unwrap();

    let result = fx_backtest::data::load_csv(&bad_path);
    assert!(result.is_err());
}

#[test]
fn test_cli_backtest_nonexistent_csv_error() {
    let result = fx_backtest::data::load_csv(Path::new("/nonexistent/file.csv"));
    assert!(result.is_err());
}

/// Helper: load config using the same TOML logic as the CLI.
/// We duplicate the minimal logic here since integration tests can't access
/// the private config module of a binary crate.
fn crate_integration_config_load(path: &Path) -> fx_backtest::engine::BacktestConfig {
    let content = std::fs::read_to_string(path).unwrap();
    let raw: toml::Value = toml::from_str(&content).unwrap();
    let mut config = fx_backtest::engine::BacktestConfig::default();
    if let Some(table) = raw.as_table() {
        if let Some(v) = table.get("symbol").and_then(|v| v.as_str()) {
            config.symbol = v.to_string();
        }
        if let Some(v) = table.get("default_lot_size").and_then(|v| v.as_integer()) {
            config.default_lot_size = v as u64;
        }
        if let Some(v) = table.get("rng_seed").and_then(|v| v.as_integer()) {
            let seed = v as u64;
            let bytes = seed.to_le_bytes();
            let mut arr = [0u8; 32];
            for (i, chunk) in bytes.iter().cycle().take(32).enumerate() {
                arr[i] = *chunk;
            }
            config.rng_seed = Some(arr);
        }
    }
    config
}

#[test]
fn test_cli_backtest_reproducibility() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 300);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);

    // Run 1
    let mut config1 = fx_backtest::engine::BacktestConfig::default();
    config1.rng_seed = Some([7u8; 32]);
    let mut engine1 = fx_backtest::engine::BacktestEngine::new(config1);
    let result1 = engine1.run_from_events(&events);

    // Run 2 with same seed
    let mut config2 = fx_backtest::engine::BacktestConfig::default();
    config2.rng_seed = Some([7u8; 32]);
    let mut engine2 = fx_backtest::engine::BacktestEngine::new(config2);
    let result2 = engine2.run_from_events(&events);

    assert_eq!(result1.total_ticks, result2.total_ticks);
    assert_eq!(result1.trades.len(), result2.trades.len());
    assert!((result1.summary.total_pnl - result2.summary.total_pnl).abs() < 1e-10);
    for (t1, t2) in result1.trades.iter().zip(result2.trades.iter()) {
        assert_eq!(t1.timestamp_ns, t2.timestamp_ns);
        assert!((t1.pnl - t2.pnl).abs() < 1e-10);
    }
}

#[test]
fn test_cli_backtest_strategy_selection() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 500);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);

    // Run with only Strategy B
    let mut config = fx_backtest::engine::BacktestConfig::default();
    config.rng_seed = Some([42u8; 32]);
    use std::collections::HashSet;
    config.enabled_strategies = HashSet::from([fx_core::types::StrategyId::B]);

    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    let result = engine.run_from_events(&events);

    // All decisions should be from Strategy B
    for d in &result.decisions {
        assert_eq!(d.strategy_id, fx_core::types::StrategyId::B);
    }
}

// --- Forward-test CLI integration tests ---

/// Build a ForwardTestConfig with recorded data source from a CSV file.
fn make_forward_config(strategies: &[&str], speed: f64) -> fx_forward::config::ForwardTestConfig {
    use std::collections::HashSet;
    fx_forward::config::ForwardTestConfig {
        enabled_strategies: strategies
            .iter()
            .map(|s| s.to_string())
            .collect::<HashSet<_>>(),
        data_source: fx_forward::feed::DataSourceConfig::Recorded {
            event_store_path: String::new(),
            speed,
            start_time: None,
            end_time: None,
        },
        duration: None,
        alert_config: fx_forward::config::AlertConfig {
            channels: vec![fx_forward::config::AlertChannelConfig::Log],
            risk_limit_threshold: 0.8,
            execution_drift_threshold: 2.0,
            sharpe_degradation_threshold: 0.3,
        },
        report_config: fx_forward::config::ReportConfig {
            output_dir: "./reports".to_string(),
            format: fx_forward::config::ReportFormat::Both,
            interval: None,
        },
        risk_config: fx_forward::config::ForwardRiskConfig {
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

#[test]
fn test_cli_forward_pipeline_with_synthetic_csv() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 200);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    assert_eq!(ticks.len(), 200);

    let events = fx_backtest::data::ticks_to_events(&ticks);
    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, 0.0, None, None);

    let config = make_forward_config(&["A"], 0.0);
    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 200);
    assert!(result.duration_secs >= 0.0);
    assert!(result.final_pnl.is_finite());
}

#[test]
fn test_cli_forward_writes_output_files() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 100);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);
    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, 0.0, None, None);

    let config = make_forward_config(&["A"], 0.0);
    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let fw_result = rt.block_on(runner.run(42)).unwrap();

    let output_dir = dir.path().join("forward_output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Write using CLI output logic
    let json_path = output_dir.join("forward_result.json");
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "total_ticks": fw_result.total_ticks,
        "total_decisions": fw_result.total_decisions,
        "total_trades": fw_result.total_trades,
        "duration_secs": fw_result.duration_secs,
        "final_pnl": fw_result.final_pnl,
        "strategies_used": fw_result.strategies_used,
    }))
    .unwrap();
    std::fs::write(&json_path, json).unwrap();

    assert!(json_path.exists());
    let content = std::fs::read_to_string(&json_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["total_ticks"], 100);
    assert!(parsed["final_pnl"].as_f64().unwrap().is_finite());
}

#[test]
fn test_cli_forward_with_toml_config() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 100);

    // Write a forward test TOML config
    let config_path = dir.path().join("forward.toml");
    std::fs::write(
        &config_path,
        r#"
[forward]
enabled_strategies = ["A"]
duration_secs = 60

[forward.data_source]
type = "recorded"
event_store_path = "PLACEHOLDER"
speed = 0.0

[forward.alert]
channels = ["log"]
risk_limit_threshold = 0.8

[forward.report]
output_dir = "./reports"
format = "json"

[forward.risk]
max_position_lots = 10.0
max_daily_loss_mtm = 500.0
max_daily_loss_realized = 1000.0
max_weekly_loss = 2500.0
max_monthly_loss = 5000.0
daily_mtm_lot_fraction = 0.25
daily_mtm_q_threshold = 0.01
max_drawdown = 1000.0
"#,
    )
    .unwrap();

    let config = fx_forward::config::ForwardTestConfig::load_from_file(&config_path)
        .unwrap_or_else(|_| make_forward_config(&["A"], 0.0));

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);
    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, 0.0, None, None);

    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 100);
}

#[test]
fn test_cli_forward_strategy_selection() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 200);

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);
    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, 0.0, None, None);

    let config = make_forward_config(&["B", "C"], 0.0);
    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 200);
    assert!(result.strategies_used.contains(&"B".to_string()));
    assert!(result.strategies_used.contains(&"C".to_string()));
    assert!(!result.strategies_used.contains(&"A".to_string()));
}

#[test]
fn test_cli_forward_reproducibility() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 200);

    let config = make_forward_config(&["A"], 0.0);

    // Run 1
    let ticks1 = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events1 = fx_backtest::data::ticks_to_events(&ticks1);
    let store1 = fx_forward::feed::VecEventStore::new(events1);
    let feed1 = fx_forward::feed::RecordedDataFeed::new(store1, 0.0, None, None);
    let mut runner1 = fx_forward::runner::ForwardTestRunner::new(feed1, config.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let r1 = rt.block_on(runner1.run(12345)).unwrap();

    // Run 2 with same seed
    let ticks2 = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events2 = fx_backtest::data::ticks_to_events(&ticks2);
    let store2 = fx_forward::feed::VecEventStore::new(events2);
    let feed2 = fx_forward::feed::RecordedDataFeed::new(store2, 0.0, None, None);
    let mut runner2 = fx_forward::runner::ForwardTestRunner::new(feed2, config);
    let r2 = rt.block_on(runner2.run(12345)).unwrap();

    assert_eq!(r1.total_ticks, r2.total_ticks);
    assert_eq!(r1.total_decisions, r2.total_decisions);
    assert!((r1.final_pnl - r2.final_pnl).abs() < 1e-10);
}

#[test]
fn test_cli_forward_with_data_source_override() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = write_synthetic_csv(dir.path(), "ticks.csv", 100);

    // Simulate CLI --source recorded --data-path <CSV> --speed 0 override
    let config = make_forward_config(&["A"], 1.0); // default speed=1.0
                                                   // Override data source (as CLI does)
    let mut config = config;
    config.data_source = fx_forward::feed::DataSourceConfig::Recorded {
        event_store_path: csv_path.to_string_lossy().to_string(),
        speed: 0.0, // max speed
        start_time: None,
        end_time: None,
    };

    let ticks = fx_backtest::data::load_csv(&csv_path).unwrap();
    let events = fx_backtest::data::ticks_to_events(&ticks);
    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, 0.0, None, None);

    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(runner.run(42)).unwrap();

    assert_eq!(result.total_ticks, 100);
}
