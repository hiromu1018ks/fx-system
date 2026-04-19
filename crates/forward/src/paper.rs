use serde::{Deserialize, Serialize};

/// Result of a paper execution (simulated order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperOrderResult {
    pub order_id: String,
    pub symbol: String,
    pub side: String,
    pub requested_lots: f64,
    pub filled_lots: f64,
    pub fill_price: Option<f64>,
    pub slippage: f64,
    pub fill_probability: f64,
    pub rejected: bool,
    pub rejection_reason: Option<String>,
    pub timestamp_ns: u64,
}

/// Paper execution engine — simulates OTC execution without actual orders.
pub struct PaperExecutionEngine {
    // Will be expanded with OTC model parameters in subsequent tasks
}

impl PaperExecutionEngine {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for PaperExecutionEngine {
    fn default() -> Self {
        Self::new()
    }
}
