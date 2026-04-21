use anyhow::Result;
use fx_core::types::{Direction, EventTier, StreamId};
use fx_events::event::GenericEvent;
use fx_events::header::EventHeader;
use fx_execution::gateway::{
    ExecutionGateway, ExecutionGatewayConfig, ExecutionRequest, ExecutionResult,
};
use prost::Message;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use tracing::info;

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
///
/// Structurally guaranteed to never connect to real order pathways.
/// Uses the existing ExecutionGateway's OTC model (Last-Look, slippage,
/// fill probability) for realistic simulation.
pub struct PaperExecutionEngine {
    gateway: ExecutionGateway,
    rng: SmallRng,
}

impl PaperExecutionEngine {
    pub fn new(config: ExecutionGatewayConfig, seed: u64) -> Self {
        Self {
            gateway: ExecutionGateway::new(config),
            rng: SmallRng::seed_from_u64(seed),
        }
    }

    /// Execute a paper order — simulates OTC execution without real order submission.
    pub fn execute(&mut self, request: &ExecutionRequest) -> Result<PaperOrderResult> {
        let (paper_result, _) = self.simulate(request)?;
        Ok(paper_result)
    }

    /// Execute a paper order and also return the underlying execution model result.
    pub fn simulate(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<(PaperOrderResult, ExecutionResult)> {
        let result = self
            .gateway
            .simulate_execution(request, &mut self.rng)
            .map_err(|e| anyhow::anyhow!("Execution simulation failed: {:?}", e))?;

        let paper_result = Self::to_paper_result(request, &result);
        Ok((paper_result, result))
    }

    /// Build an ExecutionEvent proto from a request/result pair and publish as GenericEvent.
    pub fn build_execution_event(
        &self,
        request: &ExecutionRequest,
        result: &ExecutionResult,
    ) -> GenericEvent {
        let proto_event = self.gateway.build_execution_event(request, result);
        let payload = proto_event.encode_to_vec();
        let header = EventHeader::new(StreamId::Execution, 0, EventTier::Tier1Critical);
        GenericEvent::new(header, payload)
    }

    /// Check for LP switch signals (adversarial detection).
    pub fn check_lp_switch(&mut self) -> Option<String> {
        self.gateway
            .check_lp_switch()
            .map(|signal| signal.to_lp_id.clone())
    }

    /// Check if recalibration safe mode has completed.
    pub fn check_recalibration(&mut self, timestamp_ns: u64) -> bool {
        self.gateway.check_recalibration(timestamp_ns)
    }

    /// Get the current lot multiplier (reduced during recalibration).
    pub fn lot_multiplier(&self) -> f64 {
        self.gateway.recalibration_lot_multiplier()
    }

    /// Get the active LP ID.
    pub fn active_lp_id(&self) -> &str {
        self.gateway.active_lp_id()
    }

    /// Get mutable reference to the underlying gateway for advanced operations.
    pub fn gateway_mut(&mut self) -> &mut ExecutionGateway {
        &mut self.gateway
    }

    pub fn gateway(&self) -> &ExecutionGateway {
        &self.gateway
    }

    fn to_paper_result(request: &ExecutionRequest, result: &ExecutionResult) -> PaperOrderResult {
        let paper_result = PaperOrderResult {
            order_id: result.order_id.clone(),
            symbol: request.symbol.clone(),
            side: match request.direction {
                Direction::Buy => "buy".to_string(),
                Direction::Sell => "sell".to_string(),
            },
            requested_lots: request.lots as f64,
            filled_lots: if result.filled { result.fill_size } else { 0.0 },
            fill_price: if result.filled {
                Some(result.fill_price)
            } else {
                None
            },
            slippage: result.slippage,
            fill_probability: result.effective_fill_probability,
            rejected: !result.filled,
            rejection_reason: result.reject_reason.clone(),
            timestamp_ns: request.timestamp_ns,
        };

        info!(
            order_id = %paper_result.order_id,
            filled = paper_result.fill_price.is_some(),
            slippage = paper_result.slippage,
            "Paper execution completed"
        );

        paper_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::types::StrategyId;
    use fx_execution::gateway::ExecutionGatewayConfig;

    fn default_config() -> ExecutionGatewayConfig {
        ExecutionGatewayConfig::default()
    }

    fn make_request(direction: Direction, lots: u64) -> ExecutionRequest {
        ExecutionRequest {
            direction,
            lots,
            strategy_id: StrategyId::A,
            current_mid_price: 1.1000,
            volatility: 0.001,
            expected_profit: 0.0002,
            symbol: "EUR/USD".to_string(),
            timestamp_ns: 1_000_000,
            time_urgent: false,
        }
    }

    #[test]
    fn test_paper_execution_filled() {
        let config = default_config();
        let mut engine = PaperExecutionEngine::new(config, 42);

        // Use a request with high expected profit and low vol — high fill probability
        let request = make_request(Direction::Buy, 1);
        let result = engine.execute(&request).unwrap();

        // With seed 42, the stochastic outcome may be filled or rejected
        // Just verify structural correctness
        assert_eq!(result.symbol, "EUR/USD");
        assert_eq!(result.side, "buy");
        assert_eq!(result.requested_lots, 1.0);
        assert_eq!(result.timestamp_ns, 1_000_000);
        assert!(!result.order_id.is_empty());
    }

    #[test]
    fn test_paper_execution_reproducible() {
        let config1 = default_config();
        let config2 = default_config();
        let mut engine1 = PaperExecutionEngine::new(config1, 12345);
        let mut engine2 = PaperExecutionEngine::new(config2, 12345);

        let request = make_request(Direction::Sell, 2);
        let r1 = engine1.execute(&request).unwrap();
        let r2 = engine2.execute(&request).unwrap();

        assert_eq!(r1.fill_price, r2.fill_price);
        assert_eq!(r1.slippage, r2.slippage);
        assert_eq!(r1.rejected, r2.rejected);
    }

    #[test]
    fn test_paper_execution_different_seeds() {
        let config1 = default_config();
        let config2 = default_config();
        let mut engine1 = PaperExecutionEngine::new(config1, 111);
        let mut engine2 = PaperExecutionEngine::new(config2, 999);

        let request = make_request(Direction::Buy, 1);
        let r1 = engine1.execute(&request).unwrap();
        let r2 = engine2.execute(&request).unwrap();

        // Different seeds may produce different outcomes (not guaranteed, but likely)
        assert_eq!(r1.symbol, r2.symbol);
        // Results are structurally valid regardless
        assert!(r1.fill_probability >= 0.0 && r1.fill_probability <= 1.0);
        assert!(r2.fill_probability >= 0.0 && r2.fill_probability <= 1.0);
    }

    #[test]
    fn test_build_execution_event() {
        let config = default_config();
        let mut engine = PaperExecutionEngine::new(config, 42);

        let request = make_request(Direction::Buy, 1);
        let _paper_result = engine.execute(&request).unwrap();

        // Verify the event can be built (using the gateway directly)
        let mut rng2 = rand::rngs::SmallRng::seed_from_u64(42);
        let config2 = default_config();
        let mut gw = ExecutionGateway::new(config2);
        let exec_result = gw.simulate_execution(&request, &mut rng2).unwrap();

        let event = engine.build_execution_event(&request, &exec_result);
        assert_eq!(event.header.stream_id, StreamId::Execution);
        assert_eq!(event.header.tier, EventTier::Tier1Critical);
        assert!(!event.payload.is_empty());
    }

    #[test]
    fn test_active_lp() {
        let config = default_config();
        let engine = PaperExecutionEngine::new(config, 42);
        assert_eq!(engine.active_lp_id(), "LP_PRIMARY");
    }

    #[test]
    fn test_lot_multiplier_normal() {
        let config = default_config();
        let engine = PaperExecutionEngine::new(config, 42);
        assert_eq!(engine.lot_multiplier(), 1.0);
    }

    #[test]
    fn test_paper_execution_multiple_orders() {
        let config = default_config();
        let mut engine = PaperExecutionEngine::new(config, 42);

        for i in 0..10 {
            let request = ExecutionRequest {
                direction: if i % 2 == 0 {
                    Direction::Buy
                } else {
                    Direction::Sell
                },
                lots: 1,
                strategy_id: StrategyId::A,
                current_mid_price: 1.1000 + i as f64 * 0.0001,
                volatility: 0.001,
                expected_profit: 0.0002,
                symbol: "EUR/USD".to_string(),
                timestamp_ns: 1_000_000 + i as u64 * 1000,
                time_urgent: false,
            };

            let result = engine.execute(&request).unwrap();
            assert_eq!(result.symbol, "EUR/USD");
            assert!(result.fill_probability >= 0.0);
        }
    }
}
