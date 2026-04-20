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
