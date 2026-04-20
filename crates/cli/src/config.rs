use anyhow::{Context, Result};
use std::path::Path;

use fx_backtest::engine::BacktestConfig;

/// Load a BacktestConfig from a TOML file.
///
/// The TOML file should contain fields that map to BacktestConfig's structure.
/// Missing fields use defaults.
pub fn load_backtest_config(path: &Path) -> Result<BacktestConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let raw: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config TOML: {}", path.display()))?;

    let mut config = BacktestConfig::default();
    if let Some(table) = raw.as_table() {
        if let Some(v) = table.get("symbol").and_then(|v| v.as_str()) {
            config.symbol = v.to_string();
        }
        if let Some(v) = table
            .get("global_position_limit")
            .and_then(|v| v.as_float())
        {
            config.global_position_limit = v;
        }
        if let Some(v) = table.get("default_lot_size").and_then(|v| v.as_integer()) {
            config.default_lot_size = v as u64;
        }
        if let Some(v) = table.get("replay_speed").and_then(|v| v.as_float()) {
            config.replay_speed = v;
        }
        if let Some(v) = table.get("start_time_ns").and_then(|v| v.as_integer()) {
            config.start_time_ns = v as u64;
        }
        if let Some(v) = table.get("end_time_ns").and_then(|v| v.as_integer()) {
            config.end_time_ns = v as u64;
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_load_backtest_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
symbol = "EUR/USD"
global_position_limit = 5.0
default_lot_size = 50000
replay_speed = 10.0
"#
        )
        .unwrap();

        let config = load_backtest_config(&path).unwrap();
        assert_eq!(config.symbol, "EUR/USD");
        assert!((config.global_position_limit - 5.0).abs() < 1e-10);
        assert_eq!(config.default_lot_size, 50000);
        assert!((config.replay_speed - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_load_backtest_config_defaults_on_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();

        let config = load_backtest_config(&path).unwrap();
        assert_eq!(config.symbol, "USD/JPY");
        assert_eq!(config.default_lot_size, 100_000);
    }

    #[test]
    fn test_load_backtest_config_file_not_found() {
        let result = load_backtest_config(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_backtest_config_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml [[[[").unwrap();
        let result = load_backtest_config(&path);
        assert!(result.is_err());
    }
}
