mod args;
mod config;
mod output;

use anyhow::{Context, Result};
use args::{Cli, Commands};
use clap::Parser;
use fx_core::types::StrategyId;
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
    info!(data = ?cmd.data, config = ?cmd.config, output = ?cmd.output, "Starting backtest");

    let config = if let Some(config_path) = &cmd.config {
        config::load_backtest_config(config_path)?
    } else {
        fx_backtest::engine::BacktestConfig::default()
    };

    let mut config = config;
    if let Some(strategies_str) = &cmd.strategies {
        config.enabled_strategies = parse_strategies(strategies_str)?;
    }

    println!("Running streaming backtest on {}", cmd.data.display());

    let start = std::time::Instant::now();
    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    let result = engine.run_from_stream_file(&cmd.data)
        .with_context(|| format!("Failed to run streaming backtest from: {}", cmd.data.display()))?;
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

    let output_dir = cmd.output.unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    output::write_backtest_result(&result, &output_dir)?;

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
            ..
        } => run_recorded_forward(config, event_store_path, *speed, seed, &output_dir),
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
    seed: u64,
    output_dir: &Path,
) -> Result<()> {
    let path = PathBuf::from(data_path);
    let ticks = fx_backtest::data::load_csv(&path)
        .with_context(|| format!("Failed to load data from: {}", path.display()))?;
    let total_ticks = ticks.len();
    info!(tick_count = total_ticks, "Loaded tick data");
    println!("Loaded {total_ticks} ticks from {}", path.display());

    let events = fx_backtest::data::ticks_to_events(&ticks);
    let store = fx_forward::feed::VecEventStore::new(events);
    let feed = fx_forward::feed::RecordedDataFeed::new(store, speed, None, None);
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
    let num_features_arg = cmd
        .num_features
        .map(|n| n.to_string())
        .unwrap_or_else(|| "45".to_string());

    println!("Running Python validation pipeline...");
    let output = std::process::Command::new(&cmd.python_path)
        .arg(&bridge_script)
        .arg("--input")
        .arg(result_path)
        .arg("--output")
        .arg(&validation_output)
        .arg("--num-features")
        .arg(&num_features_arg)
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
}
