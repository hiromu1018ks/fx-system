use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Datelike};
use chrono_tz::Tz;
use fx_core::observability::{
    l2_distance, softmax_entropy, AnomalyConfig, ObservabilityManager, PreFailureMetrics,
    RollingStats,
};
use fx_core::random::expand_u64_seed;
use fx_core::types::{Direction, EventTier, StrategyId, StreamId};
use fx_events::bus::PartitionedEventBus;
use fx_events::event::{Event, GenericEvent};
use fx_events::gap_detector::GapDetector;
use fx_events::header::EventHeader;
use fx_events::projector::{StateProjector, StateSnapshot};
use fx_events::proto;
use fx_events::runtime::{
    action_type, build_decision_event, build_trade_skip_event, proto_header, DecisionEventContext,
    RuntimeSequencer,
};
use fx_execution::gateway::{ExecutionGatewayConfig, ExecutionRequest};
use fx_gateway::market::TickData;
use fx_risk::barrier::DynamicRiskBarrier;
use fx_risk::global_position::GlobalPositionChecker;
use fx_risk::kill_switch::KillSwitch;
use fx_risk::lifecycle::{EpisodeSummary, LifecycleManager};
use fx_risk::limits::{CloseReason, HierarchicalRiskLimiter, RiskLimitsConfig};
use fx_strategy::bayesian_lr::QAction;
use fx_strategy::change_point::ChangePointDetector;
use fx_strategy::extractor::FeatureExtractor;
use fx_strategy::features::FeatureVector;
use fx_strategy::mc_eval::{McEvalConfig, McEvaluator, TerminalReason};
use fx_strategy::regime::RegimeCache;
use fx_strategy::thompson_sampling::{compute_dynamic_k, ThompsonDecision, ThompsonSamplingPolicy};
use prost::Message;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

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
    pub strategy_events_published: u64,
    pub state_snapshots_published: u64,
    pub duration_secs: f64,
    pub final_pnl: f64,
    pub strategies_used: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct PeriodicLimitTracker {
    day_key: Option<(i32, u32)>,
    week_key: Option<(i32, u32)>,
    month_key: Option<(i32, u32)>,
    day_realized_start: f64,
    week_realized_start: f64,
    month_realized_start: f64,
}

impl PeriodicLimitTracker {
    fn update(
        &mut self,
        tick_ns: u64,
        total_realized_pnl: f64,
        total_unrealized_pnl: f64,
        config: &RiskLimitsConfig,
    ) -> fx_events::projector::LimitStateData {
        let (day_key, week_key, month_key) = current_period_keys(tick_ns);

        if self.day_key != Some(day_key) {
            self.day_key = Some(day_key);
            self.day_realized_start = total_realized_pnl;
        }
        if self.week_key != Some(week_key) {
            self.week_key = Some(week_key);
            self.week_realized_start = total_realized_pnl;
        }
        if self.month_key != Some(month_key) {
            self.month_key = Some(month_key);
            self.month_realized_start = total_realized_pnl;
        }

        let daily_realized = total_realized_pnl - self.day_realized_start;
        let weekly_realized = total_realized_pnl - self.week_realized_start;
        let monthly_realized = total_realized_pnl - self.month_realized_start;
        let daily_mtm = daily_realized + total_unrealized_pnl;

        HierarchicalRiskLimiter::compute_limit_state(
            config,
            daily_mtm,
            daily_realized,
            weekly_realized,
            monthly_realized,
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum TradingHaltState {
    Daily { day_key: (i32, u32) },
    Weekly { week_key: (i32, u32) },
    Monthly { month_key: (i32, u32) },
}

impl TradingHaltState {
    fn is_active(self, tick_ns: u64) -> bool {
        let (day_key, week_key, month_key) = current_period_keys(tick_ns);
        match self {
            Self::Daily { day_key: halted_on } => halted_on == day_key,
            Self::Weekly {
                week_key: halted_on,
            } => halted_on == week_key,
            Self::Monthly {
                month_key: halted_on,
            } => halted_on == month_key,
        }
    }
}

struct StrategyRuntime {
    policy: ThompsonSamplingPolicy,
    change_point_detector: ChangePointDetector,
}

#[derive(Debug, Clone)]
struct RuntimeObservabilityState {
    execution_drift: RollingStats,
    q_value_adjustment_frequency: RollingStats,
    liquidity_evolvement: RollingStats,
    dynamic_cost_estimate_error: RollingStats,
    policy_entropy: f64,
    self_impact_ratio: f64,
    bayesian_posterior_drift: f64,
    last_action_scores: HashMap<StrategyId, [f64; 3]>,
}

impl Default for RuntimeObservabilityState {
    fn default() -> Self {
        Self {
            execution_drift: RollingStats::new(256),
            q_value_adjustment_frequency: RollingStats::new(256),
            liquidity_evolvement: RollingStats::new(256),
            dynamic_cost_estimate_error: RollingStats::new(256),
            policy_entropy: 0.0,
            self_impact_ratio: 0.0,
            bayesian_posterior_drift: 0.0,
            last_action_scores: HashMap::new(),
        }
    }
}

impl RuntimeObservabilityState {
    fn record_action_scores(&mut self, strategy_id: StrategyId, action_scores: [f64; 3]) {
        let adjustment = self
            .last_action_scores
            .insert(strategy_id, action_scores)
            .map(|previous| {
                let relative_change = action_scores
                    .iter()
                    .zip(previous.iter())
                    .map(|(current, prior)| (current - prior).abs() / prior.abs().max(1e-6))
                    .fold(0.0_f64, f64::max);
                if relative_change > 0.05 {
                    1.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        self.q_value_adjustment_frequency.update(adjustment);
    }

    fn record_liquidity_evolvement(&mut self, depth_change_rate: f64) {
        self.liquidity_evolvement.update(depth_change_rate.abs());
    }

    fn record_execution_fill(
        &mut self,
        execution_drift: f64,
        slippage: f64,
        estimated_dynamic_cost: Option<f64>,
    ) {
        self.execution_drift.update(execution_drift);
        if let Some(dynamic_cost) = estimated_dynamic_cost {
            self.dynamic_cost_estimate_error
                .update((dynamic_cost.abs() - slippage.abs()).abs());
        }
    }
}

fn mean_or_zero(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn current_period_keys(tick_ns: u64) -> ((i32, u32), (i32, u32), (i32, u32)) {
    let helsinki: Tz = chrono_tz::Europe::Helsinki;
    let secs = (tick_ns / 1_000_000_000) as i64;
    let nanos = (tick_ns % 1_000_000_000) as u32;
    let timestamp = DateTime::from_timestamp(secs, nanos)
        .unwrap_or_default()
        .with_timezone(&helsinki);
    let iso_week = timestamp.iso_week();
    (
        (timestamp.year(), timestamp.ordinal()),
        (iso_week.year(), iso_week.week()),
        (timestamp.year(), timestamp.month()),
    )
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
        let subscribed_symbols = self.subscribed_symbols();

        self.feed.connect().await?;
        self.feed.subscribe(&subscribed_symbols).await?;

        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, self.config.risk_config.max_position_lots, 1);
        let mut gap_detector = GapDetector::new(&bus, 1);
        let mut observability_manager = ObservabilityManager::new(AnomalyConfig::default());
        let mut limit_tracker = PeriodicLimitTracker::default();
        let mut halt_state: Option<TradingHaltState> = None;
        let mut market_sequence_id = 0_u64;
        let mut runtime_sequencer = RuntimeSequencer::new(1);

        let exec_config = ExecutionGatewayConfig::default();
        let mut paper_engine = PaperExecutionEngine::new(exec_config, seed);
        let mut rng = SmallRng::from_seed(expand_u64_seed(seed));

        let mut feature_extractor = FeatureExtractor::new(Default::default());
        let policy_config = fx_strategy::thompson_sampling::ThompsonSamplingConfig::default();

        let barrier_config = fx_risk::barrier::DynamicRiskBarrierConfig::default();
        let risk_barrier = DynamicRiskBarrier::new(barrier_config);
        let mut lifecycle = LifecycleManager::new(fx_risk::lifecycle::LifecycleConfig::default());
        let limits_config = self.config.risk_config.to_risk_limits_config();
        let position_config = fx_risk::global_position::GlobalPositionConfig::default();
        let kill_switch = KillSwitch::new(fx_risk::kill_switch::KillSwitchConfig::default());
        let mut regime_cache = RegimeCache::new(self.config.regime_config.clone());

        let mut total_ticks: u64 = 0;
        let mut total_decisions: u64 = 0;
        let mut total_trades: u64 = 0;
        let mut strategy_events_published: u64 = 0;
        let mut state_snapshots_published: u64 = 0;
        let mut runtime_observability = RuntimeObservabilityState::default();
        let mut posterior_snapshots: HashMap<String, Vec<f64>> = HashMap::new();
        let mut mc_evaluator = McEvaluator::new(McEvalConfig {
            reward: Default::default(),
        });

        let enabled_strategies = self.get_enabled_strategies();
        let mut strategy_runtimes: HashMap<StrategyId, StrategyRuntime> = enabled_strategies
            .iter()
            .copied()
            .map(|strategy_id| {
                let q_function = fx_strategy::bayesian_lr::QFunction::new(
                    FeatureVector::DIM,
                    1.0,
                    500,
                    1.0,
                    0.01,
                );
                (
                    strategy_id,
                    StrategyRuntime {
                        policy: ThompsonSamplingPolicy::new(q_function, policy_config.clone()),
                        change_point_detector: ChangePointDetector::new_default(FeatureVector::DIM),
                    },
                )
            })
            .collect();

        let mut last_mid_price: f64 = 0.0;
        let mut last_volatility: f64 = 0.0;

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
            let tick_ns = tick.timestamp_ns;
            let tick_mid = tick.mid();
            let tick_vol = if tick_mid > 0.0 {
                tick.spread() / tick_mid
            } else {
                0.0
            };
            last_mid_price = tick_mid;
            last_volatility = tick_vol;
            market_sequence_id = market_sequence_id.saturating_add(1);
            let market_event = self.tick_to_event(&tick, market_sequence_id);

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

            self.sync_limit_state(&mut projector, &mut limit_tracker, &limits_config, tick_ns);
            self.maybe_emit_snapshot_event(
                &mut runtime_sequencer,
                &projector,
                tick_ns,
                Some(market_event.header.event_id),
                false,
                &mut state_snapshots_published,
            );
            let snapshot = projector.snapshot().clone();
            let staleness_ms = snapshot.staleness_ms;

            let metrics = self.collect_pre_failure_metrics(
                &snapshot,
                &limits_config,
                &regime_cache,
                &paper_engine,
                &runtime_observability,
                kill_switch.stats().std_interval_ns,
            );
            observability_manager.tick(metrics, tick_ns);

            if halt_state.is_some_and(|state| state.is_active(tick_ns)) {
                for &strategy_id in &enabled_strategies {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "risk_limit_rejected",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                }
                self.tracker.update(
                    tick_ns,
                    snapshot.total_realized_pnl,
                    snapshot.total_unrealized_pnl,
                );
                debug!(
                    ts = tick_ns,
                    ?halt_state,
                    "Trading halted by hard loss limit"
                );
                continue;
            }
            halt_state = None;

            // Check kill switch
            if kill_switch.validate_order().is_err() {
                for &strategy_id in &enabled_strategies {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "kill_switch_masked",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                }
                debug!(ts = tick_ns, "Kill switch active, skipping");
                continue;
            }

            // Check gap detector halt
            if gap_detector.is_trading_halted() {
                for &strategy_id in &enabled_strategies {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "gap_detected",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                }
                debug!(ts = tick_ns, "Trading halted due to severe gap");
                continue;
            }

            // Check risk barrier (staleness)
            if risk_barrier.validate_order(staleness_ms).is_err() {
                for &strategy_id in &enabled_strategies {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "staleness_rejected",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                }
                debug!(ts = tick_ns, staleness_ms, "Risk barrier blocked");
                continue;
            }

            let (limit_result, close_reason) =
                HierarchicalRiskLimiter::evaluate(&limits_config, &snapshot.limit_state);
            if let Some(reason) = close_reason {
                self.force_close_open_positions(
                    &mut runtime_sequencer,
                    &mut projector,
                    &mut paper_engine,
                    &mut runtime_observability,
                    &mut limit_tracker,
                    &limits_config,
                    tick_ns,
                    tick.symbol.clone(),
                    &mut total_trades,
                    tick_mid,
                    tick_vol,
                )?;
                // End MC episodes for all strategies that were force-closed
                let terminal = match reason {
                    CloseReason::DailyRealizedHalt => TerminalReason::DailyHardLimit,
                    CloseReason::WeeklyHalt => TerminalReason::WeeklyHardLimit,
                    CloseReason::MonthlyHalt => TerminalReason::MonthlyHardLimit,
                    CloseReason::WeekendHalt => TerminalReason::WeekendHalt,
                };
                for &sid in &enabled_strategies {
                    if mc_evaluator.has_active_episode(sid) {
                        let q_fn = strategy_runtimes.get_mut(&sid).map(|r| r.policy.q_function_mut());
                        if let Some(q) = q_fn {
                            let _ = mc_evaluator.end_episode_and_update(sid, terminal, tick_ns, q);
                        }
                    }
                }
                self.maybe_emit_snapshot_event(
                    &mut runtime_sequencer,
                    &projector,
                    tick_ns,
                    Some(market_event.header.event_id),
                    true,
                    &mut state_snapshots_published,
                );
                halt_state = Some(match reason {
                    CloseReason::DailyRealizedHalt => TradingHaltState::Daily {
                        day_key: current_period_keys(tick_ns).0,
                    },
                    CloseReason::WeeklyHalt => TradingHaltState::Weekly {
                        week_key: current_period_keys(tick_ns).1,
                    },
                    CloseReason::MonthlyHalt => TradingHaltState::Monthly {
                        month_key: current_period_keys(tick_ns).2,
                    },
                    CloseReason::WeekendHalt => TradingHaltState::Daily {
                        day_key: current_period_keys(tick_ns).0,
                    },
                });
                self.tracker.update(
                    tick_ns,
                    projector.snapshot().total_realized_pnl,
                    projector.snapshot().total_unrealized_pnl,
                );
                continue;
            }
            if limit_result.is_err() {
                for &strategy_id in &enabled_strategies {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "risk_limit_rejected",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                }
                debug!(ts = tick_ns, "Loss limit reached");
                continue;
            }

            feature_extractor.process_market_event(&market_event);

            // Update regime detection (ONNX model if available, otherwise heuristic)
            {
                let base_features =
                    feature_extractor.extract(&market_event, &snapshot, StrategyId::A, tick_ns);
                let n_regimes = regime_cache.config().n_regimes;
                let feature_dim = regime_cache.config().feature_dim;

                if regime_cache.has_onnx_model() {
                    let phi = base_features.flattened_for_regime_model();
                    if let Some(posterior) = regime_cache.predict_onnx(&phi) {
                        regime_cache.update(posterior, tick_ns);
                        if phi.len() == feature_dim {
                            regime_cache.update_drift(&phi);
                        }
                    }
                } else {
                    let spread_z = base_features.spread_zscore.abs();
                    let rv = base_features.realized_volatility;
                    let vol_ratio = base_features.volatility_ratio;

                    let calm_score = -(spread_z + rv * 10.0 + vol_ratio * 2.0);
                    let normal_score = -((spread_z - 1.0).abs()
                        + (rv - 0.01).abs() * 10.0
                        + (vol_ratio - 1.0).abs() * 2.0);
                    let turbulent_score = -((spread_z - 2.0).abs()
                        + (rv - 0.03).abs() * 10.0
                        + (vol_ratio - 2.0).abs() * 2.0);
                    let crisis_score = -(spread_z - 3.0).abs()
                        - (rv - 0.05).abs() * 10.0
                        - (vol_ratio - 3.0).abs() * 2.0;

                    let mut scores = vec![calm_score, normal_score, turbulent_score, crisis_score];
                    scores.resize(n_regimes, 0.0);

                    let max_score = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let exp_scores: Vec<f64> =
                        scores.iter().map(|s| (s - max_score).exp()).collect();
                    let sum_exp: f64 = exp_scores.iter().sum();
                    let posterior: Vec<f64> = exp_scores.iter().map(|e| e / sum_exp).collect();

                    regime_cache.update(posterior, tick_ns);
                }
            }

            if regime_cache.state().is_unknown() {
                for &strategy_id in &enabled_strategies {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "unknown_regime",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                }
                debug!(ts = tick_ns, "Trading halted due to unknown regime");
                self.tracker.update(
                    tick_ns,
                    projector.snapshot().total_realized_pnl,
                    projector.snapshot().total_unrealized_pnl,
                );
                continue;
            }

            let snapshot = projector.snapshot().clone();

            // Phase 1: Close positions that exceeded per-strategy MAX_HOLD_TIME
            let max_hold_ms: HashMap<StrategyId, u64> = [
                (StrategyId::A, 30_000),
                (StrategyId::B, 300_000),
                (StrategyId::C, 600_000),
            ]
            .into_iter()
            .collect();
            for &sid in &enabled_strategies {
                let max_ns = max_hold_ms
                    .get(&sid)
                    .copied()
                    .unwrap_or(300_000)
                    * 1_000_000;
                let should_close = snapshot
                    .positions
                    .get(&sid)
                    .map(|p| {
                        p.is_open()
                            && p.entry_timestamp_ns > 0
                            && tick_ns.saturating_sub(p.entry_timestamp_ns) >= max_ns
                    })
                    .unwrap_or(false);
                if should_close {
                    if let Some((_direction, _lots)) = self.close_strategy_position_forward(
                        &mut runtime_sequencer,
                        &mut projector,
                        &mut paper_engine,
                        &mut runtime_observability,
                        &mut limit_tracker,
                        &limits_config,
                        tick_ns,
                        tick.symbol.clone(),
                        sid,
                        "MAX_HOLD_TIME",
                        tick_mid,
                        tick_vol,
                    ) {
                        total_trades += 1;
                        // End MC episode and update Q-function
                        if mc_evaluator.has_active_episode(sid) {
                            let q_fn = strategy_runtimes.get_mut(&sid).map(|r| r.policy.q_function_mut());
                            if let Some(q) = q_fn {
                                let _result = mc_evaluator.end_episode_and_update(
                                    sid,
                                    TerminalReason::MaxHoldTimeExceeded,
                                    tick_ns,
                                    q,
                                );
                            }
                        }
                    }
                }
            }
            let snapshot = projector.snapshot().clone();

            let mut strategy_decisions = Vec::new();
            for &strategy_id in &enabled_strategies {
                let features =
                    feature_extractor.extract(&market_event, &snapshot, strategy_id, tick_ns);
                let Some(runtime) = strategy_runtimes.get_mut(&strategy_id) else {
                    continue;
                };

                runtime.change_point_detector.observe_and_respond(
                    &features.flattened(),
                    tick_ns,
                    runtime.policy.q_function_mut(),
                );

                // Check lifecycle
                if !lifecycle.is_alive(strategy_id) {
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(market_event.header.event_id),
                        tick_ns,
                        strategy_id,
                        "strategy_culled",
                        0.0,
                        0.0,
                        &snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                    continue;
                }

                lifecycle.validate_order(strategy_id).ok();

                let decision = runtime.policy.decide(
                    &features,
                    &snapshot,
                    strategy_id,
                    tick.latency_ms,
                    &mut rng,
                );
                // Record MC transition if episode is active
                if mc_evaluator.has_active_episode(strategy_id) {
                    let q_action = match &decision.action {
                        fx_strategy::policy::Action::Buy(_) => QAction::Buy,
                        fx_strategy::policy::Action::Sell(_) => QAction::Sell,
                        fx_strategy::policy::Action::Hold => QAction::Hold,
                    };
                    let vol_sq = if tick.mid() > 0.0 {
                        (tick.spread() / tick.mid()).powi(2)
                    } else {
                        0.0
                    };
                    mc_evaluator.record_transition(
                        strategy_id,
                        tick_ns,
                        q_action,
                        features.flattened(),
                        projector.snapshot(),
                        vol_sq,
                    );
                }
                total_decisions += 1;
                strategy_decisions.push((strategy_id, features, decision));
            }

            self.update_runtime_observability(
                &strategy_decisions,
                &strategy_runtimes,
                &mut posterior_snapshots,
                &mut runtime_observability,
            );

            let all_q: HashMap<StrategyId, f64> = strategy_decisions
                .iter()
                .map(|(strategy_id, _, decision)| (*strategy_id, decision.q_point))
                .collect();

            // Sort by Q-value descending for priority (matching backtest behavior)
            strategy_decisions.sort_by(|a, b| {
                b.2.q_sampled
                    .total_cmp(&a.2.q_sampled)
                    .then_with(|| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)))
            });

            for (strategy_id, features, decision) in strategy_decisions {
                let decision_snapshot = projector.snapshot().clone();
                let decision_event_id = self.emit_decision_event(
                    &mut runtime_sequencer,
                    &mut projector,
                    Some(market_event.header.event_id),
                    tick_ns,
                    strategy_id,
                    &features,
                    &decision_snapshot,
                    &decision,
                    &regime_cache,
                    tick.latency_ms,
                );
                strategy_events_published = strategy_events_published.saturating_add(1);

                let (direction, lots) = match &decision.action {
                    fx_strategy::policy::Action::Buy(l) => (Direction::Buy, *l),
                    fx_strategy::policy::Action::Sell(l) => (Direction::Sell, *l),
                    fx_strategy::policy::Action::Hold => continue,
                };

                // Signal-driven exit or already_in_position guard
                let pos_size = snapshot
                    .positions
                    .get(&strategy_id)
                    .map(|p| p.size)
                    .unwrap_or(0.0);
                if pos_size.abs() > f64::EPSILON {
                    let is_closing = match direction {
                        Direction::Buy => pos_size < -f64::EPSILON,
                        Direction::Sell => pos_size > f64::EPSILON,
                    };
                    if is_closing {
                        // Signal-driven exit: Q-function selected opposite direction
                        if let Some((_close_dir, _close_lots)) =
                            self.close_strategy_position_forward(
                                &mut runtime_sequencer,
                                &mut projector,
                                &mut paper_engine,
                                &mut runtime_observability,
                                &mut limit_tracker,
                                &limits_config,
                                tick_ns,
                                tick.symbol.clone(),
                                strategy_id,
                                "TRIGGER_EXIT",
                                tick_mid,
                                tick_vol,
                            )
                        {
                            total_trades += 1;
                            // End MC episode and update Q-function
                            if mc_evaluator.has_active_episode(strategy_id) {
                                let q_fn = strategy_runtimes.get_mut(&strategy_id).map(|r| r.policy.q_function_mut());
                                if let Some(q) = q_fn {
                                    let _result = mc_evaluator.end_episode_and_update(
                                        strategy_id,
                                        TerminalReason::PositionClosed,
                                        tick_ns,
                                        q,
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    // Same direction — already in position
                    let skip_snapshot = projector.snapshot().clone();
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(decision_event_id),
                        tick_ns,
                        strategy_id,
                        "already_in_position",
                        decision.q_sampled,
                        decision.q_point,
                        &skip_snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                    continue;
                }

                if lots == 0 {
                    let skip_snapshot = projector.snapshot().clone();
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(decision_event_id),
                        tick_ns,
                        strategy_id,
                        "zero_effective_lot",
                        decision.q_sampled,
                        decision.q_point,
                        &skip_snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
                    continue;
                }

                // Check global position constraint (static method)
                let effective_lots = match GlobalPositionChecker::validate_order(
                    &position_config,
                    &snapshot,
                    strategy_id,
                    direction,
                    lots as f64,
                    decision.q_sampled,
                    &all_q,
                ) {
                    Ok(r) => r.effective_lot.max(0.0) as u64,
                    Err(_) => {
                        let skip_snapshot = projector.snapshot().clone();
                        self.emit_trade_skip_event(
                            &mut runtime_sequencer,
                            &mut projector,
                            Some(decision_event_id),
                            tick_ns,
                            strategy_id,
                            "global_position_rejected",
                            decision.q_sampled,
                            decision.q_point,
                            &skip_snapshot,
                            &regime_cache,
                        );
                        strategy_events_published = strategy_events_published.saturating_add(1);
                        continue;
                    }
                };

                if effective_lots == 0 {
                    let skip_snapshot = projector.snapshot().clone();
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(decision_event_id),
                        tick_ns,
                        strategy_id,
                        "zero_effective_lot",
                        decision.q_sampled,
                        decision.q_point,
                        &skip_snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
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
                    lots: effective_lots,
                    strategy_id,
                    current_mid_price: mid_price,
                    volatility,
                    expected_profit: decision.q_sampled,
                    symbol: tick.symbol.clone(),
                    timestamp_ns: tick_ns,
                    time_urgent: false,
                };

                let realized_before = projector
                    .snapshot()
                    .positions
                    .get(&strategy_id)
                    .map(|position| position.realized_pnl)
                    .unwrap_or_default();
                let (paper_result, exec_result) = match paper_engine.simulate(&request) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Paper execution error: {}", e);
                        let skip_snapshot = projector.snapshot().clone();
                        self.emit_trade_skip_event(
                            &mut runtime_sequencer,
                            &mut projector,
                            Some(decision_event_id),
                            tick_ns,
                            strategy_id,
                            "execution_rejected",
                            decision.q_sampled,
                            decision.q_point,
                            &skip_snapshot,
                            &regime_cache,
                        );
                        strategy_events_published = strategy_events_published.saturating_add(1);
                        continue;
                    }
                };

                // Build execution event and feed back to projector
                let exec_event = self.build_runtime_execution_event(
                    &mut runtime_sequencer,
                    &paper_engine,
                    &request,
                    &exec_result,
                    Some(decision_event_id),
                );
                if let Err(e) = projector.process_execution_for_strategy(&exec_event, strategy_id) {
                    warn!("Projector execution error: {}", e);
                }
                self.sync_limit_state(&mut projector, &mut limit_tracker, &limits_config, tick_ns);
                self.maybe_emit_snapshot_event(
                    &mut runtime_sequencer,
                    &projector,
                    tick_ns,
                    Some(exec_event.header.event_id),
                    true,
                    &mut state_snapshots_published,
                );
                let realized_after = projector
                    .snapshot()
                    .positions
                    .get(&strategy_id)
                    .map(|position| position.realized_pnl)
                    .unwrap_or_default();

                // Record episode in lifecycle
                let pnl = if paper_result.fill_price.is_some() {
                    total_trades += 1;
                    runtime_observability.record_execution_fill(
                        exec_result.fill_price - exec_result.requested_price,
                        exec_result.slippage,
                        Some(features.dynamic_cost),
                    );
                    self.tracker.record_trade(realized_after - realized_before);
                    self.tracker.record_execution_drift(
                        exec_result.fill_price - exec_result.requested_price,
                    );
                    // Start MC episode on position open
                    if !mc_evaluator.has_active_episode(strategy_id) {
                        let equity = projector
                            .snapshot()
                            .positions
                            .get(&strategy_id)
                            .map(|p| p.realized_pnl + p.unrealized_pnl)
                            .unwrap_or(0.0);
                        mc_evaluator.start_episode(strategy_id, tick_ns, equity);
                    }
                    realized_after - realized_before
                } else {
                    let skip_snapshot = projector.snapshot().clone();
                    self.emit_trade_skip_event(
                        &mut runtime_sequencer,
                        &mut projector,
                        Some(decision_event_id),
                        tick_ns,
                        strategy_id,
                        "execution_rejected",
                        decision.q_sampled,
                        decision.q_point,
                        &skip_snapshot,
                        &regime_cache,
                    );
                    strategy_events_published = strategy_events_published.saturating_add(1);
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

        if projector
            .snapshot()
            .positions
            .values()
            .any(|position| position.is_open())
        {
            let shutdown_ts = projector.snapshot().last_market_data_ns.max(1);
            self.force_close_open_positions(
                &mut runtime_sequencer,
                &mut projector,
                &mut paper_engine,
                &mut runtime_observability,
                &mut limit_tracker,
                &limits_config,
                shutdown_ts,
                self.default_symbol(),
                &mut total_trades,
                last_mid_price,
                last_volatility,
            )?;
            // End MC episodes for shutdown-closed positions
            for &sid in &enabled_strategies {
                if mc_evaluator.has_active_episode(sid) {
                    let q_fn = strategy_runtimes.get_mut(&sid).map(|r| r.policy.q_function_mut());
                    if let Some(q) = q_fn {
                        let _ = mc_evaluator.end_episode_and_update(
                            sid,
                            TerminalReason::PositionClosed,
                            shutdown_ts,
                            q,
                        );
                    }
                }
            }
            self.maybe_emit_snapshot_event(
                &mut runtime_sequencer,
                &projector,
                shutdown_ts,
                None,
                true,
                &mut state_snapshots_published,
            );
            let snap = projector.snapshot();
            self.tracker.update(
                shutdown_ts,
                snap.total_realized_pnl,
                snap.total_unrealized_pnl,
            );
        }

        self.feed.disconnect().await?;

        let elapsed = start.elapsed();
        let mut strategies_used: Vec<_> = self.config.enabled_strategies.iter().cloned().collect();
        strategies_used.sort();
        let result = ForwardTestResult {
            total_ticks,
            total_decisions,
            total_trades,
            strategy_events_published,
            state_snapshots_published,
            duration_secs: elapsed.as_secs_f64(),
            final_pnl: self.tracker.snapshot().cumulative_pnl,
            strategies_used,
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

    fn tick_to_event(&self, tick: &TickData, sequence_id: u64) -> GenericEvent {
        let proto = tick.to_proto();
        let payload = proto.encode_to_vec();
        let header = EventHeader::new(StreamId::Market, sequence_id, EventTier::Tier3Raw);
        GenericEvent::new(
            EventHeader {
                timestamp_ns: tick.timestamp_ns,
                ..header
            },
            payload,
        )
    }

    fn subscribed_symbols(&self) -> Vec<String> {
        match &self.config.data_source {
            crate::feed::DataSourceConfig::ExternalApi { symbols, .. } => symbols.clone(),
            crate::feed::DataSourceConfig::Recorded { .. } => Vec::new(),
        }
    }

    fn default_symbol(&self) -> String {
        self.subscribed_symbols()
            .first()
            .cloned()
            .unwrap_or_else(|| "EUR/USD".to_string())
    }

    fn decision_event_context(
        &self,
        strategy_id: StrategyId,
        features: &FeatureVector,
        snapshot: &fx_events::projector::StateSnapshot,
        decision: &fx_strategy::thompson_sampling::ThompsonDecision,
        direction: Option<Direction>,
        lots: u64,
        latency_ms: f64,
    ) -> DecisionEventContext {
        use fx_strategy::bayesian_lr::QAction;

        let position = snapshot.positions.get(&strategy_id);
        let position_before = position.map(|p| p.size).unwrap_or_default();
        let signed_lots = match direction {
            Some(Direction::Buy) => lots as f64,
            Some(Direction::Sell) => -(lots as f64),
            None => 0.0,
        };
        let regime_stability = (1.0 - features.volatility_ratio.min(1.0)).max(0.0);
        let dynamic_k = compute_dynamic_k(1.0, features.realized_volatility, regime_stability);
        let sigma_execution = features.recent_slippage.abs();
        let sigma_latency = latency_ms;

        DecisionEventContext {
            feature_vector: features.flattened(),
            q_buy: *decision.all_sampled_q.get(&QAction::Buy).unwrap_or(&0.0),
            q_sell: *decision.all_sampled_q.get(&QAction::Sell).unwrap_or(&0.0),
            q_hold: *decision.all_sampled_q.get(&QAction::Hold).unwrap_or(&0.0),
            q_selected: decision.q_sampled,
            posterior_mean: decision.q_point,
            posterior_std: decision.posterior_std,
            sampled_q: decision.q_sampled,
            position_size: position_before,
            entry_price: position.map(|p| p.entry_price).unwrap_or_default(),
            pnl_unrealized: position.map(|p| p.unrealized_pnl).unwrap_or_default(),
            holding_time_ms: position
                .map(|p| p.holding_time_ms(snapshot.last_market_data_ns) as f64)
                .unwrap_or_default(),
            staleness_ms: snapshot.staleness_ms as f64,
            lot_multiplier: snapshot.lot_multiplier,
            daily_pnl: snapshot.limit_state.daily_pnl_realized,
            regime_posterior: Vec::new(),
            regime_entropy: 0.0,
            q_tilde_final_values: vec![
                *decision.all_sampled_q.get(&QAction::Buy).unwrap_or(&0.0),
                *decision.all_sampled_q.get(&QAction::Sell).unwrap_or(&0.0),
                *decision.all_sampled_q.get(&QAction::Hold).unwrap_or(&0.0),
            ],
            q_point_selected: decision.q_point,
            q_tilde_selected: decision.q_sampled,
            sigma_model: decision.posterior_std,
            sigma_execution,
            sigma_latency,
            sigma_non_model: (sigma_execution.powi(2) + sigma_latency.powi(2)).sqrt(),
            dynamic_k,
            position_before,
            position_after: position_before + signed_lots,
            position_max_limit: snapshot.global_position_limit,
            velocity_limit: lots as f64,
            dynamic_cost: features.dynamic_cost,
            latency_penalty: latency_ms,
        }
    }

    fn emit_decision_event(
        &self,
        sequencer: &mut RuntimeSequencer,
        projector: &mut StateProjector,
        parent_event_id: Option<Uuid>,
        tick_ns: u64,
        strategy_id: StrategyId,
        features: &FeatureVector,
        snapshot: &fx_events::projector::StateSnapshot,
        decision: &fx_strategy::thompson_sampling::ThompsonDecision,
        regime_cache: &RegimeCache,
        latency_ms: f64,
    ) -> Uuid {
        let (direction, lots) = match decision.action {
            fx_strategy::policy::Action::Buy(lots) => (Some(Direction::Buy), lots),
            fx_strategy::policy::Action::Sell(lots) => (Some(Direction::Sell), lots),
            fx_strategy::policy::Action::Hold => (None, 0),
        };
        let header = sequencer.next_header(
            StreamId::Strategy,
            tick_ns,
            EventTier::Tier2Derived,
            parent_event_id,
        );
        let mut context = self.decision_event_context(
            strategy_id,
            features,
            snapshot,
            decision,
            direction,
            lots,
            latency_ms,
        );
        context.regime_posterior = regime_cache.state().posterior().to_vec();
        context.regime_entropy = regime_cache.state().entropy();
        let event = build_decision_event(
            header.clone(),
            strategy_id,
            action_type(direction),
            lots,
            context,
            if matches!(decision.action, fx_strategy::policy::Action::Hold) {
                Some("hold")
            } else {
                None
            },
        );
        let _ = projector.process_event(&event);
        header.event_id
    }

    fn emit_trade_skip_event(
        &self,
        sequencer: &mut RuntimeSequencer,
        projector: &mut StateProjector,
        parent_event_id: Option<Uuid>,
        tick_ns: u64,
        strategy_id: StrategyId,
        reason: &str,
        q_selected: f64,
        q_point_selected: f64,
        snapshot: &fx_events::projector::StateSnapshot,
        regime_cache: &RegimeCache,
    ) {
        let header = sequencer.next_header(
            StreamId::Strategy,
            tick_ns,
            EventTier::Tier2Derived,
            parent_event_id,
        );
        let event = build_trade_skip_event(
            header,
            strategy_id,
            reason,
            q_selected,
            q_point_selected,
            snapshot.staleness_ms as f64,
            regime_cache.state().entropy(),
            snapshot.lot_multiplier,
        );
        let _ = projector.process_event(&event);
    }

    fn maybe_emit_snapshot_event(
        &self,
        sequencer: &mut RuntimeSequencer,
        projector: &StateProjector,
        tick_ns: u64,
        parent_event_id: Option<Uuid>,
        force: bool,
        published: &mut u64,
    ) {
        const SNAPSHOT_INTERVAL: u64 = 100;
        if !force && projector.state_version() % SNAPSHOT_INTERVAL != 0 {
            return;
        }
        let header = sequencer.next_header(
            StreamId::State,
            tick_ns,
            EventTier::Tier1Critical,
            parent_event_id,
        );
        let event = projector.build_snapshot_event(header);
        assert!(
            !event.payload.is_empty(),
            "snapshot event payload must not be empty"
        );
        *published = published.saturating_add(1);
    }

    fn update_runtime_observability(
        &self,
        strategy_decisions: &[(StrategyId, FeatureVector, ThompsonDecision)],
        strategy_runtimes: &HashMap<StrategyId, StrategyRuntime>,
        posterior_snapshots: &mut HashMap<String, Vec<f64>>,
        runtime_observability: &mut RuntimeObservabilityState,
    ) {
        let mut entropies = Vec::new();
        let mut impact_ratios = Vec::new();
        let mut drifts = Vec::new();
        let mut liquidity_changes = Vec::new();

        for (strategy_id, features, decision) in strategy_decisions {
            let action_scores = [
                decision.all_point_q[&fx_strategy::bayesian_lr::QAction::Buy],
                decision.all_point_q[&fx_strategy::bayesian_lr::QAction::Sell],
                decision.all_point_q[&fx_strategy::bayesian_lr::QAction::Hold],
            ];
            runtime_observability.record_action_scores(*strategy_id, action_scores);
            entropies.push(softmax_entropy(&action_scores));
            liquidity_changes.push(features.depth_change_rate.abs());

            let max_abs_q = action_scores
                .iter()
                .map(|value| value.abs())
                .fold(0.0_f64, f64::max);
            if max_abs_q > f64::EPSILON {
                impact_ratios.push(features.self_impact.abs() / max_abs_q);
            }

            let Some(runtime) = strategy_runtimes.get(strategy_id) else {
                continue;
            };
            for &action in fx_strategy::bayesian_lr::QAction::all() {
                let key = format!("{strategy_id:?}:{action:?}");
                let weights: Vec<f64> = runtime
                    .policy
                    .q_function()
                    .model(action)
                    .weights()
                    .iter()
                    .copied()
                    .collect();
                let drift = posterior_snapshots
                    .get(&key)
                    .map(|previous| l2_distance(previous, &weights))
                    .unwrap_or(0.0);
                posterior_snapshots.insert(key, weights);
                drifts.push(drift);
            }
        }

        runtime_observability.policy_entropy = mean_or_zero(&entropies);
        runtime_observability.self_impact_ratio = mean_or_zero(&impact_ratios);
        runtime_observability.bayesian_posterior_drift = mean_or_zero(&drifts);
        runtime_observability.record_liquidity_evolvement(mean_or_zero(&liquidity_changes));
    }

    fn collect_pre_failure_metrics(
        &self,
        snapshot: &StateSnapshot,
        limits_config: &RiskLimitsConfig,
        regime_cache: &RegimeCache,
        paper_engine: &PaperExecutionEngine,
        runtime_observability: &RuntimeObservabilityState,
        latency_variance: f64,
    ) -> PreFailureMetrics {
        let regime_state = regime_cache.state();
        let daily_pnl_vs_limit = if limits_config.max_daily_loss_mtm.abs() > f64::EPSILON {
            snapshot.limit_state.daily_pnl_mtm / limits_config.max_daily_loss_mtm.abs()
        } else {
            0.0
        };
        let weekly_pnl_vs_limit = if limits_config.max_weekly_loss.abs() > f64::EPSILON {
            snapshot.limit_state.weekly_pnl / limits_config.max_weekly_loss.abs()
        } else {
            0.0
        };
        let monthly_pnl_vs_limit = if limits_config.max_monthly_loss.abs() > f64::EPSILON {
            snapshot.limit_state.monthly_pnl / limits_config.max_monthly_loss.abs()
        } else {
            0.0
        };
        let position_constraint_saturation_rate =
            if snapshot.global_position_limit.abs() > f64::EPSILON {
                snapshot.global_position.abs() / snapshot.global_position_limit.abs()
            } else {
                0.0
            };
        let gateway = paper_engine.gateway();

        PreFailureMetrics {
            rolling_variance_latency: latency_variance,
            feature_distribution_kl_divergence: regime_state.kl_divergence(),
            q_value_adjustment_frequency: runtime_observability.q_value_adjustment_frequency.mean(),
            execution_drift_trend: runtime_observability.execution_drift.mean(),
            latency_risk_trend: snapshot.staleness_ms as f64,
            self_impact_ratio: runtime_observability.self_impact_ratio,
            liquidity_evolvement: runtime_observability.liquidity_evolvement.mean(),
            policy_entropy: runtime_observability.policy_entropy,
            regime_posterior_entropy: regime_state.entropy(),
            hidden_liquidity_sigma: gateway
                .active_hidden_liquidity_sigma()
                .max(runtime_observability.execution_drift.std()),
            position_constraint_saturation_rate,
            last_look_rejection_rate: gateway.aggregate_rejection_rate(),
            dynamic_cost_estimate_error: runtime_observability.dynamic_cost_estimate_error.mean(),
            lp_adversarial_score: gateway.active_lp_adversarial_score(),
            daily_pnl_vs_limit,
            weekly_pnl_vs_limit,
            monthly_pnl_vs_limit,
            lp_recalibration_progress: gateway.lp_recalibration_progress(),
            bayesian_posterior_drift: runtime_observability.bayesian_posterior_drift,
        }
    }

    fn build_runtime_execution_event(
        &self,
        sequencer: &mut RuntimeSequencer,
        paper_engine: &PaperExecutionEngine,
        request: &ExecutionRequest,
        exec_result: &fx_execution::gateway::ExecutionResult,
        parent_event_id: Option<Uuid>,
    ) -> GenericEvent {
        let mut event = paper_engine.build_execution_event(request, exec_result);
        let header = sequencer.next_header(
            StreamId::Execution,
            request.timestamp_ns,
            EventTier::Tier1Critical,
            parent_event_id,
        );
        let mut payload = proto::ExecutionEventPayload::decode(event.payload_bytes())
            .expect("paper execution event payload must decode");
        payload.header = Some(proto_header(&header));
        event.header = header;
        event.payload = payload.encode_to_vec();
        event
    }

    fn sync_limit_state(
        &self,
        projector: &mut StateProjector,
        limit_tracker: &mut PeriodicLimitTracker,
        limits_config: &RiskLimitsConfig,
        tick_ns: u64,
    ) {
        let snapshot = projector.snapshot();
        let limit_state = limit_tracker.update(
            tick_ns,
            snapshot.total_realized_pnl,
            snapshot.total_unrealized_pnl,
            limits_config,
        );
        projector.update_limit_state(limit_state);
    }

    fn force_close_open_positions(
        &mut self,
        sequencer: &mut RuntimeSequencer,
        projector: &mut StateProjector,
        paper_engine: &mut PaperExecutionEngine,
        runtime_observability: &mut RuntimeObservabilityState,
        limit_tracker: &mut PeriodicLimitTracker,
        limits_config: &RiskLimitsConfig,
        tick_ns: u64,
        symbol: String,
        total_trades: &mut u64,
        mid_price: f64,
        volatility: f64,
    ) -> Result<()> {
        let mut open_positions: Vec<(StrategyId, f64)> = projector
            .snapshot()
            .positions
            .iter()
            .filter_map(|(strategy_id, position)| {
                if position.is_open() {
                    Some((*strategy_id, position.size))
                } else {
                    None
                }
            })
            .collect();
        open_positions.sort_by_key(|(sid, _)| format!("{:?}", sid));

        for (strategy_id, size) in open_positions {
            let lots = size.abs().round() as u64;
            if lots == 0 {
                continue;
            }

            let direction = if size > 0.0 {
                Direction::Sell
            } else {
                Direction::Buy
            };
            let realized_before = projector
                .snapshot()
                .positions
                .get(&strategy_id)
                .map(|position| position.realized_pnl)
                .unwrap_or_default();
            let request = ExecutionRequest {
                direction,
                lots,
                strategy_id,
                current_mid_price: mid_price,
                volatility,
                expected_profit: 0.0,
                symbol: symbol.clone(),
                timestamp_ns: tick_ns,
                time_urgent: true,
            };
            let (paper_result, exec_result) = paper_engine.simulate(&request)?;
            let exec_event = self.build_runtime_execution_event(
                sequencer,
                paper_engine,
                &request,
                &exec_result,
                None,
            );
            if let Err(error) = projector.process_execution_for_strategy(&exec_event, strategy_id) {
                warn!(?error, ?strategy_id, "Projector close-out error");
                continue;
            }
            self.sync_limit_state(projector, limit_tracker, limits_config, tick_ns);
            let realized_after = projector
                .snapshot()
                .positions
                .get(&strategy_id)
                .map(|position| position.realized_pnl)
                .unwrap_or_default();
            if paper_result.fill_price.is_some() {
                *total_trades += 1;
                runtime_observability.record_execution_fill(
                    exec_result.fill_price - exec_result.requested_price,
                    exec_result.slippage,
                    None,
                );
                self.tracker.record_trade(realized_after - realized_before);
                self.tracker
                    .record_execution_drift(exec_result.fill_price - exec_result.requested_price);
            }
        }

        Ok(())
    }

    /// Close a single strategy's open position (used for MAX_HOLD_TIME and signal-driven exits).
    fn close_strategy_position_forward(
        &mut self,
        sequencer: &mut RuntimeSequencer,
        projector: &mut StateProjector,
        paper_engine: &mut PaperExecutionEngine,
        runtime_observability: &mut RuntimeObservabilityState,
        limit_tracker: &mut PeriodicLimitTracker,
        limits_config: &RiskLimitsConfig,
        tick_ns: u64,
        symbol: String,
        strategy_id: StrategyId,
        close_reason: &str,
        mid_price: f64,
        volatility: f64,
    ) -> Option<(Direction, u64)> {
        let snap = projector.snapshot();
        let pos = snap.positions.get(&strategy_id)?;
        if !pos.is_open() {
            return None;
        }

        let direction = if pos.size > 0.0 {
            Direction::Sell
        } else {
            Direction::Buy
        };
        let lots = pos.size.abs().round() as u64;
        if lots == 0 {
            return None;
        }

        let realized_before = projector
            .snapshot()
            .positions
            .get(&strategy_id)
            .map(|p| p.realized_pnl)
            .unwrap_or_default();

        let request = ExecutionRequest {
            direction,
            lots,
            strategy_id,
            current_mid_price: mid_price,
            volatility,
            expected_profit: 0.0,
            symbol: symbol.clone(),
            timestamp_ns: tick_ns,
            time_urgent: true,
        };

        let (paper_result, exec_result) = match paper_engine.simulate(&request) {
            Ok(r) => r,
            Err(e) => {
                warn!(?e, ?strategy_id, "Close execution error");
                return None;
            }
        };

        let exec_event =
            self.build_runtime_execution_event(sequencer, paper_engine, &request, &exec_result, None);
        if let Err(e) = projector.process_execution_for_strategy(&exec_event, strategy_id) {
            warn!(?e, ?strategy_id, "Projector close-out error");
            return None;
        }
        self.sync_limit_state(projector, limit_tracker, limits_config, tick_ns);

        let realized_after = projector
            .snapshot()
            .positions
            .get(&strategy_id)
            .map(|p| p.realized_pnl)
            .unwrap_or_default();

        if paper_result.fill_price.is_some() {
            runtime_observability.record_execution_fill(
                exec_result.fill_price - exec_result.requested_price,
                exec_result.slippage,
                None,
            );
            self.tracker.record_trade(realized_after - realized_before);
            self.tracker
                .record_execution_drift(exec_result.fill_price - exec_result.requested_price);
        }

        debug!(
            ?strategy_id,
            ?direction,
            lots,
            close_reason,
            pnl = realized_after - realized_before,
            "Position closed"
        );

        Some((direction, lots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::feed::DataSourceConfig;
    use std::collections::VecDeque;
    use std::time::Duration;

    #[test]
    fn test_runtime_observability_tracks_non_placeholder_metrics() {
        let mut state = RuntimeObservabilityState::default();

        state.record_action_scores(StrategyId::A, [0.10, 0.05, 0.01]);
        state.record_action_scores(StrategyId::A, [0.20, 0.05, 0.01]);
        state.record_liquidity_evolvement(-0.25);
        state.record_execution_fill(0.003, 0.002, Some(0.001));

        assert!(state.q_value_adjustment_frequency.mean() > 0.0);
        assert!(state.liquidity_evolvement.mean() > 0.0);
        assert!(state.dynamic_cost_estimate_error.mean() > 0.0);
        assert!(state.execution_drift.mean() > 0.0);
    }

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
                max_daily_loss_mtm: 500.0,
                max_daily_loss_realized: 1_000.0,
                max_weekly_loss: 2_500.0,
                max_monthly_loss: 5_000.0,
                daily_mtm_lot_fraction: 0.25,
                daily_mtm_q_threshold: 0.01,
                max_drawdown: 1000.0,
            },
            comparison_config: None,
            regime_config: fx_strategy::regime::RegimeConfig::default(),
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
        assert!(result.strategy_events_published > 0);
        assert!(result.state_snapshots_published > 0);
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
        assert_eq!(result.strategy_events_published, 0);
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
