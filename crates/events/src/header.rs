use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventHeader {
    pub event_id: Uuid,
    pub parent_event_id: Option<Uuid>,
    pub stream_id: fx_core::types::StreamId,
    pub sequence_id: u64,
    pub timestamp_ns: u64,
    pub schema_version: u32,
    pub tier: fx_core::types::EventTier,
}

impl EventHeader {
    pub fn new(
        stream_id: fx_core::types::StreamId,
        sequence_id: u64,
        tier: fx_core::types::EventTier,
    ) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            parent_event_id: None,
            stream_id,
            sequence_id,
            timestamp_ns: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64,
            schema_version: 1,
            tier,
        }
    }
}
