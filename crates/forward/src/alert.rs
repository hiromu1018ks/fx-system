use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

/// Alert types for the forward test risk monitoring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AlertType {
    RiskLimit,
    ExecutionDrift,
    KillSwitch,
    SharpeDegradation,
    StrategyCulled,
    FeedAnomaly,
}

/// Alert severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

/// An alert generated during forward testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub alert_type: AlertType,
    pub severity: AlertSeverity,
    pub message: String,
    pub timestamp_ns: u64,
}

/// Alert channel trait for sending notifications.
pub trait AlertChannel: Send + Sync {
    fn send_alert(&self, alert: &Alert) -> Result<()>;
}

/// Log-based alert channel using the tracing crate.
pub struct LogAlertChannel;

impl AlertChannel for LogAlertChannel {
    fn send_alert(&self, alert: &Alert) -> Result<()> {
        match alert.severity {
            AlertSeverity::Critical => {
                error!(
                    alert_type = ?alert.alert_type,
                    message = %alert.message,
                    ts = alert.timestamp_ns,
                    "ALERT"
                );
            }
            AlertSeverity::Warning | AlertSeverity::Info => {
                warn!(
                    alert_type = ?alert.alert_type,
                    message = %alert.message,
                    ts = alert.timestamp_ns,
                    "ALERT"
                );
            }
        }
        Ok(())
    }
}

/// Webhook-based alert channel using HTTP POST.
pub struct WebhookAlertChannel {
    url: String,
    client: reqwest::Client,
}

impl WebhookAlertChannel {
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }
}

impl AlertChannel for WebhookAlertChannel {
    fn send_alert(&self, alert: &Alert) -> Result<()> {
        // Fire-and-forget: spawn the HTTP request
        let url = self.url.clone();
        let payload = serde_json::to_value(alert)?;
        let client = self.client.clone();

        tokio::spawn(async move {
            let _ = client.post(&url).json(&payload).send().await;
        });
        Ok(())
    }
}

/// Evaluates alerts based on performance metrics and thresholds.
pub struct AlertEvaluator {
    debounce: HashMap<AlertType, u64>,
    debounce_interval_ns: u64,
    risk_limit_threshold: f64,
    execution_drift_threshold: f64,
    sharpe_degradation_threshold: f64,
}

impl AlertEvaluator {
    pub fn new(
        debounce_interval: Duration,
        risk_limit_threshold: f64,
        execution_drift_threshold: f64,
        sharpe_degradation_threshold: f64,
    ) -> Self {
        Self {
            debounce: HashMap::new(),
            debounce_interval_ns: debounce_interval.as_nanos() as u64,
            risk_limit_threshold,
            execution_drift_threshold,
            sharpe_degradation_threshold,
        }
    }

    /// Check if an alert should be suppressed due to debounce.
    fn should_debounce(&mut self, alert_type: &AlertType, timestamp_ns: u64) -> bool {
        if let Some(&last_ts) = self.debounce.get(alert_type) {
            if timestamp_ns.saturating_sub(last_ts) < self.debounce_interval_ns {
                return true;
            }
        }
        self.debounce.insert(alert_type.clone(), timestamp_ns);
        false
    }

    /// Evaluate risk limit proximity.
    pub fn evaluate_risk_limit(
        &mut self,
        current_loss: f64,
        max_loss: f64,
        timestamp_ns: u64,
    ) -> Option<Alert> {
        if max_loss <= 0.0 {
            return None;
        }
        let ratio = current_loss.abs() / max_loss;
        if ratio >= self.risk_limit_threshold {
            if self.should_debounce(&AlertType::RiskLimit, timestamp_ns) {
                return None;
            }
            let severity = if ratio >= 1.0 {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            };
            Some(Alert {
                alert_type: AlertType::RiskLimit,
                severity,
                message: format!(
                    "Risk limit at {:.1}% (loss={:.2}, max={:.2})",
                    ratio * 100.0,
                    current_loss,
                    max_loss
                ),
                timestamp_ns,
            })
        } else {
            None
        }
    }

    /// Evaluate execution drift anomaly.
    pub fn evaluate_execution_drift(
        &mut self,
        drift_mean: f64,
        drift_std: f64,
        timestamp_ns: u64,
    ) -> Option<Alert> {
        let z_score = if drift_std > f64::EPSILON {
            drift_mean.abs() / drift_std
        } else {
            0.0
        };

        if z_score >= self.execution_drift_threshold {
            if self.should_debounce(&AlertType::ExecutionDrift, timestamp_ns) {
                return None;
            }
            Some(Alert {
                alert_type: AlertType::ExecutionDrift,
                severity: AlertSeverity::Warning,
                message: format!(
                    "Execution drift anomaly: z={:.2} (mean={:.6}, std={:.6})",
                    z_score, drift_mean, drift_std
                ),
                timestamp_ns,
            })
        } else {
            None
        }
    }

    /// Evaluate Sharpe degradation.
    pub fn evaluate_sharpe_degradation(
        &mut self,
        current_sharpe: f64,
        baseline_sharpe: f64,
        timestamp_ns: u64,
    ) -> Option<Alert> {
        if baseline_sharpe <= f64::EPSILON {
            return None;
        }
        let degradation = (baseline_sharpe - current_sharpe) / baseline_sharpe;
        if degradation >= self.sharpe_degradation_threshold {
            if self.should_debounce(&AlertType::SharpeDegradation, timestamp_ns) {
                return None;
            }
            Some(Alert {
                alert_type: AlertType::SharpeDegradation,
                severity: AlertSeverity::Warning,
                message: format!(
                    "Sharpe degradation: {:.1}% (current={:.3}, baseline={:.3})",
                    degradation * 100.0,
                    current_sharpe,
                    baseline_sharpe
                ),
                timestamp_ns,
            })
        } else {
            None
        }
    }

    /// Generate a kill switch alert.
    pub fn kill_switch_alert(&mut self, reason: &str, timestamp_ns: u64) -> Option<Alert> {
        if self.should_debounce(&AlertType::KillSwitch, timestamp_ns) {
            return None;
        }
        Some(Alert {
            alert_type: AlertType::KillSwitch,
            severity: AlertSeverity::Critical,
            message: format!("Kill switch activated: {}", reason),
            timestamp_ns,
        })
    }

    /// Generate a strategy culled alert.
    pub fn strategy_culled_alert(
        &mut self,
        strategy: &str,
        sharpe: f64,
        timestamp_ns: u64,
    ) -> Option<Alert> {
        if self.should_debounce(&AlertType::StrategyCulled, timestamp_ns) {
            return None;
        }
        Some(Alert {
            alert_type: AlertType::StrategyCulled,
            severity: AlertSeverity::Warning,
            message: format!("Strategy {} culled (Sharpe={:.3})", strategy, sharpe),
            timestamp_ns,
        })
    }

    /// Generate a feed anomaly alert.
    pub fn feed_anomaly_alert(&mut self, reason: &str, timestamp_ns: u64) -> Option<Alert> {
        if self.should_debounce(&AlertType::FeedAnomaly, timestamp_ns) {
            return None;
        }
        Some(Alert {
            alert_type: AlertType::FeedAnomaly,
            severity: AlertSeverity::Warning,
            message: format!("Feed anomaly: {}", reason),
            timestamp_ns,
        })
    }
}

/// Alert system that manages multiple channels and evaluation.
pub struct AlertSystem {
    channels: Vec<Box<dyn AlertChannel>>,
    evaluator: AlertEvaluator,
}

impl AlertSystem {
    pub fn new(channels: Vec<Box<dyn AlertChannel>>, evaluator: AlertEvaluator) -> Self {
        Self {
            channels,
            evaluator,
        }
    }

    /// Send an alert to all configured channels.
    pub fn send(&self, alert: &Alert) {
        for channel in &self.channels {
            if let Err(e) = channel.send_alert(alert) {
                warn!("Alert channel error: {}", e);
            }
        }
    }

    /// Get mutable access to the evaluator.
    pub fn evaluator(&mut self) -> &mut AlertEvaluator {
        &mut self.evaluator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_alert_channel() {
        let channel = LogAlertChannel;
        let alert = Alert {
            alert_type: AlertType::RiskLimit,
            severity: AlertSeverity::Warning,
            message: "Test alert".to_string(),
            timestamp_ns: 1000,
        };
        assert!(channel.send_alert(&alert).is_ok());
    }

    #[test]
    fn test_risk_limit_alert() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        // Below threshold — no alert (350/500 = 0.7 < 0.8)
        assert!(eval.evaluate_risk_limit(-350.0, 500.0, 1000).is_none());

        // At threshold — alert
        let alert = eval.evaluate_risk_limit(-450.0, 500.0, 2000).unwrap();
        assert_eq!(alert.alert_type, AlertType::RiskLimit);
        assert_eq!(alert.severity, AlertSeverity::Warning);
    }

    #[test]
    fn test_risk_limit_critical() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        let alert = eval.evaluate_risk_limit(-600.0, 500.0, 1000).unwrap();
        assert_eq!(alert.severity, AlertSeverity::Critical);
    }

    #[test]
    fn test_execution_drift_alert() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        // Normal drift — no alert
        assert!(eval.evaluate_execution_drift(0.001, 0.01, 1000).is_none());

        // Anomalous drift — alert
        let alert = eval.evaluate_execution_drift(0.05, 0.01, 2000).unwrap();
        assert_eq!(alert.alert_type, AlertType::ExecutionDrift);
    }

    #[test]
    fn test_sharpe_degradation_alert() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        // Normal — no alert
        assert!(eval.evaluate_sharpe_degradation(0.9, 1.0, 1000).is_none());

        // Degraded — alert
        let alert = eval.evaluate_sharpe_degradation(0.5, 1.0, 2000).unwrap();
        assert_eq!(alert.alert_type, AlertType::SharpeDegradation);
    }

    #[test]
    fn test_debounce() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(10), 0.8, 2.0, 0.3);
        // First alert fires
        assert!(eval.evaluate_risk_limit(-450.0, 500.0, 1000).is_some());
        // Same time window — debounced
        assert!(eval.evaluate_risk_limit(-450.0, 500.0, 2000).is_none());
        // After debounce window — fires again
        assert!(eval
            .evaluate_risk_limit(-450.0, 500.0, 11_000_000_000)
            .is_some());
    }

    #[test]
    fn test_kill_switch_alert() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        let alert = eval.kill_switch_alert("tick anomaly", 1000).unwrap();
        assert_eq!(alert.alert_type, AlertType::KillSwitch);
        assert_eq!(alert.severity, AlertSeverity::Critical);
    }

    #[test]
    fn test_strategy_culled_alert() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        let alert = eval.strategy_culled_alert("A", -0.5, 1000).unwrap();
        assert_eq!(alert.alert_type, AlertType::StrategyCulled);
    }

    #[test]
    fn test_feed_anomaly_alert() {
        let mut eval = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        let alert = eval.feed_anomaly_alert("gap detected", 1000).unwrap();
        assert_eq!(alert.alert_type, AlertType::FeedAnomaly);
    }

    #[test]
    fn test_alert_system_send() {
        let channels: Vec<Box<dyn AlertChannel>> = vec![Box::new(LogAlertChannel)];
        let evaluator = AlertEvaluator::new(Duration::from_secs(1), 0.8, 2.0, 0.3);
        let system = AlertSystem::new(channels, evaluator);

        let alert = Alert {
            alert_type: AlertType::FeedAnomaly,
            severity: AlertSeverity::Info,
            message: "test".to_string(),
            timestamp_ns: 0,
        };
        system.send(&alert); // Should not panic
    }
}
