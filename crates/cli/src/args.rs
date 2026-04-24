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
    /// Validate a backtest result using the Python statistical pipeline.
    Validate(ValidateCmd),
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

    /// Inclusive start timestamp for the backtest period (nanoseconds or RFC3339).
    #[arg(long)]
    pub start_time: Option<String>,

    /// Inclusive end timestamp for the backtest period (nanoseconds or RFC3339).
    #[arg(long)]
    pub end_time: Option<String>,

    /// Optional CSV path to stream regime-input features during backtest.
    #[arg(long)]
    pub dump_features: Option<PathBuf>,

    /// Path to a previously exported Q-state snapshot (.json or .bin) to import.
    #[arg(long)]
    pub import_q_state: Option<PathBuf>,

    /// Path to write the learned Q-state snapshot after the run (.json or .bin).
    #[arg(long)]
    pub export_q_state: Option<PathBuf>,

    /// RNG seed for reproducibility. When omitted, preserves config/default engine behavior.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Disable adaptive policy updates during backtest (frozen evaluation mode).
    #[arg(long)]
    pub no_learn: bool,
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

/// Validate subcommand arguments.
#[derive(Parser, Debug)]
pub struct ValidateCmd {
    /// Path to backtest result JSON file to validate.
    #[arg(short, long)]
    pub backtest_result: PathBuf,

    /// Path to Python interpreter (default: "python3").
    #[arg(long, default_value = "python3")]
    pub python_path: String,

    /// Output directory for validation results.
    #[arg(short, long, default_value = ".")]
    pub output: Option<PathBuf>,

    /// Number of features used by the strategy (overrides JSON value).
    #[arg(long)]
    pub num_features: Option<usize>,
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
                assert!(cmd.start_time.is_none());
                assert!(cmd.end_time.is_none());
                assert!(cmd.dump_features.is_none());
                assert!(cmd.import_q_state.is_none());
                assert!(cmd.export_q_state.is_none());
                assert_eq!(cmd.seed, None);
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
            "--start-time",
            "2024-01-01T00:00:00Z",
            "--end-time",
            "2024-01-01T01:00:00Z",
            "--dump-features",
            "/tmp/features.csv",
            "--import-q-state",
            "/tmp/q-state.json",
            "--export-q-state",
            "/tmp/q-state.bin",
        ]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::Backtest(cmd) => {
                assert_eq!(cmd.data, PathBuf::from("data.csv"));
                assert_eq!(cmd.config, Some(PathBuf::from("config.toml")));
                assert_eq!(cmd.output, Some(PathBuf::from("/tmp/results")));
                assert_eq!(cmd.strategies.as_deref(), Some("A,B"));
                assert_eq!(cmd.start_time.as_deref(), Some("2024-01-01T00:00:00Z"));
                assert_eq!(cmd.end_time.as_deref(), Some("2024-01-01T01:00:00Z"));
                assert_eq!(cmd.dump_features, Some(PathBuf::from("/tmp/features.csv")));
                assert_eq!(cmd.import_q_state, Some(PathBuf::from("/tmp/q-state.json")));
                assert_eq!(cmd.export_q_state, Some(PathBuf::from("/tmp/q-state.bin")));
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

    #[test]
    fn test_parse_validate_minimal() {
        let cli = Cli::try_parse_from(["fx-cli", "validate", "--backtest-result", "result.json"]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::Validate(cmd) => {
                assert_eq!(cmd.backtest_result, PathBuf::from("result.json"));
                assert_eq!(cmd.python_path, "python3");
            }
            _ => panic!("expected Validate command"),
        }
    }

    #[test]
    fn test_parse_validate_full() {
        let cli = Cli::try_parse_from([
            "fx-cli",
            "validate",
            "--backtest-result",
            "backtest.json",
            "--python-path",
            "/usr/bin/python3",
            "--output",
            "/tmp/validation",
            "--num-features",
            "30",
        ]);
        assert!(cli.is_ok());
        let cli = cli.unwrap();
        match cli.command {
            Commands::Validate(cmd) => {
                assert_eq!(cmd.backtest_result, PathBuf::from("backtest.json"));
                assert_eq!(cmd.python_path, "/usr/bin/python3");
                assert_eq!(cmd.output, Some(PathBuf::from("/tmp/validation")));
                assert_eq!(cmd.num_features, Some(30));
            }
            _ => panic!("expected Validate command"),
        }
    }

    #[test]
    fn test_parse_validate_missing_input_fails() {
        let result = Cli::try_parse_from(["fx-cli", "validate"]);
        assert!(result.is_err());
    }
}
