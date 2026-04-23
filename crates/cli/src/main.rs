mod args;
mod config;
mod output;

use anyhow::{Context, Result};
use args::{Cli, Commands};
use chrono::DateTime;
use clap::Parser;
use fx_core::random::expand_u64_seed;
use fx_core::types::StrategyId;
use fx_events::store::{EventStore, Tier1Store};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::info;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Backtest(cmd) => run_backtest(cmd),
        Commands::ForwardTest(cmd) => run_forward_test(cmd),
        Commands::Validate(cmd) => run_validate(cmd),
    }
}

fn run_backtest(cmd: args::BacktestCmd) -> Result<()> {
    info!(
        data = ?cmd.data,
        config = ?cmd.config,
        output = ?cmd.output,
        start_time = ?cmd.start_time,
        end_time = ?cmd.end_time,
        import_q_state = ?cmd.import_q_state,
        export_q_state = ?cmd.export_q_state,
        "Starting backtest"
    );

    let mut config = if let Some(config_path) = &cmd.config {
        config::load_backtest_config(config_path)?
    } else {
        fx_backtest::engine::BacktestConfig::default()
    };

    apply_backtest_overrides(&mut config, &cmd)?;

    apply_backtest_seed_override(&mut config, cmd.seed);

    println!("Running streaming backtest on {}", cmd.data.display());

    let start = std::time::Instant::now();
    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    if let Some(path) = &cmd.import_q_state {
        let snapshot = fx_strategy::q_state::StrategySetQStateSnapshot::read_from_path(path)?;
        engine.import_q_state(&snapshot).with_context(|| {
            format!("Failed to import q-state snapshot from {}", path.display())
        })?;
    }
    if let Some(path) = &cmd.dump_features {
        engine.enable_feature_dump(path)?;
    }
    let result = engine.run_from_stream_file(&cmd.data).with_context(|| {
        format!(
            "Failed to run streaming backtest from: {}",
            cmd.data.display()
        )
    })?;
    let elapsed = start.elapsed();

    let total_trades = result.trades.len();
    let total_decisions = result.decisions.len();
    info!(
        total_ticks = result.total_ticks,
        total_trades,
        total_decisions,
        wall_time_ms = result.wall_time_ms,
        "Backtest completed"
    );

    println!(
        "Backtest completed in {:.1}s: {} ticks processed, {} trades, {} decisions",
        elapsed.as_secs_f64(),
        result.total_ticks,
        total_trades,
        total_decisions,
    );
    println!(
        "  PnL: {:.2} | Win rate: {:.1}% | Max DD: {:.2} | Sharpe: {:.3}",
        result.summary.total_pnl,
        result.summary.win_rate * 100.0,
        result.summary.max_drawdown,
        result.summary.sharpe_ratio,
    );
    let diagnostics = output::backtest_decision_diagnostics(&result);
    let risk_metric = output::backtest_risk_metric_summary(&result);
    println!(
        "  Triggered: {} | Entry attempts: {} | Filled: {} | Close trades: {}",
        diagnostics.triggered_decisions,
        diagnostics.entry_attempts,
        diagnostics.filled_entries,
        diagnostics.close_trades,
    );
    println!(
        "  Sharpe basis: {} ({} returns)",
        risk_metric.basis,
        risk_metric.returns.len(),
    );
    if !diagnostics.skip_reasons.is_empty() {
        let top_skips = diagnostics
            .skip_reasons
            .iter()
            .take(3)
            .map(|entry| format!("{}={}", entry.reason, entry.count))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  Top skips: {}", top_skips);
    }

    // Trigger diagnostics
    let td = &result.trigger_diagnostics;
    for sid in fx_core::types::StrategyId::all() {
        let eval = td.evaluated.get(sid).unwrap_or(&0);
        let trig = td.triggered.get(sid).unwrap_or(&0);
        let idle = td.idle_triggered.get(sid).unwrap_or(&0);
        let attempted = td.order_attempted.get(sid).unwrap_or(&0);
        let risk_passed = td.risk_passed.get(sid).unwrap_or(&0);
        let filled = td.filled.get(sid).unwrap_or(&0);
        let closed = td.closed.get(sid).unwrap_or(&0);
        let strat_skips: Vec<String> = diagnostics
            .skip_reasons_by_strategy
            .iter()
            .filter(|s| s.strategy == format!("{:?}", sid))
            .flat_map(|s| s.reasons.iter().take(3).map(|r| format!("{}={}", r.reason, r.count)))
            .collect();
        let skip_str = if strat_skips.is_empty() { String::new() } else { format!(" | skips: {}", strat_skips.join(", ")) };
        println!(
            "  {:?}: evaluated={}, triggered={}, idle_triggered={}, decide_called={}, order_attempted={}, risk_passed={}, filled={}, closed={}{}",
            sid,
            eval,
            trig,
            idle,
            td.decide_called.get(sid).unwrap_or(&0),
            attempted,
            risk_passed,
            filled,
            closed,
            skip_str,
        );
    }

    let output_dir = cmd.output.unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    output::write_backtest_result(&result, &output_dir)?;
    if let Some(path) = &cmd.export_q_state {
        let mut snapshot = engine.export_q_state();
        snapshot
            .write_to_path(path)
            .with_context(|| format!("Failed to export q-state snapshot to {}", path.display()))?;
        println!("Q-state snapshot written to {}", path.display());
    }

    info!(dir = %output_dir.display(), "Results written");
    println!("Results written to {}", output_dir.display());
    Ok(())
}

fn run_forward_test(cmd: args::ForwardTestCmd) -> Result<()> {
    info!(config = ?cmd.config, "Starting forward test");

    let mut config = if let Some(config_path) = &cmd.config {
        fx_forward::config::ForwardTestConfig::load_from_file(config_path)?
    } else {
        fx_forward::config::ForwardTestConfig::default()
    };

    // Apply CLI arg overrides
    if let Some(duration_secs) = cmd.duration {
        config.duration = Some(std::time::Duration::from_secs(duration_secs));
    }
    if let Some(strategies) = &cmd.strategies {
        config.enabled_strategies = parse_forward_strategies(strategies)?;
    }
    if let Some(output) = &cmd.output {
        config.report_config.output_dir = output.to_string_lossy().to_string();
    }

    // Override data source from CLI args
    if let Some(source) = &cmd.source {
        match source.as_str() {
            "recorded" => {
                let path = cmd
                    .data_path
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--data-path required for recorded source"))?;
                config.data_source = fx_forward::feed::DataSourceConfig::Recorded {
                    event_store_path: path.to_string_lossy().to_string(),
                    speed: cmd.speed,
                    start_time: None,
                    end_time: None,
                };
            }
            "external" => {
                let provider = cmd
                    .provider
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--provider required for external source"))?;
                let credentials = cmd
                    .credentials
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--credentials required for external source"))?;
                config.data_source = fx_forward::feed::DataSourceConfig::ExternalApi {
                    provider: provider.clone(),
                    credentials_path: credentials.to_string_lossy().to_string(),
                    symbols: vec!["USD/JPY".to_string()],
                };
            }
            _ => {
                anyhow::bail!(
                    "Unknown source: '{}'. Must be 'recorded' or 'external'.",
                    source
                )
            }
        }
    }

    let output_dir = cmd
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("./reports"));
    let seed = cmd.seed;

    // Clone data_source to avoid borrow conflict when moving config
    let data_source = config.data_source.clone();
    match &data_source {
        fx_forward::feed::DataSourceConfig::Recorded {
            event_store_path,
            speed,
            start_time,
            end_time,
        } => run_recorded_forward(
            config,
            event_store_path,
            *speed,
            start_time.as_deref(),
            end_time.as_deref(),
            seed,
            &output_dir,
        ),
        fx_forward::feed::DataSourceConfig::ExternalApi {
            provider,
            credentials_path,
            symbols,
        } => run_external_forward(
            config,
            provider,
            credentials_path,
            symbols,
            seed,
            &output_dir,
        ),
    }
}

fn run_recorded_forward(
    config: fx_forward::config::ForwardTestConfig,
    data_path: &str,
    speed: f64,
    start_time: Option<&str>,
    end_time: Option<&str>,
    seed: u64,
    output_dir: &Path,
) -> Result<()> {
    let path = PathBuf::from(data_path);
    let events = load_recorded_market_events(&path)?;
    let total_ticks = events.len();
    let (start_time_ns, end_time_ns) = resolve_cli_time_range(start_time, end_time)?;
    info!(
        event_count = total_ticks,
        ?start_time_ns,
        ?end_time_ns,
        "Loaded recorded market events"
    );
    println!(
        "Loaded {total_ticks} recorded market events from {}",
        path.display()
    );

    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, speed, start_time_ns, end_time_ns);
    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);

    println!("Running forward test on {total_ticks} events...");

    let rt = tokio::runtime::Runtime::new()?;
    let start = std::time::Instant::now();

    let result = rt.block_on(async {
        tokio::select! {
            result = runner.run(seed) => Some(result),
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nCtrl+C received, shutting down gracefully...");
                None
            }
        }
    });

    let elapsed = start.elapsed();

    if result.is_none() {
        // Ctrl+C — capture partial results from tracker
        let snapshot = runner.tracker().snapshot();
        let partial = fx_forward::runner::ForwardTestResult {
            total_ticks: 0,
            total_decisions: 0,
            total_trades: snapshot.total_trades,
            strategy_events_published: 0,
            state_snapshots_published: 0,
            duration_secs: elapsed.as_secs_f64(),
            final_pnl: snapshot.cumulative_pnl,
            strategies_used: vec![],
        };
        println!(
            "Forward test interrupted after {:.1}s: {} trades, PnL: {:.2}",
            elapsed.as_secs_f64(),
            partial.total_trades,
            partial.final_pnl,
        );
        std::fs::create_dir_all(output_dir)?;
        output::write_forward_result(&partial, output_dir)?;
        println!("Partial results written to {}", output_dir.display());
        return Ok(());
    }

    let fw_result = result.unwrap()?;

    print_forward_summary(&fw_result, elapsed.as_secs_f64());

    std::fs::create_dir_all(output_dir)?;
    output::write_forward_result(&fw_result, output_dir)?;

    info!(dir = %output_dir.display(), "Forward test results written");
    println!("Results written to {}", output_dir.display());
    Ok(())
}

fn load_recorded_market_events(path: &Path) -> Result<Vec<fx_events::event::GenericEvent>> {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
    {
        let ticks = fx_backtest::data::load_csv(path)
            .with_context(|| format!("Failed to load CSV data from: {}", path.display()))?;
        return Ok(fx_backtest::data::ticks_to_events(&ticks));
    }

    let store = Tier1Store::open(
        path.to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid event store path: {}", path.display()))?,
    )
    .with_context(|| format!("Failed to open recorded Event Store: {}", path.display()))?;
    store
        .replay(fx_core::types::StreamId::Market, 0)
        .with_context(|| format!("Failed to replay market events from: {}", path.display()))
}

fn apply_backtest_overrides(
    config: &mut fx_backtest::engine::BacktestConfig,
    cmd: &args::BacktestCmd,
) -> Result<()> {
    if let Some(strategies_str) = &cmd.strategies {
        config.enabled_strategies = parse_strategies(strategies_str)?;
    }

    let (start_time_ns, end_time_ns) =
        resolve_cli_time_range(cmd.start_time.as_deref(), cmd.end_time.as_deref())?;
    if let Some(start_time_ns) = start_time_ns {
        config.start_time_ns = start_time_ns;
    }
    if let Some(end_time_ns) = end_time_ns {
        config.end_time_ns = end_time_ns;
    }

    Ok(())
}

fn apply_backtest_seed_override(
    config: &mut fx_backtest::engine::BacktestConfig,
    seed: Option<u64>,
) {
    if let Some(seed) = seed {
        config.rng_seed = Some(expand_u64_seed(seed));
    }
}

fn resolve_cli_time_range(
    start_time: Option<&str>,
    end_time: Option<&str>,
) -> Result<(Option<u64>, Option<u64>)> {
    let start_time_ns = parse_cli_time(start_time)?;
    let end_time_ns = parse_cli_time(end_time)?;
    if let (Some(start), Some(end)) = (start_time_ns, end_time_ns) {
        if start > end {
            anyhow::bail!("--start-time must be <= --end-time");
        }
    }
    Ok((start_time_ns, end_time_ns))
}

fn parse_cli_time(value: Option<&str>) -> Result<Option<u64>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    if let Ok(ns) = raw.parse::<u64>() {
        return Ok(Some(ns));
    }

    let timestamp = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("Invalid timestamp '{raw}'. Use ns or RFC3339."))?;
    let ns = timestamp
        .timestamp_nanos_opt()
        .ok_or_else(|| anyhow::anyhow!("Timestamp out of range: {raw}"))?;
    if ns < 0 {
        anyhow::bail!("Timestamp must be >= 0: {raw}");
    }
    Ok(Some(ns as u64))
}

fn run_external_forward(
    config: fx_forward::config::ForwardTestConfig,
    provider: &str,
    credentials_path: &str,
    symbols: &[String],
    seed: u64,
    output_dir: &Path,
) -> Result<()> {
    info!(provider, "Starting forward test with external API");

    let api_config = fx_forward::feed::ApiFeedConfig {
        provider: provider.to_string(),
        credentials_path: credentials_path.to_string(),
        symbols: symbols.to_vec(),
        ..Default::default()
    };
    let feed = fx_forward::feed::ExternalApiFeed::new(api_config);
    let mut runner = fx_forward::runner::ForwardTestRunner::new(feed, config);

    println!("Running forward test with external API ({provider})...");

    let rt = tokio::runtime::Runtime::new()?;
    let start = std::time::Instant::now();

    let result = rt.block_on(async {
        tokio::select! {
            result = runner.run(seed) => Some(result),
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nCtrl+C received, shutting down gracefully...");
                None
            }
        }
    });

    let elapsed = start.elapsed();

    if result.is_none() {
        let snapshot = runner.tracker().snapshot();
        let partial = fx_forward::runner::ForwardTestResult {
            total_ticks: 0,
            total_decisions: 0,
            total_trades: snapshot.total_trades,
            strategy_events_published: 0,
            state_snapshots_published: 0,
            duration_secs: elapsed.as_secs_f64(),
            final_pnl: snapshot.cumulative_pnl,
            strategies_used: vec![],
        };
        println!(
            "Forward test interrupted after {:.1}s: {} trades, PnL: {:.2}",
            elapsed.as_secs_f64(),
            partial.total_trades,
            partial.final_pnl,
        );
        std::fs::create_dir_all(output_dir)?;
        output::write_forward_result(&partial, output_dir)?;
        return Ok(());
    }

    let fw_result = result.unwrap()?;

    print_forward_summary(&fw_result, elapsed.as_secs_f64());

    std::fs::create_dir_all(output_dir)?;
    output::write_forward_result(&fw_result, output_dir)?;

    println!("Results written to {}", output_dir.display());
    Ok(())
}

fn print_forward_summary(result: &fx_forward::runner::ForwardTestResult, elapsed_secs: f64) {
    println!(
        "Forward test completed in {:.1}s: {} ticks, {} trades, {} decisions",
        elapsed_secs, result.total_ticks, result.total_trades, result.total_decisions,
    );
    println!(
        "  PnL: {:.2} | Strategies: {}",
        result.final_pnl,
        result.strategies_used.join(", "),
    );
}

fn parse_forward_strategies(s: &str) -> Result<HashSet<String>> {
    let mut set = HashSet::new();
    for part in s.split(',') {
        let name = part.trim().to_uppercase();
        match name.as_str() {
            "A" | "B" | "C" => {
                set.insert(name);
            }
            _ => anyhow::bail!("Unknown strategy: '{}'. Must be A, B, or C.", name),
        }
    }
    if set.is_empty() {
        anyhow::bail!("At least one strategy must be specified");
    }
    Ok(set)
}

fn parse_strategies(s: &str) -> Result<HashSet<StrategyId>> {
    let mut set = HashSet::new();
    for part in s.split(',') {
        let name = part.trim().to_uppercase();
        match name.as_str() {
            "A" => {
                set.insert(StrategyId::A);
            }
            "B" => {
                set.insert(StrategyId::B);
            }
            "C" => {
                set.insert(StrategyId::C);
            }
            _ => anyhow::bail!("Unknown strategy: '{}'. Must be A, B, or C.", name),
        }
    }
    if set.is_empty() {
        anyhow::bail!("At least one strategy must be specified");
    }
    Ok(set)
}

fn run_validate(cmd: args::ValidateCmd) -> Result<()> {
    info!(
        backtest_result = ?cmd.backtest_result,
        python_path = %cmd.python_path,
        "Starting validation"
    );

    let result_path = &cmd.backtest_result;
    if !result_path.exists() {
        anyhow::bail!("Backtest result file not found: {}", result_path.display());
    }

    println!("Reading backtest result from {}", result_path.display());

    // Find the bridge script — look in research/bridge/cli.py relative to project root
    let bridge_script = find_bridge_script()?;

    let output_dir = cmd.output.unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    let validation_output = output_dir.join("validation_result.json");

    // Build arguments for Python bridge
    println!("Running Python validation pipeline...");
    let mut command = std::process::Command::new(&cmd.python_path);
    command
        .arg(&bridge_script)
        .arg("--input")
        .arg(result_path)
        .arg("--output")
        .arg(&validation_output);
    if let Some(num_features) = cmd.num_features {
        command.arg("--num-features").arg(num_features.to_string());
    }
    let output = command
        .output()
        .with_context(|| format!("Failed to execute Python at '{}'", cmd.python_path))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Python validation failed:\n{stderr}");
    }

    if !validation_output.exists() {
        anyhow::bail!(
            "Python validation did not produce output: {}",
            validation_output.display()
        );
    }

    let validation = output::ValidationResult::from_json_file(&validation_output)?;
    validation.print_summary();

    info!(
        all_passed = validation.all_passed,
        n_passed = validation.n_passed,
        n_failed = validation.n_failed,
        "Validation completed"
    );

    Ok(())
}

fn find_bridge_script() -> Result<PathBuf> {
    let candidates = [
        PathBuf::from("research/bridge/cli.py"),
        PathBuf::from("../research/bridge/cli.py"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }
    anyhow::bail!(
        "Bridge script not found. Expected research/bridge/cli.py relative to working directory."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_parse_strategies_single() {
        let set = parse_strategies("A").unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&StrategyId::A));
    }

    #[test]
    fn test_parse_strategies_multiple() {
        let set = parse_strategies("A,B,C").unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.contains(&StrategyId::A));
        assert!(set.contains(&StrategyId::B));
        assert!(set.contains(&StrategyId::C));
    }

    #[test]
    fn test_parse_strategies_case_insensitive() {
        let set = parse_strategies("a,b").unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&StrategyId::A));
        assert!(set.contains(&StrategyId::B));
    }

    #[test]
    fn test_parse_strategies_unknown_fails() {
        let result = parse_strategies("X");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_strategies_empty_fails() {
        let result = parse_strategies("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_cli_time_accepts_nanoseconds() {
        let parsed = parse_cli_time(Some("1704067200000000000")).unwrap();
        assert_eq!(parsed, Some(1_704_067_200_000_000_000));
    }

    #[test]
    fn test_parse_cli_time_accepts_rfc3339() {
        let parsed = parse_cli_time(Some("2024-01-01T00:00:00Z")).unwrap();
        assert_eq!(parsed, Some(1_704_067_200_000_000_000));
    }

    #[test]
    fn test_resolve_cli_time_range_rejects_inverted_range() {
        let err =
            resolve_cli_time_range(Some("2024-01-01T01:00:00Z"), Some("2024-01-01T00:00:00Z"))
                .unwrap_err()
                .to_string();
        assert!(err.contains("--start-time must be <= --end-time"));
    }

    #[test]
    fn test_apply_backtest_overrides_sets_time_range() {
        let mut config = fx_backtest::engine::BacktestConfig::default();
        let cmd = args::BacktestCmd {
            data: PathBuf::from("ticks.csv"),
            config: None,
            output: None,
            strategies: Some("A,B".to_string()),
            start_time: Some("2024-01-01T00:00:00Z".to_string()),
            end_time: Some("2024-01-01T00:01:00Z".to_string()),
            dump_features: None,
            import_q_state: None,
            export_q_state: None,
            seed: Some(42),
        };

        apply_backtest_overrides(&mut config, &cmd).unwrap();

        assert!(config.enabled_strategies.contains(&StrategyId::A));
        assert!(config.enabled_strategies.contains(&StrategyId::B));
        assert_eq!(config.start_time_ns, 1_704_067_200_000_000_000);
        assert_eq!(config.end_time_ns, 1_704_067_260_000_000_000);
    }

    #[test]
    fn test_apply_backtest_seed_override_preserves_existing_seed_when_omitted() {
        let mut config = fx_backtest::engine::BacktestConfig::default();
        config.rng_seed = Some(expand_u64_seed(99));

        apply_backtest_seed_override(&mut config, None);

        assert_eq!(config.rng_seed, Some(expand_u64_seed(99)));
    }

    #[test]
    fn test_apply_backtest_seed_override_replaces_existing_seed_when_provided() {
        let mut config = fx_backtest::engine::BacktestConfig::default();
        config.rng_seed = Some(expand_u64_seed(99));

        apply_backtest_seed_override(&mut config, Some(7));

        assert_eq!(config.rng_seed, Some(expand_u64_seed(7)));
    }
}
