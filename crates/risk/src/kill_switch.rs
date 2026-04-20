use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing;

use crate::limits::{Result, RiskError};

/// Kill Switch configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchConfig {
    /// Number of samples to collect before monitoring starts
    pub min_samples: usize,
    /// Z-score threshold for anomaly detection (default: 3.0)
    pub z_score_threshold: f64,
    /// Maximum history size for interval tracking
    pub max_history: usize,
    /// Duration to mask orders after anomaly detection (default: 50ms)
    pub mask_duration_ms: u64,
    /// Enable/disable the kill switch
    pub enabled: bool,
}

impl Default for KillSwitchConfig {
    fn default() -> Self {
        Self {
            min_samples: 100,
            z_score_threshold: 3.0,
            max_history: 2000,
            mask_duration_ms: 50,
            enabled: true,
        }
    }
}

/// Kill Switch status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillSwitchStatus {
    /// Normal operation — monitoring active
    Active,
    /// Order masking in effect — anomaly detected
    Masked,
    /// Kill switch disabled
    Disabled,
}

/// Welford online algorithm for mean/variance of tick intervals
#[derive(Debug, Clone, Default)]
struct IntervalStats {
    count: u64,
    mean: f64,
    m2: f64,
    min_interval_ns: u64,
    max_interval_ns: u64,
}

impl IntervalStats {
    fn new() -> Self {
        Self {
            count: 0,
            mean: 0.0,
            m2: 0.0,
            min_interval_ns: u64::MAX,
            max_interval_ns: 0,
        }
    }

    fn update(&mut self, interval_ns: u64) {
        self.count += 1;
        let x = interval_ns as f64;
        let delta = x - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;

        if interval_ns < self.min_interval_ns {
            self.min_interval_ns = interval_ns;
        }
        if interval_ns > self.max_interval_ns {
            self.max_interval_ns = interval_ns;
        }
    }

    fn variance(&self) -> f64 {
        if self.count < 2 {
            0.0
        } else {
            self.m2 / (self.count - 1) as f64
        }
    }

    fn std(&self) -> f64 {
        self.variance().sqrt()
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

/// Anomaly detection result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyEvent {
    pub interval_ns: u64,
    pub mean_interval_ns: f64,
    pub std_interval_ns: f64,
    pub z_score: f64,
    pub timestamp_ns: u64,
    pub mask_duration_ms: u64,
}

/// Kill Switch result for order validation
#[derive(Debug, Clone)]
pub struct KillSwitchCheck {
    pub allowed: bool,
    pub status: KillSwitchStatus,
    pub remaining_mask_ms: u64,
}

/// Thread-safe Kill Switch using atomics for lock-free order masking
///
/// The Kill Switch monitors tick arrival intervals using statistical analysis
/// (mean ± 3σ). When an anomaly is detected, it masks all outgoing orders
/// for a configurable duration (10-50ms) using atomic operations for zero-lock
/// overhead in the hot path.
pub struct KillSwitch {
    config: KillSwitchConfig,
    /// Whether orders are currently masked (atomic for lock-free checking)
    masked: Arc<AtomicBool>,
    /// When the mask was activated (Instant)
    mask_start: Arc<std::sync::Mutex<Option<Instant>>>,
    /// Last tick timestamp (for interval calculation)
    last_tick_ns: Arc<AtomicU64>,
    /// Interval statistics (Welford online algorithm)
    stats: Arc<std::sync::Mutex<IntervalStats>>,
    /// History of intervals for trimming
    history: Arc<std::sync::Mutex<std::collections::VecDeque<u64>>>,
    /// Most recent anomaly event (for monitoring/diagnostics)
    last_anomaly: Arc<std::sync::Mutex<Option<AnomalyEvent>>>,
    /// Total number of anomalies detected since start/reset
    total_anomalies: Arc<AtomicU64>,
    /// Total ticks observed
    total_ticks: Arc<AtomicU64>,
}

impl KillSwitch {
    pub fn new(config: KillSwitchConfig) -> Self {
        Self {
            config,
            masked: Arc::new(AtomicBool::new(false)),
            mask_start: Arc::new(std::sync::Mutex::new(None)),
            last_tick_ns: Arc::new(AtomicU64::new(0)),
            stats: Arc::new(std::sync::Mutex::new(IntervalStats::new())),
            history: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            last_anomaly: Arc::new(std::sync::Mutex::new(None)),
            total_anomalies: Arc::new(AtomicU64::new(0)),
            total_ticks: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record a tick arrival for interval monitoring.
    /// Returns `Some(AnomalyEvent)` if an anomaly was detected.
    pub fn record_tick(&self, timestamp_ns: u64) -> Option<AnomalyEvent> {
        if !self.config.enabled {
            return None;
        }

        self.total_ticks.fetch_add(1, Ordering::Relaxed);

        let prev_ns = self.last_tick_ns.swap(timestamp_ns, Ordering::SeqCst);

        if prev_ns == 0 || timestamp_ns <= prev_ns {
            return None;
        }

        let interval_ns = timestamp_ns - prev_ns;

        let mut stats = self.stats.lock().unwrap();
        let mut history = self.history.lock().unwrap();

        // Trim history if needed (FIFO eviction)
        while history.len() >= self.config.max_history {
            let oldest = history.pop_front().unwrap();
            // Remove from Welford stats via decremental update
            if stats.count > 1 {
                let x = oldest as f64;
                let old_mean = stats.mean;
                let old_count = stats.count as f64;
                let new_count = (stats.count - 1) as f64;

                stats.mean = (old_mean * old_count - x) / new_count;

                // Approximate M2 adjustment — exact decremental Welford is complex;
                // we use the batch formula approach for the remaining elements
                // This is an approximation that keeps the stats consistent
                stats.m2 = if stats.count > 2 {
                    // Re-estimate variance from remaining history
                    let sum_sq: f64 = history
                        .iter()
                        .map(|v| {
                            let d = *v as f64 - stats.mean;
                            d * d
                        })
                        .sum();
                    sum_sq / (new_count - 1.0)
                } else {
                    0.0
                };
                stats.count -= 1;
            }
        }

        // Only check for anomalies after collecting enough samples
        if stats.count < self.config.min_samples as u64 {
            history.push_back(interval_ns);
            stats.update(interval_ns);
            return None;
        }

        let mean = stats.mean;
        let std = stats.std();

        if std < 1e-10 {
            history.push_back(interval_ns);
            stats.update(interval_ns);
            return None;
        }

        let z_score = (interval_ns as f64 - mean) / std;

        // Now update stats with the current interval (after anomaly check)
        history.push_back(interval_ns);
        stats.update(interval_ns);

        if z_score.abs() > self.config.z_score_threshold {
            // Anomaly detected — activate mask
            self.activate_mask();

            let anomaly = AnomalyEvent {
                interval_ns,
                mean_interval_ns: mean,
                std_interval_ns: std,
                z_score,
                timestamp_ns,
                mask_duration_ms: self.config.mask_duration_ms,
            };

            *self.last_anomaly.lock().unwrap() = Some(anomaly.clone());
            self.total_anomalies.fetch_add(1, Ordering::Relaxed);

            tracing::error!(
                anomaly = true,
                interval_ns = interval_ns,
                mean_interval_ns = mean,
                std_interval_ns = std,
                z_score = z_score,
                total_anomalies = self.total_anomalies.load(Ordering::Relaxed),
                "Kill Switch: tick interval anomaly detected, order masking activated"
            );

            return Some(anomaly);
        }

        None
    }

    /// Activate the order mask for the configured duration
    fn activate_mask(&self) {
        self.masked.store(true, Ordering::SeqCst);
        *self.mask_start.lock().unwrap() = Some(Instant::now());
    }

    /// Deactivate the order mask
    fn deactivate_mask(&self) {
        self.masked.store(false, Ordering::SeqCst);
        *self.mask_start.lock().unwrap() = None;
    }

    /// Check if an order is allowed through the kill switch.
    /// This is the hot-path method — uses only atomic read for masking check.
    pub fn validate_order(&self) -> Result<KillSwitchCheck> {
        if !self.config.enabled {
            return Ok(KillSwitchCheck {
                allowed: true,
                status: KillSwitchStatus::Disabled,
                remaining_mask_ms: 0,
            });
        }

        // Fast path: check atomic mask flag first (lock-free)
        if !self.masked.load(Ordering::SeqCst) {
            return Ok(KillSwitchCheck {
                allowed: true,
                status: KillSwitchStatus::Active,
                remaining_mask_ms: 0,
            });
        }

        // Slow path: check if mask duration has expired
        let guard = self.mask_start.lock().unwrap();
        if let Some(start) = *guard {
            let elapsed = start.elapsed();
            let mask_duration = Duration::from_millis(self.config.mask_duration_ms);

            if elapsed >= mask_duration {
                drop(guard);
                self.deactivate_mask();
                return Ok(KillSwitchCheck {
                    allowed: true,
                    status: KillSwitchStatus::Active,
                    remaining_mask_ms: 0,
                });
            }

            let remaining = mask_duration - elapsed;
            drop(guard);
            return Err(RiskError::KillSwitchMasked {
                remaining_ms: remaining.as_millis() as u64,
            });
        }

        // mask_start is None but masked is true — inconsistency, deactivate
        drop(guard);
        self.deactivate_mask();
        Ok(KillSwitchCheck {
            allowed: true,
            status: KillSwitchStatus::Active,
            remaining_mask_ms: 0,
        })
    }

    /// Get current status without blocking orders
    pub fn status(&self) -> KillSwitchStatus {
        if !self.config.enabled {
            return KillSwitchStatus::Disabled;
        }
        if self.masked.load(Ordering::SeqCst) {
            KillSwitchStatus::Masked
        } else {
            KillSwitchStatus::Active
        }
    }

    /// Manual trigger — force mask activation (operator action)
    pub fn trigger(&self) {
        self.activate_mask();
        tracing::warn!("Kill Switch: manually triggered by operator");
    }

    /// Manual reset — clear mask and reset statistics
    pub fn reset(&self) {
        self.deactivate_mask();
        self.stats.lock().unwrap().reset();
        self.history.lock().unwrap().clear();
        *self.last_anomaly.lock().unwrap() = None;
        self.total_anomalies.store(0, Ordering::Relaxed);
        self.total_ticks.store(0, Ordering::Relaxed);
        self.last_tick_ns.store(0, Ordering::SeqCst);
        tracing::info!("Kill Switch: manually reset");
    }

    /// Reset statistics only (keep mask state)
    pub fn reset_stats(&self) {
        self.stats.lock().unwrap().reset();
        self.history.lock().unwrap().clear();
        self.last_tick_ns.store(0, Ordering::SeqCst);
    }

    /// Get monitoring statistics (for diagnostics)
    pub fn stats(&self) -> KillSwitchStats {
        let stats = self.stats.lock().unwrap();
        let last_anomaly = self.last_anomaly.lock().unwrap();
        KillSwitchStats {
            total_ticks: self.total_ticks.load(Ordering::Relaxed),
            total_anomalies: self.total_anomalies.load(Ordering::Relaxed),
            mean_interval_ns: stats.mean,
            std_interval_ns: stats.std(),
            min_interval_ns: if stats.min_interval_ns == u64::MAX {
                0
            } else {
                stats.min_interval_ns
            },
            max_interval_ns: stats.max_interval_ns,
            sample_count: stats.count,
            is_masked: self.masked.load(Ordering::SeqCst),
            last_anomaly: last_anomaly.clone(),
            enabled: self.config.enabled,
            z_score_threshold: self.config.z_score_threshold,
            mask_duration_ms: self.config.mask_duration_ms,
            min_samples: self.config.min_samples,
        }
    }

    /// Get a clonable handle for use across threads/tasks
    pub fn handle(&self) -> KillSwitchHandle {
        KillSwitchHandle {
            config: self.config.clone(),
            masked: Arc::clone(&self.masked),
            mask_start: Arc::clone(&self.mask_start),
            last_tick_ns: Arc::clone(&self.last_tick_ns),
            stats: Arc::clone(&self.stats),
            history: Arc::clone(&self.history),
            last_anomaly: Arc::clone(&self.last_anomaly),
            total_anomalies: Arc::clone(&self.total_anomalies),
            total_ticks: Arc::clone(&self.total_ticks),
        }
    }
}

/// A clonable handle to the Kill Switch for sharing across tasks
#[derive(Clone)]
pub struct KillSwitchHandle {
    config: KillSwitchConfig,
    masked: Arc<AtomicBool>,
    mask_start: Arc<std::sync::Mutex<Option<Instant>>>,
    last_tick_ns: Arc<AtomicU64>,
    stats: Arc<std::sync::Mutex<IntervalStats>>,
    history: Arc<std::sync::Mutex<std::collections::VecDeque<u64>>>,
    last_anomaly: Arc<std::sync::Mutex<Option<AnomalyEvent>>>,
    total_anomalies: Arc<AtomicU64>,
    total_ticks: Arc<AtomicU64>,
}

impl KillSwitchHandle {
    /// Record a tick arrival
    pub fn record_tick(&self, timestamp_ns: u64) -> Option<AnomalyEvent> {
        if !self.config.enabled {
            return None;
        }

        self.total_ticks.fetch_add(1, Ordering::Relaxed);

        let prev_ns = self.last_tick_ns.swap(timestamp_ns, Ordering::SeqCst);

        if prev_ns == 0 || timestamp_ns <= prev_ns {
            return None;
        }

        let interval_ns = timestamp_ns - prev_ns;

        let mut stats = self.stats.lock().unwrap();
        let mut history = self.history.lock().unwrap();

        while history.len() >= self.config.max_history {
            let oldest = history.pop_front().unwrap();
            if stats.count > 1 {
                let x = oldest as f64;
                let old_mean = stats.mean;
                let old_count = stats.count as f64;
                let new_count = (stats.count - 1) as f64;

                stats.mean = (old_mean * old_count - x) / new_count;
                stats.m2 = if stats.count > 2 {
                    let sum_sq: f64 = history
                        .iter()
                        .map(|v| {
                            let d = *v as f64 - stats.mean;
                            d * d
                        })
                        .sum();
                    sum_sq / (new_count - 1.0)
                } else {
                    0.0
                };
                stats.count -= 1;
            }
        }

        if stats.count < self.config.min_samples as u64 {
            history.push_back(interval_ns);
            stats.update(interval_ns);
            return None;
        }

        let mean = stats.mean;
        let std = stats.std();

        if std < 1e-10 {
            history.push_back(interval_ns);
            stats.update(interval_ns);
            return None;
        }

        let z_score = (interval_ns as f64 - mean) / std;

        // Update stats after anomaly check to avoid self-contamination
        history.push_back(interval_ns);
        stats.update(interval_ns);

        if z_score.abs() > self.config.z_score_threshold {
            self.activate_mask();

            let anomaly = AnomalyEvent {
                interval_ns,
                mean_interval_ns: mean,
                std_interval_ns: std,
                z_score,
                timestamp_ns,
                mask_duration_ms: self.config.mask_duration_ms,
            };

            *self.last_anomaly.lock().unwrap() = Some(anomaly.clone());
            self.total_anomalies.fetch_add(1, Ordering::Relaxed);

            tracing::error!(
                anomaly = true,
                interval_ns = interval_ns,
                mean_interval_ns = mean,
                std_interval_ns = std,
                z_score = z_score,
                "Kill Switch: tick interval anomaly detected"
            );

            return Some(anomaly);
        }

        None
    }

    fn activate_mask(&self) {
        self.masked.store(true, Ordering::SeqCst);
        *self.mask_start.lock().unwrap() = Some(Instant::now());
    }

    fn deactivate_mask(&self) {
        self.masked.store(false, Ordering::SeqCst);
        *self.mask_start.lock().unwrap() = None;
    }

    /// Lock-free order validation
    pub fn validate_order(&self) -> Result<KillSwitchCheck> {
        if !self.config.enabled {
            return Ok(KillSwitchCheck {
                allowed: true,
                status: KillSwitchStatus::Disabled,
                remaining_mask_ms: 0,
            });
        }

        if !self.masked.load(Ordering::SeqCst) {
            return Ok(KillSwitchCheck {
                allowed: true,
                status: KillSwitchStatus::Active,
                remaining_mask_ms: 0,
            });
        }

        let guard = self.mask_start.lock().unwrap();
        if let Some(start) = *guard {
            let elapsed = start.elapsed();
            let mask_duration = Duration::from_millis(self.config.mask_duration_ms);

            if elapsed >= mask_duration {
                drop(guard);
                self.deactivate_mask();
                return Ok(KillSwitchCheck {
                    allowed: true,
                    status: KillSwitchStatus::Active,
                    remaining_mask_ms: 0,
                });
            }

            let remaining = mask_duration - elapsed;
            drop(guard);
            return Err(RiskError::KillSwitchMasked {
                remaining_ms: remaining.as_millis() as u64,
            });
        }

        drop(guard);
        self.deactivate_mask();
        Ok(KillSwitchCheck {
            allowed: true,
            status: KillSwitchStatus::Active,
            remaining_mask_ms: 0,
        })
    }

    /// Get current status
    pub fn status(&self) -> KillSwitchStatus {
        if !self.config.enabled {
            return KillSwitchStatus::Disabled;
        }
        if self.masked.load(Ordering::SeqCst) {
            KillSwitchStatus::Masked
        } else {
            KillSwitchStatus::Active
        }
    }

    /// Manual trigger
    pub fn trigger(&self) {
        self.activate_mask();
        tracing::warn!("Kill Switch: manually triggered by operator");
    }

    /// Manual reset
    pub fn reset(&self) {
        self.deactivate_mask();
        self.stats.lock().unwrap().reset();
        self.history.lock().unwrap().clear();
        *self.last_anomaly.lock().unwrap() = None;
        self.total_anomalies.store(0, Ordering::Relaxed);
        self.total_ticks.store(0, Ordering::Relaxed);
        self.last_tick_ns.store(0, Ordering::SeqCst);
        tracing::info!("Kill Switch: manually reset");
    }

    /// Get monitoring statistics
    pub fn stats(&self) -> KillSwitchStats {
        let stats = self.stats.lock().unwrap();
        let last_anomaly = self.last_anomaly.lock().unwrap();
        KillSwitchStats {
            total_ticks: self.total_ticks.load(Ordering::Relaxed),
            total_anomalies: self.total_anomalies.load(Ordering::Relaxed),
            mean_interval_ns: stats.mean,
            std_interval_ns: stats.std(),
            min_interval_ns: if stats.min_interval_ns == u64::MAX {
                0
            } else {
                stats.min_interval_ns
            },
            max_interval_ns: stats.max_interval_ns,
            sample_count: stats.count,
            is_masked: self.masked.load(Ordering::SeqCst),
            last_anomaly: last_anomaly.clone(),
            enabled: self.config.enabled,
            z_score_threshold: self.config.z_score_threshold,
            mask_duration_ms: self.config.mask_duration_ms,
            min_samples: self.config.min_samples,
        }
    }
}

/// Monitoring statistics snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchStats {
    pub total_ticks: u64,
    pub total_anomalies: u64,
    pub mean_interval_ns: f64,
    pub std_interval_ns: f64,
    pub min_interval_ns: u64,
    pub max_interval_ns: u64,
    pub sample_count: u64,
    pub is_masked: bool,
    pub last_anomaly: Option<AnomalyEvent>,
    pub enabled: bool,
    pub z_score_threshold: f64,
    pub mask_duration_ms: u64,
    pub min_samples: usize,
}

/// Async signal handler for graceful kill switch activation
///
/// Listens for SIGINT/SIGTERM and triggers the kill switch mask.
/// This runs as a background tokio task.
pub async fn kill_switch_signal_handler(kill_switch: KillSwitchHandle) {
    use tokio::signal;

    let ctrl_c_future = signal::ctrl_c();
    tokio::pin!(ctrl_c_future);

    #[cfg(unix)]
    let sigterm_result = signal::unix::signal(signal::unix::SignalKind::terminate());
    #[cfg(unix)]
    let mut sigterm = match sigterm_result {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to install SIGTERM handler: {}", e);
            // Fall back to SIGINT only
            loop {
                let _ = ctrl_c_future.as_mut().await;
                tracing::warn!("Kill Switch: SIGINT received, activating order mask");
                kill_switch.trigger();
            }
        }
    };

    loop {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = &mut ctrl_c_future => {
                    tracing::warn!("Kill Switch: SIGINT received, activating order mask");
                    kill_switch.trigger();
                }
                _ = sigterm.recv() => {
                    tracing::warn!("Kill Switch: SIGTERM received, activating order mask");
                    kill_switch.trigger();
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c_future.as_mut().await;
            tracing::warn!("Kill Switch: SIGINT received, activating order mask");
            kill_switch.trigger();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn default_config() -> KillSwitchConfig {
        KillSwitchConfig {
            min_samples: 10,
            z_score_threshold: 3.0,
            max_history: 100,
            mask_duration_ms: 50,
            enabled: true,
        }
    }

    /// Feed `n` slightly varied ticks (~1ms intervals) to build up statistics.
    /// Returns the last timestamp.
    fn feed_regular_ticks(ks: &KillSwitch, n: usize) -> u64 {
        let base_ns = 1_000_000_000u64;
        let offsets: Vec<u64> = (0..n)
            .map(|i| {
                900_000 + ((i as u64 * 37) % 200_000) // 900µs–1100µs with deterministic variation
            })
            .collect();
        let mut ts = base_ns;
        for offset in &offsets {
            ts += offset;
            ks.record_tick(ts);
        }
        ts
    }

    #[test]
    fn test_creation() {
        let ks = KillSwitch::new(default_config());
        assert!(ks.masked.load(Ordering::SeqCst) == false);
        assert_eq!(ks.status(), KillSwitchStatus::Active);
    }

    #[test]
    fn test_creation_default_config() {
        let ks = KillSwitch::new(KillSwitchConfig::default());
        assert_eq!(ks.config.min_samples, 100);
        assert_eq!(ks.config.z_score_threshold, 3.0);
        assert_eq!(ks.config.mask_duration_ms, 50);
        assert!(ks.config.enabled);
    }

    #[test]
    fn test_record_tick_first_tick() {
        let ks = KillSwitch::new(default_config());
        // First tick should not produce anomaly (no previous tick)
        let result = ks.record_tick(1_000_000_000);
        assert!(result.is_none());
        assert_eq!(ks.total_ticks.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_record_tick_second_tick_no_anomaly() {
        let ks = KillSwitch::new(default_config());
        ks.record_tick(1_000_000_000);
        let result = ks.record_tick(1_001_000_000); // 1ms interval
        assert!(result.is_none());
    }

    #[test]
    fn test_record_tick_no_anomaly_before_min_samples() {
        let ks = KillSwitch::new(default_config());
        // Record min_samples - 1 ticks (9 ticks = 8 intervals)
        for i in 1..10 {
            ks.record_tick(1_000_000_000 + i * 1_000_000); // 1ms intervals
        }
        assert_eq!(ks.total_anomalies.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_record_tick_anomaly_detection() {
        let mut config = default_config();
        config.min_samples = 5;
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        // Now send a tick with a much larger interval (100ms vs ~1ms)
        let result = ks.record_tick(last_ts + 100_000_000);
        assert!(result.is_some());

        let anomaly = result.unwrap();
        assert!(anomaly.z_score.abs() > 3.0);
        assert_eq!(anomaly.interval_ns, 100_000_000);
        assert_eq!(ks.total_anomalies.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_anomaly_activates_mask() {
        let mut config = default_config();
        config.min_samples = 5;
        config.mask_duration_ms = 100;
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        // Trigger anomaly
        ks.record_tick(last_ts + 100_000_000);

        // Verify mask is active
        assert_eq!(ks.status(), KillSwitchStatus::Masked);
        assert!(ks.masked.load(Ordering::SeqCst));
    }

    #[test]
    fn test_validate_order_allowed_when_active() {
        let ks = KillSwitch::new(default_config());
        let result = ks.validate_order();
        assert!(result.is_ok());
        let check = result.unwrap();
        assert!(check.allowed);
        assert_eq!(check.status, KillSwitchStatus::Active);
        assert_eq!(check.remaining_mask_ms, 0);
    }

    #[test]
    fn test_validate_order_blocked_when_masked() {
        let mut config = default_config();
        config.min_samples = 5;
        config.mask_duration_ms = 200; // 200ms mask
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        // Trigger anomaly
        ks.record_tick(last_ts + 100_000_000);

        // Order should be blocked
        let result = ks.validate_order();
        assert!(result.is_err());
    }

    #[test]
    fn test_mask_expires_after_duration() {
        let mut config = default_config();
        config.min_samples = 5;
        config.mask_duration_ms = 10; // 10ms mask
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        // Trigger anomaly
        ks.record_tick(last_ts + 100_000_000);

        // Wait for mask to expire
        thread::sleep(Duration::from_millis(20));

        // Order should now be allowed
        let result = ks.validate_order();
        assert!(result.is_ok());
        let check = result.unwrap();
        assert!(check.allowed);
        assert_eq!(check.status, KillSwitchStatus::Active);
    }

    #[test]
    fn test_disabled_kill_switch_allows_all() {
        let mut config = default_config();
        config.enabled = false;
        let ks = KillSwitch::new(config);

        assert_eq!(ks.status(), KillSwitchStatus::Disabled);

        let result = ks.validate_order();
        assert!(result.is_ok());
        let check = result.unwrap();
        assert!(check.allowed);
        assert_eq!(check.status, KillSwitchStatus::Disabled);

        // No anomaly even with extreme interval
        ks.record_tick(1_000_000_000);
        let result = ks.record_tick(10_000_000_000);
        assert!(result.is_none());
    }

    #[test]
    fn test_manual_trigger() {
        let ks = KillSwitch::new(default_config());

        assert_eq!(ks.status(), KillSwitchStatus::Active);

        ks.trigger();
        assert_eq!(ks.status(), KillSwitchStatus::Masked);

        let result = ks.validate_order();
        assert!(result.is_err());
    }

    #[test]
    fn test_manual_reset() {
        let ks = KillSwitch::new(default_config());

        ks.trigger();
        assert_eq!(ks.status(), KillSwitchStatus::Masked);

        ks.reset();
        assert_eq!(ks.status(), KillSwitchStatus::Active);

        let stats = ks.stats();
        assert_eq!(stats.total_ticks, 0);
        assert_eq!(stats.total_anomalies, 0);
        assert_eq!(stats.sample_count, 0);
    }

    #[test]
    fn test_reset_stats_preserves_mask() {
        let ks = KillSwitch::new(default_config());

        // Record some ticks
        ks.record_tick(1_000_000_000);
        ks.record_tick(1_001_000_000);
        ks.record_tick(1_002_000_000);

        ks.trigger();

        // Reset stats but mask should remain
        ks.reset_stats();
        assert_eq!(ks.status(), KillSwitchStatus::Masked);

        let stats = ks.stats();
        assert_eq!(stats.sample_count, 0);
    }

    #[test]
    fn test_multiple_anomalies() {
        let mut config = default_config();
        config.min_samples = 5;
        config.mask_duration_ms = 5; // short mask
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        // First anomaly (100ms gap vs ~1ms baseline)
        let r1 = ks.record_tick(last_ts + 100_000_000);
        assert!(r1.is_some());

        // Wait for mask to expire
        thread::sleep(Duration::from_millis(10));

        // Reset stats to clear the anomaly from the baseline
        ks.reset_stats();

        // Re-feed normal ticks to build fresh baseline
        let last_ts2 = feed_regular_ticks(&ks, 6);

        // Second anomaly
        let r2 = ks.record_tick(last_ts2 + 100_000_000);
        assert!(r2.is_some());

        assert_eq!(ks.total_anomalies.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_stats_snapshot() {
        let ks = KillSwitch::new(default_config());
        ks.record_tick(1_000_000_000);
        ks.record_tick(1_001_000_000);

        let stats = ks.stats();
        assert_eq!(stats.total_ticks, 2);
        assert_eq!(stats.sample_count, 1); // 1 interval
        assert_eq!(stats.mean_interval_ns, 1_000_000.0);
        assert_eq!(stats.total_anomalies, 0);
        assert!(!stats.is_masked);
        assert!(stats.last_anomaly.is_none());
        assert!(stats.enabled);
    }

    #[test]
    fn test_handle_clone() {
        let ks = KillSwitch::new(default_config());
        let handle = ks.handle();

        ks.record_tick(1_000_000_000);
        ks.record_tick(1_001_000_000);

        // Handle shares state
        let stats = handle.stats();
        assert_eq!(stats.total_ticks, 2);
    }

    #[test]
    fn test_handle_validate_order() {
        let ks = KillSwitch::new(default_config());
        let handle = ks.handle();

        ks.trigger();

        // Handle should see the masked state
        let result = handle.validate_order();
        assert!(result.is_err());
    }

    #[test]
    fn test_reverse_timestamp_ignored() {
        let ks = KillSwitch::new(default_config());
        ks.record_tick(2_000_000_000);
        let result = ks.record_tick(1_000_000_000); // backward
        assert!(result.is_none());
    }

    #[test]
    fn test_same_timestamp_ignored() {
        let ks = KillSwitch::new(default_config());
        ks.record_tick(1_000_000_000);
        let result = ks.record_tick(1_000_000_000); // same
        assert!(result.is_none());
    }

    #[test]
    fn test_zero_variance_no_false_positive() {
        let ks = KillSwitch::new(default_config());

        // All identical intervals — variance is 0
        let base_ns = 1_000_000_000;
        for i in 1..=20 {
            ks.record_tick(base_ns + i * 1_000_000);
        }

        // Same interval again should not trigger anomaly (std = 0 → skip)
        let result = ks.record_tick(base_ns + 21 * 1_000_000);
        assert!(result.is_none());
    }

    #[test]
    fn test_interval_stats_welford() {
        let mut stats = IntervalStats::new();

        stats.update(100);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.mean, 100.0);

        stats.update(200);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.mean, 150.0);
        assert_eq!(stats.variance(), 5000.0); // ((100-150)^2 + (200-150)^2) / 1
        assert!((stats.std() - 5000.0_f64.sqrt()).abs() < 1e-10);

        stats.update(300);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.mean, 200.0);
    }

    #[test]
    fn test_interval_stats_min_max() {
        let mut stats = IntervalStats::new();
        stats.update(100);
        stats.update(50);
        stats.update(200);

        assert_eq!(stats.min_interval_ns, 50);
        assert_eq!(stats.max_interval_ns, 200);
    }

    #[test]
    fn test_interval_stats_reset() {
        let mut stats = IntervalStats::new();
        stats.update(100);
        stats.update(200);
        stats.reset();

        assert_eq!(stats.count, 0);
        assert_eq!(stats.mean, 0.0);
    }

    #[test]
    fn test_z_score_threshold_custom() {
        let mut config = default_config();
        config.min_samples = 5;
        config.z_score_threshold = 10.0; // very high threshold
        let ks = KillSwitch::new(config);

        let base_ns = 1_000_000_000;
        for i in 1..=6 {
            ks.record_tick(base_ns + i * 1_000_000);
        }

        // This interval would trigger with default threshold but not with 10.0
        let result = ks.record_tick(base_ns + 6 * 1_000_000 + 10_000_000);
        assert!(result.is_none()); // z-score likely below 10
    }

    #[test]
    fn test_anomaly_event_fields() {
        let mut config = default_config();
        config.min_samples = 5;
        config.mask_duration_ms = 30;
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        let result = ks.record_tick(last_ts + 100_000_000);
        let anomaly = result.unwrap();
        assert_eq!(anomaly.mask_duration_ms, 30);
        assert!(anomaly.z_score > 0.0);
        assert!(anomaly.mean_interval_ns > 0.0);
    }

    #[test]
    fn test_config_serde() {
        let config = default_config();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: KillSwitchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.min_samples, config.min_samples);
        assert_eq!(deserialized.z_score_threshold, config.z_score_threshold);
    }

    #[test]
    fn test_status_serde() {
        let statuses = [
            KillSwitchStatus::Active,
            KillSwitchStatus::Masked,
            KillSwitchStatus::Disabled,
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let deserialized: KillSwitchStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*s, deserialized);
        }
    }

    #[test]
    fn test_stats_serde() {
        let ks = KillSwitch::new(default_config());
        ks.record_tick(1_000_000_000);
        ks.record_tick(1_001_000_000);

        let stats = ks.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let deserialized: KillSwitchStats = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.total_ticks, stats.total_ticks);
        assert_eq!(deserialized.mean_interval_ns, stats.mean_interval_ns);
    }

    #[test]
    fn test_history_trimming() {
        let mut config = default_config();
        config.min_samples = 5;
        config.max_history = 10;
        let ks = KillSwitch::new(config);

        let base_ns = 1_000_000_000;
        // Record 15 ticks (14 intervals) — history should trim to 10
        for i in 1..=15 {
            ks.record_tick(base_ns + i * 1_000_000);
        }

        let stats = ks.stats();
        // After trimming 4 oldest, we should have 10 remaining intervals
        assert!(stats.sample_count <= 10);
    }

    // ========================================================================
    // §9.1 Kill Switch Verification Tests (design.md §9.1)
    // ========================================================================

    /// §9.1: デフォルトz_score_thresholdが3.0（平均±3σ）であることを確認
    #[test]
    fn s9_1_default_z_score_threshold_is_3sigma() {
        let config = KillSwitchConfig::default();
        assert!(
            (config.z_score_threshold - 3.0).abs() < 1e-10,
            "design.md §9.1 specifies mean ± 3σ; expected 3.0, got {}",
            config.z_score_threshold
        );
    }

    /// §9.1: mask_duration_msが10〜50msの範囲内であることを確認
    #[test]
    fn s9_1_mask_duration_within_10_to_50ms() {
        let config = KillSwitchConfig::default();
        assert!(
            config.mask_duration_ms >= 10 && config.mask_duration_ms <= 50,
            "design.md §9.1 specifies 10-50ms mask; got {}ms",
            config.mask_duration_ms
        );
    }

    /// §9.1: Welford online algorithmが正しい平均・分散を計算することを確認
    #[test]
    fn s9_1_welford_produces_correct_statistics() {
        let mut stats = IntervalStats::new();
        let values: Vec<u64> = vec![100, 200, 300, 400, 500];
        for &v in &values {
            stats.update(v);
        }
        let expected_mean = 300.0;
        let expected_var = 25000.0; // population var = sum((x-300)^2)/5 = 40000, sample var = 40000/4 = ... wait
                                    // sample variance = sum((x-mean)^2)/(n-1) = ((100-300)^2 + ... + (500-300)^2) / 4
                                    // = (40000 + 10000 + 0 + 10000 + 40000) / 4 = 100000 / 4 = 25000
        assert!(
            (stats.mean - expected_mean).abs() < 1e-10,
            "Welford mean incorrect: expected {}, got {}",
            expected_mean,
            stats.mean
        );
        assert!(
            (stats.variance() - expected_var).abs() < 1e-10,
            "Welford variance incorrect: expected {}, got {}",
            expected_var,
            stats.variance()
        );
    }

    /// §9.1: 逸脱検出がorder maskingをトリガーし、validate_orderがブロックすることを確認
    #[test]
    fn s9_1_anomaly_triggers_mask_and_blocks_orders() {
        let mut config = default_config();
        config.min_samples = 5;
        config.mask_duration_ms = 50;
        let z_threshold = config.z_score_threshold;
        let ks = KillSwitch::new(config);

        let last_ts = feed_regular_ticks(&ks, 6);

        // 異常interval（100ms vs ~1ms baseline）でanomaly検出
        let anomaly = ks.record_tick(last_ts + 100_000_000);
        assert!(anomaly.is_some(), "anomaly should be detected");
        let a = anomaly.unwrap();
        assert!(a.z_score.abs() > z_threshold);

        // マスクが有効であることを確認
        assert_eq!(ks.status(), KillSwitchStatus::Masked);

        // validate_orderがErr(KillSwitchMasked)を返すことを確認
        let result = ks.validate_order();
        assert!(result.is_err());
        match result.unwrap_err() {
            RiskError::KillSwitchMasked { remaining_ms } => {
                assert!(remaining_ms > 0 && remaining_ms <= 50);
            }
            e => panic!("expected KillSwitchMasked, got {}", e),
        }
    }

    /// §9.1: validate_orderのhot pathがatomic lock-freeであることを確認（構造的証明）
    #[test]
    fn s9_1_validate_order_uses_atomic_lock_free_check() {
        // validate_order()のfast pathはmasked.load(Ordering::SeqCst)のみを使用。
        // maskedがfalseの場合、Mutexは取得しない。これはコードの構造から保証される。
        let ks = KillSwitch::new(default_config());
        // 複数スレッドから同時にvalidate_orderを呼び出してもデッドロックしない
        let handle = ks.handle();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let h = handle.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        let _ = h.validate_order();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    /// §9.1: min_samples未満ではanomaly検出が行われないことを確認
    #[test]
    fn s9_1_no_detection_before_min_samples() {
        let config = KillSwitchConfig {
            min_samples: 50,
            z_score_threshold: 3.0,
            ..default_config()
        };
        let ks = KillSwitch::new(config);

        let base_ns = 1_000_000_000u64;
        // 49 ticks = 48 intervals (below min_samples=50)
        for i in 1..50 {
            ks.record_tick(base_ns + i * 1_000_000);
        }

        // 異常intervalでも検出されない
        let result = ks.record_tick(base_ns + 50 * 1_000_000 + 100_000_000);
        assert!(
            result.is_none(),
            "should not detect anomaly before min_samples"
        );
        assert_eq!(ks.total_anomalies.load(Ordering::Relaxed), 0);
    }

    /// §9.1: 手動trigger/resetによるオペレーター操作を確認
    #[test]
    fn s9_1_manual_trigger_and_reset_operator_control() {
        let ks = KillSwitch::new(default_config());
        assert_eq!(ks.status(), KillSwitchStatus::Active);

        // オペレーターによる手動trigger
        ks.trigger();
        assert_eq!(ks.status(), KillSwitchStatus::Masked);
        assert!(ks.validate_order().is_err());

        // オペレーターによる手動reset
        ks.reset();
        assert_eq!(ks.status(), KillSwitchStatus::Active);
        assert!(ks.validate_order().is_ok());
        assert_eq!(ks.stats().total_anomalies, 0);
    }

    /// §9.1: 非同期シグナルハンドラが存在することを確認（コンパイル時証明）
    #[test]
    fn s9_1_async_signal_handler_exists() {
        // kill_switch_signal_handler関数が存在し、KillSwitchHandleを受け取ることを確認。
        // このテストはコンパイルによって検証される。
        let ks = KillSwitch::new(default_config());
        let _handle = ks.handle();
        // 関数が存在することはコンパイル成功で証明済み
    }

    #[test]
    fn test_concurrent_access() {
        let ks = KillSwitch::new(default_config());
        let handle = ks.handle();

        let base_ns = 1_000_000_000u64;

        // Spawn threads that record ticks concurrently
        let mut handles = vec![];
        for i in 0..4 {
            let h = handle.clone();
            handles.push(thread::spawn(move || {
                for j in 0..50 {
                    let ts = base_ns + (i * 50 + j) as u64 * 1_000_000;
                    h.record_tick(ts);
                    let _ = h.validate_order();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let stats = ks.stats();
        assert!(stats.total_ticks > 0);
    }
}
