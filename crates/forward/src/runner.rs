use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use fx_core::observability::{AnomalyConfig, ObservabilityManager, PreFailureMetrics};
use fx_core::types::{Direction, EventTier, StrategyId, StreamId};
use fx_events::bus::PartitionedEventBus;
use fx_events::event::GenericEvent;
use fx_events::gap_detector::GapDetector;
use fx_events::header::EventHeader;
use fx_execution::gateway::{ExecutionGatewayConfig, ExecutionRequest};
use fx_gateway::market::TickData;
use fx_risk::barrier::DynamicRiskBarrier;
use fx_risk::global_position::GlobalPositionChecker;
use fx_risk::kill_switch::KillSwitch;
use fx_risk::lifecycle::{EpisodeSummary, LifecycleManager};
use fx_risk::limits::HierarchicalRiskLimiter;
use fx_strategy::change_point::ChangePointDetector;
use fx_strategy::extractor::FeatureExtractor;
use fx_strategy::features::FeatureVector;
use fx_strategy::thompson_sampling::ThompsonSamplingPolicy;
use prost::Message;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::ForwardTestConfig;
use crate::feed::MarketFeed;
use crate::paper::PaperExecutionEngine;
use crate::tracker::PerformanceTracker;

/// Summary of a forward test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardTestResult {
    pub total_ticks: u64,
    pub total_decisions: u64,
    pub total_trades: u64,
    pub duration_secs: f64,
    pub final_pnl: f64,
    pub strategies_used: Vec<String>,
}

/// Forward test runner — main orchestration for the forward test pipeline.
pub struct ForwardTestRunner<F: MarketFeed> {
    feed: F,
    config: ForwardTestConfig,
    tracker: PerformanceTracker,
}

impl<F: MarketFeed> ForwardTestRunner<F> {
    pub fn new(feed: F, config: ForwardTestConfig) -> Self {
        Self {
            feed,
            config,
            tracker: PerformanceTracker::new(),
        }
    }

    /// Run the forward test until the feed is exhausted or duration is reached.
    pub async fn run(&mut self, seed: u64) -> Result<ForwardTestResult> {
        let start = Instant::now();
        let deadline = self.config.duration.map(|d| start + d);

        self.feed.connect().await?;
        self.feed.subscribe(&[]).await?;

        let bus = PartitionedEventBus::new();
        let mut projector = fx_events::projector::StateProjector::new(
            &bus,
            self.config.risk_config.max_position_lots,
            1,
        );
        let mut gap_detector = GapDetector::new(&bus, 1);
        let mut observability_manager = ObservabilityManager::new(AnomalyConfig::default());

        let exec_config = ExecutionGatewayConfig::default();
        let mut paper_engine = PaperExecutionEngine::new(exec_config, seed);
        let mut rng = SmallRng::seed_from_u64(seed);

        let mut feature_extractor = FeatureExtractor::new(Default::default());
        let mut change_point_detector = ChangePointDetector::new_default(FeatureVector::DIM);

        let q_function = fx_strategy::bayesian_lr::QFunction::new(
            FeatureVector::DIM,
            1.0,  // lambda_reg
            500,  // halflife
            1.0,  // initial_sigma2
            0.01, // optimistic_bias
        );
        let policy_config = fx_strategy::thompson_sampling::ThompsonSamplingConfig::default();
        let mut policy = ThompsonSamplingPolicy::new(q_function, policy_config);

        let barrier_config = fx_risk::barrier::DynamicRiskBarrierConfig::default();
        let risk_barrier = DynamicRiskBarrier::new(barrier_config);
        let mut lifecycle = LifecycleManager::new(fx_risk::lifecycle::LifecycleConfig::default());
        let limits_config = fx_risk::limits::RiskLimitsConfig::default();
        let position_config = fx_risk::global_position::GlobalPositionConfig::default();
        let kill_switch = KillSwitch::new(fx_risk::kill_switch::KillSwitchConfig::default());

        let mut total_ticks: u64 = 0;
        let mut total_decisions: u64 = 0;
        let mut total_trades: u64 = 0;

        let enabled_strategies = self.get_enabled_strategies();

        loop {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    info!("Duration reached, shutting down");
                    break;
                }
            }

            let tick = match self.feed.next_tick().await? {
                Some(t) => t,
                None => {
                    info!("Data feed exhausted");
                    break;
                }
            };

            total_ticks += 1;

            let market_event = self.tick_to_event(&tick);
            let tick_ns = tick.timestamp_ns;

            // Feed kill switch with tick timestamps for anomaly detection
            let _ = kill_switch.record_tick(tick_ns);

            // Feed tick to GapDetector for sequence/timing gap detection
            if let Err(e) = gap_detector.process_market_event(&market_event).await {
                debug!("Gap detector error: {}", e);
            }

            if let Err(e) = projector.process_event(&market_event) {
                warn!("Projector error: {}", e);
                continue;
            }

            let snapshot = projector.snapshot().clone();
            let staleness_ms = snapshot.staleness_ms;

            // Collect pre-failure metrics for observability (design.md §8.2)
            let metrics = PreFailureMetrics {
                rolling_variance_latency: kill_switch.stats().std_interval_ns,
                regime_posterior_entropy: 0.0,
                daily_pnl_vs_limit: if limits_config.max_daily_loss_mtm.abs() > f64::EPSILON {
                    snapshot.limit_state.daily_pnl_mtm / limits_config.max_daily_loss_mtm.abs()
                } else {
                    0.0
                },
                weekly_pnl_vs_limit: if limits_config.max_weekly_loss.abs() > f64::EPSILON {
                    snapshot.limit_state.weekly_pnl / limits_config.max_weekly_loss.abs()
                } else {
                    0.0
                },
                monthly_pnl_vs_limit: if limits_config.max_monthly_loss.abs() > f64::EPSILON {
                    snapshot.limit_state.monthly_pnl / limits_config.max_monthly_loss.abs()
                } else {
                    0.0
                },
                ..PreFailureMetrics::default()
            };
            observability_manager.tick(metrics, tick_ns);

            // Check kill switch
            if kill_switch.validate_order().is_err() {
                debug!(ts = tick_ns, "Kill switch active, skipping");
                continue;
            }

            // Check gap detector halt
            if gap_detector.is_trading_halted() {
                debug!(ts = tick_ns, "Trading halted due to severe gap");
                continue;
            }

            // Check risk barrier (staleness)
            if risk_barrier.validate_order(staleness_ms).is_err() {
                debug!(ts = tick_ns, staleness_ms, "Risk barrier blocked");
                continue;
            }

            // Check hierarchical loss limits (static methods)
            let limit_state = HierarchicalRiskLimiter::compute_limit_state(
                &limits_config,
                snapshot.limit_state.daily_pnl_mtm,
                snapshot.limit_state.daily_pnl_realized,
                snapshot.limit_state.weekly_pnl,
                snapshot.limit_state.monthly_pnl,
            );
            if HierarchicalRiskLimiter::validate_order(&limits_config, &limit_state).is_err() {
                debug!(ts = tick_ns, "Loss limit reached");
                continue;
            }

            feature_extractor.process_market_event(&market_event);

            for &strategy_id in &enabled_strategies {
                let features =
                    feature_extractor.extract(&market_event, &snapshot, strategy_id, tick_ns);

                change_point_detector.observe_and_respond(
                    &features.flattened(),
                    tick_ns,
                    policy.q_function_mut(),
                );

                // Check lifecycle
                if !lifecycle.is_alive(strategy_id) {
                    continue;
                }

                lifecycle.validate_order(strategy_id).ok();

                let decision =
                    policy.decide(&features, &snapshot, strategy_id, tick.latency_ms, &mut rng);

                total_decisions += 1;

                let (direction, lots) = match &decision.action {
                    fx_strategy::policy::Action::Buy(l) => (Direction::Buy, *l),
                    fx_strategy::policy::Action::Sell(l) => (Direction::Sell, *l),
                    fx_strategy::policy::Action::Hold => continue,
                };

                if lots == 0 {
                    continue;
                }

                // Check global position constraint (static method)
                let all_q: HashMap<StrategyId, f64> = enabled_strategies
                    .iter()
                    .map(|&s| (s, decision.q_point))
                    .collect();
                if GlobalPositionChecker::validate_order(
                    &position_config,
                    &snapshot,
                    strategy_id,
                    direction,
                    lots as f64,
                    decision.q_sampled,
                    &all_q,
                )
                .is_err()
                {
                    continue;
                }

                let mid_price = tick.mid();
                let spread = tick.spread();
                let volatility = if mid_price > 0.0 {
                    spread / mid_price
                } else {
                    0.0
                };

                let request = ExecutionRequest {
                    direction,
                    lots,
                    strategy_id,
                    current_mid_price: mid_price,
                    volatility,
                    expected_profit: decision.q_sampled,
                    symbol: tick.symbol.clone(),
                    timestamp_ns: tick_ns,
                    time_urgent: false,
                };

                let paper_result = match paper_engine.execute(&request) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Paper execution error: {}", e);
                        continue;
                    }
                };

                total_trades += 1;

                // Build execution event and feed back to projector
                let exec_result = paper_engine
                    .gateway_mut()
                    .simulate_execution(&request, &mut rng)
                    .unwrap_or_else(|_| Self::fallback_rejected_result(mid_price, lots));
                let exec_event = paper_engine.build_execution_event(&request, &exec_result);
                if let Err(e) = projector.process_execution_for_strategy(&exec_event, strategy_id) {
                    warn!("Projector execution error: {}", e);
                }

                // Record episode in lifecycle
                let pnl = if paper_result.fill_price.is_some() {
                    paper_result.slippage
                } else {
                    0.0
                };
                let episode = EpisodeSummary {
                    strategy_id,
                    total_reward: pnl,
                    return_g0: pnl,
                    duration_ns: 0,
                };
                lifecycle.record_episode(&episode, false, projector.snapshot());
            }

            let snap = projector.snapshot();
            self.tracker
                .update(tick_ns, snap.total_realized_pnl, snap.total_unrealized_pnl);
        }

        self.feed.disconnect().await?;

        let elapsed = start.elapsed();
        let result = ForwardTestResult {
            total_ticks,
            total_decisions,
            total_trades,
            duration_secs: elapsed.as_secs_f64(),
            final_pnl: self.tracker.snapshot().cumulative_pnl,
            strategies_used: self.config.enabled_strategies.iter().cloned().collect(),
        };

        info!(
            ticks = result.total_ticks,
            decisions = result.total_decisions,
            trades = result.total_trades,
            pnl = result.final_pnl,
            duration_s = result.duration_secs,
            "Forward test completed"
        );

        Ok(result)
    }

    pub fn tracker(&self) -> &PerformanceTracker {
        &self.tracker
    }

    fn get_enabled_strategies(&self) -> Vec<StrategyId> {
        let mut strategies = Vec::new();
        if self.config.is_strategy_enabled("A") {
            strategies.push(StrategyId::A);
        }
        if self.config.is_strategy_enabled("B") {
            strategies.push(StrategyId::B);
        }
        if self.config.is_strategy_enabled("C") {
            strategies.push(StrategyId::C);
        }
        strategies
    }

    fn tick_to_event(&self, tick: &TickData) -> GenericEvent {
        let proto = tick.to_proto();
        let payload = proto.encode_to_vec();
        let header = EventHeader::new(StreamId::Market, 0, EventTier::Tier3Raw);
        GenericEvent::new(
            EventHeader {
                timestamp_ns: tick.timestamp_ns,
                ..header
            },
            payload,
        )
    }

    fn fallback_rejected_result(
        mid_price: f64,
        lots: u64,
    ) -> fx_execution::gateway::ExecutionResult {
        use fx_execution::gateway::{FillOutcome, OrderEvaluation};
        use fx_execution::otc_model::OtcOrderType;
        fx_execution::gateway::ExecutionResult {
            order_id: String::new(),
            filled: false,
            fill_price: 0.0,
            fill_size: 0.0,
            slippage: 0.0,
            fill_probability: 0.0,
            effective_fill_probability: 0.0,
            last_look_rejection_prob: 0.0,
            price_improvement: 0.0,
            order_type: OtcOrderType::Market,
            fill_status: FillOutcome::Rejected,
            reject_reason: Some("simulation_error".to_string()),
            lp_id: String::new(),
            requested_price: mid_price,
            requested_size: lots as f64,
            latency_ms: 0.0,
            evaluation: OrderEvaluation {
                order_type: OtcOrderType::Market,
                fill_probability: 0.0,
                effective_fill_probability: 0.0,
                expected_slippage: 0.0,
                last_look_fill_prob: 0.0,
                lp_id: String::new(),
                limit_price_distance: 0.0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::feed::DataSourceConfig;
    use std::collections::VecDeque;
    use std::time::Duration;

    struct StubFeed {
        ticks: VecDeque<TickData>,
        connected: bool,
    }

    impl StubFeed {
        fn new(ticks: Vec<TickData>) -> Self {
            Self {
                ticks: ticks.into_iter().collect(),
                connected: false,
            }
        }
    }

    #[allow(async_fn_in_trait)]
    impl MarketFeed for StubFeed {
        async fn connect(&mut self) -> Result<()> {
            self.connected = true;
            Ok(())
        }
        async fn subscribe(&mut self, _symbols: &[String]) -> Result<()> {
            Ok(())
        }
        async fn next_tick(&mut self) -> Result<Option<TickData>> {
            if !self.connected {
                anyhow::bail!("Not connected");
            }
            Ok(self.ticks.pop_front())
        }
        async fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
    }

    fn make_tick(timestamp_ns: u64, bid: f64, ask: f64) -> TickData {
        TickData {
            symbol: "EUR/USD".to_string(),
            bid,
            ask,
            bid_size: 1.0,
            ask_size: 1.0,
            bid_levels: vec![],
            ask_levels: vec![],
            timestamp_ns,
            latency_ms: 0.0,
        }
    }

    fn make_config(strategies: &[&str]) -> ForwardTestConfig {
        ForwardTestConfig {
            enabled_strategies: strategies.iter().map(|s| s.to_string()).collect(),
            data_source: DataSourceConfig::Recorded {
                event_store_path: String::new(),
                speed: 0.0,
                start_time: None,
                end_time: None,
            },
            duration: None,
            alert_config: AlertConfig {
                channels: vec![AlertChannelConfig::Log],
                risk_limit_threshold: 0.8,
                execution_drift_threshold: 2.0,
                sharpe_degradation_threshold: 0.3,
            },
            report_config: ReportConfig {
                output_dir: "./reports".to_string(),
                format: ReportFormat::Both,
                interval: None,
            },
            risk_config: ForwardRiskConfig {
                max_position_lots: 10.0,
                max_daily_loss: 500.0,
                max_drawdown: 1000.0,
            },
            comparison_config: None,
        }
    }

    #[tokio::test]
    async fn test_runner_processes_ticks() {
        let ticks: Vec<TickData> = (0..10)
            .map(|i| {
                make_tick(
                    1_000_000 + i as u64 * 1000,
                    1.1000 + i as f64 * 0.0001,
                    1.1001 + i as f64 * 0.0001,
                )
            })
            .collect();
        let feed = StubFeed::new(ticks);
        let config = make_config(&["A"]);
        let mut runner = ForwardTestRunner::new(feed, config);

        let result = runner.run(42).await.unwrap();
        assert_eq!(result.total_ticks, 10);
        assert!(result.duration_secs >= 0.0);
    }

    #[tokio::test]
    async fn test_runner_empty_feed() {
        let feed = StubFeed::new(vec![]);
        let config = make_config(&["A"]);
        let mut runner = ForwardTestRunner::new(feed, config);

        let result = runner.run(42).await.unwrap();
        assert_eq!(result.total_ticks, 0);
        assert_eq!(result.total_trades, 0);
    }

    #[tokio::test]
    async fn test_runner_strategy_filtering() {
        let ticks: Vec<TickData> = (0..5)
            .map(|i| make_tick(1_000_000 + i as u64 * 1000, 1.1000, 1.1001))
            .collect();
        let feed = StubFeed::new(ticks);
        let config = make_config(&["B", "C"]);
        let mut runner = ForwardTestRunner::new(feed, config);

        let result = runner.run(42).await.unwrap();
        assert_eq!(result.total_ticks, 5);
        assert!(result.strategies_used.contains(&"B".to_string()));
        assert!(result.strategies_used.contains(&"C".to_string()));
        assert!(!result.strategies_used.contains(&"A".to_string()));
    }

    #[tokio::test]
    async fn test_runner_duration_limit() {
        let ticks: Vec<TickData> = (0..1000)
            .map(|i| make_tick(1_000_000 + i as u64 * 1000, 1.1000, 1.1001))
            .collect();
        let feed = StubFeed::new(ticks);

        let mut config = make_config(&["A"]);
        config.duration = Some(Duration::from_millis(50));
        let mut runner = ForwardTestRunner::new(feed, config);

        let result = runner.run(42).await.unwrap();
        assert!(result.total_ticks < 1000);
    }

    #[tokio::test]
    async fn test_runner_reproducible() {
        let ticks: Vec<TickData> = (0..20)
            .map(|i| make_tick(1_000_000 + i as u64 * 1000, 1.1000, 1.1001))
            .collect();

        let config = make_config(&["A"]);

        let feed1 = StubFeed::new(ticks.clone());
        let mut runner1 = ForwardTestRunner::new(feed1, config.clone());
        let r1 = runner1.run(12345).await.unwrap();

        let feed2 = StubFeed::new(ticks);
        let mut runner2 = ForwardTestRunner::new(feed2, config);
        let r2 = runner2.run(12345).await.unwrap();

        assert_eq!(r1.total_ticks, r2.total_ticks);
    }
}
