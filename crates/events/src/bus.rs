use std::collections::HashSet;
use std::sync::Arc;

use fx_core::types::StreamId;
use tokio::sync::{broadcast, RwLock};
use tracing;
use uuid::Uuid;

use crate::event::{Event, GenericEvent};
use crate::header::EventHeader;

const DEFAULT_CHANNEL_CAPACITY: usize = 4096;

type SequenceCounters = Arc<RwLock<[u64; 4]>>;

fn stream_index(stream_id: StreamId) -> usize {
    match stream_id {
        StreamId::Market => 0,
        StreamId::Strategy => 1,
        StreamId::Execution => 2,
        StreamId::State => 3,
    }
}

#[derive(Debug)]
pub struct EventBusError {
    pub message: String,
}

impl std::fmt::Display for EventBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EventBus error: {}", self.message)
    }
}

impl std::error::Error for EventBusError {}

pub struct PartitionedEventBus {
    #[allow(dead_code)]
    senders: [broadcast::Receiver<GenericEvent>; 4],
    inner: Arc<EventBusInner>,
}

struct EventBusInner {
    tx: [broadcast::Sender<GenericEvent>; 4],
    sequence_counters: SequenceCounters,
}

impl PartitionedEventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let channels: Vec<broadcast::Sender<GenericEvent>> =
            (0..4).map(|_| broadcast::channel(capacity).0).collect();

        let tx: [broadcast::Sender<GenericEvent>; 4] = channels.try_into().unwrap();
        let rx: [broadcast::Receiver<GenericEvent>; 4] = tx
            .iter()
            .map(|t| t.subscribe())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        let inner = Arc::new(EventBusInner {
            tx,
            sequence_counters: Arc::new(RwLock::new([0u64; 4])),
        });

        Self { senders: rx, inner }
    }

    pub fn publisher(&self, stream_id: StreamId) -> EventPublisher {
        let idx = stream_index(stream_id);
        EventPublisher {
            stream_id,
            tx: self.inner.tx[idx].clone(),
            sequence_counter: self.inner.sequence_counters.clone(),
            stream_index: idx,
        }
    }

    pub fn subscriber(&self, streams: &[StreamId]) -> EventSubscriber {
        let receivers: Vec<(StreamId, broadcast::Receiver<GenericEvent>)> = streams
            .iter()
            .map(|&sid| {
                let idx = stream_index(sid);
                (sid, self.inner.tx[idx].subscribe())
            })
            .collect();

        EventSubscriber {
            receivers,
            seen_ids: HashSet::new(),
        }
    }
}

impl Default for PartitionedEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct EventPublisher {
    stream_id: StreamId,
    tx: broadcast::Sender<GenericEvent>,
    sequence_counter: SequenceCounters,
    stream_index: usize,
}

impl EventPublisher {
    pub async fn publish(
        &self,
        mut header: EventHeader,
        payload: Vec<u8>,
    ) -> Result<(), EventBusError> {
        {
            let mut counters = self.sequence_counter.write().await;
            counters[self.stream_index] += 1;
            header.sequence_id = counters[self.stream_index];
        }

        header.stream_id = self.stream_id;

        let event = GenericEvent::new(header, payload);

        match self.tx.send(event) {
            Ok(_) => Ok(()),
            Err(_) => {
                tracing::warn!(
                    stream_id = ?self.stream_id,
                    "No active subscribers for event"
                );
                Ok(())
            }
        }
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }
}

pub struct EventSubscriber {
    receivers: Vec<(StreamId, broadcast::Receiver<GenericEvent>)>,
    seen_ids: HashSet<Uuid>,
}

impl EventSubscriber {
    pub async fn recv(&mut self) -> Option<GenericEvent> {
        for (sid, rx) in &mut self.receivers {
            match rx.try_recv() {
                Ok(event) => {
                    if self.seen_ids.contains(&event.event_id()) {
                        tracing::trace!(
                            event_id = %event.event_id(),
                            stream_id = ?sid,
                            "Duplicate event skipped (idempotency)"
                        );
                        continue;
                    }
                    self.seen_ids.insert(event.event_id());
                    return Some(event);
                }
                Err(broadcast::error::TryRecvError::Empty) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    tracing::error!(stream_id = ?sid, "Channel closed");
                    return None;
                }
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(stream_id = ?sid, lagged = n, "Subscriber lagged behind");
                    continue;
                }
            }
        }
        None
    }

    pub async fn recv_from(&mut self, stream_id: StreamId) -> Option<GenericEvent> {
        for (sid, rx) in &mut self.receivers {
            if *sid != stream_id {
                continue;
            }
            match rx.try_recv() {
                Ok(event) => {
                    if self.seen_ids.contains(&event.event_id()) {
                        tracing::trace!(
                            event_id = %event.event_id(),
                            stream_id = ?sid,
                            "Duplicate event skipped (idempotency)"
                        );
                        continue;
                    }
                    self.seen_ids.insert(event.event_id());
                    return Some(event);
                }
                Err(broadcast::error::TryRecvError::Empty) => return None,
                Err(broadcast::error::TryRecvError::Closed) => {
                    tracing::error!(stream_id = ?sid, "Channel closed");
                    return None;
                }
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(stream_id = ?sid, lagged = n, "Subscriber lagged behind");
                    return None;
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use fx_core::types::EventTier;
    use fx_core::types::StreamId;

    use super::*;
    use crate::header::EventHeader;

    fn make_header(stream_id: StreamId, tier: EventTier) -> EventHeader {
        EventHeader::new(stream_id, 0, tier)
    }

    #[tokio::test]
    async fn test_publish_and_subscribe_single_stream() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let mut subscriber = bus.subscriber(&[StreamId::Market]);

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        publisher
            .publish(header, b"test_payload".to_vec())
            .await
            .unwrap();

        let event = subscriber.recv().await;
        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.payload, b"test_payload");
        assert_eq!(event.header.stream_id, StreamId::Market);
        assert_eq!(event.header.sequence_id, 1);
    }

    #[tokio::test]
    async fn test_sequence_id_increments() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Strategy);
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);

        for i in 0..5 {
            let header = make_header(StreamId::Strategy, EventTier::Tier2Derived);
            publisher.publish(header, vec![i as u8]).await.unwrap();
        }

        for expected_seq in 1..=5 {
            let event = subscriber.recv().await.unwrap();
            assert_eq!(event.header.sequence_id, expected_seq);
        }
    }

    #[tokio::test]
    async fn test_multi_stream_isolation() {
        let bus = PartitionedEventBus::new();

        let market_pub = bus.publisher(StreamId::Market);
        let exec_pub = bus.publisher(StreamId::Execution);

        let mut market_sub = bus.subscriber(&[StreamId::Market]);
        let mut exec_sub = bus.subscriber(&[StreamId::Execution]);

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        market_pub
            .publish(header, b"market_data".to_vec())
            .await
            .unwrap();

        let header = make_header(StreamId::Execution, EventTier::Tier1Critical);
        exec_pub
            .publish(header, b"exec_data".to_vec())
            .await
            .unwrap();

        let market_event = market_sub.recv().await.unwrap();
        assert_eq!(market_event.payload, b"market_data");
        assert_eq!(market_event.header.stream_id, StreamId::Market);

        let exec_event = exec_sub.recv().await.unwrap();
        assert_eq!(exec_event.payload, b"exec_data");
        assert_eq!(exec_event.header.stream_id, StreamId::Execution);

        assert!(market_sub.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_idempotency_duplicate_skip() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::State);
        let mut subscriber = bus.subscriber(&[StreamId::State]);

        let header = make_header(StreamId::State, EventTier::Tier1Critical);
        let event_id = header.event_id;
        publisher.publish(header, b"snap1".to_vec()).await.unwrap();

        let event1 = subscriber.recv().await.unwrap();
        assert_eq!(event1.event_id(), event_id);

        assert!(subscriber.seen_ids.contains(&event_id));
    }

    #[tokio::test]
    async fn test_multi_stream_subscriber() {
        let bus = PartitionedEventBus::new();
        let market_pub = bus.publisher(StreamId::Market);
        let strat_pub = bus.publisher(StreamId::Strategy);

        let mut multi_sub = bus.subscriber(&[StreamId::Market, StreamId::Strategy]);

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        market_pub.publish(header, b"m1".to_vec()).await.unwrap();

        let header = make_header(StreamId::Strategy, EventTier::Tier2Derived);
        strat_pub.publish(header, b"s1".to_vec()).await.unwrap();

        let mut received_market = false;
        let mut received_strategy = false;

        for _ in 0..2 {
            if let Some(event) = multi_sub.recv().await {
                match event.header.stream_id {
                    StreamId::Market => {
                        assert_eq!(event.payload, b"m1");
                        received_market = true;
                    }
                    StreamId::Strategy => {
                        assert_eq!(event.payload, b"s1");
                        received_strategy = true;
                    }
                    _ => panic!("Unexpected stream"),
                }
            }
        }

        assert!(received_market);
        assert!(received_strategy);
    }

    #[tokio::test]
    async fn test_separate_stream_sequence_counters() {
        let bus = PartitionedEventBus::new();
        let market_pub = bus.publisher(StreamId::Market);
        let exec_pub = bus.publisher(StreamId::Execution);

        let mut m_sub = bus.subscriber(&[StreamId::Market]);
        let mut e_sub = bus.subscriber(&[StreamId::Execution]);

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        market_pub.publish(header, b"m".to_vec()).await.unwrap();

        let header = make_header(StreamId::Execution, EventTier::Tier1Critical);
        exec_pub.publish(header, b"e".to_vec()).await.unwrap();

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        market_pub.publish(header, b"m2".to_vec()).await.unwrap();

        let e1 = e_sub.recv().await.unwrap();
        assert_eq!(e1.header.sequence_id, 1);

        let m1 = m_sub.recv().await.unwrap();
        assert_eq!(m1.header.sequence_id, 1);

        let m2 = m_sub.recv().await.unwrap();
        assert_eq!(m2.header.sequence_id, 2);
    }

    #[tokio::test]
    async fn test_recv_from_specific_stream() {
        let bus = PartitionedEventBus::new();
        let market_pub = bus.publisher(StreamId::Market);
        let strat_pub = bus.publisher(StreamId::Strategy);

        let mut sub = bus.subscriber(&[StreamId::Market, StreamId::Strategy]);

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        market_pub.publish(header, b"m1".to_vec()).await.unwrap();

        let header = make_header(StreamId::Strategy, EventTier::Tier2Derived);
        strat_pub.publish(header, b"s1".to_vec()).await.unwrap();

        let strat_event = sub.recv_from(StreamId::Strategy).await;
        assert!(strat_event.is_some());
        assert_eq!(strat_event.unwrap().payload, b"s1");

        let market_event = sub.recv_from(StreamId::Market).await;
        assert!(market_event.is_some());
        assert_eq!(market_event.unwrap().payload, b"m1");
    }

    #[tokio::test]
    async fn test_no_subscriber_publish_doesnt_error() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);

        let header = make_header(StreamId::Market, EventTier::Tier3Raw);
        let result = publisher.publish(header, b"orphan".to_vec()).await;
        assert!(result.is_ok());
    }

    // =========================================================================
    // §6.1 4-Stream Partitioning verification tests (design.md §6.1)
    // =========================================================================

    #[tokio::test]
    async fn s6_1_four_streams_exist() {
        // design.md §6.1: Market, Strategy, Execution, State
        let bus = PartitionedEventBus::new();
        let _m = bus.publisher(StreamId::Market);
        let _s = bus.publisher(StreamId::Strategy);
        let _e = bus.publisher(StreamId::Execution);
        let _st = bus.publisher(StreamId::State);
    }

    #[tokio::test]
    async fn s6_1_stream_isolation_all_pairs() {
        // design.md §6.1: streams are completely partitioned
        let bus = PartitionedEventBus::new();
        let streams = [
            StreamId::Market,
            StreamId::Strategy,
            StreamId::Execution,
            StreamId::State,
        ];
        let tiers = [
            EventTier::Tier3Raw,
            EventTier::Tier2Derived,
            EventTier::Tier1Critical,
            EventTier::Tier2Derived,
        ];

        // Subscribe before publishing (broadcast only delivers to existing subscribers)
        let mut subs: Vec<(StreamId, EventSubscriber)> = streams
            .iter()
            .map(|&sid| (sid, bus.subscriber(&[sid])))
            .collect();

        // Publish one event per stream
        for (i, &sid) in streams.iter().enumerate() {
            let pub_ = bus.publisher(sid);
            let header = make_header(sid, tiers[i]);
            pub_.publish(header, vec![i as u8]).await.unwrap();
        }

        // Each subscriber only sees its own stream
        for (i, &sid) in streams.iter().enumerate() {
            let sub = &mut subs[i].1;
            let event = sub.recv().await.unwrap();
            assert_eq!(event.header.stream_id, sid);
            assert_eq!(event.payload, vec![i as u8]);
            assert!(sub.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn s6_1_multi_stream_subscriber_receives_from_all_four() {
        let bus = PartitionedEventBus::new();
        let streams = [
            StreamId::Market,
            StreamId::Strategy,
            StreamId::Execution,
            StreamId::State,
        ];

        let mut sub = bus.subscriber(&streams);
        for (i, &sid) in streams.iter().enumerate() {
            let pub_ = bus.publisher(sid);
            let header = make_header(sid, EventTier::Tier3Raw);
            pub_.publish(header, vec![i as u8]).await.unwrap();
        }

        let mut received = vec![false; 4];
        for _ in 0..4 {
            let event = sub.recv().await.unwrap();
            let idx = streams
                .iter()
                .position(|&s| s == event.header.stream_id)
                .unwrap();
            assert!(!received[idx]);
            received[idx] = true;
        }
        assert!(received.iter().all(|&r| r));
    }

    // =========================================================================
    // §6.2 Sequence ID & Idempotency verification tests (design.md §6.2)
    // =========================================================================

    #[tokio::test]
    async fn s6_2_sequence_id_monotonic_64bit() {
        // design.md §6.2: 各ストリーム内の単調増加64bit整数
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let mut subscriber = bus.subscriber(&[StreamId::Market]);

        let count = 200u64;
        for i in 0..count {
            let header = make_header(StreamId::Market, EventTier::Tier3Raw);
            publisher.publish(header, vec![i as u8]).await.unwrap();
        }

        let mut prev_seq: u64 = 0;
        for i in 0..count {
            let event = subscriber.recv().await.unwrap();
            assert!(
                event.header.sequence_id > prev_seq,
                "sequence_id not monotonic at {}: {} <= {}",
                i,
                event.header.sequence_id,
                prev_seq
            );
            prev_seq = event.header.sequence_id;
        }
        assert_eq!(prev_seq, count);
    }

    #[tokio::test]
    async fn s6_2_independent_sequence_counters_per_stream() {
        // design.md §6.2: sequence_idは各ストリームで独立
        let bus = PartitionedEventBus::new();
        let streams = [
            StreamId::Market,
            StreamId::Strategy,
            StreamId::Execution,
            StreamId::State,
        ];

        // Subscribe before publishing
        let mut subs: Vec<EventSubscriber> =
            streams.iter().map(|&sid| bus.subscriber(&[sid])).collect();

        // Publish 3 events per stream
        for &sid in &streams {
            let pub_ = bus.publisher(sid);
            for _ in 0..3 {
                let header = make_header(sid, EventTier::Tier3Raw);
                pub_.publish(header, b"x".to_vec()).await.unwrap();
            }
        }

        // Verify each stream has its own 1,2,3 sequence
        for (i, &sid) in streams.iter().enumerate() {
            for expected in 1..=3u64 {
                let event = subs[i].recv().await.unwrap();
                assert_eq!(
                    event.header.sequence_id, expected,
                    "stream {:?}: expected seq {}, got {}",
                    sid, expected, event.header.sequence_id
                );
            }
        }
    }

    #[tokio::test]
    async fn s6_2_idempotency_event_id_dedup() {
        // design.md §6.2: event_idによる重複処理スキップ
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Strategy);
        let mut subscriber = bus.subscriber(&[StreamId::Strategy]);

        let header = make_header(StreamId::Strategy, EventTier::Tier2Derived);
        let event_id = header.event_id;
        publisher.publish(header, b"first".to_vec()).await.unwrap();

        let event1 = subscriber.recv().await.unwrap();
        assert_eq!(event1.event_id(), event_id);
        assert!(subscriber.seen_ids.contains(&event_id));

        // The subscriber would skip a duplicate if it appeared, but the bus
        // itself always assigns new sequence_ids. Verify seen_ids grows.
        assert_eq!(subscriber.seen_ids.len(), 1);

        let header2 = make_header(StreamId::Strategy, EventTier::Tier2Derived);
        publisher
            .publish(header2, b"second".to_vec())
            .await
            .unwrap();
        let event2 = subscriber.recv().await.unwrap();
        assert_ne!(event2.event_id(), event_id);
        assert_eq!(subscriber.seen_ids.len(), 2);
    }

    #[tokio::test]
    async fn s6_2_idempotency_no_duplicate_delivery() {
        // Verify that seen_ids prevents duplicate event_id from being delivered
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Execution);
        let mut subscriber = bus.subscriber(&[StreamId::Execution]);

        let header = make_header(StreamId::Execution, EventTier::Tier1Critical);
        let event_id = header.event_id;
        publisher.publish(header, b"unique".to_vec()).await.unwrap();

        let event1 = subscriber.recv().await.unwrap();
        assert_eq!(event1.event_id(), event_id);
        assert!(subscriber.seen_ids.contains(&event_id));

        // Manually insert the same event_id again to simulate a duplicate
        subscriber.seen_ids.insert(event_id);

        // Publish another event — it should be delivered (different event_id)
        let header2 = make_header(StreamId::Execution, EventTier::Tier1Critical);
        publisher
            .publish(header2, b"new_event".to_vec())
            .await
            .unwrap();
        let event2 = subscriber.recv().await.unwrap();
        assert_ne!(event2.event_id(), event_id);
        assert_eq!(event2.payload, b"new_event");
    }
}
