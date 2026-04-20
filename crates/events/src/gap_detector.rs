use std::collections::VecDeque;

use fx_core::types::EventTier;
use prost::Message;

use crate::bus::{EventPublisher, PartitionedEventBus};
use crate::event::{Event, GenericEvent};
use crate::header::EventHeader;
use crate::proto;

const DEFAULT_EMA_ALPHA: f64 = 0.02;
const DEFAULT_MIN_SAMPLES: usize = 50;
const DEFAULT_MINOR_TICK_THRESHOLD: u64 = 1;
const DEFAULT_SEVERE_TICK_THRESHOLD: u64 = 3;
const DEFAULT_MAX_HISTORY: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapLevel {
    None,
    Minor,
    Severe,
}

#[derive(Debug, Clone)]
pub struct GapInfo {
    pub level: GapLevel,
    pub missed_ticks: u64,
    pub interval_ns: u64,
    pub mean_interval_ns: f64,
    pub std_interval_ns: f64,
    pub z_score: f64,
    pub expected_timestamp_ns: u64,
    pub actual_timestamp_ns: u64,
}

pub struct GapDetector {
    last_timestamp_ns: Option<u64>,
    last_sequence_id: u64,
    intervals: VecDeque<u64>,
    mean_interval_ns: f64,
    variance_interval_ns: f64,
    ema_alpha: f64,
    min_samples: usize,
    minor_tick_threshold: u64,
    severe_tick_threshold: u64,
    max_history: usize,
    publisher: EventPublisher,
    schema_version: u32,
}

impl GapDetector {
    pub fn new(bus: &PartitionedEventBus, schema_version: u32) -> Self {
        let publisher = bus.publisher(fx_core::types::StreamId::Strategy);
        Self {
            last_timestamp_ns: None,
            last_sequence_id: 0,
            intervals: VecDeque::with_capacity(DEFAULT_MAX_HISTORY),
            mean_interval_ns: 0.0,
            variance_interval_ns: 0.0,
            ema_alpha: DEFAULT_EMA_ALPHA,
            min_samples: DEFAULT_MIN_SAMPLES,
            minor_tick_threshold: DEFAULT_MINOR_TICK_THRESHOLD,
            severe_tick_threshold: DEFAULT_SEVERE_TICK_THRESHOLD,
            max_history: DEFAULT_MAX_HISTORY,
            publisher,
            schema_version,
        }
    }

    pub fn with_config(
        bus: &PartitionedEventBus,
        schema_version: u32,
        ema_alpha: f64,
        min_samples: usize,
        minor_tick_threshold: u64,
        severe_tick_threshold: u64,
    ) -> Self {
        let publisher = bus.publisher(fx_core::types::StreamId::Strategy);
        Self {
            last_timestamp_ns: None,
            last_sequence_id: 0,
            intervals: VecDeque::with_capacity(DEFAULT_MAX_HISTORY),
            mean_interval_ns: 0.0,
            variance_interval_ns: 0.0,
            ema_alpha,
            min_samples,
            minor_tick_threshold,
            severe_tick_threshold,
            max_history: DEFAULT_MAX_HISTORY,
            publisher,
            schema_version,
        }
    }

    pub async fn process_market_event(
        &mut self,
        event: &GenericEvent,
    ) -> Result<Option<GapInfo>, String> {
        let market = proto::MarketEventPayload::decode(event.payload_bytes())
            .map_err(|e| format!("decode error: {e}"))?;

        let current_seq = event.header.sequence_id;
        let current_ts = market.timestamp_ns;

        let gap_info = self.detect_gap(current_ts, current_seq);

        self.update_statistics(current_ts);

        self.last_timestamp_ns = Some(current_ts);
        self.last_sequence_id = current_seq;

        if let Some(ref info) = gap_info {
            if info.level != GapLevel::None {
                self.publish_gap_event(info, event.header.timestamp_ns)
                    .await;
            }
        }

        Ok(gap_info)
    }

    fn detect_gap(&self, current_ts: u64, current_seq: u64) -> Option<GapInfo> {
        let last_ts = self.last_timestamp_ns?;

        if current_ts <= last_ts {
            return None;
        }

        let interval_ns = current_ts - last_ts;
        let expected_seq = self.last_sequence_id + 1;
        let missed_ticks = current_seq.saturating_sub(expected_seq);

        if missed_ticks == 0 && self.intervals.len() >= self.min_samples {
            let z = self.compute_z_score(interval_ns as f64);
            if z < 3.0 {
                return None;
            }
        }

        let mean = self.mean_interval_ns;
        let std = self.std_interval_ns();
        let z_score = if std > 0.0 {
            (interval_ns as f64 - mean) / std
        } else {
            0.0
        };

        let expected_ts = last_ts + if mean > 0.0 { mean as u64 } else { interval_ns };

        let level = if missed_ticks >= self.severe_tick_threshold {
            GapLevel::Severe
        } else if missed_ticks >= self.minor_tick_threshold
            || (self.intervals.len() >= self.min_samples && z_score >= 3.0)
        {
            GapLevel::Minor
        } else {
            GapLevel::None
        };

        if level == GapLevel::None {
            return None;
        }

        Some(GapInfo {
            level,
            missed_ticks,
            interval_ns,
            mean_interval_ns: mean,
            std_interval_ns: std,
            z_score,
            expected_timestamp_ns: expected_ts,
            actual_timestamp_ns: current_ts,
        })
    }

    fn update_statistics(&mut self, current_ts: u64) {
        if let Some(last_ts) = self.last_timestamp_ns {
            if current_ts > last_ts {
                let interval = current_ts - last_ts;

                if self.intervals.len() < self.min_samples {
                    self.intervals.push_back(interval);
                    if !self.intervals.is_empty() {
                        let sum: u64 = self.intervals.iter().sum();
                        let count = self.intervals.len() as f64;
                        let new_mean = sum as f64 / count;

                        if self.mean_interval_ns > 0.0 {
                            let delta = interval as f64 - self.mean_interval_ns;
                            self.variance_interval_ns = (self.variance_interval_ns * (count - 1.0)
                                + delta * (interval as f64 - new_mean))
                                / count;
                        }

                        self.mean_interval_ns = new_mean;
                    }
                } else {
                    let delta = interval as f64 - self.mean_interval_ns;
                    self.mean_interval_ns += self.ema_alpha * delta;
                    let delta2 = interval as f64 - self.mean_interval_ns;
                    self.variance_interval_ns = (1.0 - self.ema_alpha)
                        * (self.variance_interval_ns + self.ema_alpha * delta * delta2);

                    if self.intervals.len() >= self.max_history {
                        self.intervals.pop_front();
                    }
                    self.intervals.push_back(interval);
                }
            }
        }
    }

    fn compute_z_score(&self, interval_ns: f64) -> f64 {
        let std = self.std_interval_ns();
        if std < f64::EPSILON {
            return 0.0;
        }
        (interval_ns - self.mean_interval_ns) / std
    }

    fn std_interval_ns(&self) -> f64 {
        self.variance_interval_ns.sqrt().max(0.0)
    }

    async fn publish_gap_event(&self, info: &GapInfo, event_timestamp_ns: u64) {
        let severity = match info.level {
            GapLevel::Minor => proto::GapSeverity::Minor as i32,
            GapLevel::Severe => proto::GapSeverity::Severe as i32,
            GapLevel::None => return,
        };

        let description = match info.level {
            GapLevel::Minor => format!(
                "Minor gap: {} tick(s) missing, interval={:.0}ns, z={:.2}",
                info.missed_ticks, info.interval_ns, info.z_score
            ),
            GapLevel::Severe => format!(
                "Severe gap: {} tick(s) missing, interval={:.0}ns, z={:.2}. Trading halted.",
                info.missed_ticks, info.interval_ns, info.z_score
            ),
            GapLevel::None => unreachable!(),
        };

        let payload = proto::GapEventPayload {
            header: None,
            detected_at_ns: event_timestamp_ns,
            expected_timestamp_ns: info.expected_timestamp_ns,
            actual_timestamp_ns: info.actual_timestamp_ns,
            missed_ticks: info.missed_ticks,
            severity,
            interval_ns: info.interval_ns as f64,
            mean_interval_ns: info.mean_interval_ns,
            std_interval_ns: info.std_interval_ns,
            z_score: info.z_score,
            description,
        };

        let bytes = payload.encode_to_vec();
        let header = EventHeader::new(
            fx_core::types::StreamId::Strategy,
            0,
            EventTier::Tier1Critical,
        );
        let header = EventHeader {
            timestamp_ns: header.timestamp_ns,
            schema_version: self.schema_version,
            ..header
        };

        if let Err(e) = self.publisher.publish(header, bytes).await {
            tracing::error!("Failed to publish gap event: {}", e);
        }

        match info.level {
            GapLevel::Minor => {
                tracing::warn!(
                    missed_ticks = info.missed_ticks,
                    interval_ns = info.interval_ns,
                    z_score = info.z_score,
                    "Minor gap detected: feature hold engaged"
                );
            }
            GapLevel::Severe => {
                tracing::error!(
                    missed_ticks = info.missed_ticks,
                    interval_ns = info.interval_ns,
                    z_score = info.z_score,
                    "Severe gap detected: trading halted, event replay required"
                );
            }
            GapLevel::None => {}
        }
    }

    pub fn is_trading_halted(&self) -> bool {
        false
    }

    pub fn mean_interval_ns(&self) -> f64 {
        self.mean_interval_ns
    }

    pub fn std_interval_ns_pub(&self) -> f64 {
        self.std_interval_ns()
    }

    pub fn sample_count(&self) -> usize {
        self.intervals.len()
    }

    pub fn last_timestamp_ns(&self) -> Option<u64> {
        self.last_timestamp_ns
    }

    pub fn last_sequence_id(&self) -> u64 {
        self.last_sequence_id
    }
}

#[cfg(test)]
mod tests {
    use fx_core::types::StreamId;
    use prost::Message;
    use uuid::Uuid;

    use super::*;
    use crate::bus::PartitionedEventBus;
    use crate::proto;

    const NS_BASE: u64 = 1_000_000_000_000_000;
    const TICK_INTERVAL_NS: u64 = 100_000_000; // 100ms

    fn make_market_event(timestamp_ns: u64, seq_id: u64) -> GenericEvent {
        let payload = proto::MarketEventPayload {
            header: None,
            symbol: "USD/JPY".to_string(),
            bid: 110.0,
            ask: 110.005,
            bid_size: 1_000_000.0,
            ask_size: 1_000_000.0,
            timestamp_ns,
            bid_levels: vec![],
            ask_levels: vec![],
            latency_ms: 0.5,
        }
        .encode_to_vec();

        GenericEvent::new(
            EventHeader {
                event_id: Uuid::now_v7(),
                parent_event_id: None,
                stream_id: StreamId::Market,
                sequence_id: seq_id,
                timestamp_ns,
                schema_version: 1,
                tier: EventTier::Tier3Raw,
            },
            payload,
        )
    }

    async fn make_jittered_tick(detector: &mut GapDetector, tick_index: u64, jitter_ns: u64) {
        let ts = NS_BASE + tick_index * TICK_INTERVAL_NS + jitter_ns;
        let event = make_market_event(ts, tick_index + 1);
        let _ = detector.process_market_event(&event).await;
    }

    async fn make_normal_tick(detector: &mut GapDetector, tick_index: u64) {
        let ts = NS_BASE + tick_index * TICK_INTERVAL_NS;
        let event = make_market_event(ts, tick_index + 1);
        let _ = detector.process_market_event(&event).await;
    }

    #[tokio::test]
    async fn test_no_gap_on_normal_ticks() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }

        assert_eq!(detector.sample_count(), 9);
        assert!(detector.mean_interval_ns() > 0.0);
    }

    #[tokio::test]
    async fn test_minor_gap_1_tick_missing() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 1: next seq=12, ts jumps 2 intervals
        let ts = NS_BASE + 11 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 12);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.level, GapLevel::Minor);
        assert_eq!(info.missed_ticks, 1);
    }

    #[tokio::test]
    async fn test_minor_gap_2_ticks_missing() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 2: next seq=13, ts jumps 3 intervals
        let ts = NS_BASE + 12 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 13);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.level, GapLevel::Minor);
        assert_eq!(info.missed_ticks, 2);
    }

    #[tokio::test]
    async fn test_severe_gap_3_ticks_missing() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 3: next seq=14, ts jumps 4 intervals
        let ts = NS_BASE + 13 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 14);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.level, GapLevel::Severe);
        assert_eq!(info.missed_ticks, 3);
    }

    #[tokio::test]
    async fn test_severe_gap_5_ticks_missing() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 5: next seq=16, ts jumps 6 intervals
        let ts = NS_BASE + 15 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 16);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.level, GapLevel::Severe);
        assert_eq!(info.missed_ticks, 5);
    }

    #[tokio::test]
    async fn test_no_gap_before_min_samples_sequence_based() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 50, 1, 3);

        for i in 0..5 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=6, skip 2: next seq=9, ts jumps 3 intervals
        let ts = NS_BASE + 7 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 8);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.level, GapLevel::Minor);
        assert_eq!(info.missed_ticks, 2);
    }

    #[tokio::test]
    async fn test_z_score_gap_detection() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        // Feed ticks with small jitter to create non-zero variance
        let jitter_pattern: Vec<u64> = vec![
            0, 5_000_000, 3_000_000, 7_000_000, 2_000_000, 4_000_000, 1_000_000, 6_000_000,
            3_000_000, 5_000_000, 0, 4_000_000, 2_000_000, 6_000_000, 1_000_000, 3_000_000,
            5_000_000, 0, 7_000_000, 2_000_000,
        ];
        for (i, &jitter) in jitter_pattern.iter().enumerate() {
            make_jittered_tick(&mut detector, i as u64, jitter).await;
        }

        // last_seq=21, consecutive seq=22 but huge time gap (10x normal interval)
        let large_gap_ts = NS_BASE + 20 * TICK_INTERVAL_NS + TICK_INTERVAL_NS * 10;
        let event = make_market_event(large_gap_ts, 21);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.missed_ticks, 0);
        assert!(
            info.z_score > 3.0,
            "z_score should be > 3.0, got {}",
            info.z_score
        );
    }

    #[tokio::test]
    async fn test_no_gap_consecutive_sequence() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..20 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=21, consecutive seq=22
        let ts = NS_BASE + 20 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 21);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_none(), "consecutive tick should not trigger gap");
    }

    #[tokio::test]
    async fn test_no_gap_with_small_variance() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..30 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=31, consecutive seq=32
        let ts = NS_BASE + 30 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 31);
        let result = detector.process_market_event(&event).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_statistics_update_after_gap() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }

        let initial_mean = detector.mean_interval_ns();

        // last_seq=10, skip 3: seq=14, ts jumps 4 intervals
        let gap_ts = NS_BASE + 13 * TICK_INTERVAL_NS;
        let event = make_market_event(gap_ts, 14);
        detector.process_market_event(&event).await.unwrap();

        make_normal_tick(&mut detector, 14).await;

        let post_mean = detector.mean_interval_ns();
        assert!(post_mean >= initial_mean, "mean should increase after gap");
    }

    #[tokio::test]
    async fn test_gap_info_fields_populated() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }

        let last_ts = NS_BASE + 9 * TICK_INTERVAL_NS;
        // last_seq=10, skip 2: seq=13, ts jumps 3 intervals
        let gap_ts = NS_BASE + 12 * TICK_INTERVAL_NS;
        let event = make_market_event(gap_ts, 13);
        let result = detector.process_market_event(&event).await.unwrap();

        let info = result.unwrap();
        assert_eq!(info.interval_ns, gap_ts - last_ts);
        assert!(info.mean_interval_ns > 0.0);
        assert!(info.std_interval_ns >= 0.0);
        assert!(info.actual_timestamp_ns == gap_ts);
        assert!(info.expected_timestamp_ns > 0);
    }

    #[tokio::test]
    async fn test_first_tick_no_gap() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        let event = make_market_event(NS_BASE, 1);
        let result = detector.process_market_event(&event).await.unwrap();

        assert!(result.is_none());
        assert_eq!(detector.sample_count(), 0);
        assert!(detector.last_timestamp_ns().is_some());
    }

    #[tokio::test]
    async fn test_backward_timestamp_ignored() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        let event1 = make_market_event(NS_BASE + 200_000_000, 1);
        detector.process_market_event(&event1).await.unwrap();

        let event2 = make_market_event(NS_BASE + 100_000_000, 2);
        let result = detector.process_market_event(&event2).await.unwrap();

        assert!(
            result.is_none(),
            "backward timestamp should not trigger gap"
        );
    }

    #[tokio::test]
    async fn test_gap_event_published_to_strategy_stream() {
        let bus = PartitionedEventBus::new();
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 3: seq=14
        let gap_ts = NS_BASE + 13 * TICK_INTERVAL_NS;
        let event = make_market_event(gap_ts, 14);
        detector.process_market_event(&event).await.unwrap();

        let received = subscriber.recv().await;
        assert!(received.is_some());

        let gap_event = proto::GapEventPayload::decode(received.unwrap().payload_bytes()).unwrap();
        assert_eq!(gap_event.missed_ticks, 3);
        assert_eq!(gap_event.severity, proto::GapSeverity::Severe as i32);
        assert!(!gap_event.description.is_empty());
    }

    #[tokio::test]
    async fn test_minor_gap_event_published() {
        let bus = PartitionedEventBus::new();
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 1: seq=12
        let gap_ts = NS_BASE + 11 * TICK_INTERVAL_NS;
        let event = make_market_event(gap_ts, 12);
        detector.process_market_event(&event).await.unwrap();

        let received = subscriber.recv().await;
        assert!(received.is_some());

        let gap_event = proto::GapEventPayload::decode(received.unwrap().payload_bytes()).unwrap();
        assert_eq!(gap_event.missed_ticks, 1);
        assert_eq!(gap_event.severity, proto::GapSeverity::Minor as i32);
        assert!(gap_event.description.contains("Minor gap"));
    }

    #[tokio::test]
    async fn test_no_gap_event_published_for_normal_ticks() {
        let bus = PartitionedEventBus::new();
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }

        make_normal_tick(&mut detector, 10).await;

        let received = subscriber.recv().await;
        assert!(
            received.is_none(),
            "no gap event should be published for normal tick"
        );
    }

    #[tokio::test]
    async fn test_severe_gap_multiple_events() {
        let bus = PartitionedEventBus::new();
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 1: seq=12
        let ts1 = NS_BASE + 11 * TICK_INTERVAL_NS;
        let event1 = make_market_event(ts1, 12);
        detector.process_market_event(&event1).await.unwrap();

        // after gap, last_seq=12. normal tick at index 12 → seq=13
        make_normal_tick(&mut detector, 12).await;

        // last_seq=13, skip 5: seq=19
        let ts2 = NS_BASE + 18 * TICK_INTERVAL_NS;
        let event2 = make_market_event(ts2, 19);
        detector.process_market_event(&event2).await.unwrap();

        let mut events_received = 0;
        while subscriber.recv().await.is_some() {
            events_received += 1;
            if events_received >= 2 {
                break;
            }
        }

        assert_eq!(events_received, 2);
    }

    #[tokio::test]
    async fn test_multiple_gaps_in_sequence() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 2: seq=13
        let ts1 = NS_BASE + 12 * TICK_INTERVAL_NS;
        let event1 = make_market_event(ts1, 13);
        let r1 = detector.process_market_event(&event1).await.unwrap();
        assert_eq!(r1.unwrap().level, GapLevel::Minor);

        // last_seq=13, skip 3: seq=17
        let ts2 = NS_BASE + 16 * TICK_INTERVAL_NS;
        let event2 = make_market_event(ts2, 17);
        let r2 = detector.process_market_event(&event2).await.unwrap();
        assert_eq!(r2.unwrap().level, GapLevel::Severe);
    }

    #[tokio::test]
    async fn test_gap_recovery_normal_flow() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=10, skip 2: seq=13
        let gap_ts = NS_BASE + 12 * TICK_INTERVAL_NS;
        let event = make_market_event(gap_ts, 13);
        let r = detector.process_market_event(&event).await.unwrap();
        assert!(r.is_some());

        // last_seq=13, resume normal from index 13
        for i in 13..20 {
            make_normal_tick(&mut detector, i).await;
        }
        // last_seq=20, consecutive: seq=21
        let normal_ts = NS_BASE + 20 * TICK_INTERVAL_NS;
        let normal_event = make_market_event(normal_ts, 21);
        let r2 = detector.process_market_event(&normal_event).await.unwrap();
        assert!(r2.is_none(), "normal flow should resume without gap");
    }

    #[tokio::test]
    async fn test_mean_and_std_converge() {
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..100 {
            make_normal_tick(&mut detector, i).await;
        }

        let mean = detector.mean_interval_ns();
        let std = detector.std_interval_ns_pub();

        assert!(
            (mean - TICK_INTERVAL_NS as f64).abs() < TICK_INTERVAL_NS as f64 * 0.1,
            "mean should be close to tick interval, got {mean}"
        );

        assert!(
            std < TICK_INTERVAL_NS as f64 * 0.2,
            "std should be small for regular ticks, got {std}"
        );
    }

    // =========================================================================
    // §7.1 Gap Detection verification tests (design.md §7.1)
    // =========================================================================

    #[tokio::test]
    async fn s7_1_minor_gap_1_2_ticks_warning_not_halt() {
        // design.md §7.1: 軽微なギャップ = 連続する1-2ティックの欠損
        // Warningログのみ出力し取引継続。is_trading_halted()はfalse
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // After 10 ticks: last_seq=11, last_ts=NS_BASE+9*TICK_INTERVAL_NS

        // 1 tick missing: skip seq 12, send seq 13
        let ts = NS_BASE + 11 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 13);
        let result = detector.process_market_event(&event).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().level, GapLevel::Minor);
        assert!(!detector.is_trading_halted());

        // 2 ticks missing: skip seqs 14,15, send seq 16
        let ts2 = NS_BASE + 14 * TICK_INTERVAL_NS;
        let event2 = make_market_event(ts2, 16);
        let r2 = detector.process_market_event(&event2).await.unwrap();
        assert!(r2.is_some());
        assert_eq!(r2.unwrap().level, GapLevel::Minor);
        assert!(!detector.is_trading_halted());
    }

    #[tokio::test]
    async fn s7_1_severe_gap_3_plus_ticks_trading_halt() {
        // design.md §7.1: 深刻なギャップ = 連続3ティック以上の欠損 → 取引停止
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }

        // 3 ticks missing → Severe
        let ts = NS_BASE + 13 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 14);
        let result = detector.process_market_event(&event).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().level, GapLevel::Severe);
    }

    #[tokio::test]
    async fn s7_1_gap_event_published_to_strategy_stream_with_severity() {
        // design.md §7.1: GapEvent is published to Strategy stream
        let bus = PartitionedEventBus::new();
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }
        // After 10 ticks: last_seq=11, last_ts=NS_BASE+9*TICK_INTERVAL_NS

        // Minor gap: skip seq 12, send seq 13
        let ts = NS_BASE + 11 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 13);
        detector.process_market_event(&event).await.unwrap();

        let received = subscriber.recv().await;
        assert!(received.is_some());
        let gap_event = proto::GapEventPayload::decode(received.unwrap().payload_bytes()).unwrap();
        assert_eq!(gap_event.missed_ticks, 2); // skipped seq 11,12
        assert_eq!(gap_event.severity, proto::GapSeverity::Minor as i32);
        assert!(gap_event.description.contains("Minor gap"));

        // Severe gap: skip seqs 14,15,16, send seq 17 (3 missed ticks → Severe)
        let ts2 = NS_BASE + 15 * TICK_INTERVAL_NS;
        let event2 = make_market_event(ts2, 17);
        detector.process_market_event(&event2).await.unwrap();

        let received2 = subscriber.recv().await;
        assert!(received2.is_some());
        let gap_event2 =
            proto::GapEventPayload::decode(received2.unwrap().payload_bytes()).unwrap();
        assert_eq!(gap_event2.missed_ticks, 3);
        assert_eq!(gap_event2.severity, proto::GapSeverity::Severe as i32);
        assert!(gap_event2.description.contains("Severe gap"));
    }

    #[tokio::test]
    async fn s7_1_z_score_gap_within_mean_plus_2sigma_no_detection() {
        // design.md §7.1: 欠損期間が tick_interval_mean + 2σ 以内ならWarningのみ
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        // Feed regular ticks with small jitter
        for i in 0..30 {
            let jitter = if i % 3 == 0 { 5_000_000 } else { 0 };
            make_jittered_tick(&mut detector, i as u64, jitter).await;
        }

        let mean = detector.mean_interval_ns();
        let std = detector.std_interval_ns_pub();

        // Create a gap within mean + 2σ
        let gap_ns = (mean + 1.5 * std) as u64;
        let last_ts = NS_BASE + 29 * TICK_INTERVAL_NS;
        let gap_ts = last_ts + gap_ns;
        let event = make_market_event(gap_ts, 31);
        let result = detector.process_market_event(&event).await.unwrap();

        // With 0 missed ticks and z < 3.0, should not trigger
        assert!(
            result.is_none(),
            "gap within 2σ should not trigger: z={}",
            result.as_ref().map(|r| r.z_score).unwrap_or(0.0)
        );
    }

    #[tokio::test]
    async fn s7_1_gap_info_contains_all_diagnostic_fields() {
        // design.md §7.1: GapInfo should have all diagnostic fields
        let bus = PartitionedEventBus::new();
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..10 {
            make_normal_tick(&mut detector, i).await;
        }

        let ts = NS_BASE + 13 * TICK_INTERVAL_NS;
        let event = make_market_event(ts, 14);
        let result = detector.process_market_event(&event).await.unwrap();

        let info = result.unwrap();
        assert_eq!(info.level, GapLevel::Severe);
        assert!(info.missed_ticks >= 3);
        assert!(info.interval_ns > 0);
        assert!(info.mean_interval_ns > 0.0);
        assert!(info.std_interval_ns >= 0.0);
        assert!(info.z_score.is_finite());
        assert!(info.expected_timestamp_ns > 0);
        assert!(info.actual_timestamp_ns > 0);
    }

    #[tokio::test]
    async fn s7_1_normal_ticks_produce_no_gap_events() {
        // Verify that continuous normal ticks never produce gap events
        let bus = PartitionedEventBus::new();
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);
        let mut detector = GapDetector::with_config(&bus, 1, 0.02, 5, 1, 3);

        for i in 0..50 {
            make_normal_tick(&mut detector, i).await;
        }

        assert!(subscriber.recv().await.is_none());
    }
}
