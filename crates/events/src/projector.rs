use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use fx_core::types::{EventTier, StrategyId, StreamId};
use prost::Message;

use crate::bus::{EventPublisher, PartitionedEventBus};
use crate::event::{Event, GenericEvent};
use crate::header::EventHeader;
use crate::proto;

#[derive(Debug, Clone)]
pub struct Position {
    pub strategy_id: StrategyId,
    pub size: f64,
    pub entry_price: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
    pub entry_timestamp_ns: u64,
}

impl Position {
    pub fn new(strategy_id: StrategyId) -> Self {
        Self {
            strategy_id,
            size: 0.0,
            entry_price: 0.0,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
            entry_timestamp_ns: 0,
        }
    }

    pub fn holding_time_ms(&self, now_ns: u64) -> u64 {
        if self.size.abs() < f64::EPSILON || self.entry_timestamp_ns == 0 {
            return 0;
        }
        (now_ns.saturating_sub(self.entry_timestamp_ns)) / 1_000_000
    }

    pub fn is_open(&self) -> bool {
        self.size.abs() > f64::EPSILON
    }
}

#[derive(Debug, Clone, Default)]
pub struct LimitStateData {
    pub daily_pnl_mtm: f64,
    pub daily_pnl_realized: f64,
    pub weekly_pnl: f64,
    pub monthly_pnl: f64,
    pub daily_mtm_limited: bool,
    pub daily_realized_halted: bool,
    pub weekly_halted: bool,
    pub monthly_halted: bool,
}

#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub positions: HashMap<StrategyId, Position>,
    pub global_position: f64,
    pub global_position_limit: f64,
    pub total_unrealized_pnl: f64,
    pub total_realized_pnl: f64,
    pub limit_state: LimitStateData,
    pub state_version: u64,
    pub staleness_ms: u64,
    pub state_hash: String,
    pub lot_multiplier: f64,
    pub last_market_data_ns: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectorError {
    #[error("event decode error: {0}")]
    Decode(String),
    #[error("invalid event for stream: {0}")]
    InvalidEvent(String),
    #[error("publish error: {0}")]
    Publish(String),
}

pub struct StateProjector {
    snapshot: StateSnapshot,
    state_publisher: EventPublisher,
    schema_version: u32,
    last_active_strategy: StrategyId,
}

fn proto_strategy_to_core(proto_id: i32) -> Option<StrategyId> {
    match proto_id {
        1 => Some(StrategyId::A),
        2 => Some(StrategyId::B),
        3 => Some(StrategyId::C),
        _ => None,
    }
}

impl StateProjector {
    pub fn new(bus: &PartitionedEventBus, global_position_limit: f64, schema_version: u32) -> Self {
        let state_publisher = bus.publisher(StreamId::State);

        let mut positions = HashMap::new();
        positions.insert(StrategyId::A, Position::new(StrategyId::A));
        positions.insert(StrategyId::B, Position::new(StrategyId::B));
        positions.insert(StrategyId::C, Position::new(StrategyId::C));

        let mut snapshot = StateSnapshot {
            positions,
            global_position: 0.0,
            global_position_limit,
            total_unrealized_pnl: 0.0,
            total_realized_pnl: 0.0,
            limit_state: LimitStateData::default(),
            state_version: 0,
            staleness_ms: 0,
            state_hash: String::new(),
            lot_multiplier: 1.0,
            last_market_data_ns: 0,
        };
        snapshot.state_hash = compute_state_hash(&snapshot);

        Self {
            snapshot,
            state_publisher,
            schema_version,
            last_active_strategy: StrategyId::A,
        }
    }

    pub fn process_event(&mut self, event: &GenericEvent) -> Result<(), ProjectorError> {
        match event.header.stream_id {
            StreamId::Market => self.process_market_event(event),
            StreamId::Strategy => self.process_strategy_event(event),
            StreamId::Execution => self.process_execution_event(event, self.last_active_strategy),
            StreamId::State => Ok(()),
        }
    }

    pub fn process_execution_for_strategy(
        &mut self,
        event: &GenericEvent,
        strategy_id: StrategyId,
    ) -> Result<(), ProjectorError> {
        assert!(
            event.header.stream_id == StreamId::Execution,
            "process_execution_for_strategy requires Execution stream event"
        );
        self.process_execution_event(event, strategy_id)
    }

    fn process_market_event(&mut self, event: &GenericEvent) -> Result<(), ProjectorError> {
        let market = proto::MarketEventPayload::decode(event.payload_bytes())
            .map_err(|e| ProjectorError::Decode(e.to_string()))?;

        self.snapshot.last_market_data_ns = market.timestamp_ns;
        self.snapshot.staleness_ms = 0;
        self.snapshot.lot_multiplier = 1.0;

        self.recompute_unrealized_pnl(market.bid, market.ask);
        self.increment_version();

        Ok(())
    }

    fn process_strategy_event(&mut self, event: &GenericEvent) -> Result<(), ProjectorError> {
        let decision = proto::DecisionEventPayload::decode(event.payload_bytes())
            .map_err(|e| ProjectorError::Decode(e.to_string()))?;

        if let Some(sid) = proto_strategy_to_core(decision.strategy_id) {
            self.last_active_strategy = sid;
        }

        self.recompute_staleness(event.header.timestamp_ns);

        Ok(())
    }

    fn process_execution_event(
        &mut self,
        event: &GenericEvent,
        strategy_id: StrategyId,
    ) -> Result<(), ProjectorError> {
        let execution = proto::ExecutionEventPayload::decode(event.payload_bytes())
            .map_err(|e| ProjectorError::Decode(e.to_string()))?;

        let fill_status = execution.fill_status;
        if fill_status == proto::FillStatus::Filled as i32
            || fill_status == proto::FillStatus::PartialFill as i32
        {
            let timestamp_ns = execution
                .header
                .as_ref()
                .map(|h| h.timestamp_ns)
                .unwrap_or(event.header.timestamp_ns);

            self.update_position_from_fill(
                execution.fill_size,
                execution.fill_price,
                timestamp_ns,
                strategy_id,
            );
            self.recompute_aggregates();
        }

        self.recompute_staleness(event.header.timestamp_ns);
        self.increment_version();

        Ok(())
    }

    fn update_position_from_fill(
        &mut self,
        fill_size: f64,
        fill_price: f64,
        now_ns: u64,
        strategy_id: StrategyId,
    ) {
        if fill_size.abs() < f64::EPSILON {
            return;
        }

        let position = self
            .snapshot
            .positions
            .entry(strategy_id)
            .or_insert_with(|| Position::new(strategy_id));

        if position.size.abs() < f64::EPSILON {
            position.size = fill_size;
            position.entry_price = fill_price;
            position.entry_timestamp_ns = now_ns;
        } else if (position.size > 0.0 && fill_size > 0.0)
            || (position.size < 0.0 && fill_size < 0.0)
        {
            let total_cost =
                position.entry_price * position.size.abs() + fill_price * fill_size.abs();
            let total_size = position.size.abs() + fill_size.abs();
            assert!(total_size > 0.0);
            position.entry_price = total_cost / total_size;
            position.size += fill_size;
        } else {
            let close_size = fill_size.abs().min(position.size.abs());
            let direction = position.size.signum();
            let pnl_per_unit = (fill_price - position.entry_price) * direction;
            position.realized_pnl += pnl_per_unit * close_size;
            position.size += fill_size;

            if position.size.abs() < f64::EPSILON {
                position.size = 0.0;
                position.entry_price = 0.0;
                position.unrealized_pnl = 0.0;
                position.entry_timestamp_ns = 0;
            }
        }
    }

    fn recompute_staleness(&mut self, event_timestamp_ns: u64) {
        if self.snapshot.last_market_data_ns == 0 {
            self.snapshot.staleness_ms = u64::MAX;
            self.snapshot.lot_multiplier = 0.0;
            return;
        }

        if event_timestamp_ns > self.snapshot.last_market_data_ns {
            self.snapshot.staleness_ms =
                (event_timestamp_ns - self.snapshot.last_market_data_ns) / 1_000_000;
        } else {
            self.snapshot.staleness_ms = 0;
        }

        let threshold_ms = 5000u64;
        if self.snapshot.staleness_ms >= threshold_ms {
            self.snapshot.lot_multiplier = 0.0;
        } else {
            let ratio = self.snapshot.staleness_ms as f64 / threshold_ms as f64;
            self.snapshot.lot_multiplier = (1.0 - ratio * ratio).max(0.0);
        }
    }

    fn recompute_unrealized_pnl(&mut self, bid: f64, ask: f64) {
        let mid = (bid + ask) / 2.0;
        let mut total_unrealized = 0.0;

        for position in self.snapshot.positions.values_mut() {
            if position.is_open() {
                position.unrealized_pnl = (mid - position.entry_price) * position.size;
                total_unrealized += position.unrealized_pnl;
            } else {
                position.unrealized_pnl = 0.0;
            }
        }

        self.snapshot.total_unrealized_pnl = total_unrealized;
    }

    fn recompute_aggregates(&mut self) {
        let mut global_pos = 0.0;
        let mut total_realized = 0.0;

        for position in self.snapshot.positions.values() {
            global_pos += position.size;
            total_realized += position.realized_pnl;
        }

        self.snapshot.global_position = global_pos;
        self.snapshot.total_realized_pnl = total_realized;
    }

    fn increment_version(&mut self) {
        self.snapshot.state_version += 1;
        self.snapshot.state_hash = compute_state_hash(&self.snapshot);
    }

    pub fn update_limit_state(&mut self, limit_state: LimitStateData) {
        self.snapshot.limit_state = limit_state;
        self.increment_version();
    }

    pub fn set_lot_multiplier(&mut self, multiplier: f64) {
        self.snapshot.lot_multiplier = multiplier.clamp(0.0, 1.0);
    }

    pub fn snapshot(&self) -> &StateSnapshot {
        &self.snapshot
    }

    pub fn snapshot_mut(&mut self) -> &mut StateSnapshot {
        &mut self.snapshot
    }

    pub fn state_version(&self) -> u64 {
        self.snapshot.state_version
    }

    pub fn verify_integrity(&self) -> bool {
        let expected = compute_state_hash(&self.snapshot);
        expected == self.snapshot.state_hash
    }

    pub async fn publish_snapshot(&mut self) -> Result<(), ProjectorError> {
        let payload = self.build_proto_payload();
        let bytes = payload.encode_to_vec();

        let header = EventHeader::new(StreamId::State, 0, EventTier::Tier1Critical);
        let header = EventHeader {
            timestamp_ns: header.timestamp_ns,
            schema_version: self.schema_version,
            ..header
        };

        self.state_publisher
            .publish(header, bytes)
            .await
            .map_err(|e| ProjectorError::Publish(e.to_string()))
    }

    pub fn build_proto_payload(&self) -> proto::StateSnapshotPayload {
        let positions: Vec<proto::PositionState> = self
            .snapshot
            .positions
            .values()
            .map(|p| proto::PositionState {
                strategy_id: format!("{:?}", p.strategy_id),
                size: p.size,
                entry_price: p.entry_price,
                unrealized_pnl: p.unrealized_pnl,
                realized_pnl: p.realized_pnl,
                holding_time_ms: p.holding_time_ms(self.snapshot.last_market_data_ns),
            })
            .collect();

        proto::StateSnapshotPayload {
            header: None,
            positions,
            global_position: self.snapshot.global_position,
            global_position_limit: self.snapshot.global_position_limit,
            total_unrealized_pnl: self.snapshot.total_unrealized_pnl,
            total_realized_pnl: self.snapshot.total_realized_pnl,
            limit_state: Some(proto::LimitState {
                daily_pnl_mtm: self.snapshot.limit_state.daily_pnl_mtm,
                daily_pnl_realized: self.snapshot.limit_state.daily_pnl_realized,
                weekly_pnl: self.snapshot.limit_state.weekly_pnl,
                monthly_pnl: self.snapshot.limit_state.monthly_pnl,
                daily_mtm_limited: self.snapshot.limit_state.daily_mtm_limited,
                daily_realized_halted: self.snapshot.limit_state.daily_realized_halted,
                weekly_halted: self.snapshot.limit_state.weekly_halted,
                monthly_halted: self.snapshot.limit_state.monthly_halted,
            }),
            state_version: self.snapshot.state_version,
            staleness_ms: self.snapshot.staleness_ms,
            state_hash: self.snapshot.state_hash.clone(),
            lot_multiplier: self.snapshot.lot_multiplier,
            last_market_data_ns: self.snapshot.last_market_data_ns,
        }
    }
}

fn compute_state_hash(snapshot: &StateSnapshot) -> String {
    let mut hasher = DefaultHasher::new();

    let mut sorted_positions: Vec<_> = snapshot.positions.iter().collect();
    sorted_positions.sort_by_key(|(k, _)| format!("{:?}", k));

    for (strategy_id, pos) in &sorted_positions {
        format!("{:?}", strategy_id).hash(&mut hasher);
        pos.size.to_bits().hash(&mut hasher);
        pos.entry_price.to_bits().hash(&mut hasher);
        pos.realized_pnl.to_bits().hash(&mut hasher);
        pos.unrealized_pnl.to_bits().hash(&mut hasher);
        pos.entry_timestamp_ns.hash(&mut hasher);
    }

    snapshot.global_position.to_bits().hash(&mut hasher);
    snapshot.global_position_limit.to_bits().hash(&mut hasher);
    snapshot.total_unrealized_pnl.to_bits().hash(&mut hasher);
    snapshot.total_realized_pnl.to_bits().hash(&mut hasher);
    snapshot.lot_multiplier.to_bits().hash(&mut hasher);
    snapshot.last_market_data_ns.hash(&mut hasher);
    snapshot.staleness_ms.hash(&mut hasher);

    snapshot
        .limit_state
        .daily_pnl_mtm
        .to_bits()
        .hash(&mut hasher);
    snapshot
        .limit_state
        .daily_pnl_realized
        .to_bits()
        .hash(&mut hasher);
    snapshot.limit_state.weekly_pnl.to_bits().hash(&mut hasher);
    snapshot.limit_state.monthly_pnl.to_bits().hash(&mut hasher);
    snapshot.limit_state.daily_mtm_limited.hash(&mut hasher);
    snapshot.limit_state.daily_realized_halted.hash(&mut hasher);
    snapshot.limit_state.weekly_halted.hash(&mut hasher);
    snapshot.limit_state.monthly_halted.hash(&mut hasher);

    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use fx_core::types::EventTier;
    use prost::Message;
    use uuid::Uuid;

    use super::*;
    use crate::bus::PartitionedEventBus;
    use crate::proto;

    fn make_header(stream_id: StreamId, timestamp_ns: u64) -> EventHeader {
        EventHeader {
            event_id: Uuid::now_v7(),
            parent_event_id: None,
            stream_id,
            sequence_id: 0,
            timestamp_ns,
            schema_version: 1,
            tier: match stream_id {
                StreamId::Market => EventTier::Tier3Raw,
                StreamId::Strategy => EventTier::Tier2Derived,
                StreamId::Execution => EventTier::Tier1Critical,
                StreamId::State => EventTier::Tier1Critical,
            },
        }
    }

    const NS_BASE: u64 = 1_000_000_000_000_000; // base timestamp in ns

    fn make_market_event(timestamp_offset_ms: u64, bid: f64, ask: f64) -> GenericEvent {
        let timestamp_ns = NS_BASE + timestamp_offset_ms * 1_000_000;
        let payload = proto::MarketEventPayload {
            header: None,
            symbol: "USD/JPY".to_string(),
            bid,
            ask,
            bid_size: 1_000_000.0,
            ask_size: 1_000_000.0,
            timestamp_ns,
            bid_levels: vec![],
            ask_levels: vec![],
            latency_ms: 0.5,
        }
        .encode_to_vec();

        GenericEvent::new(make_header(StreamId::Market, timestamp_ns), payload)
    }

    fn make_decision_event(
        timestamp_offset_ms: u64,
        strategy_proto_id: i32,
        lot_multiplier: f64,
    ) -> GenericEvent {
        let timestamp_ns = NS_BASE + timestamp_offset_ms * 1_000_000;
        let payload = proto::DecisionEventPayload {
            header: None,
            strategy_id: strategy_proto_id,
            action: proto::ActionType::Buy as i32,
            lots: 1000,
            decision: proto::TradeDecision::Execute as i32,
            feature_vector: vec![],
            q_buy: 0.001,
            q_sell: -0.0005,
            q_hold: 0.0,
            q_selected: 0.001,
            posterior_mean: 0.0008,
            posterior_std: 0.0002,
            sampled_q: 0.001,
            position_size: 0.0,
            entry_price: 0.0,
            pnl_unrealized: 0.0,
            holding_time_ms: 0.0,
            staleness_ms: 0.0,
            lot_multiplier,
            daily_pnl: 0.0,
            regime_posterior: vec![],
            regime_entropy: 0.0,
            skip_reason: String::new(),
        }
        .encode_to_vec();

        GenericEvent::new(make_header(StreamId::Strategy, timestamp_ns), payload)
    }

    fn make_execution_event(
        timestamp_offset_ms: u64,
        fill_size: f64,
        fill_price: f64,
        fill_status: i32,
    ) -> GenericEvent {
        let timestamp_ns = NS_BASE + timestamp_offset_ms * 1_000_000;
        let payload = proto::ExecutionEventPayload {
            header: None,
            order_id: "ord-001".to_string(),
            symbol: "USD/JPY".to_string(),
            order_type: proto::OrderType::OrderMarket as i32,
            fill_status,
            fill_price,
            fill_size,
            slippage: 0.00001,
            requested_price: fill_price,
            requested_size: fill_size.abs(),
            fill_probability: 0.95,
            effective_fill_probability: 0.90,
            price_improvement: 0.0,
            last_look_rejection_prob: 0.05,
            lp_id: "LP1".to_string(),
            latency_ms: 1.0,
            reject_reason: proto::RejectReason::Unspecified as i32,
            reject_message: String::new(),
        }
        .encode_to_vec();

        GenericEvent::new(make_header(StreamId::Execution, timestamp_ns), payload)
    }

    #[test]
    fn test_new_projector_initial_state() {
        let bus = PartitionedEventBus::new();
        let projector = StateProjector::new(&bus, 10.0, 1);

        let snap = projector.snapshot();
        assert_eq!(snap.state_version, 0);
        assert_eq!(snap.global_position, 0.0);
        assert_eq!(snap.global_position_limit, 10.0);
        assert_eq!(snap.total_unrealized_pnl, 0.0);
        assert_eq!(snap.total_realized_pnl, 0.0);
        assert_eq!(snap.last_market_data_ns, 0);
        assert_eq!(snap.lot_multiplier, 1.0);
        assert!(snap.positions.contains_key(&StrategyId::A));
        assert!(snap.positions.contains_key(&StrategyId::B));
        assert!(snap.positions.contains_key(&StrategyId::C));
        assert!(!snap.state_hash.is_empty());
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_process_market_event_updates_timestamps() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let event = make_market_event(0, 110.0, 110.005);
        projector.process_event(&event).unwrap();

        let snap = projector.snapshot();
        assert_eq!(snap.last_market_data_ns, NS_BASE);
        assert_eq!(snap.staleness_ms, 0);
        assert_eq!(snap.lot_multiplier, 1.0);
        assert_eq!(snap.state_version, 1);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_process_market_event_recomputes_unrealized_pnl() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let exec = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();

        let market = make_market_event(1, 110.005, 110.010);
        projector.process_event(&market).unwrap();

        let snap = projector.snapshot();
        let pos = snap.positions.get(&StrategyId::A).unwrap();
        assert!(pos.is_open());
        assert_eq!(pos.size, 1000.0);
        let mid = (110.005 + 110.010) / 2.0;
        let expected_pnl = (mid - 110.0) * 1000.0;
        assert!((pos.unrealized_pnl - expected_pnl).abs() < 1e-10);
        assert!((snap.total_unrealized_pnl - expected_pnl).abs() < 1e-10);
    }

    #[test]
    fn test_process_execution_opens_position() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let exec = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();

        let snap = projector.snapshot();
        let pos = snap.positions.get(&StrategyId::A).unwrap();
        assert_eq!(pos.size, 1000.0);
        assert_eq!(pos.entry_price, 110.0);
        assert!(pos.is_open());
        assert_eq!(snap.global_position, 1000.0);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_process_execution_closes_position_with_pnl() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let open_exec = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&open_exec, StrategyId::A)
            .unwrap();

        let close_exec =
            make_execution_event(1, -1000.0, 110.005, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&close_exec, StrategyId::A)
            .unwrap();

        let snap = projector.snapshot();
        let pos = snap.positions.get(&StrategyId::A).unwrap();
        assert!(!pos.is_open());
        assert_eq!(pos.size, 0.0);
        assert_eq!(pos.entry_price, 0.0);
        let expected_pnl = (110.005 - 110.0) * 1000.0;
        assert!((pos.realized_pnl - expected_pnl).abs() < 1e-10);
        assert!((snap.total_realized_pnl - expected_pnl).abs() < 1e-10);
        assert_eq!(snap.global_position, 0.0);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_process_execution_partial_close() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let open = make_execution_event(0, 2000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&open, StrategyId::B)
            .unwrap();

        let partial_close =
            make_execution_event(1, -500.0, 110.01, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&partial_close, StrategyId::B)
            .unwrap();

        let snap = projector.snapshot();
        let pos = snap.positions.get(&StrategyId::B).unwrap();
        assert_eq!(pos.size, 1500.0);
        let expected_pnl = (110.01 - 110.0) * 500.0;
        assert!((pos.realized_pnl - expected_pnl).abs() < 1e-10);
        assert_eq!(snap.global_position, 1500.0);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_process_execution_add_to_position() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let first = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&first, StrategyId::C)
            .unwrap();

        let second = make_execution_event(1, 500.0, 110.01, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&second, StrategyId::C)
            .unwrap();

        let snap = projector.snapshot();
        let pos = snap.positions.get(&StrategyId::C).unwrap();
        assert_eq!(pos.size, 1500.0);
        let expected_entry = (110.0 * 1000.0 + 110.01 * 500.0) / 1500.0;
        assert!((pos.entry_price - expected_entry).abs() < 1e-10);
        assert_eq!(snap.global_position, 1500.0);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_process_execution_rejected_no_position_change() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let open = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&open, StrategyId::A)
            .unwrap();

        let rejected =
            make_execution_event(1, -1000.0, 110.005, proto::FillStatus::Rejected as i32);
        projector
            .process_execution_for_strategy(&rejected, StrategyId::A)
            .unwrap();

        let pos = projector.snapshot().positions.get(&StrategyId::A).unwrap();
        assert_eq!(pos.size, 1000.0);
        assert_eq!(pos.realized_pnl, 0.0);
    }

    #[test]
    fn test_process_execution_short_position() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let open = make_execution_event(0, -1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&open, StrategyId::A)
            .unwrap();

        let pos = projector.snapshot().positions.get(&StrategyId::A).unwrap();
        assert_eq!(pos.size, -1000.0);
        assert_eq!(pos.entry_price, 110.0);

        let close = make_execution_event(1, 1000.0, 109.99, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&close, StrategyId::A)
            .unwrap();

        let pos = projector.snapshot().positions.get(&StrategyId::A).unwrap();
        assert!(!pos.is_open());
        let expected_pnl = (109.99 - 110.0) * (-1.0) * 1000.0;
        assert!((pos.realized_pnl - expected_pnl).abs() < 1e-10);
    }

    #[test]
    fn test_state_version_increments() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        assert_eq!(projector.state_version(), 0);

        let market = make_market_event(0, 110.0, 110.005);
        projector.process_event(&market).unwrap();
        assert_eq!(projector.state_version(), 1);

        let market2 = make_market_event(1, 110.001, 110.006);
        projector.process_event(&market2).unwrap();
        assert_eq!(projector.state_version(), 2);

        let exec = make_execution_event(2, 500.0, 110.002, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();
        assert_eq!(projector.state_version(), 3);
    }

    #[test]
    fn test_state_hash_integrity() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        assert!(projector.verify_integrity());

        let market = make_market_event(0, 110.0, 110.005);
        projector.process_event(&market).unwrap();
        assert!(projector.verify_integrity());

        let exec = make_execution_event(1, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_state_hash_changes_on_modification() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let hash0 = projector.snapshot().state_hash.clone();

        let market = make_market_event(0, 110.0, 110.005);
        projector.process_event(&market).unwrap();

        let hash1 = projector.snapshot().state_hash.clone();
        assert_ne!(hash0, hash1);

        let market2 = make_market_event(1, 110.001, 110.006);
        projector.process_event(&market2).unwrap();

        let hash2 = projector.snapshot().state_hash.clone();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_staleness_calculation() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let market = make_market_event(0, 110.0, 110.005);
        projector.process_event(&market).unwrap();
        assert_eq!(projector.snapshot().staleness_ms, 0);

        let decision = make_decision_event(3, 1, 1.0);
        projector.process_event(&decision).unwrap();
        assert_eq!(projector.snapshot().staleness_ms, 3);

        let exec = make_execution_event(8, 100.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();
        assert_eq!(projector.snapshot().staleness_ms, 8);
    }

    #[test]
    fn test_staleness_no_market_data() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let decision = make_decision_event(0, 1, 1.0);
        projector.process_event(&decision).unwrap();

        assert_eq!(projector.snapshot().staleness_ms, u64::MAX);
        assert_eq!(projector.snapshot().lot_multiplier, 0.0);
    }

    #[test]
    fn test_lot_multiplier_from_staleness() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let market = make_market_event(0, 110.0, 110.005);
        projector.process_event(&market).unwrap();
        assert!((projector.snapshot().lot_multiplier - 1.0).abs() < 1e-10);

        let decision = make_decision_event(2500, 1, 0.5);
        projector.process_event(&decision).unwrap();
        let expected = 1.0 - (2500.0_f64 / 5000.0_f64).powi(2);
        assert!(
            (projector.snapshot().lot_multiplier - expected).abs() < 1e-10,
            "expected {}, got {}",
            expected,
            projector.snapshot().lot_multiplier
        );

        let decision2 = make_decision_event(6000, 1, 0.8);
        projector.process_event(&decision2).unwrap();
        assert_eq!(projector.snapshot().lot_multiplier, 0.0);
    }

    #[test]
    fn test_decision_updates_last_active_strategy() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let decision_b = make_decision_event(0, 2, 1.0);
        projector.process_event(&decision_b).unwrap();

        let exec = make_execution_event(1, 500.0, 110.0, proto::FillStatus::Filled as i32);
        projector.process_event(&exec).unwrap();

        let pos_b = projector.snapshot().positions.get(&StrategyId::B).unwrap();
        assert_eq!(pos_b.size, 500.0);

        let pos_a = projector.snapshot().positions.get(&StrategyId::A).unwrap();
        assert!(!pos_a.is_open());
    }

    #[tokio::test]
    async fn test_publish_snapshot() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);
        let mut subscriber = bus.subscriber(&[StreamId::State]);

        let market = make_market_event(0, 110.0, 110.005);
        projector.process_event(&market).unwrap();

        projector.publish_snapshot().await.unwrap();

        let event = subscriber.recv().await.unwrap();
        assert_eq!(event.header.stream_id, StreamId::State);
        assert_eq!(event.header.tier, EventTier::Tier1Critical);

        let decoded = proto::StateSnapshotPayload::decode(event.payload_bytes()).unwrap();
        assert_eq!(decoded.state_version, 1);
        assert_eq!(decoded.last_market_data_ns, NS_BASE);
        assert_eq!(decoded.global_position_limit, 10.0);
        assert!(!decoded.state_hash.is_empty());
        assert_eq!(decoded.positions.len(), 3);
    }

    #[tokio::test]
    async fn test_publish_snapshot_with_positions() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);
        let mut subscriber = bus.subscriber(&[StreamId::State]);

        let exec = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();

        let market = make_market_event(5, 110.005, 110.010);
        projector.process_event(&market).unwrap();

        projector.publish_snapshot().await.unwrap();

        let event = subscriber.recv().await.unwrap();
        let decoded = proto::StateSnapshotPayload::decode(event.payload_bytes()).unwrap();

        assert_eq!(decoded.global_position, 1000.0);
        let mid = (110.005 + 110.010) / 2.0;
        let expected_pnl = (mid - 110.0) * 1000.0;
        assert!((decoded.total_unrealized_pnl - expected_pnl).abs() < 1e-8);

        let pos_a = decoded
            .positions
            .iter()
            .find(|p| p.strategy_id == "A")
            .unwrap();
        assert_eq!(pos_a.size, 1000.0);
        assert_eq!(pos_a.entry_price, 110.0);
        assert!((pos_a.unrealized_pnl - expected_pnl).abs() < 1e-8);
        assert!(pos_a.holding_time_ms > 0);
    }

    #[test]
    fn test_event_sequence_restoration() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let events = vec![
            make_market_event(0, 110.0, 110.005),
            make_execution_event(1, 1000.0, 110.0, proto::FillStatus::Filled as i32),
            make_market_event(2, 110.005, 110.010),
            make_market_event(3, 110.003, 110.008),
            make_execution_event(4, -1000.0, 110.004, proto::FillStatus::Filled as i32),
        ];

        let strategy_id = StrategyId::A;
        for event in &events {
            if event.header.stream_id == StreamId::Execution {
                projector
                    .process_execution_for_strategy(event, strategy_id)
                    .unwrap();
            } else {
                projector.process_event(event).unwrap();
            }
        }

        let snap = projector.snapshot();
        assert_eq!(snap.state_version, 5);
        assert!(!snap.positions.get(&StrategyId::A).unwrap().is_open());
        let expected_pnl = (110.004 - 110.0) * 1000.0;
        assert!((snap.total_realized_pnl - expected_pnl).abs() < 1e-10);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_limit_state_update() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let limit_state = LimitStateData {
            daily_pnl_mtm: -50.0,
            daily_pnl_realized: -20.0,
            weekly_pnl: -100.0,
            monthly_pnl: -200.0,
            daily_mtm_limited: true,
            daily_realized_halted: false,
            weekly_halted: false,
            monthly_halted: false,
        };

        projector.update_limit_state(limit_state);
        assert_eq!(projector.state_version(), 1);

        let snap = projector.snapshot();
        assert!((snap.limit_state.daily_pnl_mtm - (-50.0)).abs() < 1e-10);
        assert!(snap.limit_state.daily_mtm_limited);
        assert!(!snap.limit_state.daily_realized_halted);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_build_proto_payload_roundtrip() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let exec = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::B)
            .unwrap();

        let payload = projector.build_proto_payload();
        assert_eq!(payload.state_version, 1);
        assert_eq!(payload.global_position, 1000.0);

        let bytes = payload.encode_to_vec();
        let decoded = proto::StateSnapshotPayload::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.state_version, payload.state_version);
        assert_eq!(decoded.global_position, payload.global_position);
        assert_eq!(decoded.state_hash, payload.state_hash);
    }

    #[test]
    fn test_set_lot_multiplier_clamped() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        projector.set_lot_multiplier(1.5);
        assert!((projector.snapshot().lot_multiplier - 1.0).abs() < 1e-10);

        projector.set_lot_multiplier(-0.5);
        assert!((projector.snapshot().lot_multiplier - 0.0).abs() < 1e-10);

        projector.set_lot_multiplier(0.5);
        assert!((projector.snapshot().lot_multiplier - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_holding_time_ms() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let exec = make_execution_event(0, 1000.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();

        let market = make_market_event(5, 110.0, 110.005);
        projector.process_event(&market).unwrap();

        let pos = projector.snapshot().positions.get(&StrategyId::A).unwrap();
        assert_eq!(pos.holding_time_ms(NS_BASE + 5_000_000), 5);
        assert_eq!(pos.holding_time_ms(NS_BASE + 10_000_000), 10);

        let payload = projector.build_proto_payload();
        let pos_proto = payload
            .positions
            .iter()
            .find(|p| p.strategy_id == "A")
            .unwrap();
        assert_eq!(pos_proto.holding_time_ms, 5);
    }

    #[test]
    fn test_multiple_strategies_independent() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let exec_a = make_execution_event(0, 500.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec_a, StrategyId::A)
            .unwrap();

        let exec_b = make_execution_event(1, -300.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec_b, StrategyId::B)
            .unwrap();

        let snap = projector.snapshot();
        assert_eq!(snap.positions.get(&StrategyId::A).unwrap().size, 500.0);
        assert_eq!(snap.positions.get(&StrategyId::B).unwrap().size, -300.0);
        assert!(!snap.positions.get(&StrategyId::C).unwrap().is_open());
        assert_eq!(snap.global_position, 200.0);
        assert!(projector.verify_integrity());
    }

    #[test]
    fn test_zero_fill_size_ignored() {
        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, 10.0, 1);

        let exec = make_execution_event(0, 0.0, 110.0, proto::FillStatus::Filled as i32);
        projector
            .process_execution_for_strategy(&exec, StrategyId::A)
            .unwrap();

        let pos = projector.snapshot().positions.get(&StrategyId::A).unwrap();
        assert!(!pos.is_open());
    }

    #[test]
    fn test_state_hash_deterministic() {
        let bus = PartitionedEventBus::new();
        let mut p1 = StateProjector::new(&bus, 10.0, 1);
        let mut p2 = StateProjector::new(&bus, 10.0, 1);

        let market = make_market_event(0, 110.0, 110.005);
        p1.process_event(&market).unwrap();
        p2.process_event(&market).unwrap();

        assert_eq!(
            p1.snapshot().state_hash,
            p2.snapshot().state_hash,
            "same events should produce same hash"
        );
    }
}
