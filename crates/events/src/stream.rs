use fx_core::types::StreamId;

pub struct StreamEvent {
    pub stream_id: StreamId,
    pub sequence_id: u64,
    pub payload: Vec<u8>,
}
