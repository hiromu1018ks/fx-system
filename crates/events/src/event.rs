use fx_core::types::{EventTier, StreamId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::header::EventHeader;

pub trait Event: Send + Sync + 'static {
    fn header(&self) -> &EventHeader;
    fn payload_bytes(&self) -> &[u8];

    fn event_id(&self) -> Uuid {
        self.header().event_id
    }

    fn stream_id(&self) -> StreamId {
        self.header().stream_id
    }

    fn sequence_id(&self) -> u64 {
        self.header().sequence_id
    }

    fn tier(&self) -> EventTier {
        self.header().tier
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericEvent {
    pub header: EventHeader,
    pub payload: Vec<u8>,
}

impl GenericEvent {
    pub fn new(header: EventHeader, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }
}

impl Event for GenericEvent {
    fn header(&self) -> &EventHeader {
        &self.header
    }

    fn payload_bytes(&self) -> &[u8] {
        &self.payload
    }
}
