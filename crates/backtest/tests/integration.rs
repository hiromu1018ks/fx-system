//! End-to-end integration tests for the FX AI trading system.
//!
//! Tests the full pipeline:
//!   Market Gateway → Feature Extraction → Strategy → Risk Barrier → Execution
//!
//! plus event replay reproducibility, hierarchical limits, OTC execution model,
//! and multi-strategy global position constraints.

use std::collections::HashMap;

use fx_core::types::{Direction, EventTier, StrategyId, StreamId};
use fx_events::bus::PartitionedEventBus;
use fx_events::event::{Event, GenericEvent};
use fx_events::header::EventHeader;
use fx_events::projector::{LimitStateData, StateProjector};
use fx_events::proto;
use fx_events::store::{EventStore, Tier3Store};
use fx_execution::gateway::{ExecutionGateway, ExecutionGatewayConfig, ExecutionRequest};
use fx_risk::barrier::{BarrierStatus, DynamicRiskBarrier, DynamicRiskBarrierConfig};
use fx_risk::global_position::{GlobalPositionChecker, GlobalPositionConfig};
use fx_risk::kill_switch::{KillSwitch, KillSwitchConfig};
use fx_risk::lifecycle::{EpisodeSummary, LifecycleConfig, LifecycleManager};
use fx_risk::limits::{CloseReason, HierarchicalRiskLimiter, RiskError, RiskLimitsConfig};
use fx_strategy::bayesian_lr::QAction;
use fx_strategy::extractor::{FeatureExtractor, FeatureExtractorConfig};
use fx_strategy::features::FeatureVector;
use fx_strategy::mc_eval::{McEvaluator, RewardConfig, TerminalReason};
use fx_strategy::policy::Action;
use fx_strategy::strategy_a::{StrategyA, StrategyAConfig};
use fx_strategy::strategy_b::{StrategyB, StrategyBConfig};
use fx_strategy::strategy_c::{StrategyC, StrategyCConfig};
use prost::Message as _;
use rand::prelude::*;
use rand::rngs::SmallRng;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const NS_BASE: u64 = 1_000_000_000_000_000;

fn make_market_event(
    timestamp_ns: u64,
    symbol: &str,
    bid: f64,
    ask: f64,
    bid_size: f64,
    ask_size: f64,
) -> GenericEvent {
    let payload = proto::MarketEventPayload {
        header: None,
        symbol: symbol.to_string(),
        bid,
        ask,
        bid_size,
        ask_size,
        timestamp_ns,
        bid_levels: vec![],
        ask_levels: vec![],
        latency_ms: 0.0,
    }
    .encode_to_vec();

    let header = EventHeader {
        timestamp_ns,
        stream_id: StreamId::Market,
        sequence_id: 0,
        tier: EventTier::Tier3Raw,
        ..EventHeader::new(StreamId::Market, 0, EventTier::Tier3Raw)
    };

    GenericEvent::new(header, payload)
}

/// Build an ExecutionEvent GenericEvent. `signed_fill_size` is signed:
/// positive = Buy (adds to position), negative = Sell (subtracts from position).
fn make_execution_event(
    timestamp_ns: u64,
    order_id: &str,
    fill_price: f64,
    signed_fill_size: f64,
    slippage: f64,
    symbol: &str,
) -> GenericEvent {
    let payload = proto::ExecutionEventPayload {
        header: None,
        order_id: order_id.to_string(),
        symbol: symbol.to_string(),
        order_type: proto::OrderType::OrderMarket as i32,
        fill_status: proto::FillStatus::Filled as i32,
        fill_price,
        fill_size: signed_fill_size,
        slippage,
        requested_price: fill_price - slippage,
        requested_size: signed_fill_size.abs(),
        fill_probability: 0.95,
        effective_fill_probability: 0.90,
        price_improvement: 0.0,
        last_look_rejection_prob: 0.05,
        lp_id: "LP_PRIMARY".to_string(),
        latency_ms: 1.0,
        reject_reason: proto::RejectReason::Unknown as i32,
        reject_message: String::new(),
    }
    .encode_to_vec();

    let header = EventHeader {
        timestamp_ns,
        stream_id: StreamId::Execution,
        sequence_id: 0,
        tier: EventTier::Tier1Critical,
        ..EventHeader::new(StreamId::Execution, 0, EventTier::Tier1Critical)
    };

    GenericEvent::new(header, payload)
}

fn make_execution_reject_event(timestamp_ns: u64, order_id: &str, symbol: &str) -> GenericEvent {
    let payload = proto::ExecutionEventPayload {
        header: None,
        order_id: order_id.to_string(),
        symbol: symbol.to_string(),
        order_type: proto::OrderType::OrderMarket as i32,
        fill_status: proto::FillStatus::Rejected as i32,
        fill_price: 0.0,
        fill_size: 0.0,
        slippage: 0.0,
        requested_price: 110.0,
        requested_size: 100_000.0,
        fill_probability: 0.0,
        effective_fill_probability: 0.0,
        price_improvement: 0.0,
        last_look_rejection_prob: 1.0,
        lp_id: "LP_PRIMARY".to_string(),
        latency_ms: 0.5,
        reject_reason: proto::RejectReason::LastLook as i32,
        reject_message: "LAST_LOOK".to_string(),
    }
    .encode_to_vec();

    let header = EventHeader {
        timestamp_ns,
        stream_id: StreamId::Execution,
        sequence_id: 0,
        tier: EventTier::Tier1Critical,
        ..EventHeader::new(StreamId::Execution, 0, EventTier::Tier1Critical)
    };

    GenericEvent::new(header, payload)
}

fn generate_ticking_market_events(
    start_ns: u64,
    count: usize,
    interval_ms: u64,
    base_price: f64,
    spread_pips: f64,
) -> Vec<GenericEvent> {
    let mut events = Vec::with_capacity(count);
    for i in 0..count {
        let ts = start_ns + (i as u64) * interval_ms * 1_000_000;
        let noise = ((i % 7) as f64 - 3.0) * 0.001;
        let mid = base_price + noise;
        let half_spread = spread_pips * 0.5 / 10000.0;
        events.push(make_market_event(
            ts,
            "USD/JPY",
            mid - half_spread,
            mid + half_spread,
            1e6,
            1e6,
        ));
    }
    events
}

fn make_liquididity_shock_features() -> FeatureVector {
    FeatureVector {
        spread: 5.0,
        spread_zscore: 5.0,
        obi: 0.8,
        delta_obi: 0.3,
        depth_change_rate: -0.5,
        queue_position: 0.1,
        realized_volatility: 0.15,
        volatility_ratio: 5.0,
        volatility_decay_rate: -0.01,
        session_tokyo: 0.0,
        session_london: 1.0,
        session_ny: 0.0,
        session_sydney: 0.0,
        time_since_open_ms: 3_600_000.0,
        time_since_last_spike_ms: 500_000.0,
        holding_time_ms: 0.0,
        position_size: 0.0,
        position_direction: 0.0,
        entry_price: 0.0,
        pnl_unrealized: 0.0,
        trade_intensity: 10.0,
        signed_volume: 50000.0,
        recent_fill_rate: 0.9,
        recent_slippage: 0.0001,
        self_impact: 0.00001,
        time_decay: 1.0,
        dynamic_cost: 0.0002,
        p_revert: 0.8,
        p_continue: 0.2,
        p_trend: 0.1,
        spread_z_x_vol: 5.0 * 0.15,
        obi_x_session: 0.8,
        depth_drop_x_vol_spike: -0.5 * 5.0,
        position_size_x_vol: 0.0,
    }
}

#[allow(dead_code)]
fn make_volatility_decay_features() -> FeatureVector {
    FeatureVector {
        spread: 2.0,
        spread_zscore: 1.5,
        obi: 0.3,
        delta_obi: 0.1,
        depth_change_rate: -0.1,
        queue_position: 0.5,
        realized_volatility: 0.08,
        volatility_ratio: 3.5,
        volatility_decay_rate: -0.005,
        session_tokyo: 0.0,
        session_london: 0.0,
        session_ny: 1.0,
        session_sydney: 0.0,
        time_since_open_ms: 7_200_000.0,
        time_since_last_spike_ms: 2_000_000.0,
        holding_time_ms: 0.0,
        position_size: 0.0,
        position_direction: 0.0,
        entry_price: 0.0,
        pnl_unrealized: 0.0,
        trade_intensity: 5.0,
        signed_volume: 20000.0,
        recent_fill_rate: 0.85,
        recent_slippage: 0.0001,
        self_impact: 0.00001,
        time_decay: 1.0,
        dynamic_cost: 0.0002,
        p_revert: 0.3,
        p_continue: 0.7,
        p_trend: 0.4,
        spread_z_x_vol: 1.5 * 0.08,
        obi_x_session: 0.3,
        depth_drop_x_vol_spike: -0.1 * 3.5,
        position_size_x_vol: 0.0,
    }
}

#[allow(dead_code)]
fn make_session_bias_features() -> FeatureVector {
    FeatureVector {
        spread: 1.5,
        spread_zscore: 1.0,
        obi: 0.4,
        delta_obi: 0.05,
        depth_change_rate: -0.05,
        queue_position: 0.6,
        realized_volatility: 0.05,
        volatility_ratio: 1.5,
        volatility_decay_rate: -0.001,
        session_tokyo: 1.0,
        session_london: 0.0,
        session_ny: 0.0,
        session_sydney: 0.0,
        time_since_open_ms: 600_000.0,
        time_since_last_spike_ms: 5_000_000.0,
        holding_time_ms: 0.0,
        position_size: 0.0,
        position_direction: 0.0,
        entry_price: 0.0,
        pnl_unrealized: 0.0,
        trade_intensity: 3.0,
        signed_volume: 10000.0,
        recent_fill_rate: 0.88,
        recent_slippage: 0.0001,
        self_impact: 0.00001,
        time_decay: 1.0,
        dynamic_cost: 0.0002,
        p_revert: 0.2,
        p_continue: 0.5,
        p_trend: 0.6,
        spread_z_x_vol: 1.0 * 0.05,
        obi_x_session: 0.4,
        depth_drop_x_vol_spike: -0.05 * 1.5,
        position_size_x_vol: 0.0,
    }
}

/// Convert a projector snapshot's global_position from raw size to lot units
/// for use with GlobalPositionChecker (which expects lot-unit values).
fn snapshot_in_lot_units(
    snap: &fx_events::projector::StateSnapshot,
    lot_unit_size: f64,
) -> fx_events::projector::StateSnapshot {
    let mut converted = snap.clone();
    converted.global_position = snap.global_position / lot_unit_size;
    converted
}

// ---------------------------------------------------------------------------
// 1. End-to-End Trade Flow: Market → Feature → Strategy → Risk → Execution
// ---------------------------------------------------------------------------

#[test]
fn test_e2e_market_to_state_projection() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 50, 100, 110.0, 1.0);
    for event in &events {
        projector
            .process_event(event)
            .expect("process market event");
    }

    let snapshot = projector.snapshot();
    assert_eq!(snapshot.last_market_data_ns, NS_BASE + 49 * 100 * 1_000_000);
    assert_eq!(snapshot.staleness_ms, 0);
    assert!(snapshot.lot_multiplier > 0.99);
    assert!(snapshot.state_version > 0);
    assert!(projector.verify_integrity());
}

#[test]
fn test_e2e_feature_extraction_after_market_events() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let mut extractor = FeatureExtractor::new(FeatureExtractorConfig::default());

    let events = generate_ticking_market_events(NS_BASE, 100, 100, 110.0, 1.0);
    for event in &events {
        projector
            .process_event(event)
            .expect("process market event");
        extractor.process_market_event(event);
    }

    let snapshot = projector.snapshot();
    let features = extractor.extract(
        &events[events.len() - 1],
        snapshot,
        StrategyId::A,
        NS_BASE + 99 * 100 * 1_000_000,
    );

    assert_eq!(features.flattened().len(), FeatureVector::DIM);
    assert!(features.spread > 0.0);
    assert!(features.realized_volatility >= 0.0);
}

#[test]
fn test_e2e_strategy_a_trigger_and_decision() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let mut strategy_a = StrategyA::new(StrategyAConfig::default());
    let mut rng = SmallRng::from_seed([42u8; 32]);

    let events = generate_ticking_market_events(NS_BASE, 100, 100, 110.0, 1.0);
    for event in &events {
        projector
            .process_event(event)
            .expect("process market event");
    }

    let snapshot = projector.snapshot();
    let features = make_liquididity_shock_features();

    assert!(strategy_a.is_triggered(&features, 0.5));

    let now_ns = NS_BASE + 99 * 100 * 1_000_000;
    let decision = strategy_a.decide(&features, snapshot, 0.5, 1.0, now_ns, &mut rng);

    let _ = decision;
}

#[test]
fn test_e2e_full_pipeline_market_to_execution() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let mut extractor = FeatureExtractor::new(FeatureExtractorConfig::default());
    let mut strategy_a = StrategyA::new(StrategyAConfig::default());
    let mut execution_gateway = ExecutionGateway::new(ExecutionGatewayConfig {
        symbol: "USD/JPY".to_string(),
        ..Default::default()
    });
    let risk_barrier = DynamicRiskBarrier::new(DynamicRiskBarrierConfig::default());
    let limits_config = RiskLimitsConfig::default();
    let global_config = GlobalPositionConfig::default();
    let mut rng = SmallRng::from_seed([42u8; 32]);

    // Step 1: Feed market data
    let events = generate_ticking_market_events(NS_BASE, 100, 100, 110.0, 1.0);
    for event in &events {
        projector
            .process_event(event)
            .expect("process market event");
        extractor.process_market_event(event);
    }

    let snapshot = projector.snapshot();

    // Step 2: Risk pre-flight checks (all should pass)
    let barrier_result = risk_barrier.evaluate(snapshot.staleness_ms);
    assert!(barrier_result.allowed);

    let limit_result =
        HierarchicalRiskLimiter::validate_order(&limits_config, &snapshot.limit_state);
    assert!(limit_result.is_ok());

    // Step 3: Strategy decision with triggered features
    let features = make_liquididity_shock_features();
    let now_ns = NS_BASE + 99 * 100 * 1_000_000;
    let decision = strategy_a.decide(&features, snapshot, 0.5, 1.0, now_ns, &mut rng);
    let state_version_before = snapshot.state_version;

    // Step 4: If strategy wants to trade, execute
    let trade_executed = match &decision.action {
        Action::Buy(lots) | Action::Sell(lots) if *lots > 0 => {
            let direction = match &decision.action {
                Action::Buy(_) => Direction::Buy,
                Action::Sell(_) => Direction::Sell,
                Action::Hold => unreachable!(),
            };
            let lots_val = *lots;

            // Global position check — convert to lot units
            let snap_lots = snapshot_in_lot_units(snapshot, global_config.lot_unit_size);
            let mut all_q = HashMap::new();
            all_q.insert(StrategyId::A, decision.q_point);
            all_q.insert(StrategyId::B, 0.0);
            all_q.insert(StrategyId::C, 0.0);

            let pos_result = GlobalPositionChecker::validate_order(
                &global_config,
                &snap_lots,
                StrategyId::A,
                direction,
                lots_val as f64,
                decision.q_point,
                &all_q,
            );
            assert!(pos_result.is_ok());

            // Execution
            let last_event = &events[events.len() - 1];
            let market = proto::MarketEventPayload::decode(last_event.payload_bytes()).unwrap();
            let mid = (market.bid + market.ask) / 2.0;
            let vol = (market.ask - market.bid) / mid;

            let exec_request = ExecutionRequest {
                direction,
                lots: lots_val,
                strategy_id: StrategyId::A,
                current_mid_price: mid,
                volatility: vol,
                expected_profit: 0.0002,
                symbol: "USD/JPY".to_string(),
                timestamp_ns: now_ns,
                time_urgent: false,
            };

            let eval = execution_gateway.evaluate(&exec_request);
            assert!(eval.is_ok());
            let eval_result = eval.unwrap();
            assert!(!eval_result.lp_id.is_empty());
            assert!(eval_result.effective_fill_probability > 0.0);

            let sim_result = execution_gateway.simulate_execution(&exec_request, &mut rng);
            assert!(sim_result.is_ok());
            let result = sim_result.unwrap();

            let proto_event = execution_gateway.build_execution_event(&exec_request, &result);
            assert_eq!(proto_event.order_id, result.order_id);

            if result.filled {
                let signed_fill = match direction {
                    Direction::Buy => result.fill_size,
                    Direction::Sell => -result.fill_size,
                };
                let exec_event = make_execution_event(
                    now_ns,
                    &result.order_id,
                    result.fill_price,
                    signed_fill,
                    result.slippage,
                    "USD/JPY",
                );
                projector
                    .process_execution_for_strategy(&exec_event, StrategyId::A)
                    .expect("process execution");

                let updated = projector.snapshot();
                assert!(updated.positions[&StrategyId::A].is_open());
                assert!(updated.global_position.abs() > 0.0);
                assert!(updated.state_version > state_version_before);
            }
            true
        }
        _ => false,
    };

    let _ = trade_executed;
}

#[test]
fn test_e2e_pipeline_with_rejection() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let mut execution_gateway = ExecutionGateway::new(ExecutionGatewayConfig {
        symbol: "USD/JPY".to_string(),
        ..Default::default()
    });

    let events = generate_ticking_market_events(NS_BASE, 50, 100, 110.0, 1.0);
    for event in &events {
        projector
            .process_event(event)
            .expect("process market event");
    }

    // Open a position (Buy: positive fill_size)
    let exec_event = make_execution_event(
        NS_BASE + 50 * 100 * 1_000_000,
        "ORD-REJ-1",
        110.005,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&exec_event, StrategyId::A)
        .expect("open position");
    assert!(projector.snapshot().positions[&StrategyId::A].is_open());

    // Now process a rejection — position should remain unchanged
    let reject_event =
        make_execution_reject_event(NS_BASE + 51 * 100 * 1_000_000, "ORD-REJ-2", "USD/JPY");
    projector
        .process_execution_for_strategy(&reject_event, StrategyId::A)
        .expect("process rejection");

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::A].is_open());
    assert!((snap.positions[&StrategyId::A].size - 100_000.0).abs() < 1e-6);

    execution_gateway.process_rejection("LP_PRIMARY", "LAST_LOOK");
    let lp_state = execution_gateway.lp_monitor().get_lp_state("LP_PRIMARY");
    assert!(lp_state.is_some());
    assert_eq!(lp_state.unwrap().total_rejections, 1);
}

#[test]
fn test_e2e_position_lifecycle_open_close() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector
            .process_event(event)
            .expect("process market event");
    }

    // Open long position (Buy: positive fill_size)
    let open_event = make_execution_event(
        NS_BASE + 1_000_000_000,
        "ORD-1",
        110.005,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&open_event, StrategyId::A)
        .expect("open");

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::A].is_open());
    assert!((snap.positions[&StrategyId::A].size - 100_000.0).abs() < 1e-6);
    assert!((snap.global_position - 100_000.0).abs() < 1e-6);

    // Close position (Sell: negative fill_size)
    let close_event = make_execution_event(
        NS_BASE + 2_000_000_000,
        "ORD-2",
        110.010,
        -100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&close_event, StrategyId::A)
        .expect("close");

    let snap = projector.snapshot();
    assert!(!snap.positions[&StrategyId::A].is_open());
    assert!((snap.global_position).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 2. Event Replay Reproducibility
// ---------------------------------------------------------------------------

#[test]
fn test_event_replay_reproduces_state() {
    let bus1 = PartitionedEventBus::new();
    let bus2 = PartitionedEventBus::new();
    let mut projector1 = StateProjector::new(&bus1, 10.0, 1);
    let mut projector2 = StateProjector::new(&bus2, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 200, 100, 110.0, 1.0);

    for event in &events {
        projector1.process_event(event).expect("p1");
        projector2.process_event(event).expect("p2");
    }

    let snap1 = projector1.snapshot();
    let snap2 = projector2.snapshot();

    assert_eq!(snap1.state_version, snap2.state_version);
    assert_eq!(snap1.state_hash, snap2.state_hash);
    assert_eq!(snap1.last_market_data_ns, snap2.last_market_data_ns);
    assert_eq!(snap1.staleness_ms, snap2.staleness_ms);
    assert!((snap1.lot_multiplier - snap2.lot_multiplier).abs() < f64::EPSILON);
}

#[test]
fn test_event_replay_with_execution_reproducible() {
    let bus1 = PartitionedEventBus::new();
    let bus2 = PartitionedEventBus::new();
    let mut projector1 = StateProjector::new(&bus1, 10.0, 1);
    let mut projector2 = StateProjector::new(&bus2, 10.0, 1);

    let market_events = generate_ticking_market_events(NS_BASE, 50, 100, 110.0, 1.0);

    // Open a position (Buy: positive) and close it (Sell: negative)
    let open_event = make_execution_event(
        NS_BASE + 500_000_000,
        "ORD-1",
        110.005,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    let close_event = make_execution_event(
        NS_BASE + 5_000_000_000,
        "ORD-2",
        110.010,
        -100_000.0,
        0.0001,
        "USD/JPY",
    );

    let all_events: Vec<GenericEvent> = market_events
        .into_iter()
        .chain([open_event, close_event])
        .collect();

    for event in &all_events {
        if event.header.stream_id == StreamId::Execution {
            let _ = projector1.process_execution_for_strategy(event, StrategyId::A);
            let _ = projector2.process_execution_for_strategy(event, StrategyId::A);
        } else {
            let _ = projector1.process_event(event);
            let _ = projector2.process_event(event);
        }
    }

    let snap1 = projector1.snapshot();
    let snap2 = projector2.snapshot();

    assert_eq!(snap1.state_hash, snap2.state_hash);
    assert_eq!(snap1.total_realized_pnl, snap2.total_realized_pnl);
    assert!(!snap1.positions[&StrategyId::A].is_open());
    assert!(!snap2.positions[&StrategyId::A].is_open());
}

#[test]
fn test_event_store_replay_roundtrip() {
    let store = Tier3Store::new(Duration::from_secs(300));

    let events = generate_ticking_market_events(NS_BASE, 100, 100, 110.0, 1.0);
    for event in &events {
        store.store(event).expect("store");
    }

    let replayed = store.replay(StreamId::Market, 0).expect("replay");
    assert_eq!(replayed.len(), 100);

    for (orig, replayed) in events.iter().zip(replayed.iter()) {
        let orig_m = proto::MarketEventPayload::decode(orig.payload_bytes()).unwrap();
        let rep_m = proto::MarketEventPayload::decode(replayed.payload_bytes()).unwrap();
        assert!((orig_m.bid - rep_m.bid).abs() < 1e-10);
        assert!((orig_m.ask - rep_m.ask).abs() < 1e-10);
        assert_eq!(orig_m.timestamp_ns, rep_m.timestamp_ns);
    }
}

#[test]
fn test_backtest_engine_deterministic_replay() {
    use fx_backtest::engine::{generate_synthetic_ticks, BacktestConfig, BacktestEngine};

    let events = generate_synthetic_ticks(NS_BASE, 500, 50, 110.0, 0.005);

    let config1 = BacktestConfig {
        rng_seed: Some([99u8; 32]),
        ..Default::default()
    };
    let config2 = BacktestConfig {
        rng_seed: Some([99u8; 32]),
        ..Default::default()
    };

    let mut engine1 = BacktestEngine::new(config1);
    let result1 = engine1.run_from_events(&events);

    let mut engine2 = BacktestEngine::new(config2);
    let result2 = engine2.run_from_events(&events);

    assert_eq!(result1.total_ticks, result2.total_ticks);
    assert_eq!(result1.trades.len(), result2.trades.len());

    for (t1, t2) in result1.trades.iter().zip(result2.trades.iter()) {
        assert!((t1.fill_price - t2.fill_price).abs() < 1e-10);
        assert!((t1.slippage - t2.slippage).abs() < 1e-10);
        assert_eq!(t1.strategy_id, t2.strategy_id);
    }
}

#[test]
fn test_strategy_decision_deterministic_with_seed() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let events = generate_ticking_market_events(NS_BASE, 100, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }
    let snapshot = projector.snapshot();
    let features = make_liquididity_shock_features();

    let mut rng1 = SmallRng::from_seed([123u8; 32]);
    let mut rng2 = SmallRng::from_seed([123u8; 32]);

    let mut sa1 = StrategyA::new(StrategyAConfig::default());
    let mut sa2 = StrategyA::new(StrategyAConfig::default());

    let d1 = sa1.decide(
        &features,
        snapshot,
        0.5,
        1.0,
        NS_BASE + 99 * 100 * 1_000_000,
        &mut rng1,
    );
    let d2 = sa2.decide(
        &features,
        snapshot,
        0.5,
        1.0,
        NS_BASE + 99 * 100 * 1_000_000,
        &mut rng2,
    );

    assert_eq!(d1.action, d2.action);
}

// ---------------------------------------------------------------------------
// 3. Hierarchical Loss Limits Integration
// ---------------------------------------------------------------------------

#[test]
fn test_hierarchical_limits_block_daily_mtm() {
    let limits_config = RiskLimitsConfig::default();

    let limit_state = LimitStateData {
        daily_pnl_mtm: -600.0,
        ..Default::default()
    };

    let (result, close_reason) = HierarchicalRiskLimiter::evaluate(&limits_config, &limit_state);
    assert!(result.is_ok());
    assert!(close_reason.is_none());
    let r = result.unwrap();
    assert!(r.daily_mtm_limited);
    assert!((r.lot_multiplier - 0.25).abs() < f64::EPSILON);
    assert!((r.q_threshold - 0.01).abs() < f64::EPSILON);
}

#[test]
fn test_hierarchical_limits_daily_realized_halt() {
    let limits_config = RiskLimitsConfig::default();
    let limit_state = LimitStateData {
        daily_pnl_realized: -1100.0,
        ..Default::default()
    };

    let (result, close_reason) = HierarchicalRiskLimiter::evaluate(&limits_config, &limit_state);
    assert!(matches!(result, Err(RiskError::DailyRealizedLimit { .. })));
    assert_eq!(close_reason, Some(CloseReason::DailyRealizedHalt));
}

#[test]
fn test_hierarchical_limits_weekly_halt() {
    let limits_config = RiskLimitsConfig::default();
    let limit_state = LimitStateData {
        weekly_pnl: -3000.0,
        ..Default::default()
    };

    let (result, close_reason) = HierarchicalRiskLimiter::evaluate(&limits_config, &limit_state);
    assert!(matches!(result, Err(RiskError::WeeklyLimit { .. })));
    assert_eq!(close_reason, Some(CloseReason::WeeklyHalt));
}

#[test]
fn test_hierarchical_limits_monthly_halt() {
    let limits_config = RiskLimitsConfig::default();
    let limit_state = LimitStateData {
        monthly_pnl: -6000.0,
        ..Default::default()
    };

    let (result, close_reason) = HierarchicalRiskLimiter::evaluate(&limits_config, &limit_state);
    assert!(matches!(result, Err(RiskError::MonthlyLimit { .. })));
    assert_eq!(close_reason, Some(CloseReason::MonthlyHalt));
}

#[test]
fn test_hierarchical_limits_priority_order() {
    let limits_config = RiskLimitsConfig::default();

    // All breached — monthly should win
    let limit_state = LimitStateData {
        daily_pnl_mtm: -9999.0,
        daily_pnl_realized: -9999.0,
        weekly_pnl: -9999.0,
        monthly_pnl: -9999.0,
        ..Default::default()
    };
    let (result, close) = HierarchicalRiskLimiter::evaluate(&limits_config, &limit_state);
    assert!(matches!(result, Err(RiskError::MonthlyLimit { .. })));
    assert_eq!(close, Some(CloseReason::MonthlyHalt));

    // Weekly + daily breached — weekly wins
    let limit_state = LimitStateData {
        daily_pnl_realized: -9999.0,
        weekly_pnl: -9999.0,
        ..Default::default()
    };
    let (result, close) = HierarchicalRiskLimiter::evaluate(&limits_config, &limit_state);
    assert!(matches!(result, Err(RiskError::WeeklyLimit { .. })));
    assert_eq!(close, Some(CloseReason::WeeklyHalt));
}

#[test]
fn test_hierarchical_limits_integrated_with_projector() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let limits_config = RiskLimitsConfig::default();

    let events = generate_ticking_market_events(NS_BASE, 50, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    projector.update_limit_state(LimitStateData {
        daily_pnl_mtm: -600.0,
        daily_pnl_realized: 0.0,
        weekly_pnl: 0.0,
        monthly_pnl: 0.0,
        daily_mtm_limited: true,
        ..Default::default()
    });

    let snapshot = projector.snapshot();
    let (result, close) = HierarchicalRiskLimiter::evaluate(&limits_config, &snapshot.limit_state);
    assert!(result.is_ok());
    assert!(close.is_none());
    assert!(result.unwrap().daily_mtm_limited);
}

#[test]
fn test_halt_flag_prevents_trading() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    projector.update_limit_state(LimitStateData {
        daily_realized_halted: true,
        ..Default::default()
    });

    assert!(HierarchicalRiskLimiter::is_halted(
        &projector.snapshot().limit_state
    ));
}

// ---------------------------------------------------------------------------
// 4. OTC Execution Model Integration
// ---------------------------------------------------------------------------

#[test]
fn test_otc_execution_fill_and_rejection_flow() {
    let mut gateway = ExecutionGateway::new(ExecutionGatewayConfig {
        symbol: "USD/JPY".to_string(),
        ..Default::default()
    });
    let mut rng = SmallRng::from_seed([42u8; 32]);

    let request = ExecutionRequest {
        direction: Direction::Buy,
        lots: 100_000,
        strategy_id: StrategyId::A,
        current_mid_price: 110.0,
        volatility: 0.001,
        expected_profit: 0.0002,
        symbol: "USD/JPY".to_string(),
        timestamp_ns: NS_BASE,
        time_urgent: false,
    };

    let eval = gateway.evaluate(&request).expect("evaluate");
    assert!(!eval.lp_id.is_empty());
    assert!(eval.effective_fill_probability > 0.0);

    let mut fills = 0;
    for _ in 0..50 {
        let result = gateway
            .simulate_execution(&request, &mut rng)
            .expect("simulate");
        if result.filled {
            fills += 1;
            assert!(result.fill_price > 0.0);
            assert!(result.fill_size > 0.0);
            assert!(result.latency_ms > 0.0);
        }
    }

    assert!(fills > 0, "should have at least one fill");
    // Total fills + rejections should equal 50 across all LPs.
    // After LP switch, counts move to the new LP.
    let primary = gateway.lp_monitor().get_lp_state("LP_PRIMARY");
    let backup = gateway.lp_monitor().get_lp_state("LP_BACKUP");
    let total: u64 = [primary, backup]
        .iter()
        .filter_map(|s| s.as_ref())
        .map(|s| s.total_fills + s.total_rejections)
        .sum();
    assert_eq!(total, 50);
}

#[test]
fn test_otc_last_look_rejection_tracking() {
    let mut gateway = ExecutionGateway::new(ExecutionGatewayConfig::default());

    for _ in 0..50 {
        gateway.process_rejection("LP_PRIMARY", "LAST_LOOK");
    }

    let signal = gateway.check_lp_switch();
    assert!(signal.is_some());
    assert_eq!(gateway.active_lp_id(), "LP_BACKUP");

    assert!(gateway.is_recalibrating());
    assert!((gateway.recalibration_lot_multiplier() - 0.25).abs() < f64::EPSILON);
    assert!((gateway.recalibration_sigma_multiplier() - 2.0).abs() < f64::EPSILON);
}

#[test]
fn test_otc_execution_updates_projector() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    // Buy: positive fill_size
    let fill_event = make_execution_event(
        NS_BASE + 500_000_000,
        "OTC-1",
        110.005,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&fill_event, StrategyId::A)
        .expect("fill");

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::A].is_open());
    assert!((snap.positions[&StrategyId::A].size - 100_000.0).abs() < 1e-6);

    // Close: sell → negative fill_size
    let close_event = make_execution_event(
        NS_BASE + 5_000_000_000,
        "OTC-2",
        110.010,
        -100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&close_event, StrategyId::A)
        .expect("close");

    let snap = projector.snapshot();
    assert!(!snap.positions[&StrategyId::A].is_open());
}

#[test]
fn test_otc_slippage_model_tracks_lp_performance() {
    let mut gateway = ExecutionGateway::new(ExecutionGatewayConfig::default());

    let slippages = [0.0001, 0.0002, 0.00015, 0.0003, 0.00005];
    for &slip in &slippages {
        gateway.process_fill("LP_PRIMARY", slip);
    }

    let stats = gateway.slippage_model().get_lp_stats("LP_PRIMARY").unwrap();
    assert_eq!(stats.count, 5);
    let mean = stats.mean;
    assert!(mean > 0.0 && mean < 0.001);

    let params = gateway
        .last_look_model()
        .get_lp_params("LP_PRIMARY")
        .unwrap();
    assert!((params.alpha - 7.0).abs() < 1e-6);
}

#[test]
fn test_otc_recalibration_completion() {
    let mut gateway = ExecutionGateway::new(ExecutionGatewayConfig::default());

    // Trigger LP switch by many rejections
    for _ in 0..50 {
        gateway.process_rejection("LP_PRIMARY", "LAST_LOOK");
    }
    let _signal = gateway.check_lp_switch();
    assert!(gateway.is_recalibrating());

    // Feed observations for the new LP (LP_BACKUP) to build up statistics
    // Need min_recalibration_observations (30) + time past max duration
    let start_ts = NS_BASE;
    for i in 0..35 {
        let ts = start_ts + (i as u64) * 10_000_000;
        gateway.process_fill_with_prediction("LP_BACKUP", 0.0001, 0.0001, ts);
    }

    // Check completion with timestamp well past max_recalibration_duration (5 min)
    let completion_ns = start_ts + 301_000_000_000; // > 5 min after first observation
    let completed = gateway.check_recalibration(completion_ns);
    assert!(completed);
    assert!(!gateway.is_recalibrating());
}

// ---------------------------------------------------------------------------
// 5. Multi-Strategy Global Position Constraints
// ---------------------------------------------------------------------------

#[test]
fn test_multi_strategy_concurrent_positions() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let global_config = GlobalPositionConfig::default();

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    // Strategy A opens long (Buy: positive)
    let a_fill = make_execution_event(
        NS_BASE + 500_000_000,
        "A-1",
        110.005,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&a_fill, StrategyId::A)
        .expect("A open");

    // Strategy B opens long (Buy: positive)
    let b_fill = make_execution_event(
        NS_BASE + 600_000_000,
        "B-1",
        110.006,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&b_fill, StrategyId::B)
        .expect("B open");

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::A].is_open());
    assert!(snap.positions[&StrategyId::B].is_open());
    assert!((snap.global_position - 200_000.0).abs() < 1e-6);

    // Convert global_position to lot units for GlobalPositionChecker
    let snap_lots = snapshot_in_lot_units(snap, global_config.lot_unit_size);
    let mut all_q = HashMap::new();
    all_q.insert(StrategyId::A, 0.5);
    all_q.insert(StrategyId::B, 0.3);
    all_q.insert(StrategyId::C, 0.1);

    let result = GlobalPositionChecker::validate_order(
        &global_config,
        &snap_lots,
        StrategyId::C,
        Direction::Buy,
        100_000.0,
        0.1,
        &all_q,
    );
    assert!(result.is_ok());
}

#[test]
fn test_multi_strategy_global_limit_blocks_excess() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let global_config = GlobalPositionConfig::default();

    // Open large positions
    for i in 0..10 {
        let fill = make_execution_event(
            NS_BASE + (i as u64 + 1) * 100_000_000,
            &format!("ORD-{}", i),
            110.005,
            100_000.0,
            0.0001,
            "USD/JPY",
        );
        projector
            .process_execution_for_strategy(&fill, StrategyId::A)
            .expect("open");
    }

    let snap = projector.snapshot();
    assert!((snap.global_position - 1_000_000.0).abs() < 1e-6);

    // Convert to lot units: 10.0 lot units
    let snap_lots = snapshot_in_lot_units(snap, global_config.lot_unit_size);
    let all_q = HashMap::from([
        (StrategyId::A, 0.5),
        (StrategyId::B, 0.1),
        (StrategyId::C, 0.05),
    ]);

    // global_limit = 15 / 1.5 = 10.0. Current pos = 10.0. Buying 1 more → 11.0 > 10.0 → blocked
    let result = GlobalPositionChecker::validate_order(
        &global_config,
        &snap_lots,
        StrategyId::B,
        Direction::Buy,
        100_000.0,
        0.1,
        &all_q,
    );
    assert!(result.is_err());
}

#[test]
fn test_multi_strategy_priority_lot_reduction() {
    let global_config = GlobalPositionConfig::default();

    let snap = fx_events::projector::StateSnapshot {
        positions: HashMap::new(),
        global_position: 0.0,
        global_position_limit: 10.0,
        total_unrealized_pnl: 0.0,
        total_realized_pnl: 0.0,
        limit_state: LimitStateData::default(),
        state_version: 0,
        staleness_ms: 0,
        state_hash: String::new(),
        lot_multiplier: 1.0,
        last_market_data_ns: NS_BASE,
    };

    let all_q = HashMap::from([
        (StrategyId::A, 0.5),
        (StrategyId::B, 0.1),
        (StrategyId::C, 0.05),
    ]);

    let r_a = GlobalPositionChecker::validate_order(
        &global_config,
        &snap,
        StrategyId::A,
        Direction::Buy,
        100_000.0,
        0.5,
        &all_q,
    )
    .unwrap();
    assert_eq!(r_a.priority_rank, 0);
    assert!((r_a.effective_lot - 100_000.0).abs() < 1e-6);

    let r_b = GlobalPositionChecker::validate_order(
        &global_config,
        &snap,
        StrategyId::B,
        Direction::Buy,
        100_000.0,
        0.1,
        &all_q,
    )
    .unwrap();
    assert_eq!(r_b.priority_rank, 1);
    assert!((r_b.effective_lot - 50_000.0).abs() < 1e-6);

    let r_c = GlobalPositionChecker::validate_order(
        &global_config,
        &snap,
        StrategyId::C,
        Direction::Buy,
        100_000.0,
        0.05,
        &all_q,
    )
    .unwrap();
    assert_eq!(r_c.priority_rank, 2);
    assert!((r_c.effective_lot - 25_000.0).abs() < 1e-6);
}

#[test]
fn test_multi_strategy_opposing_positions_net_out() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    // Strategy A buys (positive fill_size)
    let a_fill = make_execution_event(
        NS_BASE + 500_000_000,
        "A-1",
        110.005,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&a_fill, StrategyId::A)
        .expect("A open");

    // Strategy B sells (negative fill_size)
    let b_fill = make_execution_event(
        NS_BASE + 600_000_000,
        "B-1",
        110.006,
        -100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&b_fill, StrategyId::B)
        .expect("B open");

    let snap = projector.snapshot();
    assert!((snap.positions[&StrategyId::A].size - 100_000.0).abs() < 1e-6);
    assert!((snap.positions[&StrategyId::B].size - (-100_000.0)).abs() < 1e-6);
    // Net position should be near zero (A: +100k, B: -100k)
    assert!((snap.global_position).abs() < 1e-6);
}

#[test]
fn test_multi_strategy_validate_full_integration() {
    let global_config = GlobalPositionConfig::default();
    let limits_config = RiskLimitsConfig::default();

    let snap = fx_events::projector::StateSnapshot {
        positions: HashMap::new(),
        global_position: 0.0,
        global_position_limit: 10.0,
        total_unrealized_pnl: 0.0,
        total_realized_pnl: 0.0,
        limit_state: LimitStateData::default(),
        state_version: 0,
        staleness_ms: 0,
        state_hash: String::new(),
        lot_multiplier: 1.0,
        last_market_data_ns: NS_BASE,
    };

    let all_q = HashMap::from([
        (StrategyId::A, 0.5),
        (StrategyId::B, 0.3),
        (StrategyId::C, 0.1),
    ]);

    // Normal case: both checks pass
    let result = GlobalPositionChecker::validate_full(
        &global_config,
        &limits_config,
        &snap,
        StrategyId::A,
        Direction::Buy,
        100_000.0,
        0.5,
        &all_q,
    );
    assert!(result.is_ok());

    // Global blocked
    let mut tight_snap = snap.clone();
    tight_snap.global_position = 20_000_000.0 / global_config.lot_unit_size; // in lot units
    let result = GlobalPositionChecker::validate_full(
        &global_config,
        &limits_config,
        &tight_snap,
        StrategyId::A,
        Direction::Buy,
        100_000.0,
        0.5,
        &all_q,
    );
    assert!(matches!(result, Err(RiskError::GlobalPositionConstraint)));

    // Limits blocked
    let mut limit_snap = snap.clone();
    limit_snap.limit_state.monthly_pnl = -9999.0;
    let result = GlobalPositionChecker::validate_full(
        &global_config,
        &limits_config,
        &limit_snap,
        StrategyId::A,
        Direction::Buy,
        100_000.0,
        0.5,
        &all_q,
    );
    assert!(matches!(result, Err(RiskError::MonthlyLimit { .. })));
}

// ---------------------------------------------------------------------------
// 6. Dynamic Risk Barrier Integration
// ---------------------------------------------------------------------------

#[test]
fn test_barrier_degrades_with_staleness() {
    let barrier = DynamicRiskBarrier::new(DynamicRiskBarrierConfig::default());

    let r0 = barrier.evaluate(0);
    assert!(r0.allowed);
    assert_eq!(r0.status, BarrierStatus::Normal);
    assert!((r0.lot_multiplier - 1.0).abs() < 1e-6);

    let r1 = barrier.evaluate(1000);
    assert!(r1.allowed);
    assert_eq!(r1.status, BarrierStatus::Warning);

    let r2 = barrier.evaluate(3000);
    assert!(r2.allowed);
    assert_eq!(r2.status, BarrierStatus::Degraded);

    let r3 = barrier.evaluate(5000);
    assert!(!r3.allowed);
    assert_eq!(r3.status, BarrierStatus::Halted);
    assert!((r3.lot_multiplier).abs() < f64::EPSILON);
}

#[test]
fn test_barrier_integrated_with_execution_pipeline() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let barrier = DynamicRiskBarrier::new(DynamicRiskBarrierConfig::default());

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    let snap = projector.snapshot();
    let barrier_result = barrier.evaluate(snap.staleness_ms);
    assert!(barrier_result.allowed);

    let stale_result = barrier.evaluate(10_000);
    assert!(!stale_result.allowed);
}

// ---------------------------------------------------------------------------
// 7. Kill Switch Integration
// ---------------------------------------------------------------------------

#[test]
fn test_kill_switch_masks_after_anomaly() {
    let config = KillSwitchConfig {
        min_samples: 5,
        z_score_threshold: 3.0,
        mask_duration_ms: 50,
        ..Default::default()
    };
    let kill_switch = KillSwitch::new(config);

    let base_ns = NS_BASE;
    // Feed intervals with slight variance (~1ms ± 10%) to build non-zero std
    for i in 1..=10u64 {
        let jitter = ((i * 37) % 200) * 1_000; // ±100µs
        kill_switch.record_tick(base_ns + i * 1_000_000 + jitter);
    }

    assert!(kill_switch.validate_order().is_ok());

    // Anomaly: 100x normal interval (100ms vs ~1ms)
    kill_switch.record_tick(base_ns + 11 * 1_000_000 + 100_000_000);
    assert!(kill_switch.validate_order().is_err());
}

#[test]
fn test_kill_switch_recovers_after_mask_expires() {
    let config = KillSwitchConfig {
        min_samples: 5,
        z_score_threshold: 3.0,
        mask_duration_ms: 50,
        ..Default::default()
    };
    let kill_switch = KillSwitch::new(config);

    let base_ns = NS_BASE;
    for i in 1..=10u64 {
        let jitter = ((i * 37) % 200) * 1_000;
        kill_switch.record_tick(base_ns + i * 1_000_000 + jitter);
    }

    kill_switch.record_tick(base_ns + 11 * 1_000_000 + 100_000_000);

    std::thread::sleep(std::time::Duration::from_millis(60));

    assert!(kill_switch.validate_order().is_ok());
}

// ---------------------------------------------------------------------------
// 8. Lifecycle Manager Integration
// ---------------------------------------------------------------------------

#[test]
fn test_lifecycle_culls_underperforming_strategy() {
    let config = LifecycleConfig {
        min_episodes_for_eval: 5,
        consecutive_death_windows: 2,
        death_sharpe_threshold: -0.3,
        sharpe_annualization_factor: 1.0,
        auto_close_culled_positions: false,
        ..Default::default()
    };
    let mut manager = LifecycleManager::new(config);

    let snap = fx_events::projector::StateSnapshot {
        positions: HashMap::new(),
        global_position: 0.0,
        global_position_limit: 10.0,
        total_unrealized_pnl: 0.0,
        total_realized_pnl: 0.0,
        limit_state: LimitStateData::default(),
        state_version: 0,
        staleness_ms: 0,
        state_hash: String::new(),
        lot_multiplier: 1.0,
        last_market_data_ns: NS_BASE,
    };

    for i in 0..10 {
        let summary = EpisodeSummary {
            strategy_id: StrategyId::A,
            total_reward: -10.0 - i as f64,
            return_g0: -10.0 - i as f64,
            duration_ns: 5_000_000_000,
        };
        let _ = manager.record_episode(&summary, false, &snap);
    }

    assert!(!manager.is_alive(StrategyId::A));
    assert!(manager.is_alive(StrategyId::B));
    assert!(manager.is_alive(StrategyId::C));
}

#[test]
fn test_lifecycle_blocks_new_entries_for_culled_strategy() {
    let config = LifecycleConfig {
        min_episodes_for_eval: 3,
        consecutive_death_windows: 1,
        death_sharpe_threshold: -0.1,
        sharpe_annualization_factor: 1.0,
        auto_close_culled_positions: false,
        ..Default::default()
    };
    let mut manager = LifecycleManager::new(config);

    let snap = fx_events::projector::StateSnapshot {
        positions: HashMap::new(),
        global_position: 0.0,
        global_position_limit: 10.0,
        total_unrealized_pnl: 0.0,
        total_realized_pnl: 0.0,
        limit_state: LimitStateData::default(),
        state_version: 0,
        staleness_ms: 0,
        state_hash: String::new(),
        lot_multiplier: 1.0,
        last_market_data_ns: NS_BASE,
    };

    for i in 0..5 {
        let summary = EpisodeSummary {
            strategy_id: StrategyId::A,
            total_reward: -100.0 - i as f64 * 10.0,
            return_g0: -100.0 - i as f64 * 10.0,
            duration_ns: 5_000_000_000,
        };
        let _ = manager.record_episode(&summary, false, &snap);
    }
    assert!(!manager.is_alive(StrategyId::A));

    assert!(manager.validate_order(StrategyId::A).is_err());
    assert!(manager.validate_order(StrategyId::B).is_ok());
}

#[test]
fn test_lifecycle_auto_close_produces_close_command() {
    let config = LifecycleConfig {
        min_episodes_for_eval: 3,
        consecutive_death_windows: 1,
        death_sharpe_threshold: -0.1,
        sharpe_annualization_factor: 1.0,
        auto_close_culled_positions: true,
        ..Default::default()
    };
    let mut manager = LifecycleManager::new(config);

    let mut positions = HashMap::new();
    positions.insert(
        StrategyId::A,
        fx_events::projector::Position {
            strategy_id: StrategyId::A,
            size: 100_000.0,
            entry_price: 110.0,
            unrealized_pnl: 5.0,
            realized_pnl: 0.0,
            entry_timestamp_ns: NS_BASE,
        },
    );
    let snap = fx_events::projector::StateSnapshot {
        positions,
        global_position: 100_000.0,
        global_position_limit: 10.0,
        total_unrealized_pnl: 5.0,
        total_realized_pnl: 0.0,
        limit_state: LimitStateData::default(),
        state_version: 1,
        staleness_ms: 0,
        state_hash: String::new(),
        lot_multiplier: 1.0,
        last_market_data_ns: NS_BASE,
    };

    for i in 0..5 {
        let summary = EpisodeSummary {
            strategy_id: StrategyId::A,
            total_reward: -100.0 - i as f64 * 10.0,
            return_g0: -100.0 - i as f64 * 10.0,
            duration_ns: 5_000_000_000,
        };
        let _ = manager.record_episode(&summary, false, &snap);
    }

    assert!(!manager.is_alive(StrategyId::A));
    let commands = manager.close_commands_for_culled(&snap);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].strategy_id, StrategyId::A);
    assert_eq!(commands[0].lots, 100_000);
}

// ---------------------------------------------------------------------------
// 9. Monte Carlo Evaluator Integration
// ---------------------------------------------------------------------------

#[test]
fn test_mc_evaluator_episode_lifecycle() {
    let reward_config = RewardConfig::default();
    let mc_config = fx_strategy::mc_eval::McEvalConfig {
        reward: reward_config,
    };
    let mut evaluator = McEvaluator::new(mc_config);

    let features = FeatureVector::zero();
    let phi = features.flattened();

    evaluator.start_episode(StrategyId::A, NS_BASE, 0.0);

    for i in 0..5 {
        evaluator.record_transition(
            StrategyId::A,
            NS_BASE + (i as u64 + 1) * 1_000_000_000,
            QAction::Hold,
            phi.clone(),
            &fx_events::projector::StateSnapshot {
                positions: HashMap::new(),
                global_position: 0.0,
                global_position_limit: 10.0,
                total_unrealized_pnl: (i as f64) * 10.0,
                total_realized_pnl: 0.0,
                limit_state: LimitStateData::default(),
                state_version: i as u64 + 1,
                staleness_ms: 0,
                state_hash: String::new(),
                lot_multiplier: 1.0,
                last_market_data_ns: NS_BASE,
            },
            0.001,
        );
    }

    let result = evaluator.end_episode(
        StrategyId::A,
        TerminalReason::PositionClosed,
        NS_BASE + 5_000_000_000,
    );

    assert_eq!(result.strategy_id, StrategyId::A);
    assert_eq!(result.num_transitions, 5);
    assert_eq!(result.terminal_reason, TerminalReason::PositionClosed);
    assert!(result.duration_ns > 0);
}

// ---------------------------------------------------------------------------
// 10. Full System Integration: All Components Together
// ---------------------------------------------------------------------------

#[test]
fn test_full_system_all_components_wired() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);
    let mut extractor = FeatureExtractor::new(FeatureExtractorConfig::default());
    let mut strategy_a = StrategyA::new(StrategyAConfig::default());
    let _strategy_b = StrategyB::new(StrategyBConfig::default());
    let _strategy_c = StrategyC::new(StrategyCConfig::default());
    let mut execution_gateway = ExecutionGateway::new(ExecutionGatewayConfig {
        symbol: "USD/JPY".to_string(),
        ..Default::default()
    });
    let barrier = DynamicRiskBarrier::new(DynamicRiskBarrierConfig::default());
    let _limits_config = RiskLimitsConfig::default();
    let global_config = GlobalPositionConfig::default();
    let kill_switch = KillSwitch::new(KillSwitchConfig {
        min_samples: 3,
        ..Default::default()
    });
    let mut lifecycle = LifecycleManager::new(LifecycleConfig::default());
    let mut rng = SmallRng::from_seed([42u8; 32]);

    let events = generate_ticking_market_events(NS_BASE, 200, 100, 110.0, 1.0);
    let mut _executions = 0;

    for (i, event) in events.iter().enumerate() {
        let ts = event.header.timestamp_ns;

        projector.process_event(event).expect("projector");
        extractor.process_market_event(event);
        let snapshot = projector.snapshot();

        if i > 0 {
            kill_switch.record_tick(ts);
        }
        if kill_switch.validate_order().is_err() {
            continue;
        }

        let barrier_result = barrier.evaluate(snapshot.staleness_ms);
        if !barrier_result.allowed {
            continue;
        }

        if HierarchicalRiskLimiter::is_halted(&snapshot.limit_state) {
            continue;
        }

        let features = extractor.extract(event, snapshot, StrategyId::A, ts);

        if lifecycle.is_alive(StrategyId::A) {
            let regime_kl = 0.5;
            let decision = strategy_a.decide(&features, snapshot, regime_kl, 1.0, ts, &mut rng);

            let (direction, lots) = match &decision.action {
                Action::Buy(l) => (Direction::Buy, *l),
                Action::Sell(l) => (Direction::Sell, *l),
                Action::Hold => continue,
            };

            if lots > 0 {
                let snap_lots = snapshot_in_lot_units(snapshot, global_config.lot_unit_size);
                let mut all_q = HashMap::new();
                all_q.insert(StrategyId::A, decision.q_point);
                all_q.insert(StrategyId::B, 0.0);
                all_q.insert(StrategyId::C, 0.0);

                let pos_check = GlobalPositionChecker::validate_order(
                    &global_config,
                    &snap_lots,
                    StrategyId::A,
                    direction,
                    lots as f64,
                    decision.q_point,
                    &all_q,
                );

                if pos_check.is_ok() {
                    let market = proto::MarketEventPayload::decode(event.payload_bytes()).unwrap();
                    let mid = (market.bid + market.ask) / 2.0;
                    let vol = (market.ask - market.bid) / mid;

                    let exec_request = ExecutionRequest {
                        direction,
                        lots,
                        strategy_id: StrategyId::A,
                        current_mid_price: mid,
                        volatility: vol,
                        expected_profit: 0.0002,
                        symbol: "USD/JPY".to_string(),
                        timestamp_ns: ts,
                        time_urgent: false,
                    };

                    let sim_result = execution_gateway.simulate_execution(&exec_request, &mut rng);
                    if let Ok(result) = sim_result {
                        if result.filled {
                            let signed_fill = match direction {
                                Direction::Buy => result.fill_size,
                                Direction::Sell => -result.fill_size,
                            };
                            let exec_event = make_execution_event(
                                ts,
                                &result.order_id,
                                result.fill_price,
                                signed_fill,
                                result.slippage,
                                "USD/JPY",
                            );
                            let _ = projector
                                .process_execution_for_strategy(&exec_event, StrategyId::A);
                            _executions += 1;
                        }
                    }
                }
            }
        }

        let snap2 = projector.snapshot();
        let _ = lifecycle.record_episode(
            &EpisodeSummary {
                strategy_id: StrategyId::A,
                total_reward: 0.0,
                return_g0: 0.0,
                duration_ns: 100_000_000,
            },
            false,
            snap2,
        );
    }

    let final_snap = projector.snapshot();
    assert!(final_snap.state_version > 0);
    assert!(projector.verify_integrity());
    assert!(lifecycle.is_alive(StrategyId::A));

    // Verify the active LP is a known LP (may have switched during execution)
    let active_lp = execution_gateway.active_lp_id();
    assert!(!active_lp.is_empty());
}

#[test]
fn test_full_pipeline_with_partial_close() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    // Open large position (Buy: positive fill_size)
    let open_event = make_execution_event(
        NS_BASE + 500_000_000,
        "ORD-1",
        110.005,
        200_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&open_event, StrategyId::A)
        .expect("open");

    let snap = projector.snapshot();
    assert!((snap.positions[&StrategyId::A].size - 200_000.0).abs() < 1e-6);

    // Partial close (Sell 50k: negative fill_size)
    let partial_close = make_execution_event(
        NS_BASE + 2_000_000_000,
        "ORD-2",
        110.010,
        -50_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&partial_close, StrategyId::A)
        .expect("partial close");

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::A].is_open());
    assert!((snap.positions[&StrategyId::A].size - 150_000.0).abs() < 1e-6);

    // Full close remaining (Sell 150k: negative fill_size)
    let full_close = make_execution_event(
        NS_BASE + 3_000_000_000,
        "ORD-3",
        110.015,
        -150_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&full_close, StrategyId::A)
        .expect("full close");

    let snap = projector.snapshot();
    assert!(!snap.positions[&StrategyId::A].is_open());
}

#[test]
fn test_all_strategies_independent_positions() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    // Each strategy opens independently (Buy: positive fill_size)
    for (sid, order_id) in [
        (StrategyId::A, "A-1"),
        (StrategyId::B, "B-1"),
        (StrategyId::C, "C-1"),
    ] {
        let fill = make_execution_event(
            NS_BASE + 500_000_000,
            order_id,
            110.005,
            100_000.0,
            0.0001,
            "USD/JPY",
        );
        projector
            .process_execution_for_strategy(&fill, sid)
            .expect("open");
    }

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::A].is_open());
    assert!(snap.positions[&StrategyId::B].is_open());
    assert!(snap.positions[&StrategyId::C].is_open());
    assert!((snap.global_position - 300_000.0).abs() < 1e-6);

    // Close only Strategy A (Sell: negative fill_size)
    let close_a = make_execution_event(
        NS_BASE + 5_000_000_000,
        "A-2",
        110.010,
        -100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&close_a, StrategyId::A)
        .expect("close A");

    let snap = projector.snapshot();
    assert!(!snap.positions[&StrategyId::A].is_open());
    assert!(snap.positions[&StrategyId::B].is_open());
    assert!(snap.positions[&StrategyId::C].is_open());
    assert!((snap.global_position - 200_000.0).abs() < 1e-6);
}

#[test]
fn test_short_position_lifecycle() {
    let bus = PartitionedEventBus::new();
    let mut projector = StateProjector::new(&bus, 10.0, 1);

    let events = generate_ticking_market_events(NS_BASE, 10, 100, 110.0, 1.0);
    for event in &events {
        projector.process_event(event).expect("process");
    }

    // Open short (Sell: negative fill_size)
    let open_event = make_execution_event(
        NS_BASE + 500_000_000,
        "SHORT-1",
        110.005,
        -100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&open_event, StrategyId::B)
        .expect("open short");

    let snap = projector.snapshot();
    assert!(snap.positions[&StrategyId::B].size < 0.0);

    // Close short (Buy: positive fill_size)
    let close_event = make_execution_event(
        NS_BASE + 5_000_000_000,
        "SHORT-2",
        110.010,
        100_000.0,
        0.0001,
        "USD/JPY",
    );
    projector
        .process_execution_for_strategy(&close_event, StrategyId::B)
        .expect("close short");

    let snap = projector.snapshot();
    assert!(!snap.positions[&StrategyId::B].is_open());
    assert!((snap.global_position).abs() < 1e-6);
}
