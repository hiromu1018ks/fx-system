use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::feed::DataSourceConfig;
use fx_risk::limits::RiskLimitsConfig;
use fx_strategy::regime::RegimeConfig;

/// Forward test configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardTestConfig {
    pub enabled_strategies: HashSet<String>,
    pub data_source: DataSourceConfig,
    #[serde(with = "duration_opt", default)]
    pub duration: Option<Duration>,
    pub alert_config: AlertConfig,
    pub report_config: ReportConfig,
    pub risk_config: ForwardRiskConfig,
    pub comparison_config: Option<ComparisonConfig>,
    /// Regime detection configuration. If model_path is Some, ONNX inference is used.
    #[serde(default)]
    pub regime_config: RegimeConfig,
}

/// Alert configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertConfig {
    pub channels: Vec<AlertChannelConfig>,
    pub risk_limit_threshold: f64,
    pub execution_drift_threshold: f64,
    pub sharpe_degradation_threshold: f64,
}

/// Alert channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertChannelConfig {
    Log,
    Webhook { url: String },
}

/// Report configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportConfig {
    pub output_dir: String,
    pub format: ReportFormat,
    #[serde(with = "duration_opt", default)]
    pub interval: Option<Duration>,
}

/// Report output format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReportFormat {
    Json,
    Csv,
    Both,
}

/// Forward test risk configuration (wraps existing risk settings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardRiskConfig {
    pub max_position_lots: f64,
    /// Absolute MTM loss magnitude that triggers stage-1 daily warning logic.
    pub max_daily_loss_mtm: f64,
    /// Absolute realized loss magnitude that triggers daily hard stop.
    pub max_daily_loss_realized: f64,
    /// Absolute realized weekly loss magnitude.
    pub max_weekly_loss: f64,
    /// Absolute realized monthly loss magnitude.
    pub max_monthly_loss: f64,
    /// Lot fraction to apply while MTM warning is active.
    pub daily_mtm_lot_fraction: f64,
    /// Minimum |Q| required while MTM warning is active.
    pub daily_mtm_q_threshold: f64,
    pub max_drawdown: f64,
}

/// Backtest comparison configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonConfig {
    pub backtest_results_path: String,
    pub metrics: Vec<String>,
}

impl Default for ForwardTestConfig {
    fn default() -> Self {
        Self {
            enabled_strategies: HashSet::from(["A".to_string(), "B".to_string(), "C".to_string()]),
            data_source: DataSourceConfig::Recorded {
                event_store_path: String::new(),
                speed: 1.0,
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
            regime_config: RegimeConfig::default(),
        }
    }
}

impl ForwardTestConfig {
    /// Load configuration from a TOML file.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config TOML: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from a TOML string.
    pub fn load_from_str(content: &str) -> Result<Self> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        for strategy in &self.enabled_strategies {
            if !["A", "B", "C"].contains(&strategy.as_str()) {
                anyhow::bail!("Invalid strategy name: '{}'. Must be A, B, or C.", strategy);
            }
        }

        if self.alert_config.risk_limit_threshold <= 0.0
            || self.alert_config.risk_limit_threshold > 1.0
        {
            anyhow::bail!(
                "risk_limit_threshold must be in (0, 1], got {}",
                self.alert_config.risk_limit_threshold
            );
        }

        if self.alert_config.execution_drift_threshold <= 0.0 {
            anyhow::bail!(
                "execution_drift_threshold must be positive, got {}",
                self.alert_config.execution_drift_threshold
            );
        }

        if self.risk_config.max_position_lots <= 0.0 {
            anyhow::bail!(
                "max_position_lots must be positive, got {}",
                self.risk_config.max_position_lots
            );
        }

        if self.risk_config.max_daily_loss_mtm <= 0.0 {
            anyhow::bail!(
                "max_daily_loss_mtm must be positive, got {}",
                self.risk_config.max_daily_loss_mtm
            );
        }

        if self.risk_config.max_daily_loss_realized <= 0.0 {
            anyhow::bail!(
                "max_daily_loss_realized must be positive, got {}",
                self.risk_config.max_daily_loss_realized
            );
        }

        if self.risk_config.max_weekly_loss <= 0.0 {
            anyhow::bail!(
                "max_weekly_loss must be positive, got {}",
                self.risk_config.max_weekly_loss
            );
        }

        if self.risk_config.max_monthly_loss <= 0.0 {
            anyhow::bail!(
                "max_monthly_loss must be positive, got {}",
                self.risk_config.max_monthly_loss
            );
        }

        if self.risk_config.daily_mtm_lot_fraction <= 0.0
            || self.risk_config.daily_mtm_lot_fraction > 1.0
        {
            anyhow::bail!(
                "daily_mtm_lot_fraction must be in (0, 1], got {}",
                self.risk_config.daily_mtm_lot_fraction
            );
        }

        if self.risk_config.daily_mtm_q_threshold < 0.0 {
            anyhow::bail!(
                "daily_mtm_q_threshold must be >= 0, got {}",
                self.risk_config.daily_mtm_q_threshold
            );
        }

        if let Some(duration) = self.duration {
            if duration.as_secs() == 0 {
                anyhow::bail!("duration must be > 0 if specified");
            }
        }

        match &self.data_source {
            DataSourceConfig::Recorded { speed, .. } => {
                if *speed < 0.0 {
                    anyhow::bail!("Recorded data speed must be >= 0, got {}", speed);
                }
            }
            DataSourceConfig::ExternalApi {
                provider,
                credentials_path,
                symbols,
            } => {
                if provider.is_empty() {
                    anyhow::bail!("External API provider must not be empty");
                }
                if credentials_path.is_empty() {
                    anyhow::bail!("External API credentials_path must not be empty");
                }
                if symbols.is_empty() {
                    anyhow::bail!("External API symbols must not be empty");
                }
            }
        }

        for channel in &self.alert_config.channels {
            if let AlertChannelConfig::Webhook { url } = channel {
                if url.is_empty() {
                    anyhow::bail!("Webhook URL must not be empty");
                }
            }
        }

        Ok(())
    }

    /// Check if a strategy is enabled.
    pub fn is_strategy_enabled(&self, strategy: &str) -> bool {
        self.enabled_strategies.contains(strategy)
    }
}

impl ForwardRiskConfig {
    pub fn to_risk_limits_config(&self) -> RiskLimitsConfig {
        RiskLimitsConfig {
            max_daily_loss_mtm: -self.max_daily_loss_mtm.abs(),
            max_daily_loss_realized: -self.max_daily_loss_realized.abs(),
            max_weekly_loss: -self.max_weekly_loss.abs(),
            max_monthly_loss: -self.max_monthly_loss.abs(),
            daily_mtm_lot_fraction: self.daily_mtm_lot_fraction,
            daily_mtm_q_threshold: self.daily_mtm_q_threshold,
        }
    }
}

mod duration_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(d) => serializer.serialize_some(&d.as_secs()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<u64> = Option::deserialize(deserializer)?;
        Ok(opt.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ForwardTestConfig::default();
        assert!(config.is_strategy_enabled("A"));
        assert!(config.is_strategy_enabled("B"));
        assert!(config.is_strategy_enabled("C"));
        assert!(!config.is_strategy_enabled("D"));
        assert!(config.duration.is_none());
        assert!(config.comparison_config.is_none());
    }

    #[test]
    fn test_default_config_validates() {
        let config = ForwardTestConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_toml_roundtrip() {
        let config = ForwardTestConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: ForwardTestConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.is_strategy_enabled("A"));
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn test_load_from_toml_string() {
        let toml_str = r#"
enabled_strategies = ["A", "B"]

[data_source]
Recorded = { event_store_path = "/data/store", speed = 2.0 }

[alert_config]
channels = ["Log"]
risk_limit_threshold = 0.9
execution_drift_threshold = 1.5
sharpe_degradation_threshold = 0.2

[report_config]
output_dir = "./output"
format = "Json"

[risk_config]
max_position_lots = 5.0
max_daily_loss_mtm = 300.0
max_daily_loss_realized = 600.0
max_weekly_loss = 1500.0
max_monthly_loss = 3000.0
daily_mtm_lot_fraction = 0.25
daily_mtm_q_threshold = 0.01
max_drawdown = 800.0
"#;
        let config = ForwardTestConfig::load_from_str(toml_str).unwrap();
        assert!(config.is_strategy_enabled("A"));
        assert!(config.is_strategy_enabled("B"));
        assert!(!config.is_strategy_enabled("C"));
    }

    #[test]
    fn test_load_from_toml_with_webhook() {
        let toml_str = r#"
enabled_strategies = ["A"]

[data_source]
Recorded = { event_store_path = "/data", speed = 1.0 }

[alert_config]
channels = ["Log", { Webhook = { url = "https://hooks.slack.com/test" } }]
risk_limit_threshold = 0.8
execution_drift_threshold = 2.0
sharpe_degradation_threshold = 0.3

[report_config]
output_dir = "./reports"
format = "Both"

[risk_config]
max_position_lots = 10.0
max_daily_loss_mtm = 500.0
max_daily_loss_realized = 1000.0
max_weekly_loss = 2500.0
max_monthly_loss = 5000.0
daily_mtm_lot_fraction = 0.25
daily_mtm_q_threshold = 0.01
max_drawdown = 1000.0
"#;
        let config = ForwardTestConfig::load_from_str(toml_str).unwrap();
        assert_eq!(config.alert_config.channels.len(), 2);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_invalid_strategy() {
        let mut config = ForwardTestConfig::default();
        config.enabled_strategies.insert("X".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_invalid_threshold() {
        let mut config = ForwardTestConfig::default();
        config.alert_config.risk_limit_threshold = 1.5;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_negative_speed() {
        let mut config = ForwardTestConfig::default();
        config.data_source = DataSourceConfig::Recorded {
            event_store_path: "/data".to_string(),
            speed: -1.0,
            start_time: None,
            end_time: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_zero_position_lots() {
        let mut config = ForwardTestConfig::default();
        config.risk_config.max_position_lots = 0.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_load_from_file_not_found() {
        let result = ForwardTestConfig::load_from_file(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_from_file_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml [[[[").unwrap();
        let result = ForwardTestConfig::load_from_file(&path);
        assert!(result.is_err());
    }
}
