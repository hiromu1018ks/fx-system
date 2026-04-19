use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::feed::DataSourceConfig;

/// Forward test configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardTestConfig {
    pub enabled_strategies: HashSet<String>,
    pub data_source: DataSourceConfig,
    pub duration: Option<Duration>,
    pub alert_config: AlertConfig,
    pub report_config: ReportConfig,
    pub risk_config: ForwardRiskConfig,
    pub comparison_config: Option<ComparisonConfig>,
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
    pub max_daily_loss: f64,
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
                max_daily_loss: 500.0,
                max_drawdown: 1000.0,
            },
            comparison_config: None,
        }
    }
}
