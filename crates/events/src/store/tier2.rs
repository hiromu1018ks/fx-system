use std::io::{Read, Write};

use anyhow::{Context, Result};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use fx_core::types::StreamId;
use serde::{Deserialize, Serialize};
use sled::Db;
use uuid::Uuid;

use super::EventStore;
use crate::event::GenericEvent;

const EVENTS_TREE: &[u8] = b"tier2_events";
const HEADERS_TREE: &[u8] = b"tier2_headers";
const STREAM_INDEX_TREE: &[u8] = b"tier2_stream_index";
const BASES_TREE: &[u8] = b"tier2_bases";
const LAST_PAYLOADS_TREE: &[u8] = b"tier2_last_payloads";

const DEFAULT_BASE_INTERVAL: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEntry {
    is_full: bool,
    data: Vec<u8>,
}

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

pub fn compute_delta(prev: &[u8], current: &[u8]) -> Vec<u8> {
    let min_len = prev.len().min(current.len());
    let mut delta = Vec::with_capacity(current.len());
    for i in 0..min_len {
        delta.push(prev[i] ^ current[i]);
    }
    delta.extend_from_slice(&current[min_len..]);
    delta
}

pub fn apply_delta(prev: &[u8], delta: &[u8]) -> Vec<u8> {
    let min_len = prev.len().min(delta.len());
    let mut result = Vec::with_capacity(delta.len());
    for i in 0..min_len {
        result.push(prev[i] ^ delta[i]);
    }
    result.extend_from_slice(&delta[min_len..]);
    result
}

pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .context("Compression write failed")?;
    encoder.finish().context("Compression flush failed")
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = DeflateDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .context("Decompression failed")?;
    Ok(decompressed)
}

pub struct Tier2Store {
    db: Db,
    base_interval: u64,
    #[allow(dead_code)]
    _temp_dir: Option<tempfile::TempDir>,
}

impl Tier2Store {
    pub fn open(path: &str) -> Result<Self> {
        let db = sled::open(path).context("Failed to open Tier2Store database")?;
        Ok(Self {
            db,
            base_interval: DEFAULT_BASE_INTERVAL,
            _temp_dir: None,
        })
    }

    pub fn open_temp() -> Result<Self> {
        let dir = tempfile::tempdir().context("Failed to create temp dir")?;
        let db = sled::open(dir.path()).context("Failed to open Tier2Store temp database")?;
        Ok(Self {
            db,
            base_interval: DEFAULT_BASE_INTERVAL,
            _temp_dir: Some(dir),
        })
    }

    pub fn with_base_interval(mut self, interval: u64) -> Self {
        assert!(interval > 0, "Base interval must be positive");
        self.base_interval = interval;
        self
    }

    fn stream_event_count(&self, stream_id: StreamId) -> Result<u64> {
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;
        let prefix = &[stream_id_byte(stream_id)];
        Ok(stream_index.scan_prefix(prefix).count() as u64)
    }

    fn find_base_before(&self, stream_id: StreamId, seq: u64) -> Result<Option<(u64, [u8; 16])>> {
        let bases = self
            .db
            .open_tree(BASES_TREE)
            .context("Failed to open bases tree")?;
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;

        let prefix = &[stream_id_byte(stream_id)];
        let mut best: Option<(u64, [u8; 16])> = None;

        for item in bases.scan_prefix(prefix) {
            let (key, _) = item?;
            if key.len() < 9 {
                continue;
            }
            let base_seq = u64::from_be_bytes(key[1..9].try_into().context("Invalid base key")?);
            if base_seq <= seq {
                let idx_key = stream_seq_key(stream_id, base_seq);
                if let Some(event_id_bytes) = stream_index.get(&idx_key)? {
                    let arr: [u8; 16] = event_id_bytes
                        .as_ref()
                        .try_into()
                        .context("Invalid event ID length")?;
                    best = Some((base_seq, arr));
                }
            }
        }
        Ok(best)
    }

    fn replay_payloads_from_base(
        &self,
        stream_id: StreamId,
        target_seq: u64,
    ) -> Result<Option<Vec<u8>>> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;

        let base = self.find_base_before(stream_id, target_seq)?;
        let start_seq = base.as_ref().map(|(seq, _)| *seq).unwrap_or(target_seq);

        let prefix = &[stream_id_byte(stream_id)];
        let mut current_payload: Vec<u8> = Vec::new();
        let mut base_loaded = base.is_none();
        let mut found_target = false;

        for item in stream_index.scan_prefix(prefix) {
            let (idx_key, eid_bytes) = item?;
            if idx_key.len() < 9 {
                continue;
            }
            let seq = u64::from_be_bytes(idx_key[1..9].try_into().context("Invalid seq key")?);
            if seq < start_seq {
                continue;
            }

            let entry_bytes = events
                .get(&eid_bytes)?
                .context("Event data not found during replay")?;
            let entry: StoredEntry =
                serde_json::from_slice(&entry_bytes).context("Failed to deserialize entry")?;

            if entry.is_full {
                current_payload = decompress(&entry.data)?;
                base_loaded = true;
            } else if base_loaded {
                let delta = decompress(&entry.data)?;
                current_payload = apply_delta(&current_payload, &delta);
            } else {
                continue;
            }

            if seq == target_seq {
                found_target = true;
                break;
            }
        }

        if found_target {
            Ok(Some(current_payload))
        } else {
            Ok(None)
        }
    }
}

impl EventStore for Tier2Store {
    fn store(&self, event: &GenericEvent) -> Result<()> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let headers = self
            .db
            .open_tree(HEADERS_TREE)
            .context("Failed to open headers tree")?;
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;
        let last_payloads = self
            .db
            .open_tree(LAST_PAYLOADS_TREE)
            .context("Failed to open last payloads tree")?;
        let bases = self
            .db
            .open_tree(BASES_TREE)
            .context("Failed to open bases tree")?;

        let payload_bytes = &event.payload;
        let stream_key = &[stream_id_byte(event.header.stream_id)];

        let count = self.stream_event_count(event.header.stream_id)?;
        let is_full = count % self.base_interval == 0;

        let data = if is_full {
            compress(payload_bytes)?
        } else {
            let prev = last_payloads
                .get(stream_key)?
                .map(|v| v.to_vec())
                .unwrap_or_default();
            let delta = compute_delta(&prev, payload_bytes);
            compress(&delta)?
        };

        let entry = StoredEntry { is_full, data };
        let entry_bytes = serde_json::to_vec(&entry).context("Failed to serialize entry")?;

        let event_key = event.header.event_id.as_bytes();
        events.insert(event_key, entry_bytes)?;

        let header_bytes =
            serde_json::to_vec(&event.header).context("Failed to serialize header")?;
        headers.insert(event_key, header_bytes)?;

        let idx_key = stream_seq_key(event.header.stream_id, event.header.sequence_id);
        stream_index.insert(&idx_key, event_key)?;

        last_payloads.insert(stream_key, payload_bytes.as_slice())?;

        if is_full {
            bases.insert(&idx_key, &[])?;
        }

        self.db.flush()?;
        Ok(())
    }

    fn load(&self, event_id: Uuid) -> Result<Option<GenericEvent>> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let headers = self
            .db
            .open_tree(HEADERS_TREE)
            .context("Failed to open headers tree")?;

        let key = event_id.as_bytes();
        let entry_bytes = match events.get(key).context("Failed to read events tree")? {
            Some(b) => b,
            None => return Ok(None),
        };

        let entry: StoredEntry =
            serde_json::from_slice(&entry_bytes).context("Failed to deserialize entry")?;

        let header_bytes = headers
            .get(key)
            .context("Failed to read header")?
            .context("Header not found for event")?;
        let header: crate::header::EventHeader =
            serde_json::from_slice(&header_bytes).context("Failed to deserialize header")?;

        let payload = if entry.is_full {
            decompress(&entry.data)?
        } else {
            self.replay_payloads_from_base(header.stream_id, header.sequence_id)?
                .context("Failed to reconstruct delta-encoded event")?
        };

        Ok(Some(GenericEvent::new(header, payload)))
    }

    fn replay(&self, stream_id: StreamId, from_seq: u64) -> Result<Vec<GenericEvent>> {
        let events = self
            .db
            .open_tree(EVENTS_TREE)
            .context("Failed to open events tree")?;
        let headers = self
            .db
            .open_tree(HEADERS_TREE)
            .context("Failed to open headers tree")?;
        let stream_index = self
            .db
            .open_tree(STREAM_INDEX_TREE)
            .context("Failed to open stream index tree")?;

        let prefix = &[stream_id_byte(stream_id)];
        let base = self.find_base_before(stream_id, from_seq)?;
        let start_seq = base.as_ref().map(|(seq, _)| *seq).unwrap_or(from_seq);

        let mut result = Vec::new();
        let mut current_payload: Vec<u8> = Vec::new();
        let mut base_loaded = base.is_none();

        for item in stream_index.scan_prefix(prefix) {
            let (idx_key, eid_bytes) = item?;
            if idx_key.len() < 9 {
                continue;
            }
            let seq = u64::from_be_bytes(idx_key[1..9].try_into().context("Invalid seq key")?);
            if seq < start_seq {
                continue;
            }

            let entry_bytes = events
                .get(&eid_bytes)?
                .context("Event data not found during replay")?;
            let entry: StoredEntry =
                serde_json::from_slice(&entry_bytes).context("Failed to deserialize entry")?;

            if entry.is_full {
                current_payload = decompress(&entry.data)?;
                base_loaded = true;
            } else if base_loaded {
                let delta = decompress(&entry.data)?;
                current_payload = apply_delta(&current_payload, &delta);
            } else {
                continue;
            }

            if seq >= from_seq {
                let header_bytes = headers
                    .get(&eid_bytes)?
                    .context("Event header not found during replay")?;
                let header: crate::header::EventHeader = serde_json::from_slice(&header_bytes)?;
                result.push(GenericEvent::new(header, current_payload.clone()));
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
        let headers = self
            .db
            .open_tree(HEADERS_TREE)
            .context("Failed to open headers tree")?;
        let key = event_id.as_bytes();
        let header_removed = headers.remove(key)?.is_some();
        let data_removed = events.remove(key)?.is_some();
        self.db.flush()?;
        Ok(header_removed || data_removed)
    }
}

#[cfg(test)]
mod tests {
    use fx_core::types::{EventTier, StreamId};

    use super::*;
    use crate::header::EventHeader;

    fn make_event(stream_id: StreamId, seq: u64, payload: &[u8]) -> GenericEvent {
        let mut header = EventHeader::new(stream_id, seq, EventTier::Tier2Derived);
        header.sequence_id = seq;
        GenericEvent::new(header, payload.to_vec())
    }

    #[test]
    fn test_store_and_load_base_event() {
        let store = Tier2Store::open_temp().unwrap();
        let event = make_event(StreamId::Strategy, 1, b"decision_payload");

        store.store(&event).unwrap();
        let loaded = store.load(event.header.event_id).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.payload, b"decision_payload");
    }

    #[test]
    fn test_delta_round_trip() {
        let prev = b"hello world";
        let current = b"hello rust!";
        let delta = compute_delta(prev, current);
        let reconstructed = apply_delta(prev, &delta);
        assert_eq!(reconstructed, current);
    }

    #[test]
    fn test_delta_different_lengths() {
        let prev = b"short";
        let current = b"longer payload here";
        let delta = compute_delta(prev, current);
        let reconstructed = apply_delta(prev, &delta);
        assert_eq!(reconstructed, current);
    }

    #[test]
    fn test_compress_decompress_round_trip() {
        let data = b"some event payload data that should compress well";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_store_multiple_with_delta() {
        let store = Tier2Store::open_temp().unwrap().with_base_interval(3);

        let payloads: Vec<&[u8]> = vec![b"alpha", b"beta", b"gamma", b"delta", b"epsilon"];
        let mut event_ids = Vec::new();

        for (i, payload) in payloads.iter().enumerate() {
            let event = make_event(StreamId::Execution, (i + 1) as u64, payload);
            event_ids.push(event.header.event_id);
            store.store(&event).unwrap();
        }

        // Load each event and verify payload
        for (i, id) in event_ids.iter().enumerate() {
            let loaded = store.load(*id).unwrap().unwrap();
            assert_eq!(
                loaded.payload, payloads[i],
                "Payload mismatch at index {}",
                i
            );
        }
    }

    #[test]
    fn test_replay_with_delta_encoding() {
        let store = Tier2Store::open_temp().unwrap().with_base_interval(3);

        for i in 0..7 {
            let payload = format!("event_{}", i);
            let event = make_event(StreamId::Market, i + 1, payload.as_bytes());
            store.store(&event).unwrap();
        }

        let events = store.replay(StreamId::Market, 1).unwrap();
        assert_eq!(events.len(), 7);

        for (i, e) in events.iter().enumerate() {
            let expected = format!("event_{}", i);
            assert_eq!(
                e.payload,
                expected.as_bytes(),
                "Replay payload mismatch at index {}",
                i
            );
        }
    }

    #[test]
    fn test_replay_from_middle() {
        let store = Tier2Store::open_temp().unwrap().with_base_interval(5);

        for i in 0..8 {
            let payload = format!("ev_{}", i);
            let event = make_event(StreamId::State, i + 1, payload.as_bytes());
            store.store(&event).unwrap();
        }

        let events = store.replay(StreamId::State, 4).unwrap();
        assert_eq!(events.len(), 5); // seq 4,5,6,7,8

        for (i, e) in events.iter().enumerate() {
            let expected = format!("ev_{}", i + 3);
            assert_eq!(e.payload, expected.as_bytes());
        }
    }

    // =========================================================================
    // §7.3 Tier2 (Derived) verification tests (design.md §7.3)
    // =========================================================================

    #[test]
    fn s7_3_tier2_delta_encoding_reconstructs_original() {
        // design.md §7.3: Tier2 (Derived) → Delta Encoding + 圧縮でアーカイブ
        let store = Tier2Store::open_temp().unwrap().with_base_interval(3);

        let payloads: Vec<Vec<u8>> = (0..10)
            .map(|i| format!("decision_event_payload_{}", i).into_bytes())
            .collect();

        let mut event_ids = Vec::new();
        for (i, payload) in payloads.iter().enumerate() {
            let event = make_event(StreamId::Strategy, (i + 1) as u64, payload);
            event_ids.push(event.header.event_id);
            store.store(&event).unwrap();
        }

        // All events must be perfectly reconstructed through delta decoding
        for (i, payload) in payloads.iter().enumerate() {
            let loaded = store.load(event_ids[i]).unwrap();
            assert!(loaded.is_some(), "event {} failed to load", i);
            assert_eq!(
                loaded.unwrap().payload,
                *payload,
                "payload mismatch at {}",
                i
            );
        }
    }

    #[test]
    fn s7_3_tier2_compression_reduces_size() {
        // design.md §7.3: 圧縮（flate2 Deflate）
        let data = b"This is a decision event payload that should compress well with repetitive patterns and longer content";
        let compressed = compress(data).unwrap();
        // Compressed data should be smaller than original for compressible data
        assert!(
            compressed.len() < data.len(),
            "compressed {} >= original {}",
            compressed.len(),
            data.len()
        );

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn s7_3_tier2_replay_across_base_boundaries() {
        // design.md §7.3: DecisionEvent, PolicyCommand → Tier2
        // Verify replay works correctly across base snapshot boundaries
        let store = Tier2Store::open_temp().unwrap().with_base_interval(3);

        for i in 0..9 {
            let payload = format!("policy_cmd_{}", i);
            let event = make_event(StreamId::Strategy, i + 1, payload.as_bytes());
            store.store(&event).unwrap();
        }

        // Replay from seq 2 (crosses base boundary at seq 3 and 6)
        let events = store.replay(StreamId::Strategy, 2).unwrap();
        assert_eq!(events.len(), 8); // seq 2-9

        for (i, e) in events.iter().enumerate() {
            let expected = format!("policy_cmd_{}", i + 1);
            assert_eq!(e.payload, expected.as_bytes(), "mismatch at index {}", i);
        }
    }

    #[test]
    fn s7_3_tier2_delta_xor_round_trip() {
        // Verify XOR delta encoding is symmetric
        let prev = b"previous_event_data_here";
        let current = b"current_event_data_modified";
        let delta = compute_delta(prev, current);
        let reconstructed = apply_delta(prev, &delta);
        assert_eq!(reconstructed, current);

        // Different lengths
        let short = b"abc";
        let long = b"abcdefghij";
        let delta2 = compute_delta(short, long);
        let reconstructed2 = apply_delta(short, &delta2);
        assert_eq!(reconstructed2, long);
    }
}
