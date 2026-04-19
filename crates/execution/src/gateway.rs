use fx_core::types::{Direction, StrategyId};
use fx_events::proto;
use rand::prelude::*;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::lp_monitor::{LpMonitorConfig, LpRiskMonitor, LpSwitchSignal};
use crate::lp_recalibration::{LpRecalibrationManager, RecalibrationConfig, RecalibrationStatus};
use crate::otc_model::*;

// ============================================================
// Execution Request / Result
// ============================================================

#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    pub direction: Direction,
    pub lots: u64,
    pub strategy_id: StrategyId,
    pub current_mid_price: f64,
    pub volatility: f64,
    pub expected_profit: f64,
    pub symbol: String,
    pub timestamp_ns: u64,
    pub time_urgent: bool,
}

#[derive(Debug, Clone)]
pub struct OrderEvaluation {
    pub order_type: OtcOrderType,
    pub fill_probability: f64,
    pub effective_fill_probability: f64,
    pub expected_slippage: f64,
    pub last_look_fill_prob: f64,
    pub lp_id: String,
    pub limit_price_distance: f64,
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub order_id: String,
    pub filled: bool,
    pub fill_price: f64,
    pub fill_size: f64,
    pub slippage: f64,
    pub fill_probability: f64,
    pub effective_fill_probability: f64,
    pub last_look_rejection_prob: f64,
    pub price_improvement: f64,
    pub order_type: OtcOrderType,
    pub fill_status: FillOutcome,
    pub reject_reason: Option<String>,
    pub lp_id: String,
    pub requested_price: f64,
    pub requested_size: f64,
    pub latency_ms: f64,
    pub evaluation: OrderEvaluation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    Filled,
    PartialFill,
    Rejected,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("no LP available")]
    NoLpAvailable,
    #[error("zero lot order rejected")]
    ZeroLot,
}

// ============================================================
// ExecutionGatewayConfig
// ============================================================

#[derive(Debug, Clone)]
pub struct ExecutionGatewayConfig {
    pub symbol: String,
    pub known_lps: Vec<String>,
    pub last_look: LastLookConfig,
    pub fill_probability: FillProbabilityConfig,
    pub slippage: SlippageConfig,
    pub order_type: OrderTypeConfig,
    pub lp_monitor: LpMonitorConfig,
    pub recalibration: RecalibrationConfig,
}

impl Default for ExecutionGatewayConfig {
    fn default() -> Self {
        Self {
            symbol: "EUR/USD".to_string(),
            known_lps: vec!["LP_PRIMARY".to_string(), "LP_BACKUP".to_string()],
            last_look: LastLookConfig::default(),
            fill_probability: FillProbabilityConfig::default(),
            slippage: SlippageConfig::default(),
            order_type: OrderTypeConfig::default(),
            lp_monitor: LpMonitorConfig::default(),
            recalibration: RecalibrationConfig::default(),
        }
    }
}

// ============================================================
// ExecutionGateway
// ============================================================

#[derive(Debug)]
pub struct ExecutionGateway {
    _config: ExecutionGatewayConfig,
    last_look_model: LastLookModel,
    fill_prob_model: FillProbabilityModel,
    slippage_model: SlippageModel,
    order_type_selector: OrderTypeSelector,
    lp_monitor: LpRiskMonitor,
    recalibration_manager: LpRecalibrationManager,
    order_counter: u64,
}

impl ExecutionGateway {
    pub fn new(config: ExecutionGatewayConfig) -> Self {
        let known_lps = config.known_lps.clone();
        let recalibration = config.recalibration.clone();
        Self {
            _config: config,
            last_look_model: LastLookModel::new(LastLookConfig::default()),
            fill_prob_model: FillProbabilityModel::new(FillProbabilityConfig::default()),
            slippage_model: SlippageModel::new(SlippageConfig::default()),
            order_type_selector: OrderTypeSelector::new(OrderTypeConfig::default()),
            lp_monitor: LpRiskMonitor::new(LpMonitorConfig::default(), known_lps),
            recalibration_manager: LpRecalibrationManager::new(recalibration),
            order_counter: 0,
        }
    }

    fn next_order_id(&mut self) -> String {
        self.order_counter += 1;
        format!("ORD-{}-{}", self.order_counter, Uuid::now_v7())
    }

    // --- Order Evaluation Pipeline ---

    pub fn evaluate(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<OrderEvaluation, ExecutionError> {
        if request.lots == 0 {
            return Err(ExecutionError::ZeroLot);
        }

        let lp_id = self.lp_monitor.active_lp_id().to_string();
        let vol = request.volatility;

        // 1. Last-Look fill probability
        let last_look_fill_prob = self.last_look_model.fill_probability(&lp_id, vol);

        // 2. Order type selection (tentative Market for fill prob calculation)
        let (order_type, limit_distance) = self.order_type_selector.select_with_urgency(
            request.expected_profit,
            0.9, // preliminary estimate
            self.slippage_model.expected_slippage(
                request.direction,
                request.lots as f64,
                vol,
                &lp_id,
            ),
            request.time_urgent,
        );

        // 3. Effective fill probability
        let eff_fill_prob = self.fill_prob_model.effective_fill_probability(
            order_type,
            limit_distance,
            last_look_fill_prob,
        );

        // Recalculate order type with actual fill prob
        let expected_slip = self.slippage_model.expected_slippage(
            request.direction,
            request.lots as f64,
            vol,
            &lp_id,
        );
        let (order_type, limit_distance) = self.order_type_selector.select_with_urgency(
            request.expected_profit,
            eff_fill_prob,
            expected_slip,
            request.time_urgent,
        );

        let eff_fill_prob = self.fill_prob_model.effective_fill_probability(
            order_type,
            limit_distance,
            last_look_fill_prob,
        );

        let raw_fill_prob = self
            .fill_prob_model
            .request_fill_probability(order_type, limit_distance);

        debug!(
            order_type = ?order_type,
            lp_id = %lp_id,
            fill_prob = eff_fill_prob,
            expected_slippage = expected_slip,
            "Order evaluation complete"
        );

        Ok(OrderEvaluation {
            order_type,
            fill_probability: raw_fill_prob,
            effective_fill_probability: eff_fill_prob,
            expected_slippage: expected_slip,
            last_look_fill_prob,
            lp_id,
            limit_price_distance: limit_distance,
        })
    }

    // --- Simulated Execution (for backtesting) ---

    pub fn simulate_execution(
        &mut self,
        request: &ExecutionRequest,
        rng: &mut impl Rng,
    ) -> Result<ExecutionResult, ExecutionError> {
        let eval = self.evaluate(request)?;
        let lp_id = eval.lp_id.clone();
        let vol = request.volatility;

        // Determine fill/rejection based on effective fill probability
        let eff_fill = self.fill_prob_model.effective_fill_probability_sampled(
            eval.order_type,
            eval.limit_price_distance,
            eval.last_look_fill_prob,
            rng,
        );

        let filled = rng.gen::<f64>() < eff_fill;
        let is_last_look_rejection = !filled && rng.gen::<f64>() < eval.last_look_fill_prob;

        let requested_price = match request.direction {
            Direction::Buy => request.current_mid_price + eval.limit_price_distance,
            Direction::Sell => request.current_mid_price - eval.limit_price_distance,
        };

        let mut result = ExecutionResult {
            order_id: self.next_order_id(),
            filled,
            fill_price: requested_price,
            fill_size: 0.0,
            slippage: 0.0,
            fill_probability: eval.fill_probability,
            effective_fill_probability: eff_fill,
            last_look_rejection_prob: 1.0 - eval.last_look_fill_prob,
            price_improvement: 0.0,
            order_type: eval.order_type,
            fill_status: FillOutcome::Rejected,
            reject_reason: None,
            lp_id: lp_id.clone(),
            requested_price,
            requested_size: request.lots as f64,
            latency_ms: 0.5 + rng.gen::<f64>() * 2.0,
            evaluation: eval,
        };

        if filled {
            let actual_slippage = self.slippage_model.sample_slippage(
                request.direction,
                request.lots as f64,
                vol,
                &lp_id,
                rng,
            );

            result.slippage = actual_slippage;
            result.fill_price = match request.direction {
                Direction::Buy => requested_price + actual_slippage,
                Direction::Sell => requested_price - actual_slippage,
            };
            result.fill_size = request.lots as f64;
            result.price_improvement = -actual_slippage; // negative slippage = improvement
            result.fill_status = FillOutcome::Filled;

            self.process_fill(&lp_id, actual_slippage);
        } else {
            let reason = if is_last_look_rejection {
                "LAST_LOOK".to_string()
            } else {
                "INSUFFICIENT_LIQUIDITY".to_string()
            };
            result.reject_reason = Some(reason.clone());
            self.process_rejection(&lp_id, &reason);
        }

        let _ = self.check_lp_switch();
        Ok(result)
    }

    // --- Event Processing ---

    pub fn process_fill(&mut self, lp_id: &str, slippage: f64) {
        self.last_look_model.update_fill(lp_id);
        self.slippage_model.update_observation(lp_id, slippage);
        self.lp_monitor.record_fill(lp_id);
    }

    pub fn process_fill_with_prediction(
        &mut self,
        lp_id: &str,
        observed_slippage: f64,
        predicted_slippage: f64,
        timestamp_ns: u64,
    ) {
        self.last_look_model.update_fill(lp_id);
        self.slippage_model
            .update_observation(lp_id, observed_slippage);
        self.lp_monitor.record_fill(lp_id);
        self.recalibration_manager.record_fill(
            lp_id,
            observed_slippage,
            predicted_slippage,
            timestamp_ns,
        );
    }

    pub fn process_rejection(&mut self, lp_id: &str, _reason: &str) {
        self.last_look_model.update_rejection(lp_id);
        self.lp_monitor.record_rejection(lp_id);
    }

    pub fn process_rejection_with_timestamp(
        &mut self,
        lp_id: &str,
        _reason: &str,
        timestamp_ns: u64,
    ) {
        self.last_look_model.update_rejection(lp_id);
        self.lp_monitor.record_rejection(lp_id);
        self.recalibration_manager
            .record_rejection(lp_id, timestamp_ns);
    }

    pub fn check_lp_switch(&mut self) -> Option<LpSwitchSignal> {
        let signal = self.lp_monitor.check_adversarial()?;
        warn!(
            from = %signal.from_lp_id,
            to = %signal.to_lp_id,
            reason = %signal.reason,
            "LP switch triggered"
        );
        info!(new_lp = %signal.to_lp_id, "Entering safe mode: lot 25%");
        let new_lp_id = &signal.to_lp_id;
        let baseline_fill_rate = self.last_look_model.fill_probability(new_lp_id, 0.1);
        let baseline_slippage = self
            .slippage_model
            .get_lp_stats(new_lp_id)
            .map(|s| s.mean)
            .unwrap_or(0.0);
        self.recalibration_manager
            .enter_safe_mode(&signal, baseline_fill_rate, baseline_slippage);
        Some(signal)
    }

    /// Check if recalibration should complete. Call periodically with current timestamp.
    pub fn check_recalibration(&mut self, timestamp_ns: u64) -> bool {
        if self.recalibration_manager.check_completion(timestamp_ns) {
            self.recalibration_manager.reset();
            true
        } else {
            false
        }
    }

    /// Get the current recalibration lot multiplier. During safe mode, returns reduced value.
    pub fn recalibration_lot_multiplier(&self) -> f64 {
        self.recalibration_manager.lot_multiplier()
    }

    /// Get the current σ_execution multiplier. During safe mode, returns doubled value.
    pub fn recalibration_sigma_multiplier(&self) -> f64 {
        self.recalibration_manager.sigma_multiplier()
    }

    /// Check if the gateway is currently in recalibration safe mode.
    pub fn is_recalibrating(&self) -> bool {
        self.recalibration_manager.is_safe_mode()
    }

    /// Get the recalibration status.
    pub fn recalibration_status(&self) -> &RecalibrationStatus {
        self.recalibration_manager.status()
    }

    /// Get the recalibration manager (for advanced use).
    pub fn recalibration_manager(&self) -> &LpRecalibrationManager {
        &self.recalibration_manager
    }

    // --- Proto Conversion ---

    pub fn build_execution_event(
        &self,
        request: &ExecutionRequest,
        result: &ExecutionResult,
    ) -> proto::ExecutionEventPayload {
        let order_type_i32 = match result.order_type {
            OtcOrderType::Market => proto::OrderType::OrderMarket as i32,
            OtcOrderType::Limit => proto::OrderType::OrderLimit as i32,
        };

        let fill_status_i32 = match result.fill_status {
            FillOutcome::Filled => proto::FillStatus::Filled as i32,
            FillOutcome::PartialFill => proto::FillStatus::PartialFill as i32,
            FillOutcome::Rejected => proto::FillStatus::Rejected as i32,
        };

        let reject_reason_i32 = match result.reject_reason.as_deref() {
            Some("LAST_LOOK") => proto::RejectReason::LastLook as i32,
            Some("INSUFFICIENT_LIQUIDITY") => proto::RejectReason::InsufficientLiquidity as i32,
            Some("MARKET_CLOSED") => proto::RejectReason::MarketClosed as i32,
            Some("RISK_LIMIT") => proto::RejectReason::RiskLimit as i32,
            _ => proto::RejectReason::Unknown as i32,
        };

        proto::ExecutionEventPayload {
            header: Some(proto::EventHeader {
                event_id: Uuid::now_v7().to_string(),
                parent_event_id: None,
                stream_id: proto::StreamId::Execution as i32,
                sequence_id: 0,
                timestamp_ns: request.timestamp_ns,
                schema_version: 1,
                tier: proto::EventTier::Tier1Critical as i32,
            }),
            order_id: result.order_id.clone(),
            symbol: request.symbol.clone(),
            order_type: order_type_i32,
            fill_status: fill_status_i32,
            fill_price: result.fill_price,
            fill_size: result.fill_size,
            slippage: result.slippage,
            requested_price: result.requested_price,
            requested_size: result.requested_size,
            fill_probability: result.fill_probability,
            effective_fill_probability: result.effective_fill_probability,
            price_improvement: result.price_improvement,
            last_look_rejection_prob: result.last_look_rejection_prob,
            lp_id: result.lp_id.clone(),
            latency_ms: result.latency_ms,
            reject_reason: reject_reason_i32,
            reject_message: result.reject_reason.clone().unwrap_or_default(),
        }
    }

    // --- Accessors ---

    pub fn active_lp_id(&self) -> &str {
        self.lp_monitor.active_lp_id()
    }

    pub fn last_look_model(&self) -> &LastLookModel {
        &self.last_look_model
    }

    pub fn fill_prob_model(&self) -> &FillProbabilityModel {
        &self.fill_prob_model
    }

    pub fn slippage_model(&self) -> &SlippageModel {
        &self.slippage_model
    }

    pub fn lp_monitor(&self) -> &LpRiskMonitor {
        &self.lp_monitor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request() -> ExecutionRequest {
        ExecutionRequest {
            direction: Direction::Buy,
            lots: 100_000,
            strategy_id: StrategyId::A,
            current_mid_price: 1.1000,
            volatility: 0.1,
            expected_profit: 0.0002,
            symbol: "EUR/USD".to_string(),
            timestamp_ns: 1_000_000_000_000,
            time_urgent: false,
        }
    }

    fn make_gateway() -> ExecutionGateway {
        ExecutionGateway::new(ExecutionGatewayConfig::default())
    }

    fn seeded_rng() -> rand::rngs::SmallRng {
        rand::rngs::SmallRng::from_seed([42u8; 32])
    }

    // --- Evaluate ---
    #[test]
    fn evaluate_returns_order() {
        let mut gw = make_gateway();
        let req = make_request();
        let eval = gw.evaluate(&req).unwrap();
        assert!(!eval.lp_id.is_empty());
        assert!(eval.effective_fill_probability > 0.0);
        assert!(eval.effective_fill_probability <= 1.0);
    }

    #[test]
    fn evaluate_zero_lot_error() {
        let mut gw = make_gateway();
        let mut req = make_request();
        req.lots = 0;
        assert!(matches!(gw.evaluate(&req), Err(ExecutionError::ZeroLot)));
    }

    #[test]
    fn evaluate_buy_positive_slippage() {
        let mut gw = make_gateway();
        let req = make_request();
        let eval = gw.evaluate(&req).unwrap();
        assert!(eval.expected_slippage > 0.0);
    }

    #[test]
    fn evaluate_sell_lower_slippage() {
        let mut gw = make_gateway();
        let mut buy_req = make_request();
        let buy_eval = gw.evaluate(&buy_req).unwrap();
        buy_req.direction = Direction::Sell;
        let sell_eval = gw.evaluate(&buy_req).unwrap();
        assert!(sell_eval.expected_slippage < buy_eval.expected_slippage);
    }

    #[test]
    fn evaluate_urgent_is_market() {
        let mut gw = make_gateway();
        let mut req = make_request();
        req.time_urgent = true;
        let eval = gw.evaluate(&req).unwrap();
        assert_eq!(eval.order_type, OtcOrderType::Market);
    }

    // --- Simulate Execution ---
    #[test]
    fn simulate_produces_result() {
        let mut gw = make_gateway();
        let req = make_request();
        let mut rng = seeded_rng();
        let result = gw.simulate_execution(&req, &mut rng).unwrap();
        assert!(!result.order_id.is_empty());
        assert!(!result.lp_id.is_empty());
    }

    #[test]
    fn simulate_fill_updates_models() {
        let mut gw = make_gateway();
        let req = make_request();
        // Use a biased RNG that always produces high values → always fill
        let mut rng = seeded_rng();
        let mut fills = 0;
        let mut rejections = 0;
        for _ in 0..20 {
            let result = gw.simulate_execution(&req, &mut rng).unwrap();
            if result.filled {
                fills += 1;
            } else {
                rejections += 1;
            }
        }
        assert!(fills + rejections == 20);
        let lp_state = gw.lp_monitor().get_lp_state("LP_PRIMARY").unwrap();
        assert!(lp_state.total_requests > 0);
    }

    #[test]
    fn simulate_result_fields() {
        let mut gw = make_gateway();
        let req = make_request();
        let mut rng = seeded_rng();
        let result = gw.simulate_execution(&req, &mut rng).unwrap();
        assert_eq!(result.requested_size, 100_000.0);
        assert!(result.latency_ms > 0.0);
        if result.filled {
            assert_eq!(result.fill_status, FillOutcome::Filled);
            assert!(result.fill_size > 0.0);
        } else {
            assert_eq!(result.fill_status, FillOutcome::Rejected);
            assert!(result.reject_reason.is_some());
        }
    }

    #[test]
    fn simulate_rejection_updates_last_look() {
        let mut gw = make_gateway();
        let req = make_request();
        let mut rng = seeded_rng();
        // Simulate many orders to get some rejections
        for _ in 0..100 {
            let _ = gw.simulate_execution(&req, &mut rng);
        }
        let params = gw.last_look_model().get_lp_params("LP_PRIMARY");
        assert!(params.is_some());
    }

    // --- Process Fill/Rejection ---
    #[test]
    fn process_fill_updates_all_models() {
        let mut gw = make_gateway();
        gw.process_fill("LP_PRIMARY", 0.0001);
        let params = gw.last_look_model().get_lp_params("LP_PRIMARY").unwrap();
        assert!(params.alpha > 2.0); // prior(2) + 1
        let slip = gw.slippage_model().get_lp_stats("LP_PRIMARY").unwrap();
        assert_eq!(slip.count, 1);
        let state = gw.lp_monitor().get_lp_state("LP_PRIMARY").unwrap();
        assert_eq!(state.total_fills, 1);
    }

    #[test]
    fn process_rejection_updates_all_models() {
        let mut gw = make_gateway();
        gw.process_rejection("LP_PRIMARY", "LAST_LOOK");
        let params = gw.last_look_model().get_lp_params("LP_PRIMARY").unwrap();
        assert!(params.beta > 1.0); // prior(1) + 1
        let state = gw.lp_monitor().get_lp_state("LP_PRIMARY").unwrap();
        assert_eq!(state.total_rejections, 1);
    }

    // --- LP Switch ---
    #[test]
    fn lp_switch_after_many_rejections() {
        let mut gw = make_gateway();
        for _ in 0..50 {
            gw.process_rejection("LP_PRIMARY", "LAST_LOOK");
        }
        let signal = gw.check_lp_switch();
        assert!(signal.is_some());
        assert_eq!(signal.unwrap().to_lp_id, "LP_BACKUP");
        assert_eq!(gw.active_lp_id(), "LP_BACKUP");
    }

    #[test]
    fn no_switch_single_lp() {
        let mut gw = ExecutionGateway::new(ExecutionGatewayConfig {
            known_lps: vec!["ONLY_LP".to_string()],
            ..Default::default()
        });
        for _ in 0..100 {
            gw.process_rejection("ONLY_LP", "LAST_LOOK");
        }
        assert!(gw.check_lp_switch().is_none());
    }

    // --- Proto Conversion ---
    #[test]
    fn build_proto_filled() {
        let gw = make_gateway();
        let req = make_request();
        let result = ExecutionResult {
            order_id: "TEST-1".to_string(),
            filled: true,
            fill_price: 1.1001,
            fill_size: 100_000.0,
            slippage: 0.0001,
            fill_probability: 0.95,
            effective_fill_probability: 0.88,
            last_look_rejection_prob: 0.05,
            price_improvement: -0.0001,
            order_type: OtcOrderType::Market,
            fill_status: FillOutcome::Filled,
            reject_reason: None,
            lp_id: "LP_PRIMARY".to_string(),
            requested_price: 1.1000,
            requested_size: 100_000.0,
            latency_ms: 1.0,
            evaluation: OrderEvaluation {
                order_type: OtcOrderType::Market,
                fill_probability: 0.95,
                effective_fill_probability: 0.88,
                expected_slippage: 0.0001,
                last_look_fill_prob: 0.95,
                lp_id: "LP_PRIMARY".to_string(),
                limit_price_distance: 0.0,
            },
        };
        let event = gw.build_execution_event(&req, &result);
        assert_eq!(event.order_id, "TEST-1");
        assert_eq!(event.fill_status, proto::FillStatus::Filled as i32);
        assert_eq!(event.order_type, proto::OrderType::OrderMarket as i32);
        assert_eq!(event.reject_reason, proto::RejectReason::Unknown as i32);
        assert_eq!(
            event.header.as_ref().unwrap().tier,
            proto::EventTier::Tier1Critical as i32
        );
        assert_eq!(
            event.header.as_ref().unwrap().stream_id,
            proto::StreamId::Execution as i32
        );
    }

    #[test]
    fn build_proto_rejected_last_look() {
        let gw = make_gateway();
        let req = make_request();
        let result = ExecutionResult {
            order_id: "TEST-2".to_string(),
            filled: false,
            fill_price: 0.0,
            fill_size: 0.0,
            slippage: 0.0,
            fill_probability: 0.0,
            effective_fill_probability: 0.0,
            last_look_rejection_prob: 0.3,
            price_improvement: 0.0,
            order_type: OtcOrderType::Limit,
            fill_status: FillOutcome::Rejected,
            reject_reason: Some("LAST_LOOK".to_string()),
            lp_id: "LP_PRIMARY".to_string(),
            requested_price: 1.0999,
            requested_size: 100_000.0,
            latency_ms: 0.5,
            evaluation: OrderEvaluation {
                order_type: OtcOrderType::Limit,
                fill_probability: 0.5,
                effective_fill_probability: 0.4,
                expected_slippage: 0.0,
                last_look_fill_prob: 0.7,
                lp_id: "LP_PRIMARY".to_string(),
                limit_price_distance: 0.0001,
            },
        };
        let event = gw.build_execution_event(&req, &result);
        assert_eq!(event.fill_status, proto::FillStatus::Rejected as i32);
        assert_eq!(event.order_type, proto::OrderType::OrderLimit as i32);
        assert_eq!(event.reject_reason, proto::RejectReason::LastLook as i32);
        assert_eq!(event.reject_message, "LAST_LOOK");
    }

    #[test]
    fn build_proto_roundtrip() {
        use prost::Message;
        let gw = make_gateway();
        let req = make_request();
        let result = ExecutionResult {
            order_id: "ROUND-1".to_string(),
            filled: true,
            fill_price: 1.10005,
            fill_size: 50_000.0,
            slippage: 0.00005,
            fill_probability: 0.9,
            effective_fill_probability: 0.85,
            last_look_rejection_prob: 0.08,
            price_improvement: -0.00005,
            order_type: OtcOrderType::Market,
            fill_status: FillOutcome::Filled,
            reject_reason: None,
            lp_id: "LP_PRIMARY".to_string(),
            requested_price: 1.1000,
            requested_size: 50_000.0,
            latency_ms: 1.5,
            evaluation: OrderEvaluation {
                order_type: OtcOrderType::Market,
                fill_probability: 0.9,
                effective_fill_probability: 0.85,
                expected_slippage: 0.00005,
                last_look_fill_prob: 0.92,
                lp_id: "LP_PRIMARY".to_string(),
                limit_price_distance: 0.0,
            },
        };
        let event = gw.build_execution_event(&req, &result);
        let bytes = event.encode_to_vec();
        let decoded = proto::ExecutionEventPayload::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.order_id, "ROUND-1");
        assert!((decoded.fill_price - 1.10005).abs() < 1e-10);
        assert!((decoded.slippage - 0.00005).abs() < 1e-10);
    }

    // --- Accessors ---
    #[test]
    fn active_lp() {
        let gw = make_gateway();
        assert_eq!(gw.active_lp_id(), "LP_PRIMARY");
    }

    #[test]
    fn order_ids_unique() {
        let mut gw = make_gateway();
        let req = make_request();
        let mut rng = seeded_rng();
        let r1 = gw.simulate_execution(&req, &mut rng).unwrap();
        let r2 = gw.simulate_execution(&req, &mut rng).unwrap();
        assert_ne!(r1.order_id, r2.order_id);
    }
}
