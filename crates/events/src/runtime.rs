use fx_core::types::{Direction, EventTier, StrategyId, StreamId};
use prost::Message;
use uuid::Uuid;

use crate::event::GenericEvent;
use crate::header::EventHeader;
use crate::proto;

fn stream_index(stream_id: StreamId) -> usize {
    match stream_id {
        StreamId::Market => 0,
        StreamId::Strategy => 1,
        StreamId::Execution => 2,
        StreamId::State => 3,
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSequencer {
    schema_version: u32,
    counters: [u64; 4],
}

impl RuntimeSequencer {
    pub fn new(schema_version: u32) -> Self {
        Self {
            schema_version,
            counters: [0; 4],
        }
    }

    pub fn next_header(
        &mut self,
        stream_id: StreamId,
        timestamp_ns: u64,
        tier: EventTier,
        parent_event_id: Option<Uuid>,
    ) -> EventHeader {
        let idx = stream_index(stream_id);
        self.counters[idx] = self.counters[idx].saturating_add(1);

        EventHeader {
            event_id: Uuid::now_v7(),
            parent_event_id,
            stream_id,
            sequence_id: self.counters[idx],
            timestamp_ns,
            schema_version: self.schema_version,
            tier,
        }
    }
}

pub fn proto_header(header: &EventHeader) -> proto::EventHeader {
    proto::EventHeader {
        event_id: header.event_id.to_string(),
        parent_event_id: header.parent_event_id.map(|id| id.to_string()),
        stream_id: match header.stream_id {
            StreamId::Market => proto::StreamId::Market as i32,
            StreamId::Strategy => proto::StreamId::Strategy as i32,
            StreamId::Execution => proto::StreamId::Execution as i32,
            StreamId::State => proto::StreamId::State as i32,
        },
        sequence_id: header.sequence_id,
        timestamp_ns: header.timestamp_ns,
        schema_version: header.schema_version,
        tier: match header.tier {
            EventTier::Tier1Critical => proto::EventTier::Tier1Critical as i32,
            EventTier::Tier2Derived => proto::EventTier::Tier2Derived as i32,
            EventTier::Tier3Raw => proto::EventTier::Tier3Raw as i32,
        },
    }
}

pub fn proto_strategy_id(strategy_id: StrategyId) -> i32 {
    match strategy_id {
        StrategyId::A => proto::StrategyId::StrategyA as i32,
        StrategyId::B => proto::StrategyId::StrategyB as i32,
        StrategyId::C => proto::StrategyId::StrategyC as i32,
    }
}

pub fn parse_strategy_id(value: &str) -> Option<StrategyId> {
    match value {
        "A" | "STRATEGY_A" | "StrategyA" => Some(StrategyId::A),
        "B" | "STRATEGY_B" | "StrategyB" => Some(StrategyId::B),
        "C" | "STRATEGY_C" | "StrategyC" => Some(StrategyId::C),
        _ => None,
    }
}

pub fn skip_reason_code(reason: &str) -> i32 {
    match reason {
        "staleness_rejected" | "STALENESS_THRESHOLD" => {
            proto::SkipReason::StalenessThreshold as i32
        }
        "risk_limit_rejected" | "daily_realized_halt" | "weekly_halt" | "monthly_halt" => {
            proto::SkipReason::RiskLimitActive as i32
        }
        "gap_detected" => proto::SkipReason::GapDetected as i32,
        "unknown_regime" => proto::SkipReason::UnknownRegime as i32,
        "strategy_culled" => proto::SkipReason::StrategyHalted as i32,
        "hold_degeneration" => proto::SkipReason::HoldDegradation as i32,
        "mtm_q_threshold_rejected" | "trigger conditions not met" => {
            proto::SkipReason::InsufficientEdge as i32
        }
        "global_position_rejected" | "already_in_position" => {
            proto::SkipReason::PositionLimit as i32
        }
        "kill_switch_masked" => proto::SkipReason::StrategyHalted as i32,
        "zero_effective_lot" => proto::SkipReason::StalenessThreshold as i32,
        _ => proto::SkipReason::Unspecified as i32,
    }
}

#[derive(Debug, Clone, Default)]
pub struct DecisionEventContext {
    pub feature_vector: Vec<f64>,
    pub q_buy: f64,
    pub q_sell: f64,
    pub q_hold: f64,
    pub q_selected: f64,
    pub posterior_mean: f64,
    pub posterior_std: f64,
    pub sampled_q: f64,
    pub position_size: f64,
    pub entry_price: f64,
    pub pnl_unrealized: f64,
    pub holding_time_ms: f64,
    pub staleness_ms: f64,
    pub lot_multiplier: f64,
    pub daily_pnl: f64,
    pub regime_posterior: Vec<f64>,
    pub regime_entropy: f64,
    pub q_tilde_final_values: Vec<f64>,
    pub q_point_selected: f64,
    pub q_tilde_selected: f64,
    pub sigma_model: f64,
    pub sigma_execution: f64,
    pub sigma_latency: f64,
    pub sigma_non_model: f64,
    pub dynamic_k: f64,
    pub position_before: f64,
    pub position_after: f64,
    pub position_max_limit: f64,
    pub velocity_limit: f64,
    pub dynamic_cost: f64,
    pub latency_penalty: f64,
}

pub fn build_decision_event(
    header: EventHeader,
    strategy_id: StrategyId,
    action: proto::ActionType,
    lots: u64,
    context: DecisionEventContext,
    skip_reason: Option<&str>,
) -> GenericEvent {
    let payload = proto::DecisionEventPayload {
        header: Some(proto_header(&header)),
        strategy_id: proto_strategy_id(strategy_id),
        action: action as i32,
        lots,
        decision: if matches!(action, proto::ActionType::Buy | proto::ActionType::Sell) {
            proto::TradeDecision::Execute as i32
        } else {
            proto::TradeDecision::Skip as i32
        },
        feature_vector: context.feature_vector,
        q_buy: context.q_buy,
        q_sell: context.q_sell,
        q_hold: context.q_hold,
        q_selected: context.q_selected,
        posterior_mean: context.posterior_mean,
        posterior_std: context.posterior_std,
        sampled_q: context.sampled_q,
        position_size: context.position_size,
        entry_price: context.entry_price,
        pnl_unrealized: context.pnl_unrealized,
        holding_time_ms: context.holding_time_ms,
        staleness_ms: context.staleness_ms,
        lot_multiplier: context.lot_multiplier,
        daily_pnl: context.daily_pnl,
        regime_posterior: context.regime_posterior,
        regime_entropy: context.regime_entropy,
        skip_reason: skip_reason.unwrap_or_default().to_string(),
        q_tilde_final_values: context.q_tilde_final_values,
        q_point_selected: context.q_point_selected,
        q_tilde_selected: context.q_tilde_selected,
        sigma_model: context.sigma_model,
        sigma_execution: context.sigma_execution,
        sigma_latency: context.sigma_latency,
        sigma_non_model: context.sigma_non_model,
        dynamic_k: context.dynamic_k,
        position_before: context.position_before,
        position_after: context.position_after,
        position_max_limit: context.position_max_limit,
        velocity_limit: context.velocity_limit,
        dynamic_cost: context.dynamic_cost,
        latency_penalty: context.latency_penalty,
    };

    GenericEvent::new(header, payload.encode_to_vec())
}

pub fn build_trade_skip_event(
    header: EventHeader,
    strategy_id: StrategyId,
    reason: &str,
    q_selected: f64,
    q_point_selected: f64,
    staleness_ms: f64,
    regime_entropy: f64,
    lot_multiplier: f64,
) -> GenericEvent {
    let payload = proto::TradeSkipEvent {
        header: Some(proto_header(&header)),
        strategy_id: format!("{strategy_id:?}"),
        reason: skip_reason_code(reason),
        description: reason.to_string(),
        q_selected,
        staleness_ms,
        regime_entropy,
        q_point_selected,
        lot_multiplier,
    };

    GenericEvent::new(header, payload.encode_to_vec())
}

pub fn action_type(direction: Option<Direction>) -> proto::ActionType {
    match direction {
        Some(Direction::Buy) => proto::ActionType::Buy,
        Some(Direction::Sell) => proto::ActionType::Sell,
        None => proto::ActionType::Hold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequencer_increments_per_stream() {
        let mut seq = RuntimeSequencer::new(7);
        let h1 = seq.next_header(StreamId::Strategy, 10, EventTier::Tier2Derived, None);
        let h2 = seq.next_header(StreamId::Strategy, 20, EventTier::Tier2Derived, None);
        let h3 = seq.next_header(StreamId::Execution, 30, EventTier::Tier1Critical, None);

        assert_eq!(h1.sequence_id, 1);
        assert_eq!(h2.sequence_id, 2);
        assert_eq!(h3.sequence_id, 1);
        assert_eq!(h1.schema_version, 7);
    }

    #[test]
    fn build_decision_event_round_trips_new_fields() {
        let mut seq = RuntimeSequencer::new(1);
        let header = seq.next_header(StreamId::Strategy, 123, EventTier::Tier2Derived, None);
        let event = build_decision_event(
            header,
            StrategyId::A,
            proto::ActionType::Buy,
            1000,
            DecisionEventContext {
                q_tilde_final_values: vec![1.0, 2.0, 3.0],
                q_point_selected: 4.0,
                q_tilde_selected: 5.0,
                sigma_non_model: 6.0,
                position_before: 7.0,
                position_after: 8.0,
                ..DecisionEventContext::default()
            },
            None,
        );

        let decoded = proto::DecisionEventPayload::decode(event.payload.as_slice()).unwrap();
        assert_eq!(decoded.header.unwrap().sequence_id, 1);
        assert_eq!(decoded.q_tilde_final_values, vec![1.0, 2.0, 3.0]);
        assert_eq!(decoded.q_point_selected, 4.0);
        assert_eq!(decoded.q_tilde_selected, 5.0);
        assert_eq!(decoded.sigma_non_model, 6.0);
        assert_eq!(decoded.position_before, 7.0);
        assert_eq!(decoded.position_after, 8.0);
    }
}
