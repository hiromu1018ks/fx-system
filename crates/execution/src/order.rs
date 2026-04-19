use fx_core::types::{Direction, StrategyId};

#[derive(Debug, Clone)]
pub struct OrderCommand {
    pub direction: Direction,
    pub lots: u64,
    pub symbol: String,
    pub strategy_id: StrategyId,
    pub order_type: crate::otc_model::OtcOrderType,
    pub limit_price: Option<f64>,
    pub timestamp_ns: u64,
}
