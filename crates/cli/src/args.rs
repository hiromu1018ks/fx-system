use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// FX AI sub-short-term automated trading system CLI.
#[derive(Parser, Debug)]
#[command(name = "fx-cli", version, about = "FX trading system CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run a backtest on historical CSV data.
    Backtest(BacktestCmd),
    /// Run a forward test (paper trading) using recorded or live data.
    ForwardTest(ForwardTestCmd),
}

/// Backtest subcommand arguments.
#[derive(Parser, Debug)]
pub struct BacktestCmd {
    /// Path to CSV file with historical tick data.
    #[arg(short, long)]
    pub data: PathBuf,

    /// Path to TOML configuration file (optional, uses defaults if omitted).
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Output directory for results.
    #[arg(short, long, default_value = ".")]
    pub output: Option<PathBuf>,

    /// Comma-separated list of strategies to enable (e.g., "A,B,C").
    #[arg(short, long)]
    pub strategies: Option<String>,
}

/// Forward-test subcommand arguments.
#[derive(Parser, Debug)]
pub struct ForwardTestCmd {
    /// Path to TOML configuration file (optional, uses defaults if omitted).
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Duration of the forward test in seconds.
    #[arg(short, long)]
    pub duration: Option<u64>,

    /// Comma-separated list of strategies to enable (e.g., "A,B,C").
    #[arg(short, long)]
    pub strategies: Option<String>,

    /// Output directory for results.
    #[arg(short, long, default_value = "./reports")]
    pub output: Option<PathBuf>,

    /// Data source type: "recorded" (CSV file) or "external" (live API).
    #[arg(long)]
    pub source: Option<String>,

    /// Path to CSV data file for recorded source.
    #[arg(long)]
    pub data_path: Option<PathBuf>,

    /// Playback speed for recorded data (0 = max speed, 1.0 = realtime).
    #[arg(long, default_value_t = 0.0)]
    pub speed: f64,

    /// Provider name for external API source (e.g., "OANDA").
    #[arg(long)]
    pub provider: Option<String>,

    /// Path to credentials file for external API source.
    #[arg(long)]
    pub credentials: Option<PathBuf>,

    /// RNG seed for reproducibility.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_parse_backtest_minimal() {
        let cli = Cli::try_parse_from(["fx-cli", "backtest", "--data", "ticks.csv"]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::Backtest(cmd) => {
                assert_eq!(cmd.data, PathBuf::from("ticks.csv"));
                assert!(cmd.config.is_none());
                assert!(cmd.strategies.is_none());
            }
            _ => panic!("expected Backtest command"),
        }
    }

    #[test]
    fn test_parse_backtest_full() {
        let cli = Cli::try_parse_from([
            "fx-cli",
            "backtest",
            "--data",
            "data.csv",
            "--config",
            "config.toml",
            "--output",
            "/tmp/results",
            "--strategies",
            "A,B",
        ]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::Backtest(cmd) => {
                assert_eq!(cmd.data, PathBuf::from("data.csv"));
                assert_eq!(cmd.config, Some(PathBuf::from("config.toml")));
                assert_eq!(cmd.output, Some(PathBuf::from("/tmp/results")));
                assert_eq!(cmd.strategies.as_deref(), Some("A,B"));
            }
            _ => panic!("expected Backtest command"),
        }
    }

    #[test]
    fn test_parse_forward_test_minimal() {
        let cli = Cli::try_parse_from(["fx-cli", "forward-test"]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::ForwardTest(cmd) => {
                assert!(cmd.config.is_none());
                assert!(cmd.duration.is_none());
                assert!(cmd.strategies.is_none());
                assert!(cmd.source.is_none());
                assert!(cmd.data_path.is_none());
                assert_eq!(cmd.speed, 0.0);
                assert_eq!(cmd.seed, 42);
            }
            _ => panic!("expected ForwardTest command"),
        }
    }

    #[test]
    fn test_parse_forward_test_full() {
        let cli = Cli::try_parse_from([
            "fx-cli",
            "forward-test",
            "--config",
            "forward.toml",
            "--duration",
            "3600",
            "--strategies",
            "C",
            "--output",
            "/tmp/fw-results",
            "--source",
            "recorded",
            "--data-path",
            "ticks.csv",
            "--speed",
            "2.0",
            "--seed",
            "99",
        ]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::ForwardTest(cmd) => {
                assert_eq!(cmd.config, Some(PathBuf::from("forward.toml")));
                assert_eq!(cmd.duration, Some(3600));
                assert_eq!(cmd.strategies.as_deref(), Some("C"));
                assert_eq!(cmd.output, Some(PathBuf::from("/tmp/fw-results")));
                assert_eq!(cmd.source.as_deref(), Some("recorded"));
                assert_eq!(cmd.data_path, Some(PathBuf::from("ticks.csv")));
                assert_eq!(cmd.speed, 2.0);
                assert_eq!(cmd.seed, 99);
            }
            _ => panic!("expected ForwardTest command"),
        }
    }

    #[test]
    fn test_parse_no_subcommand_fails() {
        let result = Cli::try_parse_from(["fx-cli"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_backtest_missing_data_fails() {
        let result = Cli::try_parse_from(["fx-cli", "backtest"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_version() {
        let result = Cli::try_parse_from(["fx-cli", "--version"]);
        assert!(result.is_err()); // --version causes early exit, which clap reports as error
    }
}
