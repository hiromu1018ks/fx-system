use anyhow::Result;
use fx_gateway::market::TickData;
use serde::{Deserialize, Serialize};

/// Market data feed trait for abstracting data sources.
#[allow(async_fn_in_trait)]
pub trait MarketFeed: Send + Sync {
    /// Connect to the data source.
    async fn connect(&mut self) -> Result<()>;

    /// Subscribe to the specified symbols.
    async fn subscribe(&mut self, symbols: &[String]) -> Result<()>;

    /// Receive the next tick from the feed.
    async fn next_tick(&mut self) -> Result<Option<TickData>>;

    /// Disconnect from the data source.
    async fn disconnect(&mut self) -> Result<()>;

    /// Check if the feed is currently connected.
    fn is_connected(&self) -> bool;
}

/// Data source configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataSourceConfig {
    /// Replay recorded events from the Event Store.
    Recorded {
        event_store_path: String,
        /// 1.0 = realtime, 0.0 = max speed, 2.0 = 2x speed
        speed: f64,
        start_time: Option<String>,
        end_time: Option<String>,
    },
    /// Connect to an external FX API.
    ExternalApi {
        provider: String,
        credentials_path: String,
        symbols: Vec<String>,
    },
}
