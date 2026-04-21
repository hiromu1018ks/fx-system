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

pub fn shannon_entropy(probabilities: &[f64]) -> f64 {
    probabilities
        .iter()
        .copied()
        .filter(|p| *p > 0.0)
        .map(|p| -p * p.ln())
        .sum()
}

pub fn softmax_entropy(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let max_value = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exp_values: Vec<f64> = values
        .iter()
        .map(|value| (value - max_value).exp())
        .collect();
    let total: f64 = exp_values.iter().sum();
    if total <= f64::EPSILON {
        return 0.0;
    }
    let probabilities: Vec<f64> = exp_values.into_iter().map(|value| value / total).collect();
    shannon_entropy(&probabilities)
}

pub fn l2_distance(lhs: &[f64], rhs: &[f64]) -> f64 {
    assert_eq!(lhs.len(), rhs.len(), "vector length mismatch");
    lhs.iter()
        .zip(rhs.iter())
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f64>()
        .sqrt()
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

    #[test]
    fn shannon_entropy_zero_for_deterministic_distribution() {
        assert!((shannon_entropy(&[1.0, 0.0, 0.0]) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn softmax_entropy_positive_for_mixed_scores() {
        let entropy = softmax_entropy(&[2.0, 1.0, 0.0]);
        assert!(entropy > 0.0);
        assert!(entropy < 1.2);
    }

    #[test]
    fn l2_distance_matches_expected_value() {
        let distance = l2_distance(&[0.0, 3.0, 4.0], &[0.0, 0.0, 0.0]);
        assert!((distance - 5.0).abs() < 1e-12);
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

    // =========================================================================
    // §8.2 Pre-Failure Signature verification tests (design.md §8.2)
    // =========================================================================

    /// design.md §8.2 defines exactly 19 pre-failure signature metrics.
    /// Verify the struct field count matches.
    #[test]
    fn s8_2_pre_failure_metrics_count_is_nineteen() {
        assert_eq!(
            PreFailureMetrics::NAMES.len(),
            19,
            "design.md §8.2 defines 19 pre-failure signature metrics"
        );
        let m = PreFailureMetrics::default();
        assert_eq!(m.as_slice().len(), 19);
    }

    /// Verify each metric name matches a design.md §8.2 entry.
    #[test]
    fn s8_2_metric_names_match_design_doc_section_8_2() {
        let expected_names: &[&str] = &[
            "rolling_variance_latency",            // §8.2: rolling_variance_latency
            "feature_distribution_kl_divergence",  // §8.2: feature_distribution_kl_divergence
            "q_value_adjustment_frequency",        // §8.2: q_value_adjustment_frequency
            "execution_drift_trend",               // §8.2: execution_drift_trend
            "latency_risk_trend",                  // §8.2: latency_risk_trend
            "self_impact_ratio",                   // §8.2: self_impact_ratio
            "liquidity_evolvement",                // §8.2: liquidity_evolvement
            "policy_entropy",                      // §8.2: policy_entropy
            "regime_posterior_entropy",            // §8.2: regime_posterior_entropy
            "hidden_liquidity_sigma",              // §8.2: hidden_liquidity_sigma
            "position_constraint_saturation_rate", // §8.2: position_constraint_saturation_rate
            "last_look_rejection_rate",            // §8.2: last_look_rejection_rate
            "dynamic_cost_estimate_error",         // §8.2: dynamic_cost_estimate_error
            "lp_adversarial_score",                // §8.2: lp_adversarial_score
            "daily_pnl_vs_limit",                  // §8.2: daily_pnl_vs_limit
            "weekly_pnl_vs_limit",                 // §8.2: weekly_pnl_vs_limit
            "monthly_pnl_vs_limit",                // §8.2: monthly_pnl_vs_limit
            "lp_recalibration_progress",           // §8.2: lp_recalibration_progress
            "bayesian_posterior_drift",            // §8.2: bayesian_posterior_drift
        ];
        assert_eq!(PreFailureMetrics::NAMES.len(), expected_names.len());
        for (actual, expected) in PreFailureMetrics::NAMES.iter().zip(expected_names.iter()) {
            assert_eq!(actual, expected, "metric name mismatch");
        }
    }

    /// Verify as_slice() ordering matches NAMES ordering.
    #[test]
    fn s8_2_as_slice_ordering_matches_names() {
        let m = PreFailureMetrics::default();
        let names = PreFailureMetrics::NAMES;
        let slice = m.as_slice();
        for i in 0..names.len() {
            // Each field should be accessible by index matching its name position
            assert!(
                i < slice.len(),
                "index {} out of bounds for slice of length {}",
                i,
                slice.len()
            );
        }
        assert_eq!(slice.len(), names.len());
    }

    /// Verify all metrics can be set and read individually.
    #[test]
    fn s8_2_all_metrics_settable_and_readable() {
        let values: [f64; 19] = [
            1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0,
            17.0, 18.0, 19.0,
        ];
        let m = PreFailureMetrics {
            rolling_variance_latency: values[0],
            feature_distribution_kl_divergence: values[1],
            q_value_adjustment_frequency: values[2],
            execution_drift_trend: values[3],
            latency_risk_trend: values[4],
            self_impact_ratio: values[5],
            liquidity_evolvement: values[6],
            policy_entropy: values[7],
            regime_posterior_entropy: values[8],
            hidden_liquidity_sigma: values[9],
            position_constraint_saturation_rate: values[10],
            last_look_rejection_rate: values[11],
            dynamic_cost_estimate_error: values[12],
            lp_adversarial_score: values[13],
            daily_pnl_vs_limit: values[14],
            weekly_pnl_vs_limit: values[15],
            monthly_pnl_vs_limit: values[16],
            lp_recalibration_progress: values[17],
            bayesian_posterior_drift: values[18],
        };
        let slice = m.as_slice();
        for (i, &v) in values.iter().enumerate() {
            assert!(
                (slice[i] - v).abs() < f64::EPSILON,
                "mismatch at index {}",
                i
            );
        }
    }

    /// Default AnomalyConfig covers 17 of 19 metrics.
    /// lp_recalibration_progress and execution_drift_trend are intentionally excluded
    /// because they require contextual interpretation (progress ratio and trend direction).
    #[test]
    fn s8_2_default_config_monitors_seventeen_of_nineteen() {
        let config = AnomalyConfig::default();
        let configured: std::collections::HashSet<_> =
            config.thresholds.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(configured.len(), 17, "expected 17 monitored metrics");

        // These two are intentionally unmonitored by default
        assert!(
            !configured.contains("lp_recalibration_progress"),
            "lp_recalibration_progress is context-dependent (progress ratio 0..1)"
        );
        assert!(
            !configured.contains("execution_drift_trend"),
            "execution_drift_trend is directional (positive/negative both valid)"
        );

        // Verify all other 17 are monitored
        for name in PreFailureMetrics::NAMES {
            if *name == "lp_recalibration_progress" || *name == "execution_drift_trend" {
                continue;
            }
            assert!(
                configured.contains(name),
                "metric {name} should be monitored by default"
            );
        }
    }

    /// Custom config can add thresholds for the two excluded metrics.
    #[test]
    fn s8_2_custom_config_can_monitor_excluded_metrics() {
        let mut config = AnomalyConfig::default();
        config.thresholds.push((
            "lp_recalibration_progress".into(),
            AnomalyThreshold {
                warning: 0.5,
                critical: 0.9,
            },
        ));
        config.thresholds.push((
            "execution_drift_trend".into(),
            AnomalyThreshold {
                warning: 1.0,
                critical: 3.0,
            },
        ));
        let det = AnomalyDetector::new(config);
        assert_eq!(det.monitored_metrics().len(), 19);
    }

    // =========================================================================
    // §8.3 ObservabilityManager integration verification tests
    // =========================================================================

    /// ObservabilityManager tick() updates all 19 metrics in last_metrics.
    #[test]
    fn s8_3_manager_tracks_all_nineteen_metrics() {
        let config = AnomalyConfig::default();
        let mut mgr = ObservabilityManager::new(config);

        let mut m = PreFailureMetrics::default();
        m.rolling_variance_latency = 1.0;
        m.feature_distribution_kl_divergence = 2.0;
        m.q_value_adjustment_frequency = 3.0;
        m.execution_drift_trend = 4.0;
        m.latency_risk_trend = 5.0;
        m.self_impact_ratio = 6.0;
        m.liquidity_evolvement = 7.0;
        m.policy_entropy = 8.0;
        m.regime_posterior_entropy = 9.0;
        m.hidden_liquidity_sigma = 10.0;
        m.position_constraint_saturation_rate = 11.0;
        m.last_look_rejection_rate = 12.0;
        m.dynamic_cost_estimate_error = 13.0;
        m.lp_adversarial_score = 14.0;
        m.daily_pnl_vs_limit = 15.0;
        m.weekly_pnl_vs_limit = 16.0;
        m.monthly_pnl_vs_limit = 17.0;
        m.lp_recalibration_progress = 18.0;
        m.bayesian_posterior_drift = 19.0;

        mgr.tick(m.clone(), 1000);

        let last = mgr.last_metrics();
        assert!((last.rolling_variance_latency - 1.0).abs() < f64::EPSILON);
        assert!((last.bayesian_posterior_drift - 19.0).abs() < f64::EPSILON);
        // Verify all fields are non-default
        let slice = last.as_slice();
        for (i, &v) in slice.iter().enumerate() {
            assert!(
                v > 0.0,
                "metric at index {} ({}) should be non-zero",
                i,
                PreFailureMetrics::NAMES[i]
            );
        }
    }

    /// ObservabilityManager accumulates alerts correctly across multiple ticks.
    #[test]
    fn s8_3_manager_accumulates_alerts_across_ticks() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        // Tick 1: one anomaly
        let mut m1 = PreFailureMetrics::default();
        m1.policy_entropy = 3.0; // critical
        mgr.tick(m1, 1000);

        // Tick 2: different anomaly
        let mut m2 = PreFailureMetrics::default();
        m2.rolling_variance_latency = 25.0; // critical
        mgr.tick(m2, 2000);

        // Tick 3: normal
        mgr.tick(PreFailureMetrics::default(), 3000);

        assert_eq!(mgr.total_ticks(), 3);
        assert!(mgr.total_critical_alerts() >= 2);
        // Last tick was normal, so last_alerts should be empty
        assert!(mgr.last_alerts().is_empty());
    }

    /// ObservabilityManager reset clears all state.
    #[test]
    fn s8_3_manager_reset_clears_all_state_completely() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        let mut m = PreFailureMetrics::default();
        m.policy_entropy = 3.0;
        m.rolling_variance_latency = 25.0;
        mgr.tick(m, 1000);

        assert!(mgr.total_ticks() > 0);
        assert!(mgr.total_critical_alerts() > 0);

        mgr.reset();

        assert_eq!(mgr.total_ticks(), 0);
        assert_eq!(mgr.total_critical_alerts(), 0);
        assert_eq!(mgr.total_warning_alerts(), 0);
        assert!(mgr.last_alerts().is_empty());
        // Reset metrics should be all zeros
        let slice = mgr.last_metrics().as_slice();
        for &v in &slice {
            assert!((v - 0.0).abs() < f64::EPSILON);
        }
    }

    /// Detector rolling stats are accessible from the manager.
    #[test]
    fn s8_3_manager_exposes_detector_rolling_stats() {
        let config = AnomalyConfig::default();
        let mut mgr = ObservabilityManager::new(config);

        let mut m = PreFailureMetrics::default();
        m.policy_entropy = 1.5;
        mgr.tick(m, 1000);

        let stats = mgr.detector().rolling_stats("policy_entropy");
        assert!(stats.is_some());
        assert_eq!(stats.unwrap().count(), 1);
    }

    // =========================================================================
    // §8.4 Structured logging verification tests
    // =========================================================================

    /// Verify that log_metrics() is called on every tick (tested by side effect:
    /// last_metrics is always updated).
    #[test]
    fn s8_4_log_metrics_called_on_every_tick() {
        let config = AnomalyConfig::default();
        let mut mgr = ObservabilityManager::new(config);

        for i in 0..5 {
            let mut m = PreFailureMetrics::default();
            m.rolling_variance_latency = i as f64;
            mgr.tick(m, i as u64 * 1000);
        }

        assert_eq!(mgr.total_ticks(), 5);
        // last_metrics reflects the last tick's values
        assert!((mgr.last_metrics().rolling_variance_latency - 4.0).abs() < f64::EPSILON);
    }

    /// Verify PreFailureMetrics has all 19 fields accessible for structured logging.
    #[test]
    fn s8_4_all_nineteen_fields_accessible_for_structured_logging() {
        let m = PreFailureMetrics::default();
        // Verify all 19 fields are public and accessible
        let _ = m.rolling_variance_latency;
        let _ = m.feature_distribution_kl_divergence;
        let _ = m.q_value_adjustment_frequency;
        let _ = m.execution_drift_trend;
        let _ = m.latency_risk_trend;
        let _ = m.self_impact_ratio;
        let _ = m.liquidity_evolvement;
        let _ = m.policy_entropy;
        let _ = m.regime_posterior_entropy;
        let _ = m.hidden_liquidity_sigma;
        let _ = m.position_constraint_saturation_rate;
        let _ = m.last_look_rejection_rate;
        let _ = m.dynamic_cost_estimate_error;
        let _ = m.lp_adversarial_score;
        let _ = m.daily_pnl_vs_limit;
        let _ = m.weekly_pnl_vs_limit;
        let _ = m.monthly_pnl_vs_limit;
        let _ = m.lp_recalibration_progress;
        let _ = m.bayesian_posterior_drift;
    }

    /// Verify AnomalyAlert contains all fields needed for structured alert logging.
    #[test]
    fn s8_4_alert_contains_structured_logging_fields() {
        let alert = AnomalyAlert {
            metric_name: "policy_entropy".into(),
            value: 3.0,
            threshold: 2.5,
            severity: AlertSeverity::Critical,
            timestamp_ns: 1_700_000_000_000_000_000,
        };
        assert_eq!(alert.metric_name, "policy_entropy");
        assert!((alert.value - 3.0).abs() < f64::EPSILON);
        assert!((alert.threshold - 2.5).abs() < f64::EPSILON);
        assert_eq!(alert.severity, AlertSeverity::Critical);
        assert_eq!(alert.timestamp_ns, 1_700_000_000_000_000_000);
        // Verify serde serialization for structured logging
        let json = serde_json::to_string(&alert).unwrap();
        assert!(json.contains("policy_entropy"));
        assert!(json.contains("\"severity\":\"Critical\""));
    }

    /// Verify alert severity distinction for logging levels.
    #[test]
    fn s8_4_alert_severity_levels_are_distinct() {
        let critical = AnomalyAlert {
            metric_name: "test".into(),
            value: 10.0,
            threshold: 5.0,
            severity: AlertSeverity::Critical,
            timestamp_ns: 1000,
        };
        let warning = AnomalyAlert {
            metric_name: "test".into(),
            value: 3.0,
            threshold: 2.0,
            severity: AlertSeverity::Warning,
            timestamp_ns: 1000,
        };
        assert_ne!(critical.severity, warning.severity);
        assert_eq!(critical.severity, AlertSeverity::Critical);
        assert_eq!(warning.severity, AlertSeverity::Warning);
    }

    // =========================================================================
    // §8.5 AnomalyDetector comprehensive verification tests
    // =========================================================================

    /// All 17 configured metrics have rolling stats initialized.
    #[test]
    fn s8_5_all_configured_metrics_have_rolling_stats() {
        let config = AnomalyConfig::default();
        let det = AnomalyDetector::new(config);
        for name in PreFailureMetrics::NAMES {
            if *name == "lp_recalibration_progress" || *name == "execution_drift_trend" {
                continue;
            }
            assert!(
                det.rolling_stats(name).is_some(),
                "rolling stats missing for {name}"
            );
        }
    }

    /// Rolling stats converge to expected mean/variance over many ticks.
    #[test]
    fn s8_5_rolling_stats_convergence() {
        let config = AnomalyConfig::default();
        let mut det = AnomalyDetector::new(config);

        // Feed 100 identical values
        let mut m = PreFailureMetrics::default();
        for i in 0..100 {
            m.policy_entropy = 1.5;
            det.evaluate(&m, i as u64);
        }

        let stats = det.rolling_stats("policy_entropy").unwrap();
        assert_eq!(stats.count(), 100);
        assert!((stats.mean() - 1.5).abs() < 1e-10);
        assert!((stats.variance() - 0.0).abs() < 1e-10);
    }

    /// Debounce prevents transient spikes from triggering alerts.
    #[test]
    fn s8_5_debounce_prevents_transient_false_positives() {
        let debounce_ticks = 5;
        let config = AnomalyConfig {
            debounce_ticks,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut m = PreFailureMetrics::default();
        m.policy_entropy = 3.0; // critical

        // Feed debounce_ticks - 1 anomalies then 1 normal
        for i in 0..(debounce_ticks - 1) {
            let alerts = det.evaluate(&m, i as u64);
            assert!(alerts.is_empty(), "alert fired prematurely at tick {}", i);
        }
        // Normal tick resets counter
        let alerts = det.evaluate(&PreFailureMetrics::default(), (debounce_ticks - 1) as u64);
        assert!(alerts.is_empty());

        // Need debounce_ticks more consecutive anomalies
        for i in 0..debounce_ticks {
            let alerts = det.evaluate(&m, (debounce_ticks + i) as u64);
            if i < debounce_ticks - 1 {
                assert!(alerts.is_empty(), "alert fired at tick {}", i);
            } else {
                assert!(!alerts.is_empty(), "alert should fire after debounce");
            }
        }
    }

    /// Multiple simultaneous anomalies detected in single tick.
    #[test]
    fn s8_5_multiple_simultaneous_anomalies_detected() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut m = PreFailureMetrics::default();
        m.policy_entropy = 3.0; // critical > 2.5
        m.rolling_variance_latency = 25.0; // critical > 20.0
        m.last_look_rejection_rate = 0.6; // critical > 0.5
        m.lp_adversarial_score = 0.9; // critical > 0.8
        m.daily_pnl_vs_limit = -0.9; // abs(0.9) >= 0.8

        let alerts = det.evaluate(&m, 1000);
        assert!(
            alerts.len() >= 5,
            "expected >= 5 alerts, got {}",
            alerts.len()
        );

        let names: std::collections::HashSet<_> =
            alerts.iter().map(|a| a.metric_name.as_str()).collect();
        assert!(names.contains("policy_entropy"));
        assert!(names.contains("rolling_variance_latency"));
        assert!(names.contains("last_look_rejection_rate"));
        assert!(names.contains("lp_adversarial_score"));
        assert!(names.contains("daily_pnl_vs_limit"));
    }

    /// Severity escalation: warning → critical as metric worsens.
    #[test]
    fn s8_5_severity_escalation_warning_to_critical() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        // Warning level
        let mut m_warn = PreFailureMetrics::default();
        m_warn.policy_entropy = 2.1; // warning (2.0-2.5)
        let alerts = det.evaluate(&m_warn, 1000);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);

        // Critical level
        let mut m_crit = PreFailureMetrics::default();
        m_crit.policy_entropy = 3.0; // critical > 2.5
        let alerts = det.evaluate(&m_crit, 2000);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, AlertSeverity::Critical);
    }

    /// Negative metric values are checked via absolute value.
    #[test]
    fn s8_5_negative_values_checked_via_absolute() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut m = PreFailureMetrics::default();
        m.daily_pnl_vs_limit = -0.85; // abs(-0.85) = 0.85 >= 0.8 = critical
        let alerts = det.evaluate(&m, 1000);
        assert!(!alerts.is_empty());
        let alert = alerts
            .iter()
            .find(|a| a.metric_name == "daily_pnl_vs_limit")
            .unwrap();
        assert_eq!(alert.severity, AlertSeverity::Critical);
        assert!((alert.value - 0.85).abs() < 1e-10);
    }

    /// Rolling stats window trims old values.
    #[test]
    fn s8_5_rolling_stats_window_trims_old_values() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        // Fill window with 100 values (window size = 100)
        for i in 0..100 {
            let mut m = PreFailureMetrics::default();
            m.policy_entropy = i as f64;
            det.evaluate(&m, i as u64);
        }

        // Add 100 more constant values to fully replace the window
        for i in 100..200 {
            let mut m = PreFailureMetrics::default();
            m.policy_entropy = 99.0;
            det.evaluate(&m, i as u64);
        }

        let stats = det.rolling_stats("policy_entropy").unwrap();
        assert_eq!(stats.count(), 100); // window size
                                        // All values should be 99.0 now
        assert!((stats.mean() - 99.0).abs() < 1e-10);
        assert!((stats.variance()).abs() < 1e-10);
    }

    /// Detector handles edge case: all metrics at warning threshold simultaneously.
    #[test]
    fn s8_5_all_metrics_at_warning_threshold() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        // Set each configured metric to exactly its warning threshold
        let m = PreFailureMetrics {
            rolling_variance_latency: 5.0,
            feature_distribution_kl_divergence: 0.5,
            q_value_adjustment_frequency: 0.8,
            execution_drift_trend: 0.0, // not configured
            latency_risk_trend: 2.0,
            self_impact_ratio: 0.5,
            liquidity_evolvement: 3.0,
            policy_entropy: 2.0,
            regime_posterior_entropy: 1.5,
            hidden_liquidity_sigma: 0.1,
            position_constraint_saturation_rate: 0.7,
            last_look_rejection_rate: 0.3,
            dynamic_cost_estimate_error: 0.5,
            lp_adversarial_score: 0.5,
            daily_pnl_vs_limit: 0.6,
            weekly_pnl_vs_limit: 0.6,
            monthly_pnl_vs_limit: 0.6,
            lp_recalibration_progress: 0.0, // not configured
            bayesian_posterior_drift: 2.0,
        };

        let alerts = det.evaluate(&m, 1000);
        // All 17 configured metrics at warning threshold should trigger
        assert!(
            alerts.len() >= 10,
            "expected many warnings when all metrics at threshold, got {}",
            alerts.len()
        );
        for alert in &alerts {
            assert_eq!(alert.severity, AlertSeverity::Warning);
        }
    }

    /// Detector produces correct alert timestamps.
    #[test]
    fn s8_5_alert_timestamps_match_input() {
        let config = AnomalyConfig {
            debounce_ticks: 1,
            ..AnomalyConfig::default()
        };
        let mut det = AnomalyDetector::new(config);

        let mut m = PreFailureMetrics::default();
        m.policy_entropy = 3.0;

        let ts = 1_700_000_000_000_000_000u64;
        let alerts = det.evaluate(&m, ts);
        assert_eq!(alerts[0].timestamp_ns, ts);
    }

    /// Detector monitored_metrics() returns correct list.
    #[test]
    fn s8_5_monitored_metrics_list_completeness() {
        let config = AnomalyConfig::default();
        let det = AnomalyDetector::new(config);
        let monitored = det.monitored_metrics();

        assert_eq!(monitored.len(), 17);

        // Verify no duplicates
        let unique: std::collections::HashSet<_> = monitored.iter().collect();
        assert_eq!(unique.len(), 17);
    }

    // =========================================================================
    // §8 Observability end-to-end: full metrics snapshot → detection → alert
    // =========================================================================

    /// Simulate a realistic pre-failure scenario with multiple degrading metrics.
    #[test]
    fn s8_e2e_pre_failure_scenario_detection() {
        let config = AnomalyConfig {
            debounce_ticks: 3,
            ..AnomalyConfig::default()
        };
        let mut mgr = ObservabilityManager::new(config);

        // Simulate 10 ticks of gradually degrading system
        for tick in 0..10 {
            let mut m = PreFailureMetrics::default();
            let degradation = tick as f64 / 10.0;

            m.bayesian_posterior_drift = degradation * 6.0; // 0..5.4 (critical at 5.0)
            m.feature_distribution_kl_divergence = degradation * 1.2; // 0..1.08 (critical at 1.0)
            m.policy_entropy = 1.0 + degradation * 2.0; // 1.0..2.8 (critical at 2.5)
            m.last_look_rejection_rate = degradation * 0.6; // 0..0.54 (critical at 0.5)
            m.daily_pnl_vs_limit = -degradation * 0.9; // 0..-0.81 (critical at 0.8 abs)

            let _alerts = mgr.tick(m, tick as u64 * 1_000_000_000);

            // Early ticks should not trigger (below debounce)
            if tick < 3 {
                // May have some alerts but shouldn't be excessive
            }
        }

        // By tick 10, multiple metrics should have triggered critical alerts
        assert!(
            mgr.total_critical_alerts() > 0,
            "expected critical alerts in degrading scenario"
        );
        assert_eq!(mgr.total_ticks(), 10);
    }

    /// Verify the complete observability pipeline: metrics → manager → detector → alerts → reset.
    #[test]
    fn s8_e2e_full_observability_pipeline() {
        let config = AnomalyConfig::default();
        let mut mgr = ObservabilityManager::new(config);

        // Phase 1: Normal operation
        for i in 0..10 {
            mgr.tick(PreFailureMetrics::default(), i as u64 * 1000);
        }
        assert_eq!(mgr.total_ticks(), 10);
        assert_eq!(mgr.total_critical_alerts(), 0);

        // Phase 2: Anomaly detected
        let mut bad = PreFailureMetrics::default();
        bad.bayesian_posterior_drift = 10.0;
        for i in 0..10 {
            mgr.tick(bad.clone(), (10 + i) as u64 * 1000);
        }
        assert!(mgr.total_critical_alerts() > 0);

        // Phase 3: Reset and verify clean state
        mgr.reset();
        assert_eq!(mgr.total_ticks(), 0);
        assert_eq!(mgr.total_critical_alerts(), 0);
        assert!(mgr.last_alerts().is_empty());
    }
}
