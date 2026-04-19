use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("connection lost: {0}")]
    ConnectionLost(String),

    #[error("reconnect failed after {attempts} attempts")]
    ReconnectFailed { attempts: u32 },

    #[error("FIX protocol error: {0}")]
    FixProtocolError(String),

    #[error("FIX logon rejected: {reason}")]
    FixLogonRejected { reason: String },

    #[error("FIX logout received: {reason}")]
    FixLogoutReceived { reason: String },

    #[error("heartbeat timeout after {timeout_ms}ms")]
    HeartbeatTimeout { timeout_ms: u64 },

    #[error("invalid message: {0}")]
    InvalidMessage(String),

    #[error("event publish failed: {0}")]
    PublishFailed(String),

    #[error("encoding error: {0}")]
    EncodingError(String),
}
