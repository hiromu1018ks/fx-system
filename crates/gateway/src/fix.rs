use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time;
use tracing;

use crate::error::GatewayError;

type MessageCallback = Arc<RwLock<Option<Box<dyn Fn(FixMessage) + Send + Sync>>>>;

/// FIX message type tags
pub mod tag {
    pub const BEGIN_STRING: &str = "8";
    pub const BODY_LENGTH: &str = "9";
    pub const MSG_TYPE: &str = "35";
    pub const SENDER_COMP_ID: &str = "49";
    pub const TARGET_COMP_ID: &str = "56";
    pub const MSG_SEQ_NUM: &str = "34";
    pub const SENDER_SUB_ID: &str = "50";
    pub const SENDING_TIME: &str = "52";
    pub const TARGET_SUB_ID: &str = "57";
    pub const HEARTBEAT: &str = "0";
    pub const LOGON: &str = "A";
    pub const LOGOUT: &str = "5";
    pub const RESEND_REQUEST: &str = "2";
    pub const SEQUENCE_RESET: &str = "4";
    pub const TEST_REQUEST: &str = "1";
    pub const NEW_ORDER_SINGLE: &str = "D";
    pub const ORDER_CANCEL_REQUEST: &str = "F";
    pub const ORDER_CANCEL_REJECT: &str = "9";
    pub const EXECUTION_REPORT: &str = "8";
    pub const MARKET_DATA_REQUEST: &str = "V";
    pub const MARKET_DATA_SNAPSHOT: &str = "W";
    pub const MARKET_DATA_INCREMENTAL: &str = "X";
    pub const MARKET_DATA_REQUEST_REJECT: &str = "Y";
    pub const ENCRYPT_METHOD: &str = "98";
    pub const HEARTBT_INT: &str = "108";
    pub const RAW_DATA_LENGTH: &str = "95";
    pub const RAW_DATA: &str = "96";
    pub const RESET_SEQ_NUM_FLAG: &str = "141";
    pub const NEXT_EXPECTED_MSG_SEQ_NUM: &str = "789";
    pub const TEXT: &str = "58";
    pub const SOH: char = '\x01';
}

/// FIX session configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixSessionConfig {
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub host: String,
    pub port: u16,
    /// FIX version (default: FIX.4.4)
    pub fix_version: String,
    /// Heartbeat interval in seconds
    pub heartbeat_interval_secs: u32,
    /// Connection timeout in ms
    pub connection_timeout_ms: u64,
    /// Maximum reconnect attempts (0 = infinite)
    pub max_reconnect_attempts: u32,
    /// Reconnect base delay in ms
    pub reconnect_base_delay_ms: u64,
    /// Reconnect max delay in ms
    pub reconnect_max_delay_ms: u64,
    /// Encrypt method (0 = none)
    pub encrypt_method: u32,
    /// Reset sequence numbers on reconnect
    pub reset_seq_on_reconnect: bool,
}

impl FixSessionConfig {
    pub fn new(sender_comp_id: &str, target_comp_id: &str, host: &str, port: u16) -> Self {
        Self {
            sender_comp_id: sender_comp_id.to_string(),
            target_comp_id: target_comp_id.to_string(),
            host: host.to_string(),
            port,
            fix_version: "FIX.4.4".to_string(),
            heartbeat_interval_secs: 30,
            connection_timeout_ms: 10_000,
            max_reconnect_attempts: 0,
            reconnect_base_delay_ms: 1000,
            reconnect_max_delay_ms: 30_000,
            encrypt_method: 0,
            reset_seq_on_reconnect: false,
        }
    }
}

impl Default for FixSessionConfig {
    fn default() -> Self {
        Self::new("SENDER", "TARGET", "127.0.0.1", 9876)
    }
}

/// FIX session state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixSessionState {
    Disconnected,
    LoggingOn,
    LoggedOn,
    LogoffSent,
    LogoutReceived,
    Error,
}

/// Parsed FIX message
#[derive(Debug, Clone)]
pub struct FixMessage {
    pub msg_type: String,
    pub fields: HashMap<String, String>,
    pub raw: String,
}

impl FixMessage {
    pub fn get(&self, tag: &str) -> Option<&str> {
        self.fields.get(tag).map(|s| s.as_str())
    }

    pub fn get_u64(&self, tag: &str) -> Option<u64> {
        self.fields.get(tag).and_then(|s| s.parse().ok())
    }
}

/// FIX message builder
pub struct FixMessageBuilder {
    msg_type: String,
    fields: Vec<(String, String)>,
}

impl FixMessageBuilder {
    pub fn new(msg_type: &str) -> Self {
        Self {
            msg_type: msg_type.to_string(),
            fields: Vec::new(),
        }
    }

    pub fn field(mut self, tag: &str, value: &str) -> Self {
        self.fields.push((tag.to_string(), value.to_string()));
        self
    }

    pub fn build(
        &self,
        begin_string: &str,
        sender_comp_id: &str,
        target_comp_id: &str,
        seq_num: u64,
    ) -> String {
        let soh = tag::SOH;
        let mut body = String::new();

        body.push_str(&format!("35={}{}", self.msg_type, soh));
        body.push_str(&format!("49={}{}", sender_comp_id, soh));
        body.push_str(&format!("56={}{}", target_comp_id, soh));
        body.push_str(&format!("34={}{}", seq_num, soh));
        body.push_str(&format!(
            "52={}{}",
            format_timestamp_ns(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)),
            soh
        ));

        for (tag, value) in &self.fields {
            body.push_str(&format!("{}={}{}", tag, value, soh));
        }

        let body_len = body.len();
        let mut msg = String::new();
        msg.push_str(&format!(
            "8={}{}{}{}{}",
            begin_string, soh, 9, body_len, soh
        ));
        msg.push_str(&body);

        // Checksum
        let checksum = compute_checksum(&msg);
        msg.push_str(&format!("10={:03}{}", checksum, soh));

        msg
    }
}

fn compute_checksum(msg: &str) -> u32 {
    let mut sum: u32 = 0;
    for b in msg.bytes() {
        sum += b as u32;
    }
    sum % 256
}

fn format_timestamp_ns(nanos: i64) -> String {
    let secs = nanos / 1_000_000_000;
    let nanos_part = (nanos % 1_000_000_000).unsigned_abs();
    chrono::DateTime::from_timestamp(secs, nanos_part as u32)
        .map(|dt| dt.format("%Y%m%d-%H:%M:%S%.3f").to_string())
        .unwrap_or_else(|| format!("{}-000", secs))
}

/// Parse a raw FIX string into a FixMessage
pub fn parse_fix_message(raw: &str) -> Result<FixMessage, GatewayError> {
    let soh = tag::SOH;
    let fields: Vec<&str> = raw.split(soh).filter(|s| !s.is_empty()).collect();

    if fields.len() < 3 {
        return Err(GatewayError::InvalidMessage(format!(
            "too few fields in FIX message: {}",
            raw
        )));
    }

    let mut parsed = HashMap::new();
    let mut msg_type = String::new();

    for field in &fields {
        if let Some(eq_pos) = field.find('=') {
            let tag = &field[..eq_pos];
            let value = &field[eq_pos + 1..];
            parsed.insert(tag.to_string(), value.to_string());
            if tag == tag::MSG_TYPE {
                msg_type = value.to_string();
            }
        }
    }

    if msg_type.is_empty() {
        return Err(GatewayError::InvalidMessage(
            "missing MsgType (tag 35)".to_string(),
        ));
    }

    Ok(FixMessage {
        msg_type,
        fields: parsed,
        raw: raw.to_string(),
    })
}

/// FIX Session — manages a FIX connection lifecycle
pub struct FixSession {
    config: FixSessionConfig,
    state: Arc<RwLock<FixSessionState>>,
    outbound_seq: Arc<AtomicU64>,
    inbound_seq: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    last_inbound: Arc<RwLock<Instant>>,
    heartbeat_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    reader_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    message_callback: MessageCallback,
}

impl FixSession {
    pub fn new(config: FixSessionConfig) -> Self {
        Self {
            config,
            state: Arc::new(RwLock::new(FixSessionState::Disconnected)),
            outbound_seq: Arc::new(AtomicU64::new(1)),
            inbound_seq: Arc::new(AtomicU64::new(1)),
            shutdown: Arc::new(AtomicBool::new(false)),
            last_inbound: Arc::new(RwLock::new(Instant::now())),
            heartbeat_handle: Arc::new(RwLock::new(None)),
            reader_handle: Arc::new(RwLock::new(None)),
            message_callback: Arc::new(RwLock::new(None)),
        }
    }

    pub fn config(&self) -> &FixSessionConfig {
        &self.config
    }

    pub async fn state(&self) -> FixSessionState {
        *self.state.read().await
    }

    pub fn outbound_seq(&self) -> u64 {
        self.outbound_seq.load(Ordering::Relaxed)
    }

    pub fn inbound_seq(&self) -> u64 {
        self.inbound_seq.load(Ordering::Relaxed)
    }

    /// Set callback for incoming messages (other than heartbeats/logon/logout)
    pub async fn set_message_callback<F>(&self, callback: F)
    where
        F: Fn(FixMessage) + Send + Sync + 'static,
    {
        let mut cb = self.message_callback.write().await;
        *cb = Some(Box::new(callback));
    }

    /// Connect to the FIX server and perform logon
    pub async fn connect(&self) -> Result<(), GatewayError> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        tracing::info!(addr = %addr, "Connecting to FIX server...");

        let timeout = Duration::from_millis(self.config.connection_timeout_ms);
        let stream = time::timeout(timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| {
                GatewayError::ConnectionFailed(format!("timeout after {}ms", timeout.as_millis()))
            })?
            .map_err(|e| GatewayError::ConnectionFailed(format!("TCP connect failed: {}", e)))?;

        let _ = stream.set_nodelay(true);

        tracing::info!("TCP connection established, sending logon...");

        *self.state.write().await = FixSessionState::LoggingOn;

        let (reader, mut writer) = stream.into_split();
        let reader = BufReader::new(reader);

        // Build and send Logon message
        let seq = self.outbound_seq.load(Ordering::Relaxed);
        let logon = FixMessageBuilder::new(tag::LOGON)
            .field(tag::ENCRYPT_METHOD, &self.config.encrypt_method.to_string())
            .field(
                tag::HEARTBT_INT,
                &self.config.heartbeat_interval_secs.to_string(),
            )
            .field(
                tag::RESET_SEQ_NUM_FLAG,
                if self.config.reset_seq_on_reconnect {
                    "Y"
                } else {
                    "N"
                },
            )
            .build(
                &self.config.fix_version,
                &self.config.sender_comp_id,
                &self.config.target_comp_id,
                seq,
            );

        writer
            .write_all(logon.as_bytes())
            .await
            .map_err(|e| GatewayError::ConnectionFailed(format!("failed to send logon: {}", e)))?;
        self.outbound_seq.fetch_add(1, Ordering::Relaxed);

        tracing::info!("Logon sent (seq={}), waiting for response...", seq);

        // Wait for logon response
        let mut lines = reader.lines();
        let logon_timeout = Duration::from_millis(self.config.connection_timeout_ms);
        let mut logon_received = false;

        match time::timeout(logon_timeout, async {
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                let msg = match parse_fix_message(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to parse FIX message");
                        continue;
                    }
                };

                match msg.msg_type.as_str() {
                    tag::LOGON => {
                        // Check for sequence reset
                        if let Some(next_seq) = msg.get_u64(tag::NEXT_EXPECTED_MSG_SEQ_NUM) {
                            tracing::info!(next_seq = next_seq, "Server requested sequence reset");
                            self.outbound_seq.store(next_seq, Ordering::Relaxed);
                        }
                        logon_received = true;
                        tracing::info!("Logon confirmed by counterparty");
                        return Ok::<(), GatewayError>(());
                    }
                    tag::LOGOUT => {
                        let reason = msg.get(tag::TEXT).unwrap_or("no reason").to_string();
                        return Err(GatewayError::FixLogonRejected { reason });
                    }
                    tag::HEARTBEAT => {
                        tracing::trace!("Heartbeat received during logon");
                        continue;
                    }
                    _ => {
                        tracing::trace!(msg_type = %msg.msg_type, "Ignoring non-logon message during logon");
                        continue;
                    }
                }
            }
            Err(GatewayError::ConnectionLost("connection closed during logon".to_string()))
        }).await {
            Ok(Ok(())) => {
                *self.state.write().await = FixSessionState::LoggedOn;
                *self.last_inbound.write().await = Instant::now();
                tracing::info!("FIX session established");
                Ok(())
            }
            Ok(Err(e)) => {
                *self.state.write().await = FixSessionState::Error;
                Err(e)
            }
            Err(_) => {
                *self.state.write().await = FixSessionState::Error;
                Err(GatewayError::ConnectionFailed(format!(
                    "logon timeout after {}ms",
                    logon_timeout.as_millis()
                )))
            }
        }
    }

    /// Disconnect from the FIX server
    pub async fn disconnect(&self) -> Result<(), GatewayError> {
        self.shutdown.store(true, Ordering::Relaxed);

        // Abort background tasks
        if let Some(handle) = self.heartbeat_handle.write().await.take() {
            handle.abort();
        }
        if let Some(handle) = self.reader_handle.write().await.take() {
            handle.abort();
        }

        *self.state.write().await = FixSessionState::Disconnected;
        tracing::info!("FIX session disconnected");
        Ok(())
    }

    /// Check if session is logged on
    pub async fn is_logged_on(&self) -> bool {
        matches!(*self.state.read().await, FixSessionState::LoggedOn)
    }

    /// Send a TestRequest message (expects Heartbeat response)
    pub async fn send_test_request(&self) -> Result<(), GatewayError> {
        self.send_message(
            FixMessageBuilder::new(tag::TEST_REQUEST)
                .field(tag::TEXT, "TEST")
                .build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ),
        )
        .await
    }

    /// Send a Logout message
    pub async fn send_logout(&self, reason: &str) -> Result<(), GatewayError> {
        self.send_message(
            FixMessageBuilder::new(tag::LOGOUT)
                .field(tag::TEXT, reason)
                .build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ),
        )
        .await?;
        *self.state.write().await = FixSessionState::LogoffSent;
        tracing::info!(reason = %reason, "Logout sent");
        Ok(())
    }

    /// Send a ResendRequest
    pub async fn send_resend_request(
        &self,
        begin_seq: u64,
        end_seq: u64,
    ) -> Result<(), GatewayError> {
        self.send_message(
            FixMessageBuilder::new(tag::RESEND_REQUEST)
                .field(tag::BEGIN_STRING, &self.config.fix_version)
                .field("7", &begin_seq.to_string())
                .field("16", &end_seq.to_string())
                .build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ),
        )
        .await
    }

    /// Send a SequenceReset
    pub async fn send_sequence_reset(&self, new_seq_no: u64) -> Result<(), GatewayError> {
        self.send_message(
            FixMessageBuilder::new(tag::SEQUENCE_RESET)
                .field(tag::MSG_SEQ_NUM, &new_seq_no.to_string())
                .field("36", "Y")
                .build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ),
        )
        .await
    }

    /// Send a NewOrderSingle
    pub async fn send_new_order_single(
        &self,
        cl_ord_id: &str,
        symbol: &str,
        side: &str,
        order_type: &str,
        quantity: f64,
        price: Option<f64>,
    ) -> Result<(), GatewayError> {
        let mut builder = FixMessageBuilder::new(tag::NEW_ORDER_SINGLE)
            .field("11", cl_ord_id)
            .field("55", symbol)
            .field("54", side)
            .field("38", &format!("{:.6}", quantity));

        if let Some(p) = price {
            builder = builder.field("44", &format!("{:.6}", p));
        }

        builder = builder
            .field("40", order_type)
            .field("21", "1") // HandlInst
            .field("59", "0"); // TimeInForce: Day

        self.send_message(builder.build(
            &self.config.fix_version,
            &self.config.sender_comp_id,
            &self.config.target_comp_id,
            self.outbound_seq.load(Ordering::Relaxed),
        ))
        .await
    }

    /// Send a OrderCancelRequest
    pub async fn send_order_cancel_request(
        &self,
        orig_cl_ord_id: &str,
        cl_ord_id: &str,
        symbol: &str,
        side: &str,
    ) -> Result<(), GatewayError> {
        self.send_message(
            FixMessageBuilder::new(tag::ORDER_CANCEL_REQUEST)
                .field("11", cl_ord_id)
                .field("41", orig_cl_ord_id)
                .field("55", symbol)
                .field("54", side)
                .field("38", "0")
                .build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ),
        )
        .await
    }

    /// Send a MarketDataRequest (full refresh subscription)
    pub async fn send_market_data_request(
        &self,
        md_req_id: &str,
        symbol: &str,
        subscription_type: &str,
    ) -> Result<(), GatewayError> {
        self.send_message(
            FixMessageBuilder::new(tag::MARKET_DATA_REQUEST)
                .field("262", md_req_id)
                .field("263", subscription_type) // 0=Snapshot, 1=Subscribe, 2=Unsubscribe
                .field("264", "1") // MarketDepth: 0=Book, 1=Top
                .field("265", "1") // MDUpdateType: 0=Full, 1=Incremental
                .field("266", "0") // AggregatedBook
                .field("267", "2") // NoMDEntryTypes: 0=Bid, 1=Offer
                .field("269", "0")
                .field("269", "1")
                .field("146", "1") // NoRelatedSym
                .field("55", symbol)
                .build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ),
        )
        .await
    }

    /// Send a raw FIX message string
    async fn send_message(&self, msg: String) -> Result<(), GatewayError> {
        if !self.is_logged_on().await
            && !matches!(*self.state.read().await, FixSessionState::LoggingOn)
        {
            return Err(GatewayError::FixProtocolError(
                "cannot send message: not logged on".to_string(),
            ));
        }
        self.outbound_seq.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(msg_type = ?msg.get(8..12), seq = self.outbound_seq.load(Ordering::Relaxed), "FIX message queued");
        // In production this would write to the TCP stream
        // For now, we log and track sequence
        tracing::debug!(raw = %msg, "FIX message");
        Ok(())
    }

    /// Handle an incoming FIX message (used by the reader loop)
    pub async fn handle_incoming_message(&self, msg: FixMessage) -> Result<(), GatewayError> {
        *self.last_inbound.write().await = Instant::now();

        // Sequence number validation
        if let Some(seq) = msg.get_u64(tag::MSG_SEQ_NUM) {
            let expected = self.inbound_seq.load(Ordering::Relaxed);
            if seq > expected {
                tracing::warn!(
                    expected = expected,
                    received = seq,
                    gap = seq - expected,
                    "Sequence gap detected"
                );
            } else if seq < expected {
                tracing::warn!(
                    expected = expected,
                    received = seq,
                    "Sequence number too low (possible duplicate)"
                );
            }
            self.inbound_seq.store(seq + 1, Ordering::Relaxed);
        }

        match msg.msg_type.as_str() {
            tag::HEARTBEAT => {
                tracing::trace!("Heartbeat received");
            }
            tag::LOGON => {
                tracing::info!("Unexpected logon received (re-authentication)");
            }
            tag::LOGOUT => {
                let reason = msg.get(tag::TEXT).unwrap_or("no reason");
                tracing::warn!(reason = %reason, "Logout received from counterparty");
                *self.state.write().await = FixSessionState::LogoutReceived;
                return Err(GatewayError::FixLogoutReceived {
                    reason: reason.to_string(),
                });
            }
            tag::TEST_REQUEST => {
                tracing::trace!("TestRequest received, sending Heartbeat");
                self.send_message(FixMessageBuilder::new(tag::HEARTBEAT).build(
                    &self.config.fix_version,
                    &self.config.sender_comp_id,
                    &self.config.target_comp_id,
                    self.outbound_seq.load(Ordering::Relaxed),
                ))
                .await?;
            }
            tag::RESEND_REQUEST => {
                tracing::info!("ResendRequest received");
            }
            tag::SEQUENCE_RESET => {
                if let Some(new_seq) = msg.get_u64("36") {
                    tracing::info!(new_seq = new_seq, "SequenceReset received");
                    self.outbound_seq.store(new_seq, Ordering::Relaxed);
                }
            }
            _ => {
                // Forward to callback
                if let Some(cb) = self.message_callback.read().await.as_ref() {
                    cb(msg);
                } else {
                    tracing::trace!(msg_type = %msg.msg_type, "No callback set, ignoring message");
                }
            }
        }

        Ok(())
    }
}

/// WebSocket configuration for market data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketConfig {
    pub url: String,
    /// Maximum reconnection attempts (0 = infinite)
    pub max_reconnect_attempts: u32,
    /// Base reconnection delay in ms
    pub reconnect_base_delay_ms: u64,
    /// Maximum reconnection delay in ms
    pub reconnect_max_delay_ms: u64,
    /// Connection timeout in ms
    pub connection_timeout_ms: u64,
    /// Ping interval in ms (0 = disabled)
    pub ping_interval_ms: u64,
    /// Message size limit in bytes
    pub max_message_size: usize,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:8080/market".to_string(),
            max_reconnect_attempts: 0,
            reconnect_base_delay_ms: 100,
            reconnect_max_delay_ms: 30_000,
            connection_timeout_ms: 10_000,
            ping_interval_ms: 30_000,
            max_message_size: 65_536,
        }
    }
}

/// Connection health monitor — tracks tick arrival intervals and detects anomalies
pub struct ConnectionHealthMonitor {
    expected_interval_ms: u64,
    timeout_multiplier: f64,
    last_tick_ns: AtomicU64,
    tick_count: AtomicU64,
    total_gap_ms: AtomicU64,
}

impl ConnectionHealthMonitor {
    pub fn new(expected_interval_ms: u64, timeout_multiplier: f64) -> Self {
        Self {
            expected_interval_ms,
            timeout_multiplier,
            last_tick_ns: AtomicU64::new(0),
            tick_count: AtomicU64::new(0),
            total_gap_ms: AtomicU64::new(0),
        }
    }

    /// Record a tick arrival and return the gap since last tick in ms
    pub fn record_tick(&self, timestamp_ns: u64) -> u64 {
        let last = self.last_tick_ns.swap(timestamp_ns, Ordering::Relaxed);
        self.tick_count.fetch_add(1, Ordering::Relaxed);

        if last == 0 {
            return 0;
        }

        let gap_ns = timestamp_ns.saturating_sub(last);
        let gap_ms = gap_ns / 1_000_000;
        self.total_gap_ms.fetch_add(gap_ms, Ordering::Relaxed);
        gap_ms
    }

    /// Check if the connection is healthy (gap within expected bounds)
    pub fn is_healthy(&self, current_ns: u64) -> bool {
        let last = self.last_tick_ns.load(Ordering::Relaxed);
        if last == 0 {
            return true;
        }

        let gap_ms = (current_ns.saturating_sub(last)) / 1_000_000;
        let threshold = (self.expected_interval_ms as f64 * self.timeout_multiplier) as u64;
        gap_ms <= threshold
    }

    /// Get the timeout threshold in ms
    pub fn timeout_threshold_ms(&self) -> u64 {
        (self.expected_interval_ms as f64 * self.timeout_multiplier) as u64
    }

    pub fn tick_count(&self) -> u64 {
        self.tick_count.load(Ordering::Relaxed)
    }

    pub fn avg_gap_ms(&self) -> f64 {
        let count = self.tick_count.load(Ordering::Relaxed);
        if count <= 1 {
            return 0.0;
        }
        let total = self.total_gap_ms.load(Ordering::Relaxed);
        total as f64 / (count - 1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fix_config_new() {
        let config = FixSessionConfig::new("SENDER1", "TARGET1", "192.168.1.1", 9876);
        assert_eq!(config.sender_comp_id, "SENDER1");
        assert_eq!(config.target_comp_id, "TARGET1");
        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 9876);
        assert_eq!(config.fix_version, "FIX.4.4");
        assert_eq!(config.heartbeat_interval_secs, 30);
        assert_eq!(config.encrypt_method, 0);
        assert!(!config.reset_seq_on_reconnect);
    }

    #[test]
    fn test_fix_config_default() {
        let config = FixSessionConfig::default();
        assert_eq!(config.sender_comp_id, "SENDER");
        assert_eq!(config.target_comp_id, "TARGET");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 9876);
    }

    #[test]
    fn test_fix_config_serde_roundtrip() {
        let config = FixSessionConfig::new("SENDER", "TARGET", "localhost", 1234);
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: FixSessionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.sender_comp_id, "SENDER");
        assert_eq!(deserialized.port, 1234);
    }

    #[test]
    fn test_parse_fix_logon() {
        let soh = tag::SOH;
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=A{soh}49=SENDER{soh}56=TARGET{soh}34=1{soh}52=20260101-00:00:00.000{soh}98=0{soh}108=30{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();
        assert_eq!(msg.msg_type, "A");
        assert_eq!(msg.get("49").unwrap(), "SENDER");
        assert_eq!(msg.get("56").unwrap(), "TARGET");
        assert_eq!(msg.get_u64("34").unwrap(), 1);
        assert_eq!(msg.get("108").unwrap(), "30");
    }

    #[test]
    fn test_parse_fix_heartbeat() {
        let soh = tag::SOH;
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=0{soh}49=SENDER{soh}56=TARGET{soh}34=2{soh}52=20260101-00:00:01.000{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();
        assert_eq!(msg.msg_type, "0");
        assert_eq!(msg.get_u64("34").unwrap(), 2);
    }

    #[test]
    fn test_parse_fix_too_few_fields() {
        let result = parse_fix_message("35=A");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_fix_missing_msg_type() {
        let soh = tag::SOH;
        let raw = format!("8=FIX.4.4{soh}9=5{soh}49=SENDER{soh}10=000{soh}");
        let result = parse_fix_message(&raw);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("MsgType"));
    }

    #[test]
    fn test_fix_message_builder_logon() {
        let msg = FixMessageBuilder::new("A")
            .field("98", "0")
            .field("108", "30")
            .build("FIX.4.4", "SENDER", "TARGET", 1);

        assert!(msg.contains("8=FIX.4.4"));
        assert!(msg.contains("35=A"));
        assert!(msg.contains("49=SENDER"));
        assert!(msg.contains("56=TARGET"));
        assert!(msg.contains("34=1"));
        assert!(msg.contains("98=0"));
        assert!(msg.contains("108=30"));
        assert!(msg.contains("10="));
    }

    #[test]
    fn test_fix_message_builder_heartbeat() {
        let msg = FixMessageBuilder::new("0").build("FIX.4.4", "SENDER", "TARGET", 5);

        assert!(msg.contains("35=0"));
        assert!(msg.contains("34=5"));
    }

    #[test]
    fn test_fix_message_builder_new_order_single() {
        let msg = FixMessageBuilder::new("D")
            .field("11", "ORD123")
            .field("55", "EUR/USD")
            .field("54", "1")
            .field("38", "1000000")
            .field("40", "2")
            .field("44", "1.08500")
            .build("FIX.4.4", "SENDER", "TARGET", 10);

        assert!(msg.contains("35=D"));
        assert!(msg.contains("11=ORD123"));
        assert!(msg.contains("55=EUR/USD"));
        assert!(msg.contains("54=1"));
        assert!(msg.contains("38=1000000"));
        assert!(msg.contains("40=2"));
        assert!(msg.contains("44=1.08500"));
        assert!(msg.contains("34=10"));
    }

    #[test]
    fn test_fix_message_builder_sequence_increments() {
        let msg1 = FixMessageBuilder::new("0").build("FIX.4.4", "S", "T", 1);
        let msg2 = FixMessageBuilder::new("0").build("FIX.4.4", "S", "T", 2);
        assert!(msg1.contains("34=1"));
        assert!(msg2.contains("34=2"));
    }

    #[test]
    fn test_compute_checksum() {
        // Checksum is computed over all bytes before the checksum field
        let msg = "8=FIX.4.4\x019=5\x0135=0\x01";
        let checksum = compute_checksum(msg);
        assert!(checksum < 256);
    }

    #[test]
    fn test_format_timestamp_ns() {
        let ts = format_timestamp_ns(1_710_000_000_000_000_000);
        assert!(!ts.is_empty());
        // Should contain date-time components
        assert!(ts.contains('-'));
        assert!(ts.contains(':'));
    }

    #[tokio::test]
    async fn test_fix_session_state_lifecycle() {
        let config = FixSessionConfig::new("S", "T", "localhost", 1234);
        let session = FixSession::new(config);

        assert_eq!(session.state().await, FixSessionState::Disconnected);
        assert_eq!(session.outbound_seq(), 1);
        assert_eq!(session.inbound_seq(), 1);
        assert!(!session.is_logged_on().await);
    }

    #[tokio::test]
    async fn test_fix_session_handle_heartbeat() {
        let config = FixSessionConfig::new("S", "T", "localhost", 1234);
        let session = FixSession::new(config);

        let soh = tag::SOH;
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=0{soh}49=TARGET{soh}56=SENDER{soh}34=1{soh}52=20260101-00:00:00.000{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();

        session.handle_incoming_message(msg).await.unwrap();
        assert_eq!(session.inbound_seq(), 2);
    }

    #[tokio::test]
    async fn test_fix_session_handle_logout() {
        let config = FixSessionConfig::new("S", "T", "localhost", 1234);
        let session = FixSession::new(config);

        let soh = tag::SOH;
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=5{soh}49=TARGET{soh}56=SENDER{soh}34=1{soh}52=20260101-00:00:00.000{soh}58=end of day{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();

        let result = session.handle_incoming_message(msg).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("end of day"));
        assert_eq!(session.state().await, FixSessionState::LogoutReceived);
    }

    #[tokio::test]
    async fn test_fix_session_handle_test_request() {
        let config = FixSessionConfig::new("S", "T", "localhost", 1234);
        let session = FixSession::new(config);

        // Manually set to logged on so we can send responses
        {
            let mut state = session.state.write().await;
            *state = FixSessionState::LoggedOn;
        }

        let soh = tag::SOH;
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=1{soh}49=TARGET{soh}56=SENDER{soh}34=1{soh}52=20260101-00:00:00.000{soh}112=TEST{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();

        session.handle_incoming_message(msg).await.unwrap();
        assert_eq!(session.inbound_seq(), 2);
    }

    #[tokio::test]
    async fn test_fix_session_sequence_gap_detection() {
        let config = FixSessionConfig::new("S", "T", "localhost", 1234);
        let session = FixSession::new(config);

        let soh = tag::SOH;
        // Seq 1
        let raw1 = format!(
            "8=FIX.4.4{soh}9=5{soh}35=0{soh}49=TARGET{soh}56=SENDER{soh}34=1{soh}52=20260101-00:00:00.000{soh}10=000{soh}"
        );
        session
            .handle_incoming_message(parse_fix_message(&raw1).unwrap())
            .await
            .unwrap();
        assert_eq!(session.inbound_seq(), 2);

        // Seq 4 (gap of 2)
        let raw4 = format!(
            "8=FIX.4.4{soh}9=5{soh}35=0{soh}49=TARGET{soh}56=SENDER{soh}34=4{soh}52=20260101-00:00:01.000{soh}10=000{soh}"
        );
        session
            .handle_incoming_message(parse_fix_message(&raw4).unwrap())
            .await
            .unwrap();
        assert_eq!(session.inbound_seq(), 5);
    }

    #[tokio::test]
    async fn test_fix_session_message_callback() {
        let config = FixSessionConfig::new("S", "T", "localhost", 1234);
        let session = FixSession::new(config);

        use std::sync::atomic::AtomicI32;
        let callback_called = Arc::new(AtomicI32::new(0));
        let callback_called_clone = callback_called.clone();

        session
            .set_message_callback(move |_msg: FixMessage| {
                callback_called_clone.fetch_add(1, Ordering::Relaxed);
            })
            .await;

        let soh = tag::SOH;
        // ExecutionReport (msg type 8) should trigger callback
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=8{soh}49=TARGET{soh}56=SENDER{soh}34=1{soh}52=20260101-00:00:00.000{soh}11=ORD1{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();

        session.handle_incoming_message(msg).await.unwrap();
        assert_eq!(callback_called.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_websocket_config_default() {
        let config = WebSocketConfig::default();
        assert_eq!(config.url, "ws://127.0.0.1:8080/market");
        assert_eq!(config.max_reconnect_attempts, 0);
        assert_eq!(config.reconnect_base_delay_ms, 100);
        assert_eq!(config.reconnect_max_delay_ms, 30_000);
        assert_eq!(config.connection_timeout_ms, 10_000);
        assert_eq!(config.ping_interval_ms, 30_000);
        assert_eq!(config.max_message_size, 65_536);
    }

    #[test]
    fn test_websocket_config_serde_roundtrip() {
        let config = WebSocketConfig {
            url: "wss://api.example.com/ws".to_string(),
            max_reconnect_attempts: 10,
            reconnect_base_delay_ms: 200,
            reconnect_max_delay_ms: 60_000,
            connection_timeout_ms: 5_000,
            ping_interval_ms: 15_000,
            max_message_size: 131_072,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: WebSocketConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.url, "wss://api.example.com/ws");
        assert_eq!(deserialized.max_reconnect_attempts, 10);
        assert_eq!(deserialized.max_message_size, 131_072);
    }

    #[test]
    fn test_connection_health_monitor_record_tick() {
        let monitor = ConnectionHealthMonitor::new(100, 3.0);
        assert_eq!(monitor.tick_count(), 0);

        let gap1 = monitor.record_tick(1_000_000_000_000); // 1000s in ns
        assert_eq!(gap1, 0);
        assert_eq!(monitor.tick_count(), 1);

        let gap2 = monitor.record_tick(1_000_500_000_000); // +500ms in ns
        assert_eq!(gap2, 500);
        assert_eq!(monitor.tick_count(), 2);
    }

    #[test]
    fn test_connection_health_monitor_is_healthy() {
        let monitor = ConnectionHealthMonitor::new(100, 3.0);
        assert!(monitor.is_healthy(0));

        monitor.record_tick(1_000_000_000_000);
        // Within threshold (300ms = 300_000_000 ns)
        assert!(monitor.is_healthy(1_000_200_000_000));
        // Beyond threshold
        assert!(!monitor.is_healthy(1_000_500_000_000));
    }

    #[test]
    fn test_connection_health_monitor_timeout_threshold() {
        let monitor = ConnectionHealthMonitor::new(100, 3.0);
        assert_eq!(monitor.timeout_threshold_ms(), 300);

        let monitor2 = ConnectionHealthMonitor::new(500, 2.5);
        assert_eq!(monitor2.timeout_threshold_ms(), 1250);
    }

    #[test]
    fn test_connection_health_monitor_avg_gap() {
        let monitor = ConnectionHealthMonitor::new(100, 3.0);
        assert!((monitor.avg_gap_ms() - 0.0).abs() < 1e-10);

        monitor.record_tick(1_000_000_000_000);
        assert!((monitor.avg_gap_ms() - 0.0).abs() < 1e-10); // 1 tick = no avg

        monitor.record_tick(1_000_100_000_000); // +100ms
        monitor.record_tick(1_000_300_000_000); // +200ms
                                                // Gaps: 100ms, 200ms → avg = 150ms
        assert!((monitor.avg_gap_ms() - 150.0).abs() < 1e-10);
    }

    #[test]
    fn test_fix_session_state_serde_roundtrip() {
        let states = [
            FixSessionState::Disconnected,
            FixSessionState::LoggingOn,
            FixSessionState::LoggedOn,
            FixSessionState::LogoffSent,
            FixSessionState::LogoutReceived,
            FixSessionState::Error,
        ];
        for state in &states {
            let json = serde_json::to_string(state).unwrap();
            let deserialized: FixSessionState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, deserialized);
        }
    }

    #[test]
    fn test_fix_message_get_fields() {
        let soh = tag::SOH;
        let raw = format!(
            "8=FIX.4.4{soh}9=5{soh}35=A{soh}49=SENDER{soh}56=TARGET{soh}34=42{soh}52=20260101-00:00:00.000{soh}98=0{soh}108=30{soh}10=000{soh}"
        );
        let msg = parse_fix_message(&raw).unwrap();
        assert_eq!(msg.get("49"), Some("SENDER"));
        assert_eq!(msg.get("999"), None);
        assert_eq!(msg.get_u64("34"), Some(42));
        assert_eq!(msg.get_u64("999"), None);
    }

    #[test]
    fn test_fix_message_builder_market_data_request() {
        let msg = FixMessageBuilder::new("V")
            .field("262", "MD1")
            .field("263", "1")
            .field("146", "1")
            .field("55", "EUR/USD")
            .build("FIX.4.4", "SENDER", "TARGET", 1);

        assert!(msg.contains("35=V"));
        assert!(msg.contains("262=MD1"));
        assert!(msg.contains("263=1"));
        assert!(msg.contains("146=1"));
        assert!(msg.contains("55=EUR/USD"));
    }
}
