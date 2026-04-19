use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fx_core::types::{EventTier, StreamId};
use fx_events::bus::EventPublisher;
use fx_events::header::EventHeader;
use fx_events::proto::{MarketEventPayload, PriceLevel};
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;
use tracing;

use crate::error::GatewayError;

/// BBO + Level 2 market data
#[derive(Debug, Clone)]
pub struct TickData {
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    pub bid_levels: Vec<PriceLevel>,
    pub ask_levels: Vec<PriceLevel>,
    pub timestamp_ns: u64,
    pub latency_ms: f64,
}

impl TickData {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    pub fn spread(&self) -> f64 {
        self.ask - self.bid
    }

    pub fn to_proto(&self) -> MarketEventPayload {
        MarketEventPayload {
            header: None,
            symbol: self.symbol.clone(),
            bid: self.bid,
            ask: self.ask,
            bid_size: self.bid_size,
            ask_size: self.ask_size,
            timestamp_ns: self.timestamp_ns,
            bid_levels: self.bid_levels.clone(),
            ask_levels: self.ask_levels.clone(),
            latency_ms: self.latency_ms,
        }
    }
}

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    ShuttingDown,
}

/// Market Gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketGatewayConfig {
    /// Symbols to subscribe to
    pub symbols: Vec<String>,
    /// Maximum reconnection attempts (0 = infinite)
    pub max_reconnect_attempts: u32,
    /// Base reconnection delay in ms (exponential backoff)
    pub reconnect_base_delay_ms: u64,
    /// Maximum reconnection delay in ms
    pub reconnect_max_delay_ms: u64,
    /// Heartbeat interval in ms (0 = disabled)
    pub heartbeat_interval_ms: u64,
    /// Heartbeat timeout in ms
    pub heartbeat_timeout_ms: u64,
    /// Event schema version
    pub schema_version: u32,
    /// Whether to include Level 2 depth
    pub include_depth: bool,
}

impl Default for MarketGatewayConfig {
    fn default() -> Self {
        Self {
            symbols: vec!["USD/JPY".to_string()],
            max_reconnect_attempts: 0,
            reconnect_base_delay_ms: 100,
            reconnect_max_delay_ms: 30_000,
            heartbeat_interval_ms: 1_000,
            heartbeat_timeout_ms: 5_000,
            schema_version: 1,
            include_depth: true,
        }
    }
}

/// Latency statistics tracker
#[derive(Debug, Clone, Default)]
pub struct LatencyStats {
    pub count: u64,
    pub total_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
}

impl LatencyStats {
    pub fn record(&mut self, latency_ms: f64) {
        self.count += 1;
        self.total_ms += latency_ms;
        if latency_ms < self.min_ms || self.count == 1 {
            self.min_ms = latency_ms;
        }
        if latency_ms > self.max_ms {
            self.max_ms = latency_ms;
        }
    }

    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ms / self.count as f64
        }
    }
}

/// Market Gateway — receives market data and publishes MarketEvents
pub struct MarketGateway {
    config: MarketGatewayConfig,
    publisher: EventPublisher,
    state: Arc<RwLock<ConnectionState>>,
    shutdown: Arc<AtomicBool>,
    last_tick_ns: Arc<AtomicU64>,
    tick_count: Arc<AtomicU64>,
    latency_stats: Arc<RwLock<LatencyStats>>,
    connection_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    state_watch: watch::Sender<ConnectionState>,
}

impl MarketGateway {
    pub fn new(config: MarketGatewayConfig, publisher: EventPublisher) -> Self {
        let (state_watch, _) = watch::channel(ConnectionState::Disconnected);
        Self {
            config,
            publisher,
            state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
            shutdown: Arc::new(AtomicBool::new(false)),
            last_tick_ns: Arc::new(AtomicU64::new(0)),
            tick_count: Arc::new(AtomicU64::new(0)),
            latency_stats: Arc::new(RwLock::new(LatencyStats::default())),
            connection_handle: Arc::new(RwLock::new(None)),
            state_watch,
        }
    }

    pub fn config(&self) -> &MarketGatewayConfig {
        &self.config
    }

    pub async fn state(&self) -> ConnectionState {
        *self.state.read().await
    }

    pub fn state_receiver(&self) -> watch::Receiver<ConnectionState> {
        self.state_watch.subscribe()
    }

    pub async fn last_tick_ns(&self) -> u64 {
        self.last_tick_ns.load(Ordering::Relaxed)
    }

    pub fn tick_count(&self) -> u64 {
        self.tick_count.load(Ordering::Relaxed)
    }

    pub async fn latency_stats(&self) -> LatencyStats {
        self.latency_stats.read().await.clone()
    }

    /// Inject a TickData (used by WebSocket/FIX handlers and backtesting)
    pub async fn publish_tick(&self, tick: TickData) -> Result<(), GatewayError> {
        let recv_time = Instant::now();
        let tick_timestamp_ns = tick.timestamp_ns;

        let proto = tick.to_proto();
        let payload = proto.encode_to_vec();
        if payload.is_empty() && proto.bid != 0.0 {
            return Err(GatewayError::EncodingError(
                "failed to encode MarketEventPayload".to_string(),
            ));
        }

        let header = EventHeader::new(StreamId::Market, 0, EventTier::Tier3Raw);
        let header = EventHeader {
            schema_version: self.config.schema_version,
            timestamp_ns: tick_timestamp_ns,
            ..header
        };

        self.publisher.publish(header, payload).await.map_err(|e| {
            GatewayError::PublishFailed(format!("failed to publish market event: {}", e))
        })?;

        // Update latency stats
        let latency_ms = recv_time.elapsed().as_secs_f64() * 1000.0;
        let mut stats = self.latency_stats.write().await;
        stats.record(latency_ms + tick.latency_ms);

        // Update tick tracking
        self.last_tick_ns
            .store(tick_timestamp_ns, Ordering::Relaxed);
        self.tick_count.fetch_add(1, Ordering::Relaxed);

        tracing::trace!(
            symbol = %tick.symbol,
            bid = tick.bid,
            ask = tick.ask,
            latency_ms = latency_ms + tick.latency_ms,
            tick_count = self.tick_count.load(Ordering::Relaxed),
            "Tick published"
        );

        Ok(())
    }

    /// Build a TickData from raw market data fields
    #[allow(clippy::too_many_arguments)]
    pub fn build_tick(
        symbol: &str,
        bid: f64,
        ask: f64,
        bid_size: f64,
        ask_size: f64,
        timestamp_ns: u64,
        bid_levels: Vec<PriceLevel>,
        ask_levels: Vec<PriceLevel>,
    ) -> TickData {
        TickData {
            symbol: symbol.to_string(),
            bid,
            ask,
            bid_size,
            ask_size,
            bid_levels,
            ask_levels,
            timestamp_ns,
            latency_ms: 0.0,
        }
    }

    /// Request graceful shutdown
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let mut handle = self.connection_handle.write().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        let mut state = self.state.write().await;
        *state = ConnectionState::ShuttingDown;
        let _ = self.state_watch.send(ConnectionState::ShuttingDown);
        tracing::info!("Market Gateway shutdown requested");
    }

    /// Check if shutdown has been requested
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// Set connection state (used by connection handlers)
    pub async fn set_state(&self, new_state: ConnectionState) {
        let mut state = self.state.write().await;
        *state = new_state;
        let _ = self.state_watch.send(new_state);
        tracing::info!(state = ?new_state, "Market Gateway state changed");
    }

    /// Store connection handle
    pub async fn set_connection_handle(&self, handle: JoinHandle<()>) {
        let mut conn = self.connection_handle.write().await;
        *conn = Some(handle);
    }
}

/// Market data feed trait — abstracts over WebSocket/FIX data sources
#[allow(async_fn_in_trait)]
pub trait MarketFeed: Send + Sync + 'static {
    /// Connect to the data source
    async fn connect(&mut self) -> Result<(), GatewayError>;

    /// Subscribe to a symbol
    async fn subscribe(&mut self, symbol: &str) -> Result<(), GatewayError>;

    /// Unsubscribe from a symbol
    async fn unsubscribe(&mut self, symbol: &str) -> Result<(), GatewayError>;

    /// Receive the next tick (blocking until data available)
    async fn receive_tick(&mut self) -> Result<TickData, GatewayError>;

    /// Disconnect from the data source
    async fn disconnect(&mut self) -> Result<(), GatewayError>;

    /// Check if the connection is alive
    fn is_connected(&self) -> bool;
}

/// Feed manager — wraps a MarketFeed and publishes ticks via the gateway
pub struct FeedManager<F: MarketFeed> {
    gateway: Arc<MarketGateway>,
    feed: F,
    subscribed_symbols: HashMap<String, bool>,
}

impl<F: MarketFeed> FeedManager<F> {
    pub fn new(gateway: Arc<MarketGateway>, feed: F) -> Self {
        let subscribed_symbols = gateway
            .config
            .symbols
            .iter()
            .map(|s| (s.clone(), false))
            .collect();
        Self {
            gateway,
            feed,
            subscribed_symbols,
        }
    }

    /// Connect to feed and subscribe to all configured symbols
    pub async fn start(&mut self) -> Result<(), GatewayError> {
        self.gateway.set_state(ConnectionState::Connecting).await;

        self.feed.connect().await?;
        self.gateway.set_state(ConnectionState::Connected).await;

        tracing::info!(
            symbols = ?self.gateway.config.symbols,
            "Connected to market data feed, subscribing..."
        );

        for symbol in &self.gateway.config.symbols.clone() {
            self.feed.subscribe(symbol).await?;
            self.subscribed_symbols.insert(symbol.clone(), true);
            tracing::info!(symbol = %symbol, "Subscribed to symbol");
        }

        Ok(())
    }

    /// Run the feed loop — receives ticks and publishes them
    pub async fn run(&mut self) -> Result<(), GatewayError> {
        while !self.gateway.is_shutdown() {
            match self.feed.receive_tick().await {
                Ok(tick) => {
                    if let Err(e) = self.gateway.publish_tick(tick).await {
                        tracing::error!(error = %e, "Failed to publish tick");
                    }
                }
                Err(GatewayError::ConnectionLost(_)) => {
                    tracing::warn!("Connection lost, attempting reconnect...");
                    self.gateway.set_state(ConnectionState::Reconnecting).await;
                    self.try_reconnect().await?;
                    self.gateway.set_state(ConnectionState::Connected).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Feed error");
                    return Err(e);
                }
            }
        }

        tracing::info!("Feed loop exited (shutdown)");
        Ok(())
    }

    /// Graceful stop
    pub async fn stop(&mut self) {
        self.gateway.shutdown().await;
        if let Err(e) = self.feed.disconnect().await {
            tracing::warn!(error = %e, "Error during disconnect");
        }
        self.gateway.set_state(ConnectionState::Disconnected).await;
    }

    async fn try_reconnect(&mut self) -> Result<(), GatewayError> {
        let base_delay = self.gateway.config.reconnect_base_delay_ms;
        let max_delay = self.gateway.config.reconnect_max_delay_ms;
        let max_attempts = self.gateway.config.max_reconnect_attempts;

        let mut attempt = 0u32;
        loop {
            if self.gateway.is_shutdown() {
                return Err(GatewayError::ConnectionFailed(
                    "shutdown requested".to_string(),
                ));
            }

            if max_attempts > 0 && attempt >= max_attempts {
                return Err(GatewayError::ReconnectFailed { attempts: attempt });
            }

            let delay = std::cmp::min(base_delay * (1 << attempt.min(10)), max_delay);

            tracing::info!(attempt = attempt, delay_ms = delay, "Reconnect attempt");
            tokio::time::sleep(Duration::from_millis(delay)).await;

            match self.feed.connect().await {
                Ok(()) => {
                    tracing::info!("Reconnected successfully");
                    // Re-subscribe
                    for symbol in self.gateway.config.symbols.clone() {
                        if let Err(e) = self.feed.subscribe(&symbol).await {
                            tracing::warn!(symbol = %symbol, error = %e, "Failed to re-subscribe");
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(attempt = attempt, error = %e, "Reconnect failed");
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_events::bus::PartitionedEventBus;
    use fx_events::proto::PriceLevel;

    fn make_price_levels(n: usize, base: f64, size: f64) -> Vec<PriceLevel> {
        (0..n)
            .map(|i| PriceLevel {
                price: base - (i as f64 * 0.001),
                size,
            })
            .collect()
    }

    fn make_test_config() -> MarketGatewayConfig {
        MarketGatewayConfig {
            symbols: vec!["EUR/USD".to_string(), "USD/JPY".to_string()],
            max_reconnect_attempts: 3,
            reconnect_base_delay_ms: 10,
            reconnect_max_delay_ms: 1000,
            heartbeat_interval_ms: 1000,
            heartbeat_timeout_ms: 5000,
            schema_version: 2,
            include_depth: true,
        }
    }

    #[test]
    fn test_tick_data_mid_and_spread() {
        let tick = TickData {
            symbol: "USD/JPY".to_string(),
            bid: 150.100,
            ask: 150.105,
            bid_size: 1000.0,
            ask_size: 2000.0,
            bid_levels: vec![],
            ask_levels: vec![],
            timestamp_ns: 1_000_000_000,
            latency_ms: 0.5,
        };
        assert!((tick.mid() - 150.1025).abs() < 1e-10);
        assert!((tick.spread() - 0.005).abs() < 1e-10);
    }

    #[test]
    fn test_tick_data_to_proto_roundtrip() {
        let bid_levels = make_price_levels(5, 150.100, 1000.0);
        let ask_levels = make_price_levels(5, 150.105, 2000.0);

        let tick = TickData {
            symbol: "EUR/USD".to_string(),
            bid: 1.0850,
            ask: 1.0851,
            bid_size: 500.0,
            ask_size: 700.0,
            bid_levels: bid_levels.clone(),
            ask_levels: ask_levels.clone(),
            timestamp_ns: 1_710_000_000_000_000_000,
            latency_ms: 1.2,
        };

        let proto = tick.to_proto();
        assert_eq!(proto.symbol, "EUR/USD");
        assert!((proto.bid - 1.0850).abs() < 1e-10);
        assert!((proto.ask - 1.0851).abs() < 1e-10);
        assert!((proto.bid_size - 500.0).abs() < 1e-10);
        assert!((proto.ask_size - 700.0).abs() < 1e-10);
        assert_eq!(proto.timestamp_ns, 1_710_000_000_000_000_000);
        assert!((proto.latency_ms - 1.2).abs() < 1e-10);
        assert_eq!(proto.bid_levels.len(), 5);
        assert_eq!(proto.ask_levels.len(), 5);
        assert!((proto.bid_levels[0].price - 150.100).abs() < 1e-10);
    }

    #[test]
    fn test_build_tick() {
        let bid_levels = make_price_levels(3, 100.0, 50.0);
        let ask_levels = make_price_levels(3, 100.001, 60.0);

        let tick = MarketGateway::build_tick(
            "GBP/USD",
            1.2600,
            1.2601,
            100.0,
            200.0,
            1234567890,
            bid_levels.clone(),
            ask_levels.clone(),
        );

        assert_eq!(tick.symbol, "GBP/USD");
        assert!((tick.bid - 1.2600).abs() < 1e-10);
        assert!((tick.ask - 1.2601).abs() < 1e-10);
        assert_eq!(tick.bid_size, 100.0);
        assert_eq!(tick.ask_size, 200.0);
        assert_eq!(tick.timestamp_ns, 1234567890);
        assert_eq!(tick.bid_levels.len(), 3);
        assert_eq!(tick.ask_levels.len(), 3);
        assert!((tick.latency_ms - 0.0).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_publish_tick() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let mut subscriber = bus.subscriber(&[StreamId::Market]);
        let config = make_test_config();

        let gateway = MarketGateway::new(config, publisher);

        let tick = TickData {
            symbol: "EUR/USD".to_string(),
            bid: 1.0850,
            ask: 1.0851,
            bid_size: 500.0,
            ask_size: 700.0,
            bid_levels: vec![],
            ask_levels: vec![],
            timestamp_ns: 1_710_000_000_000_000_000,
            latency_ms: 0.5,
        };

        gateway.publish_tick(tick).await.unwrap();

        let event = subscriber.recv().await.unwrap();
        assert_eq!(event.header.stream_id, StreamId::Market);
        assert_eq!(event.header.tier, EventTier::Tier3Raw);
        assert_eq!(event.header.schema_version, 2);
        assert_eq!(event.header.sequence_id, 1);

        let proto: MarketEventPayload = Message::decode(event.payload.as_slice()).unwrap();
        assert_eq!(proto.symbol, "EUR/USD");
        assert!((proto.bid - 1.0850).abs() < 1e-10);
        assert!((proto.ask - 1.0851).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_gateway_tick_count_and_timestamp() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let config = make_test_config();

        let gateway = MarketGateway::new(config, publisher);

        assert_eq!(gateway.tick_count(), 0);
        assert_eq!(gateway.last_tick_ns().await, 0);

        for i in 0..5 {
            let tick = TickData {
                symbol: "EUR/USD".to_string(),
                bid: 1.0850 + i as f64 * 0.0001,
                ask: 1.0851 + i as f64 * 0.0001,
                bid_size: 500.0,
                ask_size: 700.0,
                bid_levels: vec![],
                ask_levels: vec![],
                timestamp_ns: 1_710_000_000_000_000_000 + i as u64 * 1_000_000,
                latency_ms: 0.0,
            };
            gateway.publish_tick(tick).await.unwrap();
        }

        assert_eq!(gateway.tick_count(), 5);
        assert_eq!(gateway.last_tick_ns().await, 1_710_000_000_004_000_000);
    }

    #[tokio::test]
    async fn test_gateway_state_transitions() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let config = make_test_config();
        let gateway = MarketGateway::new(config, publisher);

        assert_eq!(gateway.state().await, ConnectionState::Disconnected);

        gateway.set_state(ConnectionState::Connecting).await;
        assert_eq!(gateway.state().await, ConnectionState::Connecting);

        gateway.set_state(ConnectionState::Connected).await;
        assert_eq!(gateway.state().await, ConnectionState::Connected);

        gateway.shutdown().await;
        assert_eq!(gateway.state().await, ConnectionState::ShuttingDown);
        assert!(gateway.is_shutdown());
    }

    #[tokio::test]
    async fn test_state_watch_receiver() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let config = make_test_config();
        let gateway = MarketGateway::new(config, publisher);
        let mut rx = gateway.state_receiver();

        assert_eq!(*rx.borrow(), ConnectionState::Disconnected);

        gateway.set_state(ConnectionState::Connected).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), ConnectionState::Connected);
    }

    #[tokio::test]
    async fn test_latency_stats() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let config = make_test_config();
        let gateway = MarketGateway::new(config, publisher);

        let stats = gateway.latency_stats().await;
        assert_eq!(stats.count, 0);
        assert!((stats.avg_ms() - 0.0).abs() < 1e-10);

        let tick = TickData {
            symbol: "EUR/USD".to_string(),
            bid: 1.0850,
            ask: 1.0851,
            bid_size: 500.0,
            ask_size: 700.0,
            bid_levels: vec![],
            ask_levels: vec![],
            timestamp_ns: 1_710_000_000_000_000_000,
            latency_ms: 1.0,
        };
        gateway.publish_tick(tick).await.unwrap();

        let stats = gateway.latency_stats().await;
        assert_eq!(stats.count, 1);
        assert!(stats.min_ms > 0.0);
        assert!(stats.max_ms > 0.0);
    }

    #[test]
    fn test_latency_stats_record() {
        let mut stats = LatencyStats::default();
        assert_eq!(stats.count, 0);

        stats.record(1.0);
        stats.record(2.0);
        stats.record(3.0);

        assert_eq!(stats.count, 3);
        assert!((stats.avg_ms() - 2.0).abs() < 1e-10);
        assert!((stats.min_ms - 1.0).abs() < 1e-10);
        assert!((stats.max_ms - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_latency_stats_single() {
        let mut stats = LatencyStats::default();
        stats.record(5.5);
        assert_eq!(stats.count, 1);
        assert!((stats.min_ms - 5.5).abs() < 1e-10);
        assert!((stats.max_ms - 5.5).abs() < 1e-10);
        assert!((stats.avg_ms() - 5.5).abs() < 1e-10);
    }

    #[test]
    fn test_latency_stats_zero_avg() {
        let stats = LatencyStats::default();
        assert!((stats.avg_ms() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_config_default() {
        let config = MarketGatewayConfig::default();
        assert_eq!(config.symbols, vec!["USD/JPY".to_string()]);
        assert_eq!(config.max_reconnect_attempts, 0);
        assert_eq!(config.reconnect_base_delay_ms, 100);
        assert_eq!(config.reconnect_max_delay_ms, 30_000);
        assert_eq!(config.heartbeat_interval_ms, 1_000);
        assert_eq!(config.heartbeat_timeout_ms, 5_000);
        assert_eq!(config.schema_version, 1);
        assert!(config.include_depth);
    }

    #[test]
    fn test_connection_state_serde_roundtrip() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
            ConnectionState::ShuttingDown,
        ];
        for state in &states {
            let json = serde_json::to_string(state).unwrap();
            let deserialized: ConnectionState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, deserialized);
        }
    }

    #[tokio::test]
    async fn test_multiple_ticks_sequence_ordering() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let mut subscriber = bus.subscriber(&[StreamId::Market]);
        let config = make_test_config();
        let gateway = MarketGateway::new(config, publisher);

        for i in 0..10 {
            let tick = TickData {
                symbol: "EUR/USD".to_string(),
                bid: 1.0850,
                ask: 1.0851,
                bid_size: 500.0,
                ask_size: 700.0,
                bid_levels: vec![],
                ask_levels: vec![],
                timestamp_ns: 1_710_000_000_000_000_000 + i as u64,
                latency_ms: 0.0,
            };
            gateway.publish_tick(tick).await.unwrap();
        }

        for expected_seq in 1..=10u64 {
            let event = subscriber.recv().await.unwrap();
            assert_eq!(event.header.sequence_id, expected_seq);
        }
    }

    #[tokio::test]
    async fn test_multiple_symbols() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let mut subscriber = bus.subscriber(&[StreamId::Market]);
        let config = make_test_config();
        let gateway = MarketGateway::new(config, publisher);

        let symbols = ["EUR/USD", "USD/JPY", "GBP/USD"];
        for symbol in &symbols {
            let tick = TickData {
                symbol: symbol.to_string(),
                bid: 1.0,
                ask: 1.001,
                bid_size: 100.0,
                ask_size: 100.0,
                bid_levels: vec![],
                ask_levels: vec![],
                timestamp_ns: 1_710_000_000_000_000_000,
                latency_ms: 0.0,
            };
            gateway.publish_tick(tick).await.unwrap();
        }

        let mut received_symbols = Vec::new();
        for _ in 0..3 {
            let event = subscriber.recv().await.unwrap();
            let proto: MarketEventPayload = Message::decode(event.payload.as_slice()).unwrap();
            received_symbols.push(proto.symbol);
        }
        assert_eq!(received_symbols, symbols);
    }

    #[tokio::test]
    async fn test_depth_levels_published() {
        let bus = PartitionedEventBus::new();
        let publisher = bus.publisher(StreamId::Market);
        let mut subscriber = bus.subscriber(&[StreamId::Market]);
        let config = make_test_config();
        let gateway = MarketGateway::new(config, publisher);

        let bid_levels = vec![
            PriceLevel {
                price: 150.100,
                size: 1000.0,
            },
            PriceLevel {
                price: 150.099,
                size: 2000.0,
            },
            PriceLevel {
                price: 150.098,
                size: 3000.0,
            },
        ];
        let ask_levels = vec![
            PriceLevel {
                price: 150.101,
                size: 1500.0,
            },
            PriceLevel {
                price: 150.102,
                size: 2500.0,
            },
        ];

        let tick = TickData {
            symbol: "USD/JPY".to_string(),
            bid: 150.100,
            ask: 150.101,
            bid_size: 1000.0,
            ask_size: 1500.0,
            bid_levels,
            ask_levels,
            timestamp_ns: 1_710_000_000_000_000_000,
            latency_ms: 0.0,
        };
        gateway.publish_tick(tick).await.unwrap();

        let event = subscriber.recv().await.unwrap();
        let proto: MarketEventPayload = Message::decode(event.payload.as_slice()).unwrap();
        assert_eq!(proto.bid_levels.len(), 3);
        assert_eq!(proto.ask_levels.len(), 2);
        assert!((proto.bid_levels[0].price - 150.100).abs() < 1e-10);
        assert!((proto.bid_levels[1].size - 2000.0).abs() < 1e-10);
    }
}
