mod args;
mod config;
mod output;

use anyhow::{Context, Result};
use args::{Cli, Commands};
use clap::Parser;
use fx_core::types::StrategyId;
use std::collections::HashSet;
use std::path::PathBuf;
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

    let ticks = fx_backtest::data::load_csv(&cmd.data)
        .with_context(|| format!("Failed to load CSV data from: {}", cmd.data.display()))?;

    let total_ticks = ticks.len();
    info!(tick_count = total_ticks, "Loaded tick data");
    println!("Loaded {total_ticks} ticks from {}", cmd.data.display());

    let events = fx_backtest::data::ticks_to_events(&ticks);
    println!("Running backtest on {total_ticks} events...");

    let start = std::time::Instant::now();
    let mut engine = fx_backtest::engine::BacktestEngine::new(config);
    let result = engine.run_from_events(&events);
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

    // Forward-test integration is wired up in Task 13 (CLI forward-test subcommand integration).
    // This placeholder validates config loading and argument parsing.
    let _config = if let Some(config_path) = &cmd.config {
        Some(fx_forward::config::ForwardTestConfig::load_from_file(
            config_path,
        )?)
    } else {
        None
    };

    if let Some(duration_secs) = cmd.duration {
        info!(duration_secs, "Forward test duration configured");
    }

    if let Some(strategies) = &cmd.strategies {
        info!(strategies, "Strategies configured");
    }

    let output_dir = cmd.output.unwrap_or_else(|| PathBuf::from("./reports"));
    info!(dir = %output_dir.display(), "Forward test output directory configured");

    println!("Forward test: config validated. Full integration pending (Task 13).");
    Ok(())
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
