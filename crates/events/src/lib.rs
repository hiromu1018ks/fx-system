pub mod bus;
pub mod event;
pub mod gap_detector;
pub mod header;
pub mod projector;
pub mod store;
pub mod stream;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/fx.events.rs"));
}

// ---------------------------------------------------------------------------
// §8.1 Event Structure verification tests (design.md §8.1)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::proto::*;
    use prost::Message;

    // -- EventHeader ----------------------------------------------------------

    #[test]
    fn s8_1_event_header_has_all_design_fields() {
        let h = EventHeader::default();
        // design.md §8.1: event_id, parent_event_id, stream_id, sequence_id, timestamp_ns, schema_version, tier
        assert!(h.event_id.is_empty());
        assert!(h.parent_event_id.is_none()); // optional field
        assert_eq!(h.stream_id, 0); // StreamId::Unspecified
        assert_eq!(h.sequence_id, 0);
        assert_eq!(h.timestamp_ns, 0);
        assert_eq!(h.schema_version, 0);
        assert_eq!(h.tier, 0); // EventTier::Unspecified
    }

    #[test]
    fn s8_1_event_header_roundtrip() {
        let h = EventHeader {
            event_id: "evt-001".into(),
            parent_event_id: Some("evt-000".into()),
            stream_id: StreamId::Strategy as i32,
            sequence_id: 42,
            timestamp_ns: 1_700_000_000_000_000_000,
            schema_version: 1,
            tier: EventTier::Tier1Critical as i32,
        };
        let bytes = h.encode_to_vec();
        let h2 = EventHeader::decode(bytes.as_slice()).unwrap();
        assert_eq!(h2.event_id, "evt-001");
        assert_eq!(h2.parent_event_id, Some("evt-000".into()));
        assert_eq!(h2.stream_id, StreamId::Strategy as i32);
        assert_eq!(h2.sequence_id, 42);
        assert_eq!(h2.timestamp_ns, 1_700_000_000_000_000_000);
        assert_eq!(h2.schema_version, 1);
        assert_eq!(h2.tier, EventTier::Tier1Critical as i32);
    }

    // -- DecisionEventPayload --------------------------------------------------

    #[test]
    fn s8_1_decision_event_has_strategy_and_action() {
        let p = DecisionEventPayload::default();
        assert_eq!(p.strategy_id, StrategyId::Unspecified as i32);
        assert_eq!(p.action, ActionType::Unspecified as i32);
        assert_eq!(p.lots, 0);
        assert_eq!(p.decision, TradeDecision::Unspecified as i32);
    }

    #[test]
    fn s8_1_decision_event_has_feature_vector() {
        let p = DecisionEventPayload::default();
        assert!(p.feature_vector.is_empty());
        let mut p2 = DecisionEventPayload::default();
        p2.feature_vector = vec![1.0, 2.0, 3.0];
        assert_eq!(p2.feature_vector.len(), 3);
    }

    #[test]
    fn s8_1_decision_event_has_q_values() {
        let p = DecisionEventPayload::default();
        // design.md §8.1: q_tilde_final_values (3 actions), q_point_selected, q_tilde_selected
        // Implementation uses: q_buy, q_sell, q_hold, q_selected
        assert_eq!(p.q_buy, 0.0);
        assert_eq!(p.q_sell, 0.0);
        assert_eq!(p.q_hold, 0.0);
        assert_eq!(p.q_selected, 0.0);
    }

    #[test]
    fn s8_1_decision_event_has_thompson_sampling_stats() {
        let p = DecisionEventPayload::default();
        // design.md §8.1: thompson_posterior_std, sigma_model
        // Implementation: posterior_mean, posterior_std, sampled_q
        assert_eq!(p.posterior_mean, 0.0);
        assert_eq!(p.posterior_std, 0.0);
        assert_eq!(p.sampled_q, 0.0);
    }

    #[test]
    fn s8_1_decision_event_has_position_state() {
        let p = DecisionEventPayload::default();
        // design.md §8.1: position_before, position_after, position_max_limit
        // Implementation: position_size, entry_price, pnl_unrealized, holding_time_ms
        assert_eq!(p.position_size, 0.0);
        assert_eq!(p.entry_price, 0.0);
        assert_eq!(p.pnl_unrealized, 0.0);
        assert_eq!(p.holding_time_ms, 0.0);
    }

    #[test]
    fn s8_1_decision_event_has_risk_context() {
        let p = DecisionEventPayload::default();
        assert_eq!(p.staleness_ms, 0.0);
        assert_eq!(p.lot_multiplier, 0.0);
        assert_eq!(p.daily_pnl, 0.0);
    }

    #[test]
    fn s8_1_decision_event_has_regime_info() {
        let p = DecisionEventPayload::default();
        assert!(p.regime_posterior.is_empty());
        assert_eq!(p.regime_entropy, 0.0);
    }

    #[test]
    fn s8_1_decision_event_has_skip_reason() {
        let p = DecisionEventPayload::default();
        assert!(p.skip_reason.is_empty());
        let mut p2 = DecisionEventPayload::default();
        p2.skip_reason = "kill_switch_masked".into();
        assert_eq!(p2.skip_reason, "kill_switch_masked");
    }

    #[test]
    fn s8_1_decision_event_proto_roundtrip() {
        let p = DecisionEventPayload {
            header: Some(EventHeader {
                event_id: "d-001".into(),
                parent_event_id: None,
                stream_id: StreamId::Strategy as i32,
                sequence_id: 10,
                timestamp_ns: 1000,
                schema_version: 1,
                tier: EventTier::Tier2Derived as i32,
            }),
            strategy_id: StrategyId::StrategyA as i32,
            action: ActionType::Buy as i32,
            lots: 10000,
            decision: TradeDecision::Execute as i32,
            feature_vector: vec![0.1, 0.2, 0.3],
            q_buy: 0.001,
            q_sell: -0.0005,
            q_hold: 0.0,
            q_selected: 0.001,
            posterior_mean: 0.0008,
            posterior_std: 0.0002,
            sampled_q: 0.001,
            position_size: 10000.0,
            entry_price: 150.25,
            pnl_unrealized: 5.0,
            holding_time_ms: 5000.0,
            staleness_ms: 10.0,
            lot_multiplier: 0.8,
            daily_pnl: -50.0,
            regime_posterior: vec![0.7, 0.2, 0.1],
            regime_entropy: 0.8,
            skip_reason: String::new(),
        };
        let bytes = p.encode_to_vec();
        let p2 = DecisionEventPayload::decode(bytes.as_slice()).unwrap();
        assert_eq!(p2.strategy_id, StrategyId::StrategyA as i32);
        assert_eq!(p2.action, ActionType::Buy as i32);
        assert_eq!(p2.lots, 10000);
        assert_eq!(p2.feature_vector, vec![0.1, 0.2, 0.3]);
        assert!((p2.q_buy - 0.001).abs() < 1e-10);
        assert!((p2.sampled_q - 0.001).abs() < 1e-10);
        assert!((p2.position_size - 10000.0).abs() < 1e-10);
        assert!((p2.regime_entropy - 0.8).abs() < 1e-10);
    }

    // -- ExecutionEventPayload -------------------------------------------------

    #[test]
    fn s8_1_execution_event_has_order_info() {
        let p = ExecutionEventPayload::default();
        assert!(p.order_id.is_empty());
        assert!(p.symbol.is_empty());
        assert_eq!(p.order_type, OrderType::Unspecified as i32);
    }

    #[test]
    fn s8_1_execution_event_has_fill_details() {
        let p = ExecutionEventPayload::default();
        // design.md §8.1: expected_fill_price, actual_fill_price, slippage, estimated_fill_prob
        // Implementation: fill_status, fill_price, fill_size, slippage, requested_price, requested_size
        assert_eq!(p.fill_status, FillStatus::Unspecified as i32);
        assert_eq!(p.fill_price, 0.0);
        assert_eq!(p.fill_size, 0.0);
        assert_eq!(p.slippage, 0.0);
        assert_eq!(p.requested_price, 0.0);
        assert_eq!(p.requested_size, 0.0);
    }

    #[test]
    fn s8_1_execution_event_has_fill_probability_model() {
        let p = ExecutionEventPayload::default();
        assert_eq!(p.fill_probability, 0.0);
        assert_eq!(p.effective_fill_probability, 0.0);
        assert_eq!(p.price_improvement, 0.0);
    }

    #[test]
    fn s8_1_execution_event_has_last_look_model() {
        let p = ExecutionEventPayload::default();
        assert_eq!(p.last_look_rejection_prob, 0.0);
    }

    #[test]
    fn s8_1_execution_event_has_lp_info() {
        let p = ExecutionEventPayload::default();
        assert!(p.lp_id.is_empty());
        assert_eq!(p.latency_ms, 0.0);
    }

    #[test]
    fn s8_1_execution_event_has_reject_info() {
        let p = ExecutionEventPayload::default();
        assert_eq!(p.reject_reason, RejectReason::Unspecified as i32);
        assert!(p.reject_message.is_empty());
    }

    #[test]
    fn s8_1_execution_event_proto_roundtrip() {
        let p = ExecutionEventPayload {
            header: Some(EventHeader {
                event_id: "e-001".into(),
                parent_event_id: Some("d-001".into()),
                stream_id: StreamId::Execution as i32,
                sequence_id: 20,
                timestamp_ns: 2000,
                schema_version: 1,
                tier: EventTier::Tier1Critical as i32,
            }),
            order_id: "ord-001".into(),
            symbol: "USD/JPY".into(),
            order_type: OrderType::OrderMarket as i32,
            fill_status: FillStatus::Filled as i32,
            fill_price: 150.251,
            fill_size: 10000.0,
            slippage: 0.001,
            requested_price: 150.250,
            requested_size: 10000.0,
            fill_probability: 0.95,
            effective_fill_probability: 0.90,
            price_improvement: 0.0,
            last_look_rejection_prob: 0.05,
            lp_id: "LP1".into(),
            latency_ms: 5.0,
            reject_reason: RejectReason::Unspecified as i32,
            reject_message: String::new(),
        };
        let bytes = p.encode_to_vec();
        let p2 = ExecutionEventPayload::decode(bytes.as_slice()).unwrap();
        assert_eq!(p2.order_id, "ord-001");
        assert_eq!(p2.symbol, "USD/JPY");
        assert_eq!(p2.fill_status, FillStatus::Filled as i32);
        assert!((p2.fill_price - 150.251).abs() < 1e-10);
        assert!((p2.slippage - 0.001).abs() < 1e-10);
        assert_eq!(p2.lp_id, "LP1");
        assert!((p2.latency_ms - 5.0).abs() < 1e-10);
    }

    // -- StateSnapshotPayload --------------------------------------------------

    #[test]
    fn s8_1_state_snapshot_has_positions() {
        let p = StateSnapshotPayload::default();
        assert!(p.positions.is_empty());
    }

    #[test]
    fn s8_1_state_snapshot_has_global_position() {
        let p = StateSnapshotPayload::default();
        // design.md §8.1: position_global, p_max_global
        assert_eq!(p.global_position, 0.0);
        assert_eq!(p.global_position_limit, 0.0);
    }

    #[test]
    fn s8_1_state_snapshot_has_pnl() {
        let p = StateSnapshotPayload::default();
        assert_eq!(p.total_unrealized_pnl, 0.0);
        assert_eq!(p.total_realized_pnl, 0.0);
    }

    #[test]
    fn s8_1_state_snapshot_has_limit_state() {
        let p = StateSnapshotPayload::default();
        // design.md §8.1: daily_pnl, max_daily_loss, daily_mtm_pnl, etc.
        // Implementation uses LimitState sub-message
        let ls = p.limit_state.unwrap_or_default();
        assert_eq!(ls.daily_pnl_mtm, 0.0);
        assert_eq!(ls.daily_pnl_realized, 0.0);
        assert_eq!(ls.weekly_pnl, 0.0);
        assert_eq!(ls.monthly_pnl, 0.0);
        assert!(!ls.daily_mtm_limited);
        assert!(!ls.daily_realized_halted);
        assert!(!ls.weekly_halted);
        assert!(!ls.monthly_halted);
    }

    #[test]
    fn s8_1_state_snapshot_has_integrity() {
        let p = StateSnapshotPayload::default();
        assert_eq!(p.state_version, 0);
        assert_eq!(p.staleness_ms, 0);
        assert!(p.state_hash.is_empty());
    }

    #[test]
    fn s8_1_state_snapshot_has_risk_barriers() {
        let p = StateSnapshotPayload::default();
        assert_eq!(p.lot_multiplier, 0.0);
        assert_eq!(p.last_market_data_ns, 0);
    }

    #[test]
    fn s8_1_state_snapshot_proto_roundtrip() {
        let p = StateSnapshotPayload {
            header: Some(EventHeader {
                event_id: "s-001".into(),
                parent_event_id: None,
                stream_id: StreamId::State as i32,
                sequence_id: 30,
                timestamp_ns: 3000,
                schema_version: 1,
                tier: EventTier::Tier1Critical as i32,
            }),
            positions: vec![PositionState {
                strategy_id: "A".into(),
                size: 10000.0,
                entry_price: 150.25,
                unrealized_pnl: 5.0,
                realized_pnl: 10.0,
                holding_time_ms: 5000,
            }],
            global_position: 10000.0,
            global_position_limit: 100000.0,
            total_unrealized_pnl: 5.0,
            total_realized_pnl: 10.0,
            limit_state: Some(LimitState {
                daily_pnl_mtm: -100.0,
                daily_pnl_realized: -50.0,
                weekly_pnl: -200.0,
                monthly_pnl: -500.0,
                daily_mtm_limited: true,
                daily_realized_halted: false,
                weekly_halted: false,
                monthly_halted: false,
            }),
            state_version: 42,
            staleness_ms: 5,
            state_hash: "abc123".into(),
            lot_multiplier: 0.8,
            last_market_data_ns: 2995,
        };
        let bytes = p.encode_to_vec();
        let p2 = StateSnapshotPayload::decode(bytes.as_slice()).unwrap();
        assert_eq!(p2.positions.len(), 1);
        assert!((p2.global_position - 10000.0).abs() < 1e-10);
        assert_eq!(p2.state_version, 42);
        let ls = p2.limit_state.unwrap();
        assert!(ls.daily_mtm_limited);
        assert!((ls.daily_pnl_mtm - (-100.0)).abs() < 1e-10);
    }

    // -- Enum completeness ----------------------------------------------------

    #[test]
    fn s8_1_stream_id_covers_all_four_streams() {
        // design.md §6.1: Market, Strategy, Execution, State
        assert_eq!(StreamId::Market as i32, 1);
        assert_eq!(StreamId::Strategy as i32, 2);
        assert_eq!(StreamId::Execution as i32, 3);
        assert_eq!(StreamId::State as i32, 4);
    }

    #[test]
    fn s8_1_event_tier_covers_three_tiers() {
        // design.md §7.3: Tier1 persistent, Tier2 compressed, Tier3 ephemeral
        assert_eq!(EventTier::Tier1Critical as i32, 1);
        assert_eq!(EventTier::Tier2Derived as i32, 2);
        assert_eq!(EventTier::Tier3Raw as i32, 3);
    }

    #[test]
    fn s8_1_strategy_id_covers_three_strategies() {
        assert_eq!(StrategyId::StrategyA as i32, 1);
        assert_eq!(StrategyId::StrategyB as i32, 2);
        assert_eq!(StrategyId::StrategyC as i32, 3);
    }

    #[test]
    fn s8_1_action_type_covers_three_actions() {
        // design.md §3.0: buy, sell, hold
        assert_eq!(ActionType::Buy as i32, 1);
        assert_eq!(ActionType::Sell as i32, 2);
        assert_eq!(ActionType::Hold as i32, 3);
    }

    #[test]
    fn s8_1_fill_status_covers_all_states() {
        assert_eq!(FillStatus::Filled as i32, 1);
        assert_eq!(FillStatus::PartialFill as i32, 2);
        assert_eq!(FillStatus::Rejected as i32, 3);
    }

    #[test]
    fn s8_1_reject_reason_covers_otc_scenarios() {
        // design.md §4.2: Last-Look is primary rejection reason for OTC
        assert_eq!(RejectReason::LastLook as i32, 1);
        assert_eq!(RejectReason::InsufficientLiquidity as i32, 2);
        assert_eq!(RejectReason::MarketClosed as i32, 3);
        assert_eq!(RejectReason::RiskLimit as i32, 4);
    }
}
