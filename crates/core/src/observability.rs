use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Pre-Failure Signature Metrics (§8.2)
// ---------------------------------------------------------------------------

/// Snapshot of all 19 pre-failure signature metrics from §8.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreFailureMetrics {
    pub rolling_variance_latency: f64,
    pub feature_distribution_kl_divergence: f64,
    pub q_value_adjustment_frequency: f64,
    pub execution_drift_trend: f64,
    pub latency_risk_trend: f64,
    pub self_impact_ratio: f64,
    pub liquidity_evolvement: f64,
    pub policy_entropy: f64,
    pub regime_posterior_entropy: f64,
    pub hidden_liquidity_sigma: f64,
    pub position_constraint_saturation_rate: f64,
    pub last_look_rejection_rate: f64,
    pub dynamic_cost_estimate_error: f64,
    pub lp_adversarial_score: f64,
    pub daily_pnl_vs_limit: f64,
    pub weekly_pnl_vs_limit: f64,
    pub monthly_pnl_vs_limit: f64,
    pub lp_recalibration_progress: f64,
    pub bayesian_posterior_drift: f64,
}

impl Default for PreFailureMetrics {
    fn default() -> Self {
        Self {
            rolling_variance_latency: 0.0,
            feature_distribution_kl_divergence: 0.0,
            q_value_adjustment_frequency: 0.0,
            execution_drift_trend: 0.0,
            latency_risk_trend: 0.0,
            self_impact_ratio: 0.0,
            liquidity_evolvement: 0.0,
            policy_entropy: 0.0,
            regime_posterior_entropy: 0.0,
            hidden_liquidity_sigma: 0.0,
            position_constraint_saturation_rate: 0.0,
            last_look_rejection_rate: 0.0,
            dynamic_cost_estimate_error: 0.0,
            lp_adversarial_score: 0.0,
            daily_pnl_vs_limit: 0.0,
            weekly_pnl_vs_limit: 0.0,
            monthly_pnl_vs_limit: 0.0,
            lp_recalibration_progress: 0.0,
            bayesian_posterior_drift: 0.0,
        }
    }
}

impl PreFailureMetrics {
    /// Collect metric values into an array for bulk threshold checking.
    pub fn as_slice(&self) -> [f64; 19] {
        [
            self.rolling_variance_latency,
            self.feature_distribution_kl_divergence,
            self.q_value_adjustment_frequency,
            self.execution_drift_trend,
            self.latency_risk_trend,
            self.self_impact_ratio,
            self.liquidity_evolvement,
            self.policy_entropy,
            self.regime_posterior_entropy,
            self.hidden_liquidity_sigma,
            self.position_constraint_saturation_rate,
            self.last_look_rejection_rate,
            self.dynamic_cost_estimate_error,
            self.lp_adversarial_score,
            self.daily_pnl_vs_limit,
            self.weekly_pnl_vs_limit,
            self.monthly_pnl_vs_limit,
            self.lp_recalibration_progress,
            self.bayesian_posterior_drift,
        ]
    }

    /// Metric field names, ordered to match `as_slice()`.
    pub const NAMES: &'static [&'static str] = &[
        "rolling_variance_latency",
        "feature_distribution_kl_divergence",
        "q_value_adjustment_frequency",
        "execution_drift_trend",
        "latency_risk_trend",
        "self_impact_ratio",
        "liquidity_evolvement",
        "policy_entropy",
        "regime_posterior_entropy",
        "hidden_liquidity_sigma",
        "position_constraint_saturation_rate",
        "last_look_rejection_rate",
        "dynamic_cost_estimate_error",
        "lp_adversarial_score",
        "daily_pnl_vs_limit",
        "weekly_pnl_vs_limit",
        "monthly_pnl_vs_limit",
        "lp_recalibration_progress",
        "bayesian_posterior_drift",
    ];
}

// ---------------------------------------------------------------------------
// Online rolling statistics
// ---------------------------------------------------------------------------

/// Rolling mean/variance over a fixed-size sliding window.
/// Uses exact recomputation from the window buffer for numerical stability.
#[derive(Debug, Clone)]
pub struct RollingStats {
    window: VecDeque<f64>,
    max_window: usize,
}

impl RollingStats {
    pub fn new(max_window: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(max_window),
            max_window,
        }
    }

    pub fn count(&self) -> u64 {
        self.window.len() as u64
    }

    pub fn mean(&self) -> f64 {
        if self.window.is_empty() {
            return 0.0;
        }
        self.window.iter().sum::<f64>() / (self.window.len() as f64)
    }

    pub fn variance(&self) -> f64 {
        let n = self.window.len();
        if n < 2 {
            return 0.0;
        }
        let m = self.mean();
        let sum_sq: f64 = self.window.iter().map(|&x| (x - m).powi(2)).sum();
        sum_sq / (n as f64)
    }

    pub fn std(&self) -> f64 {
        self.variance().sqrt()
    }

    pub fn update(&mut self, x: f64) {
        if self.window.len() >= self.max_window {
            self.window.pop_front();
        }
        self.window.push_back(x);
    }

    pub fn reset(&mut self) {
        self.window.clear();
    }
}

// ---------------------------------------------------------------------------
// Anomaly alert
// ---------------------------------------------------------------------------

/// Severity of an anomaly detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertSeverity {
    Warning,
    Critical,
}

/// A single anomaly alert produced by the AnomalyDetector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyAlert {
    pub metric_name: String,
    pub value: f64,
    pub threshold: f64,
    pub severity: AlertSeverity,
    pub timestamp_ns: u64,
}

// ---------------------------------------------------------------------------
// Anomaly detector configuration
// ---------------------------------------------------------------------------

/// Threshold configuration for each pre-failure signature metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyThreshold {
    pub warning: f64,
    pub critical: f64,
}

/// Per-metric thresholds. Metrics not configured here are not monitored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyConfig {
    pub thresholds: Vec<(String, AnomalyThreshold)>,
    /// Number of consecutive anomaly ticks before alert fires (debounce).
    pub debounce_ticks: u32,
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self {
            thresholds: vec![
                // Model collapse indicators
                (
                    "q_value_adjustment_frequency".into(),
                    AnomalyThreshold {
                        warning: 0.8,
                        critical: 0.95,
                    },
                ),
                (
                    "bayesian_posterior_drift".into(),
                    AnomalyThreshold {
                        warning: 2.0,
                        critical: 5.0,
                    },
                ),
                (
                    "feature_distribution_kl_divergence".into(),
                    AnomalyThreshold {
                        warning: 0.5,
                        critical: 1.0,
                    },
                ),
                // Execution quality
                (
                    "last_look_rejection_rate".into(),
                    AnomalyThreshold {
                        warning: 0.3,
                        critical: 0.5,
                    },
                ),
                (
                    "lp_adversarial_score".into(),
                    AnomalyThreshold {
                        warning: 0.5,
                        critical: 0.8,
                    },
                ),
                (
                    "dynamic_cost_estimate_error".into(),
                    AnomalyThreshold {
                        warning: 0.5,
                        critical: 1.0,
                    },
                ),
                // Latency
                (
                    "rolling_variance_latency".into(),
                    AnomalyThreshold {
                        warning: 5.0,
                        critical: 20.0,
                    },
                ),
                (
                    "latency_risk_trend".into(),
                    AnomalyThreshold {
                        warning: 2.0,
                        critical: 5.0,
                    },
                ),
                // Policy / regime
                (
                    "policy_entropy".into(),
                    AnomalyThreshold {
                        warning: 2.0,
                        critical: 2.5,
                    },
                ),
                (
                    "regime_posterior_entropy".into(),
                    AnomalyThreshold {
                        warning: 1.5,
                        critical: 1.8,
                    },
                ),
                // Liquidity
                (
                    "liquidity_evolvement".into(),
                    AnomalyThreshold {
                        warning: 3.0,
                        critical: 5.0,
                    },
                ),
                (
                    "hidden_liquidity_sigma".into(),
                    AnomalyThreshold {
                        warning: 0.1,
                        critical: 0.2,
                    },
                ),
                (
                    "self_impact_ratio".into(),
                    AnomalyThreshold {
                        warning: 0.5,
                        critical: 0.8,
                    },
                ),
                // Risk limits (absolute value checked)
                (
                    "daily_pnl_vs_limit".into(),
                    AnomalyThreshold {
                        warning: 0.6,
                        critical: 0.8,
                    },
                ),
                (
                    "weekly_pnl_vs_limit".into(),
                    AnomalyThreshold {
                        warning: 0.6,
                        critical: 0.8,
                    },
                ),
                (
                    "monthly_pnl_vs_limit".into(),
                    AnomalyThreshold {
                        warning: 0.6,
                        critical: 0.8,
                    },
                ),
                // Position constraints
                (
                    "position_constraint_saturation_rate".into(),
                    AnomalyThreshold {
                        warning: 0.7,
                        critical: 0.9,
                    },
                ),
            ],
            debounce_ticks: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Anomaly detector
// ---------------------------------------------------------------------------

/// Threshold-based anomaly detector for pre-failure signature metrics.
pub struct AnomalyDetector {
    config: AnomalyConfig,
    consecutive_counts: Vec<(String, u32)>,
    rolling: Vec<(String, RollingStats)>,
}

impl AnomalyDetector {
    pub fn new(config: AnomalyConfig) -> Self {
        let rolling = config
            .thresholds
            .iter()
            .map(|(name, _)| (name.clone(), RollingStats::new(100)))
            .collect();
        Self {
            config,
            consecutive_counts: Vec::new(),
            rolling,
        }
    }

    /// Evaluate a metrics snapshot and return alerts for breached thresholds.
    pub fn evaluate(
        &mut self,
        metrics: &PreFailureMetrics,
        timestamp_ns: u64,
    ) -> Vec<AnomalyAlert> {
        let values = metrics.as_slice();
        let names = PreFailureMetrics::NAMES;
        let mut alerts = Vec::new();

        for (metric_name, threshold) in &self.config.thresholds {
            let idx = match names.iter().position(|&n| n == metric_name.as_str()) {
                Some(i) => i,
                None => continue,
            };
            let value = values[idx];

            // Update rolling stats with absolute value
            if let Some(entry) = self.rolling.iter_mut().find(|(n, _)| n == metric_name) {
                entry.1.update(value.abs());
            }

            let severity = if value >= threshold.critical {
                Some(AlertSeverity::Critical)
            } else if value >= threshold.warning {
                Some(AlertSeverity::Warning)
            } else {
                // Also check absolute value for metrics like PnL ratios that can be negative
                let abs_val = value.abs();
                if abs_val >= threshold.critical {
                    Some(AlertSeverity::Critical)
                } else if abs_val >= threshold.warning {
                    Some(AlertSeverity::Warning)
                } else {
                    None
                }
            };

            // Debounce logic: increment or create, then check threshold
            if let Some(sev) = severity {
                let entry = self
                    .consecutive_counts
                    .iter_mut()
                    .find(|(n, _)| n == metric_name);

                let count = match entry {
                    Some((_, cnt)) => {
                        *cnt += 1;
                        *cnt
                    }
                    None => {
                        self.consecutive_counts.push((metric_name.clone(), 1));
                        1
                    }
                };

                if count >= self.config.debounce_ticks {
                    alerts.push(AnomalyAlert {
                        metric_name: metric_name.clone(),
                        value: value.abs(),
                        threshold: if sev == AlertSeverity::Critical {
                            threshold.critical
                        } else {
                            threshold.warning
                        },
                        severity: sev,
                        timestamp_ns,
                    });
                }
            } else {
                // Reset consecutive count on non-anomaly tick
                if let Some((_, cnt)) = self
                    .consecutive_counts
                    .iter_mut()
                    .find(|(n, _)| n == metric_name)
                {
                    *cnt = 0;
                }
            }
        }

        alerts
    }

    /// Get rolling statistics for a specific metric.
    pub fn rolling_stats(&self, metric_name: &str) -> Option<&RollingStats> {
        self.rolling
            .iter()
            .find(|(n, _)| n == metric_name)
            .map(|(_, s)| s)
    }

    /// Get the list of configured metric names.
    pub fn monitored_metrics(&self) -> Vec<&str> {
        self.config
            .thresholds
            .iter()
            .map(|(n, _)| n.as_str())
            .collect()
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.consecutive_counts.clear();
        for (_, stats) in &mut self.rolling {
            stats.reset();
        }
    }
}

// ---------------------------------------------------------------------------
// Observability manager (main interface)
// ---------------------------------------------------------------------------

/// Central observability manager that receives pre-failure signature metrics
/// and runs anomaly detection on each tick.
pub struct ObservabilityManager {
    detector: AnomalyDetector,
    last_metrics: PreFailureMetrics,
    last_alerts: Vec<AnomalyAlert>,
    total_ticks: u64,
    total_critical_alerts: u64,
    total_warning_alerts: u64,
}

impl ObservabilityManager {
    pub fn new(config: AnomalyConfig) -> Self {
        Self {
            detector: AnomalyDetector::new(config),
            last_metrics: PreFailureMetrics::default(),
            last_alerts: Vec::new(),
            total_ticks: 0,
            total_critical_alerts: 0,
            total_warning_alerts: 0,
        }
    }

    /// Feed a new metrics snapshot, run anomaly detection, and log via tracing.
    pub fn tick(&mut self, metrics: PreFailureMetrics, timestamp_ns: u64) -> Vec<AnomalyAlert> {
        self.total_ticks += 1;
        self.last_metrics = metrics.clone();

        Self::log_metrics(&metrics);

        let alerts = self.detector.evaluate(&metrics, timestamp_ns);
        for alert in &alerts {
            match alert.severity {
                AlertSeverity::Critical => {
                    self.total_critical_alerts += 1;
                    tracing::error!(
                        metric = %alert.metric_name,
                        value = alert.value,
                        threshold = alert.threshold,
                        "PRE-FAILURE SIGNATURE: CRITICAL anomaly detected"
                    );
                }
                AlertSeverity::Warning => {
                    self.total_warning_alerts += 1;
                    tracing::warn!(
                        metric = %alert.metric_name,
                        value = alert.value,
                        threshold = alert.threshold,
                        "PRE-FAILURE SIGNATURE: Warning anomaly detected"
                    );
                }
            }
        }

        self.last_alerts = alerts.clone();
        alerts
    }

    /// Access the most recent metrics snapshot.
    pub fn last_metrics(&self) -> &PreFailureMetrics {
        &self.last_metrics
    }

    /// Access the most recent alerts.
    pub fn last_alerts(&self) -> &[AnomalyAlert] {
        &self.last_alerts
    }

    /// Total tick count.
    pub fn total_ticks(&self) -> u64 {
        self.total_ticks
    }

    /// Total critical alerts fired.
    pub fn total_critical_alerts(&self) -> u64 {
        self.total_critical_alerts
    }

    /// Total warning alerts fired.
    pub fn total_warning_alerts(&self) -> u64 {
        self.total_warning_alerts
    }

    /// Get a reference to the underlying anomaly detector for rolling stats.
    pub fn detector(&self) -> &AnomalyDetector {
        &self.detector
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.last_metrics = PreFailureMetrics::default();
        self.last_alerts.clear();
        self.total_ticks = 0;
        self.total_critical_alerts = 0;
        self.total_warning_alerts = 0;
    }

    fn log_metrics(m: &PreFailureMetrics) {
        tracing::info!(
            rolling_var_latency = m.rolling_variance_latency,
            feature_kl = m.feature_distribution_kl_divergence,
            q_adj_freq = m.q_value_adjustment_frequency,
            exec_drift = m.execution_drift_trend,
            latency_risk = m.latency_risk_trend,
            self_impact = m.self_impact_ratio,
            liquidity_evol = m.liquidity_evolvement,
            policy_entropy = m.policy_entropy,
            regime_entropy = m.regime_posterior_entropy,
            hidden_liq_sigma = m.hidden_liquidity_sigma,
            pos_sat_rate = m.position_constraint_saturation_rate,
            ll_reject = m.last_look_rejection_rate,
            dyn_cost_err = m.dynamic_cost_estimate_error,
            lp_adv_score = m.lp_adversarial_score,
            daily_pnl_ratio = m.daily_pnl_vs_limit,
            weekly_pnl_ratio = m.weekly_pnl_vs_limit,
            monthly_pnl_ratio = m.monthly_pnl_vs_limit,
            lp_recal_prog = m.lp_recalibration_progress,
            bayes_drift = m.bayesian_posterior_drift,
            "pre-failure metrics snapshot"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- PreFailureMetrics ---------------------------------------------------

    #[test]
    fn default_metrics_all_zero() {
        let m = PreFailureMetrics::default();
        let slice = m.as_slice();
        for &v in &slice {
            assert!((v - 0.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn as_slice_length_matches_names() {
        let m = PreFailureMetrics::default();
        assert_eq!(m.as_slice().len(), PreFailureMetrics::NAMES.len());
    }

    #[test]
    fn names_match_field_count() {
        assert_eq!(PreFailureMetrics::NAMES.len(), 19);
    }

    #[test]
    fn metrics_serialize_deserialize() {
        let mut m = PreFailureMetrics::default();
        m.rolling_variance_latency = 1.5;
        m.policy_entropy = 2.3;
        let json = serde_json::to_string(&m).unwrap();
        let m2: PreFailureMetrics = serde_json::from_str(&json).unwrap();
        assert!((m2.rolling_variance_latency - 1.5).abs() < f64::EPSILON);
        assert!((m2.policy_entropy - 2.3).abs() < f64::EPSILON);
    }

    // -- RollingStats --------------------------------------------------------

    #[test]
    fn rolling_stats_single_value() {
        let mut rs = RollingStats::new(10);
        rs.update(5.0);
        assert_eq!(rs.count(), 1);
        assert!((rs.mean() - 5.0).abs() < f64::EPSILON);
        assert!((rs.variance() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rolling_stats_two_values() {
        let mut rs = RollingStats::new(10);
        rs.update(2.0);
        rs.update(4.0);
        assert_eq!(rs.count(), 2);
        assert!((rs.mean() - 3.0).abs() < f64::EPSILON);
        assert!((rs.variance() - 1.0).abs() < 1e-10);
        assert!((rs.std() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn rolling_stats_window_trimming() {
        let mut rs = RollingStats::new(3);
        rs.update(1.0);
        rs.update(2.0);
        rs.update(3.0);
        assert_eq!(rs.count(), 3);
        assert!((rs.mean() - 2.0).abs() < 1e-10);
        // Adding 4th evicts 1st → window = [2, 3, 10]
        rs.update(10.0);
        assert_eq!(rs.count(), 3);
        assert!((rs.mean() - 5.0).abs() < 1e-10);
    }

    #[test]
    fn rolling_stats_reset() {
        let mut rs = RollingStats::new(10);
        rs.update(1.0);
        rs.update(2.0);
        rs.reset();
        assert_eq!(rs.count(), 0);
        assert!((rs.mean() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rolling_stats_variance_stability() {
        let mut rs = RollingStats::new(100);
        for i in 0..50 {
            rs.update(i as f64);
        }
        // variance of 0..49 = (50^2 - 1)/12 ≈ 208.25
        let v = rs.variance();
        assert!(v > 200.0 && v < 210.0);
    }

    #[test]
    fn rolling_stats_zero_window() {
        let mut rs = RollingStats::new(1);
        rs.update(5.0);
        assert_eq!(rs.count(), 1);
        rs.update(10.0);
        assert_eq!(rs.count(), 1);
        assert!((rs.mean() - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rolling_stats_large_window() {
        let mut rs = RollingStats::new(1000);
        for i in 0..500 {
            rs.update(i as f64);
        }
        assert_eq!(rs.count(), 500);
        assert!((rs.mean() - 249.5).abs() < 1e-8);
    }

    #[test]
    fn rolling_stats_empty_mean() {
        let rs = RollingStats::new(10);
        assert_eq!(rs.count(), 0);
        assert!((rs.mean() - 0.0).abs() < f64::EPSILON);
        assert!((rs.variance() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rolling_stats_variance_after_trim() {
        let mut rs = RollingStats::new(3);
        rs.update(0.0);
        rs.update(3.0);
        rs.update(6.0);
        // var = ((0-3)^2 + (3-3)^2 + (6-3)^2)/3 = 18/3 = 6
        assert!((rs.variance() - 6.0).abs() < 1e-10);
        rs.update(9.0);
        // window = [3, 6, 9], mean=6, var = ((3-6)^2 + (6-6)^2 + (9-6)^2)/3 = 18/3 = 6
        assert!((rs.variance() - 6.0).abs() < 1e-10);
    }

    // -- AnomalyAlert --------------------------------------------------------

    #[test]
    fn alert_fields() {
        let alert = AnomalyAlert {
            metric_name: "test_metric".into(),
            value: 5.0,
            threshold: 3.0,
            severity: AlertSeverity::Critical,
            timestamp_ns: 1000,
        };
        assert_eq!(alert.metric_name, "test_metric");
        assert!((alert.value - 5.0).abs() < f64::EPSILON);
        assert_eq!(alert.severity, AlertSeverity::Critical);
        assert_eq!(alert.timestamp_ns, 1000);
    }

    #[test]
    fn alert_serde() {
        let alert = AnomalyAlert {
            metric_name: "test".into(),
            value: 1.0,
            threshold: 0.5,
            severity: AlertSeverity::Warning,
            timestamp_ns: 42,
        };
        let json = serde_json::to_string(&alert).unwrap();
        let a2: AnomalyAlert = serde_json::from_str(&json).unwrap();
        assert_eq!(a2.metric_name, alert.metric_name);
        assert_eq!(a2.severity, alert.severity);
    }

    // -- AnomalyConfig -------------------------------------------------------

    #[test]
    fn default_config_has_thresholds() {
        let config = AnomalyConfig::default();
        assert!(!config.thresholds.is_empty());
        assert!(config.debounce_ticks > 0);
    }

    #[test]
    fn default_config_covers_all_named_metrics() {
        let config = AnomalyConfig::default();
        let configured: Vec<_> = config.thresholds.iter().map(|(n, _)| n.as_str()).collect();
        for name in PreFailureMetrics::NAMES {
            if *name == "lp_recalibration_progress" || *name == "execution_drift_trend" {
                continue;
            }
            assert!(
                configured.contains(&name),
                "metric {name} not configured in default AnomalyConfig"
            );
        }
    }

    // -- AnomalyDetector -----------------------------------------------------

    fn normal_metrics() -> PreFailureMetrics {
        PreFailureMetrics::default()
    }

    #[test]
    fn detector_no_alert_on_normal() {
        let config = AnomalyConfig::default();
        let mut det = AnomalyDetector::new(config);
        let alerts = det.evaluate(&normal_metrics(), 1000);
        assert!(alerts.is_empty());
    }

    #[test]
    fn detector_alerts_on_critical_after_debounce() {
        let debounce = AnomalyConfig::default().debounce_ticks;
        let config = AnomalyConfig::default();
        let mut det = AnomalyDetector::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 3.0; // critical > 2.5

        // Need debounce_ticks consecutive breaches to fire
        for i in 0..debounce {
            let alerts = det.evaluate(&metrics, 1000 + i as u64);
            if i < debounce - 1 {
                assert!(alerts.is_empty(), "should debounce before tick {}", i + 1);
            } else {
                // Last tick in debounce window should fire
                assert!(!alerts.is_empty(), "should fire at tick {}", i + 1);
                assert_eq!(alerts[0].severity, AlertSeverity::Critical);
            }
        }
    }

    #[test]
    fn detector_warning_level() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 2.1; // warning > 2.0, critical > 2.5

        let alerts = det.evaluate(&metrics, 1000);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);
    }

    #[test]
    fn detector_resets_consecutive_on_normal() {
        let config = AnomalyConfig {
            debounce_ticks: 2,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut bad = PreFailureMetrics::default();
        bad.policy_entropy = 3.0;

        // One bad tick (count=1, < debounce)
        det.evaluate(&bad, 1000);

        // One good tick — resets counter to 0
        det.evaluate(&normal_metrics(), 1001);

        // Another bad tick — count goes to 1, still < debounce
        let alerts = det.evaluate(&bad, 1002);
        assert!(alerts.is_empty());
    }

    #[test]
    fn detector_multiple_anomalies() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 3.0;
        metrics.rolling_variance_latency = 25.0;

        let alerts = det.evaluate(&metrics, 1000);
        assert!(alerts.len() >= 2);
        let names: Vec<_> = alerts.iter().map(|a| a.metric_name.as_str()).collect();
        assert!(names.contains(&"policy_entropy"));
        assert!(names.contains(&"rolling_variance_latency"));
    }

    #[test]
    fn detector_monitored_metrics() {
        let config = AnomalyConfig::default();
        let det = AnomalyDetector::new(config);
        let monitored = det.monitored_metrics();
        assert!(!monitored.is_empty());
        assert!(monitored.contains(&"policy_entropy"));
    }

    #[test]
    fn detector_rolling_stats() {
        let config = AnomalyConfig::default();
        let mut det = AnomalyDetector::new(config);
        let metrics = normal_metrics();
        det.evaluate(&metrics, 1000);
        let stats = det.rolling_stats("policy_entropy");
        assert!(stats.is_some());
        assert_eq!(stats.unwrap().count(), 1);
    }

    #[test]
    fn detector_rolling_stats_unknown() {
        let config = AnomalyConfig::default();
        let det = AnomalyDetector::new(config);
        assert!(det.rolling_stats("nonexistent_metric").is_none());
    }

    #[test]
    fn detector_reset() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 3.0;
        det.evaluate(&metrics, 1000);
        assert!(!det.evaluate(&metrics, 1001).is_empty());

        det.reset();

        // After reset, consecutive counts cleared — needs debounce again
        let alerts = det.evaluate(&metrics, 2000);
        // With debounce_ticks=1, count becomes 1 which >= 1, so fires immediately
        assert!(!alerts.is_empty());
    }

    #[test]
    fn detector_absolute_value_check() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.daily_pnl_vs_limit = -0.9; // abs(-0.9) = 0.9 >= 0.8 = critical

        let alerts = det.evaluate(&metrics, 1000);
        assert!(!alerts.is_empty());
        assert!(alerts.iter().any(|a| a.metric_name == "daily_pnl_vs_limit"));
    }

    #[test]
    fn detector_all_metrics_zero_no_alert() {
        let config = AnomalyConfig::default();
        let mut det = AnomalyDetector::new(config);
        let m = PreFailureMetrics::default();
        for i in 0..10 {
            assert!(det.evaluate(&m, 1000 + i as u64).is_empty());
        }
    }

    // -- ObservabilityManager ------------------------------------------------

    #[test]
    fn manager_new() {
        let mgr = ObservabilityManager::new(AnomalyConfig::default());
        assert_eq!(mgr.total_ticks(), 0);
        assert!(mgr.last_alerts().is_empty());
    }

    #[test]
    fn manager_tick_increments() {
        let mut mgr = ObservabilityManager::new(AnomalyConfig::default());
        mgr.tick(PreFailureMetrics::default(), 1000);
        assert_eq!(mgr.total_ticks(), 1);
        mgr.tick(PreFailureMetrics::default(), 2000);
        assert_eq!(mgr.total_ticks(), 2);
    }

    #[test]
    fn manager_last_metrics_updated() {
        let mut mgr = ObservabilityManager::new(AnomalyConfig::default());
        let mut m = PreFailureMetrics::default();
        m.rolling_variance_latency = 42.0;
        mgr.tick(m.clone(), 1000);
        assert!((mgr.last_metrics().rolling_variance_latency - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn manager_no_alert_on_normal() {
        let mut mgr = ObservabilityManager::new(AnomalyConfig::default());
        let alerts = mgr.tick(PreFailureMetrics::default(), 1000);
        assert!(alerts.is_empty());
        assert_eq!(mgr.total_critical_alerts(), 0);
        assert_eq!(mgr.total_warning_alerts(), 0);
    }

    #[test]
    fn manager_critical_alert_counts() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 3.0;

        let alerts = mgr.tick(metrics, 1000);
        assert!(alerts.iter().any(|a| a.severity == AlertSeverity::Critical));
        assert!(mgr.total_critical_alerts() >= 1);
    }

    #[test]
    fn manager_warning_alert_counts() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 2.1;

        let alerts = mgr.tick(metrics, 1000);
        assert!(alerts.iter().any(|a| a.severity == AlertSeverity::Warning));
        assert!(mgr.total_warning_alerts() >= 1);
    }

    #[test]
    fn manager_detector_access() {
        let mgr = ObservabilityManager::new(AnomalyConfig::default());
        assert!(!mgr.detector().monitored_metrics().is_empty());
    }

    #[test]
    fn manager_reset() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        let mut metrics = PreFailureMetrics::default();
        metrics.policy_entropy = 3.0;
        mgr.tick(metrics, 1000);

        mgr.reset();
        assert_eq!(mgr.total_ticks(), 0);
        assert_eq!(mgr.total_critical_alerts(), 0);
        assert!(mgr.last_alerts().is_empty());
    }

    #[test]
    fn manager_multiple_ticks_accumulate() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        let mut m1 = PreFailureMetrics::default();
        m1.policy_entropy = 3.0;
        mgr.tick(m1, 1000);

        let mut m2 = PreFailureMetrics::default();
        m2.rolling_variance_latency = 25.0;
        mgr.tick(m2, 2000);

        assert_eq!(mgr.total_ticks(), 2);
        assert!(mgr.total_critical_alerts() >= 2);
    }

    #[test]
    fn manager_last_alerts_persist_until_next_tick() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        // First tick with anomaly
        let mut m1 = PreFailureMetrics::default();
        m1.policy_entropy = 3.0;
        mgr.tick(m1, 1000);
        assert!(!mgr.last_alerts().is_empty());

        // Second tick normal
        mgr.tick(PreFailureMetrics::default(), 2000);
        assert!(mgr.last_alerts().is_empty());
    }

    // -- AlertSeverity -------------------------------------------------------

    #[test]
    fn alert_severity_equality() {
        assert_eq!(AlertSeverity::Warning, AlertSeverity::Warning);
        assert_eq!(AlertSeverity::Critical, AlertSeverity::Critical);
        assert_ne!(AlertSeverity::Warning, AlertSeverity::Critical);
    }

    // -- AnomalyThreshold ----------------------------------------------------

    #[test]
    fn threshold_ordering() {
        let t = AnomalyThreshold {
            warning: 0.5,
            critical: 1.0,
        };
        assert!(t.warning < t.critical);
    }

    #[test]
    fn threshold_serde() {
        let t = AnomalyThreshold {
            warning: 0.5,
            critical: 1.0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let t2: AnomalyThreshold = serde_json::from_str(&json).unwrap();
        assert!((t2.warning - 0.5).abs() < f64::EPSILON);
        assert!((t2.critical - 1.0).abs() < f64::EPSILON);
    }

    // -- AnomalyConfig serde -------------------------------------------------

    #[test]
    fn config_serde_roundtrip() {
        let config = AnomalyConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let config2: AnomalyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config2.debounce_ticks, config.debounce_ticks);
        assert_eq!(config2.thresholds.len(), config.thresholds.len());
    }

    // -- Edge cases ----------------------------------------------------------

    #[test]
    fn metrics_with_nan() {
        let mut m = PreFailureMetrics::default();
        m.policy_entropy = f64::NAN;
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);
        let alerts = det.evaluate(&m, 1000);
        assert!(alerts.is_empty());
    }

    #[test]
    fn metrics_with_infinity() {
        let mut m = PreFailureMetrics::default();
        m.policy_entropy = f64::INFINITY;
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);
        let alerts = det.evaluate(&m, 1000);
        assert!(!alerts.is_empty());
    }
}
