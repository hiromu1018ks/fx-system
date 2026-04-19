pub mod bus;
pub mod event;
pub mod header;
pub mod store;
pub mod stream;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/fx.events.rs"));
}
