use tracing::warn;

use crate::limits::{Result, RiskError};

#[derive(Debug, Clone)]
pub struct DynamicRiskBarrierConfig {
    pub staleness_threshold_ms: u64,
    pub warning_threshold_ratio: f64,
    pub min_lot_multiplier: f64,
    pub default_lot_size: u64,
    pub max_lot_size: u64,
    pub min_lot_size: u64,
}

impl Default for DynamicRiskBarrierConfig {
    fn default() -> Self {
        Self {
            staleness_threshold_ms: 5000,
            warning_threshold_ratio: 0.4,
            min_lot_multiplier: 0.01,
            default_lot_size: 100_000,
            max_lot_size: 1_000_000,
            min_lot_size: 1_000,
        }
    }
}

impl DynamicRiskBarrierConfig {
    pub fn warning_threshold_ms(&self) -> u64 {
        (self.staleness_threshold_ms as f64 * self.warning_threshold_ratio) as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierStatus {
    Normal,
    Warning,
    Degraded,
    Halted,
}

#[derive(Debug, Clone)]
pub struct StalenessInfo {
    pub staleness_ms: u64,
    pub lot_multiplier: f64,
    pub status: BarrierStatus,
    pub effective_lot_size: u64,
}

#[derive(Debug, Clone)]
pub struct BarrierResult {
    pub allowed: bool,
    pub staleness_ms: u64,
    pub lot_multiplier: f64,
    pub status: BarrierStatus,
    pub effective_lot_size: u64,
}

pub struct DynamicRiskBarrier {
    config: DynamicRiskBarrierConfig,
}

impl DynamicRiskBarrier {
    pub fn new(config: DynamicRiskBarrierConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DynamicRiskBarrierConfig {
        &self.config
    }

    pub fn compute_lot_multiplier(&self, staleness_ms: u64) -> f64 {
        if staleness_ms >= self.config.staleness_threshold_ms {
            return 0.0;
        }
        let ratio = staleness_ms as f64 / self.config.staleness_threshold_ms as f64;
        (1.0 - ratio * ratio).max(0.0)
    }

    pub fn compute_status(&self, staleness_ms: u64, lot_multiplier: f64) -> BarrierStatus {
        if lot_multiplier < self.config.min_lot_multiplier || lot_multiplier <= 0.0 {
            return BarrierStatus::Halted;
        }
        if staleness_ms >= self.config.staleness_threshold_ms {
            return BarrierStatus::Halted;
        }
        if staleness_ms >= self.config.warning_threshold_ms() {
            return BarrierStatus::Degraded;
        }
        if staleness_ms >= self.config.warning_threshold_ms() / 2 {
            return BarrierStatus::Warning;
        }
        BarrierStatus::Normal
    }

    pub fn evaluate(&self, staleness_ms: u64) -> BarrierResult {
        let lot_multiplier = self.compute_lot_multiplier(staleness_ms);
        let status = self.compute_status(staleness_ms, lot_multiplier);
        let effective_lot_size = self.compute_effective_lot(lot_multiplier);
        let allowed =
            status != BarrierStatus::Halted && effective_lot_size >= self.config.min_lot_size;

        if status == BarrierStatus::Warning {
            warn!(
                staleness_ms = staleness_ms,
                threshold_ms = self.config.staleness_threshold_ms,
                "DynamicRiskBarrier: approaching staleness threshold"
            );
        }
        if status == BarrierStatus::Degraded {
            warn!(
                staleness_ms = staleness_ms,
                lot_multiplier = lot_multiplier,
                threshold_ms = self.config.staleness_threshold_ms,
                "DynamicRiskBarrier: degraded — lot size reduced"
            );
        }
        if status == BarrierStatus::Halted {
            warn!(
                staleness_ms = staleness_ms,
                threshold_ms = self.config.staleness_threshold_ms,
                "DynamicRiskBarrier: halted — staleness exceeds threshold"
            );
        }

        BarrierResult {
            allowed,
            staleness_ms,
            lot_multiplier,
            status,
            effective_lot_size,
        }
    }

    pub fn compute_effective_lot(&self, lot_multiplier: f64) -> u64 {
        if lot_multiplier < self.config.min_lot_multiplier {
            return 0;
        }
        let effective = (self.config.default_lot_size as f64 * lot_multiplier) as u64;
        effective.clamp(0, self.config.max_lot_size)
    }

    pub fn validate_order(&self, staleness_ms: u64) -> Result<StalenessInfo> {
        let result = self.evaluate(staleness_ms);

        if result.status == BarrierStatus::Halted {
            return Err(RiskError::StalenessHalted {
                staleness_ms: result.staleness_ms,
                threshold_ms: self.config.staleness_threshold_ms,
            });
        }

        if !result.allowed || result.effective_lot_size < self.config.min_lot_size {
            return Err(RiskError::StalenessDegraded {
                staleness_ms: result.staleness_ms,
                lot_multiplier: result.lot_multiplier,
                effective_lot_size: result.effective_lot_size,
                min_lot_size: self.config.min_lot_size,
            });
        }

        Ok(StalenessInfo {
            staleness_ms: result.staleness_ms,
            lot_multiplier: result.lot_multiplier,
            status: result.status,
            effective_lot_size: result.effective_lot_size,
        })
    }

    pub fn staleness_info(&self, staleness_ms: u64) -> StalenessInfo {
        let result = self.evaluate(staleness_ms);
        StalenessInfo {
            staleness_ms: result.staleness_ms,
            lot_multiplier: result.lot_multiplier,
            status: result.status,
            effective_lot_size: result.effective_lot_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_barrier() -> DynamicRiskBarrier {
        DynamicRiskBarrier::new(DynamicRiskBarrierConfig::default())
    }

    // --- compute_lot_multiplier tests ---

    #[test]
    fn test_lot_multiplier_zero_staleness() {
        let barrier = default_barrier();
        assert!((barrier.compute_lot_multiplier(0) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_lot_multiplier_quadratic_decay() {
        let barrier = default_barrier();
        let m = barrier.compute_lot_multiplier(2500);
        let expected: f64 = 1.0 - (2500.0_f64 / 5000.0_f64).powi(2);
        assert!((m - expected).abs() < 1e-10);
        assert!((m - 0.75).abs() < 1e-10);
    }

    #[test]
    fn test_lot_multiplier_half_threshold() {
        let barrier = default_barrier();
        let m = barrier.compute_lot_multiplier(2500);
        assert!((m - 0.75).abs() < 1e-10);
    }

    #[test]
    fn test_lot_multiplier_at_threshold() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_lot_multiplier(5000), 0.0);
    }

    #[test]
    fn test_lot_multiplier_beyond_threshold() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_lot_multiplier(10000), 0.0);
    }

    #[test]
    fn test_lot_multiplier_monotonic_decrease() {
        let barrier = default_barrier();
        let mut prev = 1.0;
        for ms in 0..=5000 {
            let m = barrier.compute_lot_multiplier(ms);
            assert!(
                m <= prev + 1e-10,
                "lot_multiplier not monotonic: at {}ms, {} > {}",
                ms,
                m,
                prev
            );
            prev = m;
        }
    }

    #[test]
    fn test_lot_multiplier_non_negative() {
        let barrier = default_barrier();
        for ms in [0, 100, 500, 1000, 2500, 4000, 4999, 5000, 10000, u64::MAX] {
            assert!(
                barrier.compute_lot_multiplier(ms) >= 0.0,
                "negative lot_multiplier at {}ms",
                ms
            );
        }
    }

    #[test]
    fn test_lot_multiplier_custom_threshold() {
        let config = DynamicRiskBarrierConfig {
            staleness_threshold_ms: 10_000,
            ..Default::default()
        };
        let barrier = DynamicRiskBarrier::new(config);
        let m = barrier.compute_lot_multiplier(5000);
        let expected: f64 = 1.0 - (5000.0_f64 / 10000.0_f64).powi(2);
        assert!((m - expected).abs() < 1e-10);
        assert!((m - 0.75).abs() < 1e-10);

        assert_eq!(barrier.compute_lot_multiplier(10_000), 0.0);
    }

    // --- compute_status tests ---

    #[test]
    fn test_status_normal() {
        let barrier = default_barrier();
        let m = barrier.compute_lot_multiplier(500);
        assert_eq!(barrier.compute_status(500, m), BarrierStatus::Normal);
    }

    #[test]
    fn test_status_warning() {
        let barrier = default_barrier();
        let warning_ms = barrier.config().warning_threshold_ms();
        let half_warning = warning_ms / 2;
        let m = barrier.compute_lot_multiplier(half_warning + 1);
        assert_eq!(
            barrier.compute_status(half_warning + 1, m),
            BarrierStatus::Warning
        );
    }

    #[test]
    fn test_status_degraded() {
        let barrier = default_barrier();
        let warning_ms = barrier.config().warning_threshold_ms();
        let m = barrier.compute_lot_multiplier(warning_ms);
        assert_eq!(
            barrier.compute_status(warning_ms, m),
            BarrierStatus::Degraded
        );
    }

    #[test]
    fn test_status_halted_at_threshold() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_status(5000, 0.0), BarrierStatus::Halted);
    }

    #[test]
    fn test_status_halted_below_min_multiplier() {
        let barrier = default_barrier();
        let m = 0.005;
        assert_eq!(barrier.compute_status(100, m), BarrierStatus::Halted);
    }

    #[test]
    fn test_status_halted_beyond_threshold() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_status(9999, 0.0), BarrierStatus::Halted);
    }

    // --- evaluate tests ---

    #[test]
    fn test_evaluate_normal() {
        let barrier = default_barrier();
        let result = barrier.evaluate(0);
        assert!(result.allowed);
        assert_eq!(result.lot_multiplier, 1.0);
        assert_eq!(result.status, BarrierStatus::Normal);
        assert_eq!(result.staleness_ms, 0);
        assert_eq!(result.effective_lot_size, barrier.config().default_lot_size);
    }

    #[test]
    fn test_evaluate_degraded() {
        let barrier = default_barrier();
        let warning_ms = barrier.config().warning_threshold_ms();
        let result = barrier.evaluate(warning_ms);
        assert!(result.allowed);
        assert_eq!(result.status, BarrierStatus::Degraded);
        assert!(result.lot_multiplier > 0.0);
        assert!(result.lot_multiplier < 1.0);
    }

    #[test]
    fn test_evaluate_halted() {
        let barrier = default_barrier();
        let result = barrier.evaluate(5000);
        assert!(!result.allowed);
        assert_eq!(result.status, BarrierStatus::Halted);
        assert_eq!(result.lot_multiplier, 0.0);
        assert_eq!(result.effective_lot_size, 0);
    }

    #[test]
    fn test_evaluate_effective_lot_scaled() {
        let barrier = default_barrier();
        let result = barrier.evaluate(2500);
        let expected_lot = (100_000.0 * 0.75) as u64;
        assert_eq!(result.effective_lot_size, expected_lot);
    }

    #[test]
    fn test_evaluate_max_lot_clamp() {
        let config = DynamicRiskBarrierConfig {
            default_lot_size: 2_000_000,
            max_lot_size: 1_000_000,
            ..Default::default()
        };
        let barrier = DynamicRiskBarrier::new(config);
        let result = barrier.evaluate(0);
        assert_eq!(result.effective_lot_size, 1_000_000);
    }

    #[test]
    fn test_evaluate_not_allowed_when_below_min_lot() {
        let config = DynamicRiskBarrierConfig {
            min_lot_size: 50_000,
            staleness_threshold_ms: 5000,
            ..Default::default()
        };
        let barrier = DynamicRiskBarrier::new(config);
        // At 4800ms, lot_multiplier ≈ 1 - (4800/5000)^2 = 1 - 0.9216 = 0.0784
        // effective_lot = 100000 * 0.0784 = 7840 < 50000
        let result = barrier.evaluate(4800);
        assert!(!result.allowed);
        assert_ne!(result.status, BarrierStatus::Halted);
    }

    // --- compute_effective_lot tests ---

    #[test]
    fn test_effective_lot_zero_when_below_min_multiplier() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_effective_lot(0.0), 0);
        assert_eq!(barrier.compute_effective_lot(0.005), 0);
    }

    #[test]
    fn test_effective_lot_full() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_effective_lot(1.0), 100_000);
    }

    #[test]
    fn test_effective_lot_half() {
        let barrier = default_barrier();
        assert_eq!(barrier.compute_effective_lot(0.5), 50_000);
    }

    // --- validate_order tests ---

    #[test]
    fn test_validate_order_normal() {
        let barrier = default_barrier();
        let info = barrier.validate_order(0).unwrap();
        assert_eq!(info.lot_multiplier, 1.0);
        assert_eq!(info.status, BarrierStatus::Normal);
        assert_eq!(info.effective_lot_size, 100_000);
    }

    #[test]
    fn test_validate_order_halted_error() {
        let barrier = default_barrier();
        let result = barrier.validate_order(5000);
        assert!(result.is_err());
        match result.unwrap_err() {
            RiskError::StalenessHalted {
                staleness_ms,
                threshold_ms,
            } => {
                assert_eq!(staleness_ms, 5000);
                assert_eq!(threshold_ms, 5000);
            }
            e => panic!("unexpected error: {}", e),
        }
    }

    #[test]
    fn test_validate_order_degraded_error_when_below_min_lot() {
        let config = DynamicRiskBarrierConfig {
            min_lot_size: 50_000,
            ..Default::default()
        };
        let barrier = DynamicRiskBarrier::new(config);
        let result = barrier.validate_order(4800);
        assert!(result.is_err());
        match result.unwrap_err() {
            RiskError::StalenessDegraded {
                effective_lot_size,
                min_lot_size,
                ..
            } => {
                assert_eq!(min_lot_size, 50_000);
                assert!(effective_lot_size < min_lot_size);
            }
            e => panic!("unexpected error: {}", e),
        }
    }

    #[test]
    fn test_validate_order_degraded_ok_when_above_min_lot() {
        let barrier = default_barrier();
        let info = barrier.validate_order(2500).unwrap();
        assert_eq!(info.status, BarrierStatus::Degraded);
        assert!(info.effective_lot_size >= barrier.config().min_lot_size);
    }

    #[test]
    fn test_validate_order_no_market_data() {
        let barrier = default_barrier();
        let result = barrier.validate_order(u64::MAX);
        assert!(result.is_err());
    }

    // --- staleness_info tests ---

    #[test]
    fn test_staleness_info() {
        let barrier = default_barrier();
        let info = barrier.staleness_info(1000);
        assert_eq!(info.staleness_ms, 1000);
        let expected_m: f64 = 1.0 - (1000.0_f64 / 5000.0_f64).powi(2);
        assert!((info.lot_multiplier - expected_m).abs() < 1e-10);
        assert_eq!(info.status, BarrierStatus::Warning);
    }

    // --- config tests ---

    #[test]
    fn test_default_config() {
        let config = DynamicRiskBarrierConfig::default();
        assert_eq!(config.staleness_threshold_ms, 5000);
        assert!((config.warning_threshold_ratio - 0.4).abs() < 1e-10);
        assert!((config.min_lot_multiplier - 0.01).abs() < 1e-10);
        assert_eq!(config.default_lot_size, 100_000);
        assert_eq!(config.max_lot_size, 1_000_000);
        assert_eq!(config.min_lot_size, 1_000);
    }

    #[test]
    fn test_warning_threshold_ms() {
        let config = DynamicRiskBarrierConfig::default();
        assert_eq!(config.warning_threshold_ms(), 2000);
    }

    #[test]
    fn test_custom_config_warning_threshold() {
        let config = DynamicRiskBarrierConfig {
            staleness_threshold_ms: 10_000,
            warning_threshold_ratio: 0.5,
            ..Default::default()
        };
        assert_eq!(config.warning_threshold_ms(), 5000);
    }

    // --- penalty curve tests ---

    #[test]
    fn test_penalty_curve_shape() {
        let barrier = default_barrier();
        // Verify the quadratic curve: at 0% → 1.0, at 25% → 0.9375, at 50% → 0.75, at 75% → 0.4375, at 100% → 0.0
        let points = vec![
            (0, 1.0),
            (1250, 0.9375),
            (2500, 0.75),
            (3750, 0.4375),
            (5000, 0.0),
        ];
        for (ms, expected) in points {
            let m = barrier.compute_lot_multiplier(ms);
            assert!(
                (m - expected).abs() < 1e-10,
                "at {}ms: expected {}, got {}",
                ms,
                expected,
                m
            );
        }
    }

    #[test]
    fn test_penalty_curve_convexity() {
        let barrier = default_barrier();
        // Quadratic decay should be convex: second difference should be positive (constant)
        let m0 = barrier.compute_lot_multiplier(1000);
        let m1 = barrier.compute_lot_multiplier(2000);
        let m2 = barrier.compute_lot_multiplier(3000);
        // For f(x) = 1 - (x/T)^2, f''(x) = -2/T^2 < 0 (concave in x, but we check monotonic decrease)
        assert!(m0 > m1);
        assert!(m1 > m2);
        // Decrease accelerates: (m0-m1) < (m1-m2)
        let d1 = m0 - m1;
        let d2 = m1 - m2;
        assert!(d2 > d1, "penalty should accelerate: d1={}, d2={}", d1, d2);
    }
}
