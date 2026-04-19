use std::collections::HashMap;
use std::fs;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use fx_core::types::StreamId;
use uuid::Uuid;

use super::EventStore;
use crate::event::GenericEvent;

struct StoredEntry {
    event: GenericEvent,
    stored_at: Instant,
}

pub struct Tier3Store {
    events: RwLock<HashMap<Uuid, StoredEntry>>,
    stream_index: RwLock<HashMap<StreamId, Vec<(u64, Uuid)>>>,
    ttl: Duration,
    cold_storage_path: Option<String>,
}

impl Tier3Store {
    pub fn new(ttl: Duration) -> Self {
        Self {
            events: RwLock::new(HashMap::new()),
            stream_index: RwLock::new(HashMap::new()),
            ttl,
            cold_storage_path: None,
        }
    }

    pub fn with_cold_storage(mut self, path: String) -> Self {
        self.cold_storage_path = Some(path);
        self
    }

    fn is_expired(&self, stored_at: Instant) -> bool {
        stored_at.elapsed() > self.ttl
    }

    pub fn archive_expired(&self) -> Result<Vec<Uuid>> {
        let mut events = self.events.write().unwrap();
        let mut stream_index = self.stream_index.write().unwrap();

        let expired: Vec<Uuid> = events
            .iter()
            .filter(|(_, entry)| self.is_expired(entry.stored_at))
            .map(|(id, _)| *id)
            .collect();

        let mut archived = Vec::new();
        for id in &expired {
            if let Some(entry) = events.remove(id) {
                if let Some(ref path) = self.cold_storage_path {
                    let file_name = format!("{}/{}.json", path, id);
                    if let Some(parent) = std::path::Path::new(&file_name).parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    if let Ok(json) = serde_json::to_string(&entry.event) {
                        let _ = fs::write(&file_name, json);
                    }
                }

                if let Some(entries) = stream_index.get_mut(&entry.event.header.stream_id) {
                    entries.retain(|(_, eid)| eid != id);
                }

                archived.push(*id);
            }
        }

        Ok(archived)
    }

    pub fn len(&self) -> usize {
        self.events.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.read().unwrap().is_empty()
    }
}

impl EventStore for Tier3Store {
    fn store(&self, event: &GenericEvent) -> Result<()> {
        let mut events = self.events.write().unwrap();
        let mut stream_index = self.stream_index.write().unwrap();

        let id = event.header.event_id;
        let stream_id = event.header.stream_id;
        let seq = event.header.sequence_id;

        events.insert(
            id,
            StoredEntry {
                event: event.clone(),
                stored_at: Instant::now(),
            },
        );

        stream_index.entry(stream_id).or_default().push((seq, id));

        Ok(())
    }

    fn load(&self, event_id: Uuid) -> Result<Option<GenericEvent>> {
        let events = self.events.read().unwrap();
        match events.get(&event_id) {
            Some(entry) => {
                if self.is_expired(entry.stored_at) {
                    Ok(None)
                } else {
                    Ok(Some(entry.event.clone()))
                }
            }
            None => Ok(None),
        }
    }

    fn replay(&self, stream_id: StreamId, from_seq: u64) -> Result<Vec<GenericEvent>> {
        let events = self.events.read().unwrap();
        let stream_index = self.stream_index.read().unwrap();

        let mut result = Vec::new();
        if let Some(entries) = stream_index.get(&stream_id) {
            for (seq, id) in entries {
                if *seq >= from_seq {
                    if let Some(entry) = events.get(id) {
                        if !self.is_expired(entry.stored_at) {
                            result.push(entry.event.clone());
                        }
                    }
                }
            }
        }

        result.sort_by_key(|e| e.header.sequence_id);
        Ok(result)
    }

    fn remove(&self, event_id: Uuid) -> Result<bool> {
        let mut events = self.events.write().unwrap();
        let mut stream_index = self.stream_index.write().unwrap();

        let removed = events.remove(&event_id).is_some();
        if removed {
            for entries in stream_index.values_mut() {
                entries.retain(|(_, id)| id != &event_id);
            }
        }

        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use fx_core::types::{EventTier, StreamId};

    use super::*;
    use crate::header::EventHeader;

    fn make_event(stream_id: StreamId, seq: u64, payload: &[u8]) -> GenericEvent {
        let mut header = EventHeader::new(stream_id, seq, EventTier::Tier3Raw);
        header.sequence_id = seq;
        GenericEvent::new(header, payload.to_vec())
    }

    #[test]
    fn test_store_and_load() {
        let store = Tier3Store::new(Duration::from_secs(300));
        let event = make_event(StreamId::Market, 1, b"tick_data");

        store.store(&event).unwrap();
        let loaded = store.load(event.header.event_id).unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().payload, b"tick_data");
    }

    #[test]
    fn test_load_nonexistent() {
        let store = Tier3Store::new(Duration::from_secs(300));
        assert!(store.load(Uuid::now_v7()).unwrap().is_none());
    }

    #[test]
    fn test_replay_ordered() {
        let store = Tier3Store::new(Duration::from_secs(300));
        for i in 1..=4 {
            let event = make_event(StreamId::Market, i, &[i as u8]);
            store.store(&event).unwrap();
        }

        let events = store.replay(StreamId::Market, 1).unwrap();
        assert_eq!(events.len(), 4);
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.header.sequence_id, (i + 1) as u64);
        }
    }

    #[test]
    fn test_remove() {
        let store = Tier3Store::new(Duration::from_secs(300));
        let event = make_event(StreamId::Market, 1, b"tick");

        store.store(&event).unwrap();
        assert!(store.remove(event.header.event_id).unwrap());
        assert!(store.load(event.header.event_id).unwrap().is_none());
    }

    #[test]
    fn test_len_and_empty() {
        let store = Tier3Store::new(Duration::from_secs(300));
        assert!(store.is_empty());

        let event = make_event(StreamId::Market, 1, b"tick");
        store.store(&event).unwrap();
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_archive_expired_to_cold_storage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Tier3Store::new(Duration::from_millis(1))
            .with_cold_storage(dir.path().to_string_lossy().into_owned());

        let event = make_event(StreamId::Market, 1, b"expired_tick");
        store.store(&event).unwrap();

        // Wait for TTL to expire
        std::thread::sleep(std::time::Duration::from_millis(5));

        let archived = store.archive_expired().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0], event.header.event_id);
        assert!(store.is_empty());

        // Verify cold storage file exists
        let file_path = format!(
            "{}/{}.json",
            dir.path().to_string_lossy(),
            event.header.event_id
        );
        assert!(std::path::Path::new(&file_path).exists());
    }
}
