use std::collections::HashMap;

use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct LpState {
    pub lp_id: String,
    pub total_requests: u64,
    pub total_fills: u64,
    pub total_rejections: u64,
    pub fill_rate_ema: f64,
    pub rejection_rate_ema: f64,
    pub is_adversarial: bool,
    pub consecutive_rejections: u32,
}

#[derive(Debug, Clone)]
pub struct LpMonitorConfig {
    pub ema_alpha: f64,
    pub adversarial_threshold: f64,
    pub recovery_threshold: f64,
    pub min_observations: u64,
    pub max_consecutive_rejections: u32,
}

impl Default for LpMonitorConfig {
    fn default() -> Self {
        Self {
            ema_alpha: 0.1,
            adversarial_threshold: 0.5,
            recovery_threshold: 0.8,
            min_observations: 20,
            max_consecutive_rejections: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LpSwitchSignal {
    pub from_lp_id: String,
    pub to_lp_id: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct LpRiskMonitor {
    config: LpMonitorConfig,
    lp_states: HashMap<String, LpState>,
    known_lps: Vec<String>,
    active_lp_index: usize,
}

impl LpRiskMonitor {
    pub fn new(config: LpMonitorConfig, known_lps: Vec<String>) -> Self {
        assert!(!known_lps.is_empty(), "must have at least one known LP");
        Self {
            config,
            lp_states: HashMap::new(),
            known_lps,
            active_lp_index: 0,
        }
    }

    pub fn active_lp_id(&self) -> &str {
        &self.known_lps[self.active_lp_index]
    }

    fn get_or_create_state(&mut self, lp_id: &str) -> &mut LpState {
        self.lp_states
            .entry(lp_id.to_string())
            .or_insert_with(|| LpState {
                lp_id: lp_id.to_string(),
                total_requests: 0,
                total_fills: 0,
                total_rejections: 0,
                fill_rate_ema: 0.8,
                rejection_rate_ema: 0.2,
                is_adversarial: false,
                consecutive_rejections: 0,
            })
    }

    pub fn record_fill(&mut self, lp_id: &str) {
        let a = self.config.ema_alpha;
        let state = self.get_or_create_state(lp_id);
        state.total_requests += 1;
        state.total_fills += 1;
        state.consecutive_rejections = 0;
        state.fill_rate_ema = a + (1.0 - a) * state.fill_rate_ema;
        state.rejection_rate_ema *= 1.0 - a;
    }

    pub fn record_rejection(&mut self, lp_id: &str) {
        let a = self.config.ema_alpha;
        let state = self.get_or_create_state(lp_id);
        state.total_requests += 1;
        state.total_rejections += 1;
        state.consecutive_rejections += 1;
        state.fill_rate_ema *= 1.0 - a;
        state.rejection_rate_ema = a + (1.0 - a) * state.rejection_rate_ema;
    }

    /// Check adversarial, return switch signal if needed
    pub fn check_adversarial(&mut self) -> Option<LpSwitchSignal> {
        let active_id = self.active_lp_id().to_string();
        let min_obs = self.config.min_observations;
        let adv_thresh = self.config.adversarial_threshold;
        let max_consec = self.config.max_consecutive_rejections;
        let rec_thresh = self.config.recovery_threshold;

        let state = self.get_or_create_state(&active_id);

        if state.total_requests < min_obs {
            return None;
        }

        if state.fill_rate_ema < adv_thresh && !state.is_adversarial {
            state.is_adversarial = true;
            warn!(
                lp_id = %active_id,
                fill_rate = state.fill_rate_ema,
                "LP adversarial: low fill rate"
            );
            return self.switch_lp("low fill rate");
        }

        if state.consecutive_rejections >= max_consec && !state.is_adversarial {
            state.is_adversarial = true;
            warn!(
                lp_id = %active_id,
                consecutive = state.consecutive_rejections,
                "LP adversarial: consecutive rejections"
            );
            return self.switch_lp("consecutive rejections");
        }

        if state.is_adversarial && state.fill_rate_ema >= rec_thresh {
            state.is_adversarial = false;
            info!(
                lp_id = %active_id,
                fill_rate = state.fill_rate_ema,
                "LP recovered from adversarial"
            );
        }

        None
    }

    fn switch_lp(&mut self, reason: &str) -> Option<LpSwitchSignal> {
        if self.known_lps.len() <= 1 {
            return None;
        }
        let from = self.active_lp_id().to_string();
        let old = self.active_lp_index;

        for i in 1..self.known_lps.len() {
            let idx = (old + i) % self.known_lps.len();
            let next = &self.known_lps[idx];
            let ok = self
                .lp_states
                .get(next)
                .map(|s| !s.is_adversarial)
                .unwrap_or(true);
            if ok {
                self.active_lp_index = idx;
                warn!(from = %from, to = %next, reason, "LP switch");
                return Some(LpSwitchSignal {
                    from_lp_id: from,
                    to_lp_id: next.clone(),
                    reason: reason.to_string(),
                });
            }
        }
        None
    }

    pub fn get_lp_state(&self, lp_id: &str) -> Option<&LpState> {
        self.lp_states.get(lp_id)
    }

    pub fn all_lp_states(&self) -> &HashMap<String, LpState> {
        &self.lp_states
    }

    pub fn active_fill_rate(&self) -> f64 {
        self.lp_states
            .get(self.active_lp_id())
            .map(|s| s.fill_rate_ema)
            .unwrap_or(0.8)
    }

    pub fn known_lps(&self) -> &[String] {
        &self.known_lps
    }

    pub fn set_active_lp(&mut self, lp_id: &str) -> bool {
        if let Some(idx) = self.known_lps.iter().position(|l| l == lp_id) {
            self.active_lp_index = idx;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_monitor(lps: Vec<&str>) -> LpRiskMonitor {
        LpRiskMonitor::new(
            LpMonitorConfig {
                ema_alpha: 0.2,
                adversarial_threshold: 0.5,
                recovery_threshold: 0.8,
                min_observations: 10,
                max_consecutive_rejections: 5,
            },
            lps.into_iter().map(String::from).collect(),
        )
    }

    #[test]
    fn initial_active_lp() {
        let mon = make_monitor(vec!["lp1", "lp2"]);
        assert_eq!(mon.active_lp_id(), "lp1");
    }

    #[test]
    #[should_panic(expected = "at least one")]
    fn empty_lps_panics() {
        let _ = LpRiskMonitor::new(LpMonitorConfig::default(), vec![]);
    }

    #[test]
    fn record_fill_updates_state() {
        let mut mon = make_monitor(vec!["lp1"]);
        mon.record_fill("lp1");
        let state = mon.get_lp_state("lp1").unwrap();
        assert_eq!(state.total_requests, 1);
        assert_eq!(state.total_fills, 1);
        assert_eq!(state.total_rejections, 0);
        assert!(state.fill_rate_ema > 0.0);
    }

    #[test]
    fn record_rejection_updates_state() {
        let mut mon = make_monitor(vec!["lp1"]);
        mon.record_rejection("lp1");
        let state = mon.get_lp_state("lp1").unwrap();
        assert_eq!(state.total_requests, 1);
        assert_eq!(state.total_rejections, 1);
        assert_eq!(state.consecutive_rejections, 1);
    }

    #[test]
    fn consecutive_rejections_reset_on_fill() {
        let mut mon = make_monitor(vec!["lp1"]);
        for _ in 0..5 {
            mon.record_rejection("lp1");
        }
        assert_eq!(mon.get_lp_state("lp1").unwrap().consecutive_rejections, 5);
        mon.record_fill("lp1");
        assert_eq!(mon.get_lp_state("lp1").unwrap().consecutive_rejections, 0);
    }

    #[test]
    fn fill_rate_ema_converges_high() {
        let mut mon = make_monitor(vec!["lp1"]);
        for _ in 0..50 {
            mon.record_fill("lp1");
        }
        let rate = mon.get_lp_state("lp1").unwrap().fill_rate_ema;
        assert!(rate > 0.9);
    }

    #[test]
    fn fill_rate_ema_converges_low() {
        let mut mon = make_monitor(vec!["lp1"]);
        for _ in 0..50 {
            mon.record_rejection("lp1");
        }
        let rate = mon.get_lp_state("lp1").unwrap().fill_rate_ema;
        assert!(rate < 0.2);
    }

    #[test]
    fn adversarial_detected_low_fill_rate() {
        let mut mon = make_monitor(vec!["lp1", "lp2"]);
        for _ in 0..50 {
            mon.record_rejection("lp1");
        }
        let signal = mon.check_adversarial();
        assert!(signal.is_some());
        let sig = signal.unwrap();
        assert_eq!(sig.from_lp_id, "lp1");
        assert_eq!(sig.to_lp_id, "lp2");
        assert!(mon.get_lp_state("lp1").unwrap().is_adversarial);
        assert_eq!(mon.active_lp_id(), "lp2");
    }

    #[test]
    fn adversarial_detected_consecutive_rejections() {
        let mon = LpRiskMonitor::new(
            LpMonitorConfig {
                ema_alpha: 0.2,
                adversarial_threshold: 0.5,
                recovery_threshold: 0.8,
                min_observations: 10,
                max_consecutive_rejections: 3,
            },
            vec!["lp1".into(), "lp2".into()],
        );
        let mut mon = mon;
        // 50 fills → ema ≈ 1.0
        for _ in 0..50 {
            mon.record_fill("lp1");
        }
        // 3 rejections → ema ≈ 0.8^3 ≈ 0.512 > 0.5 (above threshold)
        // consecutive = 3 = max_consecutive_rejections → triggers consecutive path
        for _ in 0..3 {
            mon.record_rejection("lp1");
        }
        let signal = mon.check_adversarial();
        assert!(signal.is_some());
        assert_eq!(signal.unwrap().reason, "consecutive rejections");
    }

    #[test]
    fn no_adversarial_below_min_observations() {
        let mut mon = make_monitor(vec!["lp1", "lp2"]);
        for _ in 0..5 {
            mon.record_rejection("lp1");
        }
        assert!(mon.check_adversarial().is_none());
    }

    #[test]
    fn no_switch_with_single_lp() {
        let mut mon = make_monitor(vec!["lp1"]);
        for _ in 0..50 {
            mon.record_rejection("lp1");
        }
        assert!(mon.check_adversarial().is_none());
        assert_eq!(mon.active_lp_id(), "lp1");
    }

    #[test]
    fn lp_recovery() {
        let mut mon = make_monitor(vec!["lp1", "lp2"]);
        for _ in 0..50 {
            mon.record_rejection("lp1");
        }
        let _ = mon.check_adversarial();
        assert!(mon.get_lp_state("lp1").unwrap().is_adversarial);
        mon.set_active_lp("lp1");
        for _ in 0..50 {
            mon.record_fill("lp1");
        }
        let _ = mon.check_adversarial();
        assert!(!mon.get_lp_state("lp1").unwrap().is_adversarial);
    }

    #[test]
    fn set_active_lp() {
        let mut mon = make_monitor(vec!["lp1", "lp2", "lp3"]);
        assert!(mon.set_active_lp("lp3"));
        assert_eq!(mon.active_lp_id(), "lp3");
        assert!(!mon.set_active_lp("unknown"));
        assert_eq!(mon.active_lp_id(), "lp3");
    }

    #[test]
    fn active_fill_rate_default() {
        let mon = make_monitor(vec!["lp1"]);
        assert!((mon.active_fill_rate() - 0.8).abs() < 1e-10);
    }

    #[test]
    fn active_fill_rate_tracked() {
        let mut mon = make_monitor(vec!["lp1"]);
        mon.record_fill("lp1");
        let rate = mon.active_fill_rate();
        assert!(rate > 0.8);
    }

    #[test]
    fn switch_skips_adversarial_lp() {
        let mut mon = make_monitor(vec!["lp1", "lp2", "lp3"]);
        // Make lp1 adversarial
        for _ in 0..50 {
            mon.record_rejection("lp1");
        }
        // Make lp2 adversarial too
        for _ in 0..50 {
            mon.record_rejection("lp2");
        }
        let _ = mon.check_adversarial(); // switches from lp1 to lp2
        assert_eq!(mon.active_lp_id(), "lp2");
        let _ = mon.check_adversarial(); // switches from lp2 to lp3
        assert_eq!(mon.active_lp_id(), "lp3");
    }

    #[test]
    fn per_lp_independent() {
        let mut mon = make_monitor(vec!["lp1", "lp2"]);
        mon.record_fill("lp1");
        mon.record_rejection("lp2");
        assert_eq!(mon.get_lp_state("lp1").unwrap().total_fills, 1);
        assert_eq!(mon.get_lp_state("lp2").unwrap().total_rejections, 1);
    }

    #[test]
    fn all_lp_states() {
        let mut mon = make_monitor(vec!["lp1", "lp2"]);
        mon.record_fill("lp1");
        mon.record_rejection("lp2");
        assert_eq!(mon.all_lp_states().len(), 2);
    }
}
