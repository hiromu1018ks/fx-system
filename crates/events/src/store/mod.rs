pub mod schema;
pub mod tier1;
pub mod tier2;
pub mod tier3;

use anyhow::Result;
use fx_core::types::StreamId;
use uuid::Uuid;

use crate::event::GenericEvent;

pub trait EventStore: Send + Sync {
    fn store(&self, event: &GenericEvent) -> Result<()>;
    fn load(&self, event_id: Uuid) -> Result<Option<GenericEvent>>;
    fn replay(&self, stream_id: StreamId, from_seq: u64) -> Result<Vec<GenericEvent>>;
    fn remove(&self, event_id: Uuid) -> Result<bool>;
}

pub use schema::{SchemaDescriptor, SchemaRegistry, Upcaster};
pub use tier1::Tier1Store;
pub use tier2::Tier2Store;
pub use tier3::Tier3Store;
