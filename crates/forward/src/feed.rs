use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Result;
use fx_core::types::StreamId;
use fx_events::event::Event;
use fx_events::proto::MarketEventPayload;
use fx_events::store::EventStore;
use fx_gateway::market::TickData;
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Market data feed trait for abstracting data sources.
#[allow(async_fn_in_trait)]
pub trait MarketFeed: Send + Sync {
    async fn connect(&mut self) -> Result<()>;
    async fn subscribe(&mut self, symbols: &[String]) -> Result<()>;
    async fn next_tick(&mut self) -> Result<Option<TickData>>;
    async fn disconnect(&mut self) -> Result<()>;
    fn is_connected(&self) -> bool;
}

/// Data source configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataSourceConfig {
    Recorded {
        event_store_path: String,
        speed: f64,
        start_time: Option<String>,
        end_time: Option<String>,
    },
    ExternalApi {
        provider: String,
        credentials_path: String,
        symbols: Vec<String>,
    },
}

/// Replays recorded market events from the Event Store at configurable speed.
pub struct RecordedDataFeed<S: EventStore> {
    store: S,
    speed: f64,
    start_time_ns: Option<u64>,
    end_time_ns: Option<u64>,
    buffer: VecDeque<TickData>,
    connected: bool,
    last_tick_ns: Option<u64>,
}

impl<S: EventStore> RecordedDataFeed<S> {
    pub fn new(store: S, speed: f64, start_time_ns: Option<u64>, end_time_ns: Option<u64>) -> Self {
        Self {
            store,
            speed,
            start_time_ns,
            end_time_ns,
            buffer: VecDeque::new(),
            connected: false,
            last_tick_ns: None,
        }
    }

    fn load_events(&mut self) -> Result<usize> {
        let events = self.store.replay(StreamId::Market, 0)?;
        let mut count = 0;

        for event in events {
            let ts = event.header().timestamp_ns;

            if let Some(start) = self.start_time_ns {
                if ts < start {
                    continue;
                }
            }
            if let Some(end) = self.end_time_ns {
                if ts > end {
                    continue;
                }
            }

            let payload = match MarketEventPayload::decode(event.payload_bytes()) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to decode market event: {}", e);
                    continue;
                }
            };

            let tick = TickData {
                symbol: payload.symbol,
                bid: payload.bid,
                ask: payload.ask,
                bid_size: payload.bid_size,
                ask_size: payload.ask_size,
                bid_levels: payload.bid_levels,
                ask_levels: payload.ask_levels,
                timestamp_ns: payload.timestamp_ns,
                latency_ms: payload.latency_ms,
            };

            self.buffer.push_back(tick);
            count += 1;
        }

        Ok(count)
    }

    fn inter_tick_delay(&self, current_ns: u64, next_ns: u64) -> Option<Duration> {
        if self.speed <= 0.0 {
            return None;
        }

        let original_delta_ns = next_ns.saturating_sub(current_ns);
        if original_delta_ns == 0 {
            return None;
        }

        let adjusted_ns = (original_delta_ns as f64 / self.speed) as u64;
        Some(Duration::from_nanos(adjusted_ns))
    }
}

#[allow(async_fn_in_trait)]
impl<S: EventStore + 'static> MarketFeed for RecordedDataFeed<S> {
    async fn connect(&mut self) -> Result<()> {
        info!(
            speed = self.speed,
            start = ?self.start_time_ns,
            end = ?self.end_time_ns,
            "Connecting RecordedDataFeed"
        );
        let count = self.load_events()?;
        self.connected = true;
        info!(event_count = count, "RecordedDataFeed connected");
        Ok(())
    }

    async fn subscribe(&mut self, _symbols: &[String]) -> Result<()> {
        Ok(())
    }

    async fn next_tick(&mut self) -> Result<Option<TickData>> {
        if !self.connected {
            anyhow::bail!("RecordedDataFeed not connected");
        }

        let next = match self.buffer.pop_front() {
            Some(tick) => tick,
            None => return Ok(None),
        };

        if let Some(current_ns) = self.last_tick_ns {
            if let Some(delay) = self.inter_tick_delay(current_ns, next.timestamp_ns) {
                sleep(delay).await;
            }
        }

        self.last_tick_ns = Some(next.timestamp_ns);
        debug!(ts = next.timestamp_ns, symbol = %next.symbol, "Emitting tick");
        Ok(Some(next))
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        self.buffer.clear();
        self.last_tick_ns = None;
        info!("RecordedDataFeed disconnected");
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }
}

/// External API feed configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiFeedConfig {
    pub provider: String,
    pub credentials_path: String,
    pub symbols: Vec<String>,
    pub reconnect_attempts: u32,
    pub reconnect_delay_ms: u64,
}

impl Default for ApiFeedConfig {
    fn default() -> Self {
        Self {
            provider: "OANDA".to_string(),
            credentials_path: String::new(),
            symbols: vec!["EUR/USD".to_string()],
            reconnect_attempts: 3,
            reconnect_delay_ms: 5000,
        }
    }
}

/// External API data feed adapter — skeleton for OANDA or similar providers.
///
/// Implements the MarketFeed trait, allowing transparent switching
/// between RecordedDataFeed and live market data.
pub struct ExternalApiFeed {
    config: ApiFeedConfig,
    connected: bool,
    subscribed_symbols: Vec<String>,
    api_key: Option<String>,
}

impl ExternalApiFeed {
    pub fn new(config: ApiFeedConfig) -> Self {
        Self {
            config,
            connected: false,
            subscribed_symbols: Vec::new(),
            api_key: None,
        }
    }

    /// Load API credentials from a file.
    fn load_credentials(&mut self) -> Result<()> {
        if self.config.credentials_path.is_empty() {
            anyhow::bail!("Credentials path not configured");
        }
        let content = std::fs::read_to_string(&self.config.credentials_path)
            .map_err(|e| anyhow::anyhow!("Failed to read credentials: {}", e))?;
        // Parse simple key=value format
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "api_key" {
                    self.api_key = Some(value.trim().to_string());
                }
            }
        }
        if self.api_key.is_none() {
            anyhow::bail!("No api_key found in credentials file");
        }
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
impl MarketFeed for ExternalApiFeed {
    async fn connect(&mut self) -> Result<()> {
        info!(provider = %self.config.provider, "Connecting ExternalApiFeed");

        // Load credentials
        self.load_credentials()?;

        // Provider-specific connection logic would go here.
        // For OANDA: establish WebSocket/streaming connection
        // This is a skeleton — actual implementation requires the provider SDK.

        self.connected = true;
        info!(provider = %self.config.provider, "ExternalApiFeed connected");
        Ok(())
    }

    async fn subscribe(&mut self, symbols: &[String]) -> Result<()> {
        if !self.connected {
            anyhow::bail!("Not connected");
        }
        self.subscribed_symbols = symbols.to_vec();
        info!(symbols = ?self.subscribed_symbols, "Subscribed to symbols");
        Ok(())
    }

    async fn next_tick(&mut self) -> Result<Option<TickData>> {
        if !self.connected {
            anyhow::bail!("Not connected");
        }
        // Skeleton: actual implementation would receive from the WebSocket/REST stream
        // and convert to TickData.
        // For now, return None to signal no data available.
        Ok(None)
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        self.api_key = None;
        self.subscribed_symbols.clear();
        info!("ExternalApiFeed disconnected");
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::types::EventTier;
    use fx_events::event::{Event, GenericEvent};
    use fx_events::header::EventHeader;
    use fx_events::proto::PriceLevel;
    use uuid::Uuid;

    struct StubStore {
        events: Vec<GenericEvent>,
    }

    impl StubStore {
        fn new(events: Vec<GenericEvent>) -> Self {
            Self { events }
        }
    }

    impl EventStore for StubStore {
        fn store(&self, _event: &GenericEvent) -> Result<()> {
            Ok(())
        }
        fn load(&self, _event_id: Uuid) -> Result<Option<GenericEvent>> {
            Ok(None)
        }
        fn replay(&self, stream_id: StreamId, from_seq: u64) -> Result<Vec<GenericEvent>> {
            let events: Vec<GenericEvent> = self
                .events
                .iter()
                .filter(|e| e.header().stream_id == stream_id && e.header().sequence_id >= from_seq)
                .cloned()
                .collect();
            Ok(events)
        }
        fn remove(&self, _event_id: Uuid) -> Result<bool> {
            Ok(false)
        }
    }

    fn make_market_event(
        seq: u64,
        timestamp_ns: u64,
        symbol: &str,
        bid: f64,
        ask: f64,
    ) -> GenericEvent {
        let payload = MarketEventPayload {
            header: None,
            symbol: symbol.to_string(),
            bid,
            ask,
            bid_size: 1.0,
            ask_size: 1.0,
            timestamp_ns,
            bid_levels: vec![PriceLevel {
                price: bid,
                size: 1.0,
            }],
            ask_levels: vec![PriceLevel {
                price: ask,
                size: 1.0,
            }],
            latency_ms: 0.0,
        };
        let header = EventHeader::new(StreamId::Market, seq, EventTier::Tier3Raw);
        GenericEvent::new(
            EventHeader {
                timestamp_ns,
                ..header
            },
            payload.encode_to_vec(),
        )
    }

    #[tokio::test]
    async fn test_connect_loads_events() {
        let store = StubStore::new(vec![
            make_market_event(0, 1000, "EUR/USD", 1.1000, 1.1001),
            make_market_event(1, 2000, "EUR/USD", 1.1002, 1.1003),
        ]);
        let mut feed = RecordedDataFeed::new(store, 0.0, None, None);

        feed.connect().await.unwrap();
        assert!(feed.is_connected());

        let t1 = feed.next_tick().await.unwrap().unwrap();
        assert_eq!(t1.symbol, "EUR/USD");
        assert_eq!(t1.bid, 1.1000);
        assert_eq!(t1.timestamp_ns, 1000);

        let t2 = feed.next_tick().await.unwrap().unwrap();
        assert_eq!(t2.bid, 1.1002);

        assert!(feed.next_tick().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_disconnect_clears_state() {
        let store = StubStore::new(vec![make_market_event(0, 1000, "EUR/USD", 1.1000, 1.1001)]);
        let mut feed = RecordedDataFeed::new(store, 0.0, None, None);
        feed.connect().await.unwrap();
        assert!(feed.is_connected());

        feed.disconnect().await.unwrap();
        assert!(!feed.is_connected());
    }

    #[tokio::test]
    async fn test_time_range_filtering() {
        let store = StubStore::new(vec![
            make_market_event(0, 1000, "EUR/USD", 1.1000, 1.1001),
            make_market_event(1, 2000, "EUR/USD", 1.1002, 1.1003),
            make_market_event(2, 3000, "EUR/USD", 1.1004, 1.1005),
        ]);
        let mut feed = RecordedDataFeed::new(store, 0.0, Some(1500), Some(2500));
        feed.connect().await.unwrap();

        let t1 = feed.next_tick().await.unwrap().unwrap();
        assert_eq!(t1.timestamp_ns, 2000);
        assert!(feed.next_tick().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_max_speed_no_delay() {
        let store = StubStore::new(vec![
            make_market_event(0, 1000, "EUR/USD", 1.1000, 1.1001),
            make_market_event(1, 2_000_000_000, "EUR/USD", 1.1002, 1.1003),
        ]);
        let mut feed = RecordedDataFeed::new(store, 0.0, None, None);
        feed.connect().await.unwrap();

        let start = std::time::Instant::now();
        let _ = feed.next_tick().await.unwrap().unwrap();
        let _ = feed.next_tick().await.unwrap().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "Max speed should have no delay"
        );
    }

    #[tokio::test]
    async fn test_not_connected_error() {
        let store = StubStore::new(vec![]);
        let mut feed = RecordedDataFeed::new(store, 1.0, None, None);
        let result = feed.next_tick().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_store() {
        let store = StubStore::new(vec![]);
        let mut feed = RecordedDataFeed::new(store, 1.0, None, None);
        feed.connect().await.unwrap();
        assert!(feed.next_tick().await.unwrap().is_none());
    }

    #[test]
    fn test_inter_tick_delay_calculation() {
        let store = StubStore::new(vec![]);

        let feed = RecordedDataFeed::new(store, 1.0, None, None);
        let delay = feed.inter_tick_delay(1000, 1_001_000);
        assert_eq!(delay, Some(Duration::from_nanos(1_000_000)));

        let feed2 = RecordedDataFeed::new(StubStore::new(vec![]), 2.0, None, None);
        let delay2 = feed2.inter_tick_delay(1000, 1_001_000);
        assert_eq!(delay2, Some(Duration::from_nanos(500_000)));

        let feed0 = RecordedDataFeed::new(StubStore::new(vec![]), 0.0, None, None);
        assert!(feed0.inter_tick_delay(1000, 1_001_000).is_none());
    }

    // --- ExternalApiFeed tests ---

    #[tokio::test]
    async fn test_external_api_feed_config() {
        let config = ApiFeedConfig::default();
        assert_eq!(config.provider, "OANDA");
        assert_eq!(config.reconnect_attempts, 3);
    }

    #[tokio::test]
    async fn test_external_api_connect_no_credentials() {
        let config = ApiFeedConfig {
            credentials_path: String::new(),
            ..Default::default()
        };
        let mut feed = ExternalApiFeed::new(config);
        assert!(feed.connect().await.is_err());
    }

    #[tokio::test]
    async fn test_external_api_connect_with_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials");
        std::fs::write(&cred_path, "api_key=test_key_123\n").unwrap();

        let config = ApiFeedConfig {
            credentials_path: cred_path.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut feed = ExternalApiFeed::new(config);
        feed.connect().await.unwrap();
        assert!(feed.is_connected());

        feed.disconnect().await.unwrap();
        assert!(!feed.is_connected());
    }

    #[tokio::test]
    async fn test_external_api_subscribe_when_connected() {
        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials");
        std::fs::write(&cred_path, "api_key=test_key\n").unwrap();

        let config = ApiFeedConfig {
            credentials_path: cred_path.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut feed = ExternalApiFeed::new(config);
        feed.connect().await.unwrap();

        feed.subscribe(&["EUR/USD".to_string(), "GBP/USD".to_string()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_external_api_next_tick_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials");
        std::fs::write(&cred_path, "api_key=test_key\n").unwrap();

        let config = ApiFeedConfig {
            credentials_path: cred_path.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut feed = ExternalApiFeed::new(config);
        feed.connect().await.unwrap();
        // Skeleton returns None
        assert!(feed.next_tick().await.unwrap().is_none());
    }
}
