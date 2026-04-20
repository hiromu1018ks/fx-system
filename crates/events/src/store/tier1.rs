use anyhow::{Context, Result};
use fx_core::types::StreamId;
use sled::Db;
use uuid::Uuid;

use super::EventStore;
use crate::event::GenericEvent;

const EVENTS_TREE: &[u8] = b"tier1_events";
const STREAM_INDEX_TREE: &[u8] = b"tier1_stream_index";

fn stream_id_byte(stream_id: StreamId) -> u8 {
    match stream_id {
        StreamId::Market => 0,
        StreamId::Strategy => 1,
        StreamId::Execution => 2,
        StreamId::State => 3,
    }
}

fn stream_seq_key(stream_id: StreamId, sequence_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.push(stream_id_byte(stream_id));
    key.extend_from_slice(&sequence_id.to_be_bytes());
    key
}

pub struct Tier1Store {
    db: Db,
    #[allow(dead_code)]
    _temp_dir: Option<tempfile::TempDir>,
}

impl Tier1Store {
    pub fn open(path: &str) -> Result<Self> {
        let db = sled::open(path).context("Failed to open Tier1Store database")?;
        Ok(Self {
            db,
            _temp_dir: None,
        })
    }

    pub fn open_temp() -> Result<Self> {
        let dir = tempfile::tempdir().context("Failed to create temp dir")?;
        let db = sled::open(dir.path()).context("Failed to open Tier1Store temp database")?;
        Ok(Self {
            db,
            _temp_dir: Some(dir),
        })
    }
}

impl EventStore for Tier1Store {
    fn store(&self, event: &GenericEvent) -> Result<()> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;

        let event_key = event.header.event_id.as_bytes();
        let value = serde_json::to_vec(event).context("Failed to serialize event")?;
        events.insert(event_key, value)?;

        let index_key = stream_seq_key(event.header.stream_id, event.header.sequence_id);
        stream_index.insert(index_key, event_key)?;

        self.db.flush()?;
        Ok(())
    }

    fn load(&self, event_id: Uuid) -> Result<Option<GenericEvent>> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let key = event_id.as_bytes();
        match events.get(key).context("Failed to read from events tree")? {
            Some(bytes) => {
                let event: GenericEvent =
                    serde_json::from_slice(&bytes).context("Failed to deserialize event")?;
                Ok(Some(event))
            }
            None => Ok(None),
        }
    }

    fn replay(&self, stream_id: StreamId, from_seq: u64) -> Result<Vec<GenericEvent>> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;

        let prefix = &[stream_id_byte(stream_id)];
        let mut result = Vec::new();

        for item in stream_index.scan_prefix(prefix) {
            let (index_key, event_id_bytes) = item?;
            if index_key.len() < 9 {
                continue;
            }
            let seq = u64::from_be_bytes(
                index_key[1..9]
                    .try_into()
                    .context("Invalid sequence key length")?,
            );
            if seq < from_seq {
                continue;
            }
            if let Some(bytes) = events.get(&event_id_bytes)? {
                let event: GenericEvent = serde_json::from_slice(&bytes)
                    .context("Failed to deserialize event during replay")?;
                result.push(event);
            }
        }

        result.sort_by_key(|e| e.header.sequence_id);
        Ok(result)
    }

    fn remove(&self, event_id: Uuid) -> Result<bool> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let key = event_id.as_bytes();
        let removed = events
            .remove(key)
            .context("Failed to remove event")?
            .is_some();
        self.db.flush()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use fx_core::types::{EventTier, StreamId};

    use super::*;
    use crate::header::EventHeader;

    fn make_event(stream_id: StreamId, seq: u64, payload: &[u8]) -> GenericEvent {
        let mut header = EventHeader::new(stream_id, seq, EventTier::Tier1Critical);
        header.sequence_id = seq;
        GenericEvent::new(header, payload.to_vec())
    }

    #[test]
    fn test_store_and_load() {
        let store = Tier1Store::open_temp().unwrap();
        let event = make_event(StreamId::Execution, 1, b"fill_event");

        store.store(&event).unwrap();
        let loaded = store.load(event.header.event_id).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.payload, b"fill_event");
        assert_eq!(loaded.header.sequence_id, 1);
    }

    #[test]
    fn test_load_nonexistent() {
        let store = Tier1Store::open_temp().unwrap();
        let result = store.load(Uuid::now_v7()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_replay_ordered() {
        let store = Tier1Store::open_temp().unwrap();
        for i in 1..=5 {
            let event = make_event(StreamId::Strategy, i, &[i as u8]);
            store.store(&event).unwrap();
        }

        let events = store.replay(StreamId::Strategy, 1).unwrap();
        assert_eq!(events.len(), 5);
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.header.sequence_id, (i + 1) as u64);
        }
    }

    #[test]
    fn test_replay_from_seq() {
        let store = Tier1Store::open_temp().unwrap();
        for i in 1..=5 {
            let event = make_event(StreamId::Market, i, &[i as u8]);
            store.store(&event).unwrap();
        }

        let events = store.replay(StreamId::Market, 3).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].header.sequence_id, 3);
        assert_eq!(events[2].header.sequence_id, 5);
    }

    #[test]
    fn test_replay_stream_isolation() {
        let store = Tier1Store::open_temp().unwrap();
        let e1 = make_event(StreamId::Market, 1, b"m1");
        let e2 = make_event(StreamId::Execution, 1, b"e1");
        store.store(&e1).unwrap();
        store.store(&e2).unwrap();

        let market_events = store.replay(StreamId::Market, 0).unwrap();
        assert_eq!(market_events.len(), 1);
        assert_eq!(market_events[0].payload, b"m1");

        let exec_events = store.replay(StreamId::Execution, 0).unwrap();
        assert_eq!(exec_events.len(), 1);
        assert_eq!(exec_events[0].payload, b"e1");
    }

    #[test]
    fn test_remove() {
        let store = Tier1Store::open_temp().unwrap();
        let event = make_event(StreamId::State, 1, b"snap");
        store.store(&event).unwrap();

        assert!(store.remove(event.header.event_id).unwrap());
        assert!(!store.remove(event.header.event_id).unwrap());
        assert!(store.load(event.header.event_id).unwrap().is_none());
    }

    // =========================================================================
    // §7.3 Tiered Event Store verification tests (design.md §7.3)
    // =========================================================================

    #[test]
    fn s7_3_tier1_persistent_across_operations() {
        // design.md §7.3: Tier1 (Critical) → NVMe SSDに永続保存
        // Verify events survive store→load cycle (persistence via sled)
        let store = Tier1Store::open_temp().unwrap();

        let events: Vec<GenericEvent> = (1..=5)
            .map(|i| {
                make_event(
                    StreamId::Execution,
                    i,
                    format!("fill_payload_{}", i).as_bytes(),
                )
            })
            .collect();

        for e in &events {
            store.store(e).unwrap();
        }

        // All events must be loadable
        for e in &events {
            let loaded = store.load(e.header.event_id).unwrap();
            assert!(loaded.is_some());
            assert_eq!(loaded.unwrap().payload, e.payload);
        }
    }

    #[test]
    fn s7_3_tier1_replay_returns_ordered_events() {
        // design.md §7.3: Tier1 stores critical events (OrderSent, Fill, StateSnapshot)
        let store = Tier1Store::open_temp().unwrap();

        for i in 1..=10 {
            let event = make_event(StreamId::State, i, format!("snapshot_{}", i).as_bytes());
            store.store(&event).unwrap();
        }

        let replayed = store.replay(StreamId::State, 1).unwrap();
        assert_eq!(replayed.len(), 10);
        for (i, e) in replayed.iter().enumerate() {
            assert_eq!(e.header.sequence_id, (i + 1) as u64);
        }

        // Partial replay
        let partial = store.replay(StreamId::State, 5).unwrap();
        assert_eq!(partial.len(), 6);
        assert_eq!(partial[0].header.sequence_id, 5);
    }

    #[test]
    fn s7_3_tier1_critical_event_types() {
        // design.md §7.3: OrderSent, Fill, StateSnapshot → Tier1
        // Verify Tier1 can store events from all 4 streams (critical ones)
        let store = Tier1Store::open_temp().unwrap();

        let critical_streams = [
            (StreamId::Execution, "fill_event"),
            (StreamId::State, "state_snapshot"),
        ];

        for (sid, payload) in &critical_streams {
            let event = make_event(*sid, 1, payload.as_bytes());
            store.store(&event).unwrap();
            let loaded = store.load(event.header.event_id).unwrap().unwrap();
            assert_eq!(loaded.payload, payload.as_bytes());
        }
    }
}
