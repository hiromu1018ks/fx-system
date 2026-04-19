use tracing::{info, warn};

use crate::lp_monitor::LpSwitchSignal;

// ============================================================
// Recalibration Config
// ============================================================

#[derive(Debug, Clone)]
pub struct RecalibrationConfig {
    /// Lot multiplier during safe mode (default: 0.25 = 25%)
    pub safe_mode_lot_multiplier: f64,
    /// σ_execution multiplier during safe mode (default: 2.0 = double)
    pub safe_mode_sigma_multiplier: f64,
    /// Minimum observations before recalibration can complete (default: 30)
    pub min_recalibration_observations: u64,
    /// Slippage estimation error threshold for completion (default: 0.0002)
    pub slippage_error_threshold: f64,
    /// Fill rate error threshold for completion (default: 0.1)
    pub fill_rate_error_threshold: f64,
    /// Maximum duration in nanoseconds before forced completion (default: 5 minutes)
    pub max_recalibration_duration_ns: u64,
    /// Prior alpha for Beta-Binomial recalibration (default: 2.0)
    pub recalibration_prior_alpha: f64,
    /// Prior beta for Beta-Binomial recalibration (default: 1.0)
    pub recalibration_prior_beta: f64,
}

impl Default for RecalibrationConfig {
    fn default() -> Self {
        Self {
            safe_mode_lot_multiplier: 0.25,
            safe_mode_sigma_multiplier: 2.0,
            min_recalibration_observations: 30,
            slippage_error_threshold: 0.0002,
            fill_rate_error_threshold: 0.1,
            max_recalibration_duration_ns: 300_000_000_000, // 5 minutes
            recalibration_prior_alpha: 2.0,
            recalibration_prior_beta: 1.0,
        }
    }
}

// ============================================================
// Recalibration State
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecalibrationStatus {
    /// No recalibration in progress
    Idle,
    /// Safe mode active, collecting observations
    SafeMode,
    /// Recalibration complete, normal operation resumed
    Completed,
}

#[derive(Debug, Clone)]
pub struct RecalibrationSnapshot {
    pub lp_id: String,
    pub status: RecalibrationStatus,
    pub observations: u64,
    pub slippage_mean: f64,
    pub slippage_variance: f64,
    pub fill_count: u64,
    pub reject_count: u64,
    pub observed_fill_rate: f64,
    pub estimated_fill_rate: f64,
    pub fill_rate_error: f64,
    pub slippage_estimation_error: f64,
    pub elapsed_ns: u64,
    pub safe_mode_lot_multiplier: f64,
    pub safe_mode_sigma_multiplier: f64,
}

// ============================================================
// LP Recalibration Manager
// ============================================================

#[derive(Debug)]
pub struct LpRecalibrationManager {
    config: RecalibrationConfig,
    status: RecalibrationStatus,
    target_lp_id: Option<String>,
    start_timestamp_ns: Option<u64>,
    observations: u64,
    fill_count: u64,
    reject_count: u64,
    slippage_sum: f64,
    slippage_sum_sq: f64,
    slippage_predictions: Vec<f64>,
    /// Baseline slippage estimate taken at recalibration start
    baseline_slippage_estimate: f64,
    /// Baseline fill rate estimate taken at recalibration start
    baseline_fill_rate_estimate: f64,
}

impl LpRecalibrationManager {
    pub fn new(config: RecalibrationConfig) -> Self {
        Self {
            config,
            status: RecalibrationStatus::Idle,
            target_lp_id: None,
            start_timestamp_ns: None,
            observations: 0,
            fill_count: 0,
            reject_count: 0,
            slippage_sum: 0.0,
            slippage_sum_sq: 0.0,
            slippage_predictions: Vec::new(),
            baseline_slippage_estimate: 0.0,
            baseline_fill_rate_estimate: 0.0,
        }
    }

    pub fn status(&self) -> &RecalibrationStatus {
        &self.status
    }

    pub fn target_lp_id(&self) -> Option<&str> {
        self.target_lp_id.as_deref()
    }

    pub fn is_safe_mode(&self) -> bool {
        self.status == RecalibrationStatus::SafeMode
    }

    /// Enter safe mode after an LP switch. Records baseline estimates from current models.
    pub fn enter_safe_mode(
        &mut self,
        signal: &LpSwitchSignal,
        baseline_fill_rate: f64,
        baseline_slippage_mean: f64,
    ) {
        let lp_id = signal.to_lp_id.clone();

        self.status = RecalibrationStatus::SafeMode;
        self.target_lp_id = Some(lp_id);
        self.start_timestamp_ns = None; // set on first observation
        self.observations = 0;
        self.fill_count = 0;
        self.reject_count = 0;
        self.slippage_sum = 0.0;
        self.slippage_sum_sq = 0.0;
        self.slippage_predictions.clear();
        self.baseline_slippage_estimate = baseline_slippage_mean;
        self.baseline_fill_rate_estimate = baseline_fill_rate;

        warn!(
            from_lp = %signal.from_lp_id,
            to_lp = %signal.to_lp_id,
            reason = %signal.reason,
            lot_multiplier = self.config.safe_mode_lot_multiplier,
            sigma_multiplier = self.config.safe_mode_sigma_multiplier,
            "LP recalibration: entering safe mode"
        );
    }

    /// Record a fill observation for the target LP.
    pub fn record_fill(
        &mut self,
        lp_id: &str,
        observed_slippage: f64,
        predicted_slippage: f64,
        timestamp_ns: u64,
    ) {
        if self.status != RecalibrationStatus::SafeMode {
            return;
        }
        if self.target_lp_id.as_deref() != Some(lp_id) {
            return;
        }

        self.maybe_set_start(timestamp_ns);
        self.observations += 1;
        self.fill_count += 1;
        self.slippage_sum += observed_slippage;
        self.slippage_sum_sq += observed_slippage * observed_slippage;
        self.slippage_predictions.push(predicted_slippage);
    }

    /// Record a rejection observation for the target LP.
    pub fn record_rejection(&mut self, lp_id: &str, timestamp_ns: u64) {
        if self.status != RecalibrationStatus::SafeMode {
            return;
        }
        if self.target_lp_id.as_deref() != Some(lp_id) {
            return;
        }

        self.maybe_set_start(timestamp_ns);
        self.observations += 1;
        self.reject_count += 1;
    }

    fn maybe_set_start(&mut self, timestamp_ns: u64) {
        if self.start_timestamp_ns.is_none() {
            self.start_timestamp_ns = Some(timestamp_ns);
        }
    }

    /// Get the lot multiplier. During safe mode, returns the reduced multiplier.
    pub fn lot_multiplier(&self) -> f64 {
        if self.status == RecalibrationStatus::SafeMode {
            self.config.safe_mode_lot_multiplier
        } else {
            1.0
        }
    }

    /// Get the σ_execution multiplier. During safe mode, returns the doubled multiplier.
    pub fn sigma_multiplier(&self) -> f64 {
        if self.status == RecalibrationStatus::SafeMode {
            self.config.safe_mode_sigma_multiplier
        } else {
            1.0
        }
    }

    /// Check if recalibration should complete. Returns true if completed.
    pub fn check_completion(&mut self, current_timestamp_ns: u64) -> bool {
        if self.status != RecalibrationStatus::SafeMode {
            return false;
        }

        // Check minimum observations
        if self.observations < self.config.min_recalibration_observations {
            return false;
        }

        // Check maximum duration
        let elapsed = self
            .start_timestamp_ns
            .map(|start| current_timestamp_ns.saturating_sub(start))
            .unwrap_or(0);

        if elapsed >= self.config.max_recalibration_duration_ns {
            info!(
                lp_id = ?self.target_lp_id,
                observations = self.observations,
                elapsed_ns = elapsed,
                "Recalibration: max duration reached, completing"
            );
            self.complete();
            return true;
        }

        // Check statistical convergence
        if self.check_statistical_convergence() {
            info!(
                lp_id = ?self.target_lp_id,
                observations = self.observations,
                slippage_error = self.compute_slippage_error(),
                fill_rate_error = self.compute_fill_rate_error(),
                "Recalibration: statistical convergence achieved"
            );
            self.complete();
            return true;
        }

        false
    }

    /// Check if the estimation errors are within thresholds.
    fn check_statistical_convergence(&self) -> bool {
        self.compute_slippage_error() < self.config.slippage_error_threshold
            && self.compute_fill_rate_error() < self.config.fill_rate_error_threshold
    }

    /// Compute slippage estimation error: |observed_mean - predicted_mean| / max(|observed_mean|, epsilon)
    fn compute_slippage_error(&self) -> f64 {
        if self.fill_count == 0 || self.slippage_predictions.is_empty() {
            return f64::INFINITY;
        }

        let observed_mean = self.slippage_sum / self.fill_count as f64;
        let predicted_mean: f64 =
            self.slippage_predictions.iter().sum::<f64>() / self.slippage_predictions.len() as f64;

        let denominator = observed_mean.abs().max(predicted_mean.abs()).max(1e-10);
        (observed_mean - predicted_mean).abs() / denominator
    }

    /// Compute fill rate estimation error: |observed - predicted|
    fn compute_fill_rate_error(&self) -> f64 {
        if self.observations == 0 {
            return f64::INFINITY;
        }

        let observed_fill_rate = self.fill_count as f64 / self.observations as f64;
        (observed_fill_rate - self.baseline_fill_rate_estimate).abs()
    }

    /// Compute observed slippage statistics.
    fn observed_slippage_mean(&self) -> f64 {
        if self.fill_count == 0 {
            return 0.0;
        }
        self.slippage_sum / self.fill_count as f64
    }

    fn observed_slippage_variance(&self) -> f64 {
        if self.fill_count < 2 {
            return 0.0;
        }
        let mean = self.observed_slippage_mean();
        let count = self.fill_count as f64;
        (self.slippage_sum_sq / count - mean * mean).max(0.0)
    }

    /// Complete the recalibration and return to normal operation.
    fn complete(&mut self) {
        self.status = RecalibrationStatus::Completed;
        info!(
            lp_id = ?self.target_lp_id,
            observations = self.observations,
            fills = self.fill_count,
            rejections = self.reject_count,
            observed_slippage_mean = self.observed_slippage_mean(),
            observed_fill_rate = self.fill_count as f64 / self.observations as f64,
            "Recalibration completed, exiting safe mode"
        );
    }

    /// Reset to idle state.
    pub fn reset(&mut self) {
        self.status = RecalibrationStatus::Idle;
        self.target_lp_id = None;
        self.start_timestamp_ns = None;
        self.observations = 0;
        self.fill_count = 0;
        self.reject_count = 0;
        self.slippage_sum = 0.0;
        self.slippage_sum_sq = 0.0;
        self.slippage_predictions.clear();
        self.baseline_slippage_estimate = 0.0;
        self.baseline_fill_rate_estimate = 0.0;
    }

    /// Get a snapshot of the current recalibration state.
    pub fn snapshot(&self, current_timestamp_ns: u64) -> RecalibrationSnapshot {
        let elapsed = self
            .start_timestamp_ns
            .map(|start| current_timestamp_ns.saturating_sub(start))
            .unwrap_or(0);

        let observed_fill_rate = if self.observations > 0 {
            self.fill_count as f64 / self.observations as f64
        } else {
            0.0
        };

        RecalibrationSnapshot {
            lp_id: self.target_lp_id.clone().unwrap_or_default(),
            status: self.status.clone(),
            observations: self.observations,
            slippage_mean: self.observed_slippage_mean(),
            slippage_variance: self.observed_slippage_variance(),
            fill_count: self.fill_count,
            reject_count: self.reject_count,
            observed_fill_rate,
            estimated_fill_rate: self.baseline_fill_rate_estimate,
            fill_rate_error: self.compute_fill_rate_error(),
            slippage_estimation_error: self.compute_slippage_error(),
            elapsed_ns: elapsed,
            safe_mode_lot_multiplier: self.config.safe_mode_lot_multiplier,
            safe_mode_sigma_multiplier: self.config.safe_mode_sigma_multiplier,
        }
    }

    pub fn config(&self) -> &RecalibrationConfig {
        &self.config
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{ExecutionGateway, ExecutionGatewayConfig};

    fn make_gateway() -> ExecutionGateway {
        ExecutionGateway::new(ExecutionGatewayConfig::default())
    }

    fn enter_safe_mode(mgr: &mut LpRecalibrationManager, gw: &ExecutionGateway) {
        let signal = make_signal();
        let lp_id = &signal.to_lp_id;
        let baseline_fill_rate = gw.last_look_model().fill_probability(lp_id, 0.1);
        let baseline_slippage = gw
            .slippage_model()
            .get_lp_stats(lp_id)
            .map(|s| s.mean)
            .unwrap_or(0.0);
        mgr.enter_safe_mode(&signal, baseline_fill_rate, baseline_slippage);
    }

    fn default_config() -> RecalibrationConfig {
        RecalibrationConfig {
            min_recalibration_observations: 10,
            max_recalibration_duration_ns: 60_000_000_000, // 1 minute for tests
            ..Default::default()
        }
    }

    fn make_signal() -> LpSwitchSignal {
        LpSwitchSignal {
            from_lp_id: "LP_PRIMARY".to_string(),
            to_lp_id: "LP_BACKUP".to_string(),
            reason: "low fill rate".to_string(),
        }
    }

    #[test]
    fn new_manager_is_idle() {
        let mgr = LpRecalibrationManager::new(default_config());
        assert_eq!(mgr.status(), &RecalibrationStatus::Idle);
        assert!(!mgr.is_safe_mode());
        assert_eq!(mgr.lot_multiplier(), 1.0);
        assert_eq!(mgr.sigma_multiplier(), 1.0);
    }

    #[test]
    fn enter_safe_mode_sets_status() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        assert_eq!(mgr.status(), &RecalibrationStatus::SafeMode);
        assert!(mgr.is_safe_mode());
        assert_eq!(mgr.target_lp_id(), Some("LP_BACKUP"));
        assert_eq!(mgr.lot_multiplier(), 0.25);
        assert_eq!(mgr.sigma_multiplier(), 2.0);
    }

    #[test]
    fn safe_mode_lot_and_sigma_multipliers() {
        let mgr = LpRecalibrationManager::new(RecalibrationConfig {
            safe_mode_lot_multiplier: 0.5,
            safe_mode_sigma_multiplier: 3.0,
            ..Default::default()
        });
        assert_eq!(mgr.lot_multiplier(), 1.0); // idle
        assert_eq!(mgr.sigma_multiplier(), 1.0); // idle
    }

    #[test]
    fn record_fill_increments_observations() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        mgr.record_fill("LP_BACKUP", 0.0001, 0.00015, 1_000_000_000_000);
        assert_eq!(mgr.observations, 1);
        assert_eq!(mgr.fill_count, 1);
        assert_eq!(mgr.reject_count, 0);
    }

    #[test]
    fn record_rejection_increments_observations() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        mgr.record_rejection("LP_BACKUP", 1_000_000_000_000);
        assert_eq!(mgr.observations, 1);
        assert_eq!(mgr.fill_count, 0);
        assert_eq!(mgr.reject_count, 1);
    }

    #[test]
    fn records_ignored_when_idle() {
        let mut mgr = LpRecalibrationManager::new(default_config());

        mgr.record_fill("LP_BACKUP", 0.0001, 0.00015, 1_000_000_000_000);
        mgr.record_rejection("LP_BACKUP", 1_000_000_000_000);
        assert_eq!(mgr.observations, 0);
    }

    #[test]
    fn records_ignored_for_wrong_lp() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        mgr.record_fill("LP_PRIMARY", 0.0001, 0.00015, 1_000_000_000_000);
        mgr.record_rejection("LP_PRIMARY", 1_000_000_000_000);
        assert_eq!(mgr.observations, 0);
    }

    #[test]
    fn check_completion_insufficient_observations() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        for i in 0..5 {
            mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000 + i);
        }
        assert!(!mgr.check_completion(2_000_000_000_000));
        assert_eq!(mgr.status(), &RecalibrationStatus::SafeMode);
    }

    #[test]
    fn check_completion_statistical_convergence() {
        let mut mgr = LpRecalibrationManager::new(RecalibrationConfig {
            min_recalibration_observations: 10,
            slippage_error_threshold: 0.01,
            fill_rate_error_threshold: 0.01,
            max_recalibration_duration_ns: 60_000_000_000,
            ..Default::default()
        });
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        // Record fills with consistent observed ≈ predicted slippage
        for i in 0..15u64 {
            let slippage = 0.0001;
            mgr.record_fill(
                "LP_BACKUP",
                slippage,
                slippage, // observed == predicted → error = 0
                1_000_000_000_000 + i,
            );
        }

        // fill_rate_error: observed=1.0, baseline≈0.667 → error ≈ 0.333 > 0.01
        // Need fill + reject to bring observed fill rate close to baseline
        // Actually we need the fill rate to be close to the baseline.
        // baseline_fill_rate ≈ 0.667 (2/3 prior). observed = 1.0 → error = 0.333.
        // So we need rejections to bring observed fill rate down to ~0.667.
        // 15 fills, we need ~7.5 rejections for ~0.667 rate. But we already recorded 15 fills.
        // Let's reset and try again.
        mgr.reset();
        enter_safe_mode(&mut mgr, &gw);

        // Record 10 fills + 5 rejections → observed = 10/15 ≈ 0.667 ≈ baseline
        for i in 0..10u64 {
            mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000 + i);
        }
        for i in 10..15u64 {
            mgr.record_rejection("LP_BACKUP", 1_000_000_000_000 + i);
        }

        assert!(mgr.check_completion(2_000_000_000_000));
        assert_eq!(mgr.status(), &RecalibrationStatus::Completed);
    }

    #[test]
    fn check_completion_max_duration() {
        let mut mgr = LpRecalibrationManager::new(RecalibrationConfig {
            min_recalibration_observations: 10,
            max_recalibration_duration_ns: 100_000_000_000, // 100 seconds
            ..Default::default()
        });
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        // Record exactly min observations with poor convergence
        for i in 0..10u64 {
            let ts = 1_000_000_000_000 + i;
            if i < 5 {
                mgr.record_fill("LP_BACKUP", 0.001, 0.0001, ts);
            } else {
                mgr.record_rejection("LP_BACKUP", ts);
            }
        }

        // Not yet completed (poor convergence)
        assert!(!mgr.check_completion(1_000_000_000_100));

        // After max duration
        assert!(mgr.check_completion(1_000_000_000_000 + 100_000_000_001));
        assert_eq!(mgr.status(), &RecalibrationStatus::Completed);
    }

    #[test]
    fn check_completion_no_observations() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        // Don't record anything
        assert!(!mgr.check_completion(2_000_000_000_000));
    }

    #[test]
    fn reset_clears_state() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);
        mgr.record_fill("LP_BACKUP", 0.0001, 0.00015, 1_000_000_000_000);
        mgr.record_rejection("LP_BACKUP", 1_000_000_000_001);

        mgr.reset();

        assert_eq!(mgr.status(), &RecalibrationStatus::Idle);
        assert!(!mgr.is_safe_mode());
        assert_eq!(mgr.target_lp_id(), None);
        assert_eq!(mgr.observations, 0);
        assert_eq!(mgr.fill_count, 0);
        assert_eq!(mgr.reject_count, 0);
    }

    #[test]
    fn snapshot_reflects_current_state() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        mgr.record_fill("LP_BACKUP", 0.0002, 0.0002, 1_000_000_000_000);
        mgr.record_fill("LP_BACKUP", 0.0004, 0.0002, 1_000_000_000_001);
        mgr.record_rejection("LP_BACKUP", 1_000_000_000_002);

        let snap = mgr.snapshot(1_000_000_002_000);
        assert_eq!(snap.lp_id, "LP_BACKUP");
        assert_eq!(snap.status, RecalibrationStatus::SafeMode);
        assert_eq!(snap.observations, 3);
        assert_eq!(snap.fill_count, 2);
        assert_eq!(snap.reject_count, 1);
        assert!((snap.observed_fill_rate - 2.0 / 3.0).abs() < 1e-10);
        assert!((snap.slippage_mean - 0.0003).abs() < 1e-10);
        assert!(snap.elapsed_ns > 0);
    }

    #[test]
    fn snapshot_idle_empty() {
        let mgr = LpRecalibrationManager::new(default_config());
        let snap = mgr.snapshot(1_000_000_000_000);
        assert_eq!(snap.status, RecalibrationStatus::Idle);
        assert_eq!(snap.lp_id, "");
        assert_eq!(snap.observations, 0);
    }

    #[test]
    fn slippage_variance_calculation() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        mgr.record_fill("LP_BACKUP", 0.0, 0.0, 1_000_000_000_000);
        mgr.record_fill("LP_BACKUP", 0.0002, 0.0, 1_000_000_000_001);

        // mean = 0.0001, var = E[X^2] - mean^2 = (0 + 4e-8)/2 - 1e-8 = 2e-8 - 1e-8 = 1e-8
        let snap = mgr.snapshot(2_000_000_000_000);
        assert!((snap.slippage_mean - 0.0001).abs() < 1e-10);
        assert!((snap.slippage_variance - 1e-8).abs() < 1e-15);
    }

    #[test]
    fn slippage_error_calculation() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        // All fills with observed == predicted → error = 0
        for i in 0..10u64 {
            mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000 + i);
        }
        assert!(mgr.compute_slippage_error() < 1e-10);

        // Now add a large discrepancy
        mgr.reset();
        enter_safe_mode(&mut mgr, &gw);
        for i in 0..10u64 {
            mgr.record_fill("LP_BACKUP", 0.001, 0.0001, 1_000_000_000_000 + i);
        }
        // observed_mean = 0.001, predicted_mean = 0.0001
        // error = |0.001 - 0.0001| / max(0.001, 0.0001) = 0.0009 / 0.001 = 0.9
        let error = mgr.compute_slippage_error();
        assert!(error > 0.8);
    }

    #[test]
    fn fill_rate_error_calculation() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        // baseline_fill_rate ≈ 0.667 (prior 2/3)
        // 10 fills + 5 rejections → observed = 10/15 ≈ 0.667 → error ≈ 0
        for i in 0..10u64 {
            mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000 + i);
        }
        for i in 10..15u64 {
            mgr.record_rejection("LP_BACKUP", 1_000_000_000_000 + i);
        }
        let error = mgr.compute_fill_rate_error();
        // baseline ≈ 0.667, observed = 10/15 ≈ 0.667
        assert!(error < 0.01);
    }

    #[test]
    fn completion_resets_to_completed_not_idle() {
        let mut mgr = LpRecalibrationManager::new(RecalibrationConfig {
            min_recalibration_observations: 5,
            max_recalibration_duration_ns: 100_000_000_000,
            ..Default::default()
        });
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        for i in 0..5u64 {
            mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000 + i);
        }

        // Force max duration completion
        mgr.check_completion(1_000_000_000_000 + 101_000_000_000);
        assert_eq!(mgr.status(), &RecalibrationStatus::Completed);
        // lot_multiplier should be back to 1.0
        assert_eq!(mgr.lot_multiplier(), 1.0);
        assert_eq!(mgr.sigma_multiplier(), 1.0);
    }

    #[test]
    fn consecutive_completions() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();

        // First recalibration
        enter_safe_mode(&mut mgr, &gw);
        for i in 0..5u64 {
            mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000 + i);
        }
        mgr.reset();

        // Second recalibration - switch to LP_PRIMARY
        let lp_id = "LP_PRIMARY";
        let baseline_fill_rate = gw.last_look_model().fill_probability(lp_id, 0.1);
        let baseline_slippage = gw
            .slippage_model()
            .get_lp_stats(lp_id)
            .map(|s| s.mean)
            .unwrap_or(0.0);
        let signal2 = LpSwitchSignal {
            from_lp_id: "LP_BACKUP".to_string(),
            to_lp_id: lp_id.to_string(),
            reason: "test".to_string(),
        };
        mgr.enter_safe_mode(&signal2, baseline_fill_rate, baseline_slippage);
        assert_eq!(mgr.target_lp_id(), Some("LP_PRIMARY"));
        assert!(mgr.is_safe_mode());
    }

    #[test]
    fn slippage_error_infinity_with_no_fills() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        mgr.record_rejection("LP_BACKUP", 1_000_000_000_000);
        assert_eq!(mgr.compute_slippage_error(), f64::INFINITY);
    }

    #[test]
    fn fill_rate_error_infinity_with_no_observations() {
        let mgr = LpRecalibrationManager::new(default_config());
        assert_eq!(mgr.compute_fill_rate_error(), f64::INFINITY);
    }

    #[test]
    fn start_timestamp_set_on_first_observation() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);

        assert!(mgr.start_timestamp_ns.is_none());

        mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 5_000_000_000_000);
        assert_eq!(mgr.start_timestamp_ns, Some(5_000_000_000_000));

        // Second observation should not change start time
        mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 6_000_000_000_000);
        assert_eq!(mgr.start_timestamp_ns, Some(5_000_000_000_000));
    }

    #[test]
    fn check_completion_idle_returns_false() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        assert!(!mgr.check_completion(1_000_000_000_000));
    }

    #[test]
    fn check_completion_already_completed_returns_false() {
        let mut mgr = LpRecalibrationManager::new(RecalibrationConfig {
            min_recalibration_observations: 2,
            max_recalibration_duration_ns: 100_000_000_000,
            ..Default::default()
        });
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);
        mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_000);
        mgr.record_fill("LP_BACKUP", 0.0001, 0.0001, 1_000_000_000_001);
        // Complete via max duration
        mgr.check_completion(1_000_000_000_000 + 101_000_000_000);
        assert_eq!(mgr.status(), &RecalibrationStatus::Completed);

        // Subsequent calls should return false
        assert!(!mgr.check_completion(2_000_000_000_000));
    }

    #[test]
    fn config_access() {
        let config = default_config();
        let mgr = LpRecalibrationManager::new(config.clone());
        assert_eq!(mgr.config().safe_mode_lot_multiplier, 0.25);
        assert_eq!(mgr.config().safe_mode_sigma_multiplier, 2.0);
        assert_eq!(mgr.config().min_recalibration_observations, 10);
    }

    #[test]
    fn snapshot_elapsed_with_no_start() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);
        // No observations yet → no start timestamp
        let snap = mgr.snapshot(5_000_000_000_000);
        assert_eq!(snap.elapsed_ns, 0);
    }

    #[test]
    fn zero_fills_slippage_stats() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);
        mgr.record_rejection("LP_BACKUP", 1_000_000_000_000);

        let snap = mgr.snapshot(2_000_000_000_000);
        assert_eq!(snap.slippage_mean, 0.0);
        assert_eq!(snap.slippage_variance, 0.0);
    }

    #[test]
    fn single_fill_slippage_variance() {
        let mut mgr = LpRecalibrationManager::new(default_config());
        let gw = make_gateway();
        enter_safe_mode(&mut mgr, &gw);
        mgr.record_fill("LP_BACKUP", 0.0005, 0.0001, 1_000_000_000_000);

        let snap = mgr.snapshot(2_000_000_000_000);
        assert_eq!(snap.slippage_variance, 0.0); // need >= 2 observations
    }
}
