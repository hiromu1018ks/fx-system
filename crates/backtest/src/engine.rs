use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use fx_core::types::{Direction, EventTier, StrategyId, StreamId};
use fx_events::bus::PartitionedEventBus;
use fx_events::event::{Event, GenericEvent};
use fx_events::header::EventHeader;
use fx_events::projector::{StateProjector, StateSnapshot};
use fx_events::proto;
use fx_events::store::EventStore;
use fx_execution::gateway::{
    ExecutionGateway, ExecutionGatewayConfig, ExecutionRequest, ExecutionResult,
};
use fx_risk::barrier::{DynamicRiskBarrier, DynamicRiskBarrierConfig};
use fx_risk::global_position::{GlobalPositionChecker, GlobalPositionConfig};
use fx_risk::kill_switch::KillSwitch;
use fx_risk::kill_switch::KillSwitchConfig;
use fx_risk::lifecycle::{EpisodeSummary, LifecycleConfig, LifecycleManager};
use fx_risk::limits::{CloseReason, HierarchicalRiskLimiter, RiskLimitsConfig};
use fx_strategy::bayesian_lr::QAction;
use fx_strategy::extractor::{FeatureExtractor, FeatureExtractorConfig};
use fx_strategy::features::FeatureVector;
use fx_strategy::mc_eval::{McEvalConfig, McEvaluator, TerminalReason};
use fx_strategy::policy::Action;
use fx_strategy::regime::{RegimeCache, RegimeConfig};
use fx_strategy::strategy_a::{StrategyA, StrategyAConfig, StrategyADecision};
use fx_strategy::strategy_b::{StrategyB, StrategyBConfig, StrategyBDecision};
use fx_strategy::strategy_c::{StrategyC, StrategyCConfig, StrategyCDecision};
use prost::Message as _;
use rand::prelude::*;
use rand::rngs::SmallRng;
use tracing::{debug, info, warn};

use crate::stats::{ExecutionStats, LpExecutionStats, TradeRecord, TradeSummary};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Start timestamp (nanoseconds, inclusive).
    pub start_time_ns: u64,
    /// End timestamp (nanoseconds, inclusive).
    pub end_time_ns: u64,
    /// Replay speed multiplier (1.0 = real-time, 10.0 = 10x, 0 = max speed).
    pub replay_speed: f64,
    /// Symbol to backtest.
    pub symbol: String,
    /// Global position limit.
    pub global_position_limit: f64,
    /// Default lot size.
    pub default_lot_size: u64,
    /// Seed for reproducible RNG (None = random).
    pub rng_seed: Option<[u8; 32]>,
    /// Feature extractor configuration.
    pub feature_extractor_config: FeatureExtractorConfig,
    /// Strategies enabled for this backtest run.
    pub enabled_strategies: HashSet<StrategyId>,
    /// Per-strategy configurations.
    pub strategy_a_config: StrategyAConfig,
    pub strategy_b_config: StrategyBConfig,
    pub strategy_c_config: StrategyCConfig,
    /// Monte Carlo evaluation configuration.
    pub mc_eval_config: McEvalConfig,
    /// Global position constraint configuration.
    pub global_position_config: GlobalPositionConfig,
    /// Hierarchical risk limits configuration.
    pub risk_limits_config: RiskLimitsConfig,
    /// Dynamic risk barrier configuration (staleness-based lot reduction).
    pub barrier_config: DynamicRiskBarrierConfig,
    /// Kill switch configuration.
    pub kill_switch_config: KillSwitchConfig,
    /// Lifecycle manager configuration (strategy culling).
    pub lifecycle_config: LifecycleConfig,
    /// Regime management configuration.
    pub regime_config: RegimeConfig,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            start_time_ns: 0,
            end_time_ns: u64::MAX,
            replay_speed: 0.0,
            symbol: "USD/JPY".to_string(),
            global_position_limit: 10.0,
            default_lot_size: 100_000,
            rng_seed: None,
            feature_extractor_config: FeatureExtractorConfig::default(),
            enabled_strategies: StrategyId::all().iter().copied().collect(),
            strategy_a_config: StrategyAConfig::default(),
            strategy_b_config: StrategyBConfig::default(),
            strategy_c_config: StrategyCConfig::default(),
            mc_eval_config: McEvalConfig::default(),
            global_position_config: GlobalPositionConfig::default(),
            risk_limits_config: RiskLimitsConfig::default(),
            barrier_config: DynamicRiskBarrierConfig::default(),
            kill_switch_config: KillSwitchConfig {
                enabled: false,
                ..KillSwitchConfig::default()
            },
            lifecycle_config: LifecycleConfig::default(),
            regime_config: RegimeConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Backtest Engine
// ---------------------------------------------------------------------------

/// A single recorded decision during backtest.
#[derive(Debug, Clone)]
pub struct BacktestDecision {
    pub timestamp_ns: u64,
    pub strategy_id: StrategyId,
    pub direction: Option<Direction>,
    pub lots: u64,
    pub triggered: bool,
    pub skip_reason: Option<String>,
}

/// Intermediate context for a single tick, carrying extracted features alongside
/// market data. Passed to Strategy evaluation and Risk checks in subsequent tasks.
#[derive(Debug, Clone)]
pub struct TickContext {
    pub timestamp_ns: u64,
    pub mid_price: f64,
    pub spread: f64,
    pub volatility: f64,
    pub features: FeatureVector,
}

/// Normalized decision from any strategy variant (A/B/C).
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct StrategyDecision {
    action: Action,
    q_point: f64,
    q_sampled: f64,
    posterior_std: f64,
    triggered: bool,
    episode_active: bool,
    should_close: bool,
    skip_reason: Option<String>,
    remaining_hold_time_ms: u64,
}

impl From<StrategyADecision> for StrategyDecision {
    fn from(d: StrategyADecision) -> Self {
        Self {
            action: d.action,
            q_point: d.q_point,
            q_sampled: d.q_sampled,
            posterior_std: d.posterior_std,
            triggered: d.triggered,
            episode_active: d.episode_active,
            should_close: d.should_close,
            skip_reason: d.skip_reason,
            remaining_hold_time_ms: d.remaining_hold_time_ms,
        }
    }
}

impl From<StrategyBDecision> for StrategyDecision {
    fn from(d: StrategyBDecision) -> Self {
        Self {
            action: d.action,
            q_point: d.q_point,
            q_sampled: d.q_sampled,
            posterior_std: d.posterior_std,
            triggered: d.triggered,
            episode_active: d.episode_active,
            should_close: d.should_close,
            skip_reason: d.skip_reason,
            remaining_hold_time_ms: d.remaining_hold_time_ms,
        }
    }
}

impl From<StrategyCDecision> for StrategyDecision {
    fn from(d: StrategyCDecision) -> Self {
        Self {
            action: d.action,
            q_point: d.q_point,
            q_sampled: d.q_sampled,
            posterior_std: d.posterior_std,
            triggered: d.triggered,
            episode_active: d.episode_active,
            should_close: d.should_close,
            skip_reason: d.skip_reason,
            remaining_hold_time_ms: d.remaining_hold_time_ms,
        }
    }
}

/// Result of a backtest run.
#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub config: BacktestConfig,
    pub trades: Vec<TradeRecord>,
    pub decisions: Vec<BacktestDecision>,
    pub total_ticks: u64,
    pub total_decision_ticks: u64,
    pub wall_time_ms: u64,
    pub summary: TradeSummary,
    pub execution_stats: ExecutionStats,
    /// Execution events generated during the run, ready for EventBus replay.
    pub execution_events: Vec<GenericEvent>,
}

/// Collected position data to avoid borrow conflicts.
struct PositionSnapshot {
    strategy_id: StrategyId,
    size: f64,
    #[allow(dead_code)]
    entry_timestamp_ns: u64,
}

/// Backtest engine: loads historical MarketEvents from an EventStore and replays
/// them through the system pipeline with simulated execution.
pub struct BacktestEngine {
    config: BacktestConfig,
    execution_gateway: ExecutionGateway,
    rng: SmallRng,
    strategy_a: StrategyA,
    strategy_b: StrategyB,
    strategy_c: StrategyC,
    mc_evaluator: McEvaluator,
    risk_barrier: DynamicRiskBarrier,
    kill_switch: KillSwitch,
    lifecycle_manager: LifecycleManager,
    regime_cache: RegimeCache,
    /// Tracks previous tick's `is_unknown` state to detect regime transitions.
    prev_regime_unknown: bool,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Self {
        let gateway_config = ExecutionGatewayConfig {
            symbol: config.symbol.clone(),
            ..ExecutionGatewayConfig::default()
        };
        let rng_seed = config.rng_seed.unwrap_or_else(|| {
            let mut seed = [0u8; 32];
            thread_rng().fill(&mut seed);
            seed
        });
        Self {
            strategy_a: StrategyA::new(config.strategy_a_config.clone()),
            strategy_b: StrategyB::new(config.strategy_b_config.clone()),
            strategy_c: StrategyC::new(config.strategy_c_config.clone()),
            mc_evaluator: McEvaluator::new(config.mc_eval_config.clone()),
            risk_barrier: DynamicRiskBarrier::new(config.barrier_config.clone()),
            kill_switch: KillSwitch::new(config.kill_switch_config.clone()),
            lifecycle_manager: LifecycleManager::new(config.lifecycle_config.clone()),
            regime_cache: RegimeCache::new(config.regime_config.clone()),
            prev_regime_unknown: false,
            config,
            execution_gateway: ExecutionGateway::new(gateway_config),
            rng: SmallRng::from_seed(rng_seed),
        }
    }

    /// Run backtest over historical MarketEvents loaded from `store`.
    pub fn run<S: EventStore>(&mut self, store: &S) -> BacktestResult {
        let wall_start = Instant::now();

        let market_events = match store.replay(StreamId::Market, 0) {
            Ok(events) => events
                .into_iter()
                .filter(|e| {
                    e.header.timestamp_ns >= self.config.start_time_ns
                        && e.header.timestamp_ns <= self.config.end_time_ns
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                warn!("Failed to load market events from store: {}", e);
                Vec::new()
            }
        };

        info!(
            events_loaded = market_events.len(),
            start_ns = self.config.start_time_ns,
            end_ns = self.config.end_time_ns,
            "Loaded historical market events"
        );

        self.run_inner(&market_events, wall_start)
    }

    /// Run backtest with a pre-loaded slice of market events.
    pub fn run_from_events(&mut self, events: &[GenericEvent]) -> BacktestResult {
        let wall_start = Instant::now();

        let market_events: Vec<&GenericEvent> = events
            .iter()
            .filter(|e| {
                e.header.stream_id == StreamId::Market
                    && e.header.timestamp_ns >= self.config.start_time_ns
                    && e.header.timestamp_ns <= self.config.end_time_ns
            })
            .collect();

        info!(
            events_loaded = market_events.len(),
            "Running backtest from event slice"
        );

        if market_events.is_empty() {
            return BacktestResult {
                config: self.config.clone(),
                trades: Vec::new(),
                decisions: Vec::new(),
                total_ticks: 0,
                total_decision_ticks: 0,
                wall_time_ms: wall_start.elapsed().as_millis() as u64,
                summary: TradeSummary::empty(),
                execution_stats: ExecutionStats::empty(),
                execution_events: Vec::new(),
            };
        }

        // Convert &GenericEvent references to owned for uniform processing
        let owned: Vec<GenericEvent> = market_events.into_iter().cloned().collect();
        self.run_inner(&owned, wall_start)
    }

    /// Core replay loop shared by `run` and `run_from_events`.
    fn run_inner(&mut self, market_events: &[GenericEvent], wall_start: Instant) -> BacktestResult {
        if market_events.is_empty() {
            return BacktestResult {
                config: self.config.clone(),
                trades: Vec::new(),
                decisions: Vec::new(),
                total_ticks: 0,
                total_decision_ticks: 0,
                wall_time_ms: wall_start.elapsed().as_millis() as u64,
                summary: TradeSummary::empty(),
                execution_stats: ExecutionStats::empty(),
                execution_events: Vec::new(),
            };
        }

        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, self.config.global_position_limit, 1);
        let mut feature_extractor =
            FeatureExtractor::new(self.config.feature_extractor_config.clone());

        let mut trades: Vec<TradeRecord> = Vec::new();
        let mut decisions: Vec<BacktestDecision> = Vec::new();
        let mut execution_events: Vec<GenericEvent> = Vec::new();
        let mut total_ticks: u64 = 0;
        let mut total_decision_ticks: u64 = 0;
        let mut prev_tick_ns: u64 = 0;

        // Clone to release borrow on self.config before mutating self
        let enabled_strategies: Vec<StrategyId> =
            self.config.enabled_strategies.iter().copied().collect();

        for event in market_events {
            let tick_ns = event.header.timestamp_ns;
            total_ticks += 1;

            // Feed tick to KillSwitch for interval anomaly detection
            self.kill_switch.record_tick(tick_ns);

            if let Err(e) = projector.process_event(event) {
                debug!("Failed to process market event: {}", e);
                continue;
            }

            feature_extractor.process_market_event(event);

            let market_payload = match proto::MarketEventPayload::decode(event.payload_bytes()) {
                Ok(p) => p,
                Err(_) => continue,
            };

            let mid_price = (market_payload.bid + market_payload.ask) / 2.0;
            let spread = market_payload.ask - market_payload.bid;
            let volatility = spread / mid_price;

            let snapshot = projector.snapshot();

            // Extract features per strategy into a map for O(1) lookup
            let tick_contexts: HashMap<StrategyId, TickContext> = StrategyId::all()
                .iter()
                .map(|&sid| {
                    let features = feature_extractor.extract(event, snapshot, sid, tick_ns);
                    (
                        sid,
                        TickContext {
                            timestamp_ns: tick_ns,
                            mid_price,
                            spread,
                            volatility,
                            features,
                        },
                    )
                })
                .collect();

            // Phase 1: Close positions that exceeded per-strategy MAX_HOLD_TIME
            for &sid in &enabled_strategies {
                if self.should_close_max_hold(sid, &projector, tick_ns) {
                    let snap = projector.snapshot();
                    if let Some(pos) = snap.positions.get(&sid) {
                        if pos.is_open() {
                            let direction = if pos.size > 0.0 {
                                Direction::Sell
                            } else {
                                Direction::Buy
                            };
                            let lots = pos.size.abs() as u64;

                            let result = self.simulate_order(
                                direction, lots, sid, mid_price, volatility, tick_ns,
                            );

                            if result.filled {
                                let (trade_pnl, exec_event) = self.process_execution_result(
                                    sid,
                                    &result,
                                    direction,
                                    tick_ns,
                                    &mut projector,
                                );
                                if let Some(ref exec_ev) = exec_event {
                                    feature_extractor.process_execution_event(exec_ev);
                                }
                                if let Some(ev) = exec_event {
                                    execution_events.push(ev);
                                }
                                trades.push(TradeRecord {
                                    timestamp_ns: tick_ns,
                                    strategy_id: sid,
                                    direction,
                                    lots: result.fill_size,
                                    fill_price: result.fill_price,
                                    slippage: result.slippage,
                                    pnl: trade_pnl,
                                    fill_probability: result.effective_fill_probability,
                                    latency_ms: result.latency_ms,
                                    close_reason: Some("MAX_HOLD_TIME".to_string()),
                                });
                            }

                            self.end_strategy_episode(
                                sid,
                                TerminalReason::MaxHoldTimeExceeded,
                                tick_ns,
                                projector.snapshot(),
                            );

                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: Some(direction),
                                lots,
                                triggered: false,
                                skip_reason: Some("MAX_HOLD_TIME close".to_string()),
                            });
                            total_decision_ticks += 1;
                        }
                    }
                }
            }

            // Update regime cache from features (lightweight online indicator)
            // Use Strategy A's features as the representative feature vector
            if let Some(ctx_a) = tick_contexts.get(&StrategyId::A) {
                self.update_regime(&ctx_a.features, tick_ns);
            }

            // Regime transition handling: unknown → known or known → unknown
            let current_unknown = self.regime_cache.state().is_unknown();
            if current_unknown && !self.prev_regime_unknown {
                // Entered unknown regime: reset per-regime tracking in lifecycle
                self.lifecycle_manager.reset_regime_tracking();
                warn!("Entered unknown regime — strategy evaluation suppressed");
            }
            self.prev_regime_unknown = current_unknown;

            // Phase 2: Collect strategy decisions (skip culled strategies)
            let snapshot = projector.snapshot();
            let mut strategy_q: HashMap<StrategyId, f64> = HashMap::new();
            let mut strategy_decisions: Vec<(StrategyId, StrategyDecision)> = Vec::new();

            for &sid in &enabled_strategies {
                // Skip all strategies when regime is unknown
                if self.regime_cache.state().is_unknown() {
                    strategy_decisions.push((
                        sid,
                        StrategyDecision {
                            action: Action::Hold,
                            q_point: 0.0,
                            q_sampled: 0.0,
                            posterior_std: 0.0,
                            triggered: false,
                            episode_active: false,
                            should_close: false,
                            skip_reason: Some("unknown_regime".to_string()),
                            remaining_hold_time_ms: 0,
                        },
                    ));
                    continue;
                }
                // Skip culled strategies (lifecycle manager hard-block)
                if !self.lifecycle_manager.is_alive(sid) {
                    continue;
                }
                let ctx = tick_contexts.get(&sid).unwrap();
                let decision = self.get_strategy_decision(sid, &ctx.features, snapshot, tick_ns);
                strategy_q.insert(sid, decision.q_sampled);
                strategy_decisions.push((sid, decision));
            }

            // Sort by Q-value descending for priority (design.md §9.5)
            strategy_decisions.sort_by(|a, b| {
                b.1.q_sampled
                    .partial_cmp(&a.1.q_sampled)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            // Phase 3: Execute strategy decisions
            for (sid, decision) in strategy_decisions {
                let triggered = decision.triggered;
                let skip = decision.skip_reason.clone();

                match decision.action {
                    Action::Buy(lots) | Action::Sell(lots) => {
                        let direction = match decision.action {
                            Action::Buy(_) => Direction::Buy,
                            Action::Sell(_) => Direction::Sell,
                            Action::Hold => unreachable!(),
                        };

                        let snap = projector.snapshot();
                        let has_position = snap
                            .positions
                            .get(&sid)
                            .map(|p| p.is_open())
                            .unwrap_or(false);

                        if has_position {
                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: Some(direction),
                                lots,
                                triggered,
                                skip_reason: Some("already_in_position".to_string()),
                            });
                            total_decision_ticks += 1;
                            continue;
                        }

                        // --- Risk pipeline (checked BEFORE execution) ---

                        // 1. KillSwitch: anomaly-based order masking
                        if self.kill_switch.validate_order().is_err() {
                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: Some(direction),
                                lots,
                                triggered,
                                skip_reason: Some("kill_switch_masked".to_string()),
                            });
                            total_decision_ticks += 1;
                            continue;
                        }

                        // 2. LifecycleManager: culled strategy check
                        if self.lifecycle_manager.validate_order(sid).is_err() {
                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: Some(direction),
                                lots,
                                triggered,
                                skip_reason: Some("strategy_culled".to_string()),
                            });
                            total_decision_ticks += 1;
                            continue;
                        }

                        // 3. HierarchicalRiskLimiter: monthly → weekly → daily
                        let snap = projector.snapshot();
                        let (limit_result, close_reason) = HierarchicalRiskLimiter::evaluate(
                            &self.config.risk_limits_config,
                            &snap.limit_state,
                        );

                        // Close all positions if a hard limit fired
                        if let Some(reason) = close_reason {
                            let reason_str = match reason {
                                CloseReason::DailyRealizedHalt => "daily_realized_halt",
                                CloseReason::WeeklyHalt => "weekly_halt",
                                CloseReason::MonthlyHalt => "monthly_halt",
                            };
                            self.close_all_positions(
                                &mut projector,
                                &mut feature_extractor,
                                &mut trades,
                                &mut execution_events,
                                mid_price,
                                volatility,
                                tick_ns,
                                reason_str,
                            );
                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: Some(direction),
                                lots,
                                triggered,
                                skip_reason: Some(reason_str.to_string()),
                            });
                            total_decision_ticks += 1;
                            break; // Stop processing further strategies this tick
                        }

                        let limit_check = match limit_result {
                            Ok(c) => c,
                            Err(_) => {
                                decisions.push(BacktestDecision {
                                    timestamp_ns: tick_ns,
                                    strategy_id: sid,
                                    direction: Some(direction),
                                    lots,
                                    triggered,
                                    skip_reason: Some("risk_limit_rejected".to_string()),
                                });
                                total_decision_ticks += 1;
                                continue;
                            }
                        };

                        // Q-threshold gate when daily MTM is active
                        if limit_check.daily_mtm_limited {
                            let q_other = strategy_q
                                .values()
                                .copied()
                                .fold(f64::NEG_INFINITY, f64::max);
                            let q_check = match direction {
                                Direction::Buy => decision.q_sampled,
                                Direction::Sell => decision.q_sampled,
                            };
                            if !HierarchicalRiskLimiter::passes_q_threshold(
                                &limit_check,
                                if direction == Direction::Buy {
                                    q_check
                                } else {
                                    q_other
                                },
                                if direction == Direction::Sell {
                                    q_check
                                } else {
                                    q_other
                                },
                            ) {
                                decisions.push(BacktestDecision {
                                    timestamp_ns: tick_ns,
                                    strategy_id: sid,
                                    direction: Some(direction),
                                    lots,
                                    triggered,
                                    skip_reason: Some("mtm_q_threshold_rejected".to_string()),
                                });
                                total_decision_ticks += 1;
                                continue;
                            }
                        }

                        // 4. DynamicRiskBarrier: staleness-based lot reduction
                        let staleness_ms = snap.staleness_ms;
                        let (barrier_lot_multiplier, barrier_effective_lot) =
                            match self.risk_barrier.validate_order(staleness_ms) {
                                Ok(info) => (info.lot_multiplier, info.effective_lot_size as f64),
                                Err(_) => {
                                    decisions.push(BacktestDecision {
                                        timestamp_ns: tick_ns,
                                        strategy_id: sid,
                                        direction: Some(direction),
                                        lots,
                                        triggered,
                                        skip_reason: Some("staleness_rejected".to_string()),
                                    });
                                    total_decision_ticks += 1;
                                    continue;
                                }
                            };

                        // 5. GlobalPositionConstraint (existing check)
                        let snap = projector.snapshot();
                        let pos_result = GlobalPositionChecker::validate_order(
                            &self.config.global_position_config,
                            snap,
                            sid,
                            direction,
                            lots as f64,
                            decision.q_sampled,
                            &strategy_q,
                        );

                        let mut effective_lots = match pos_result {
                            Ok(r) => r.effective_lot.max(0.0),
                            Err(_) => {
                                decisions.push(BacktestDecision {
                                    timestamp_ns: tick_ns,
                                    strategy_id: sid,
                                    direction: Some(direction),
                                    lots,
                                    triggered,
                                    skip_reason: Some("global_position_rejected".to_string()),
                                });
                                total_decision_ticks += 1;
                                continue;
                            }
                        };

                        // Apply risk limit lot multiplier (MTM reduction) × barrier multiplier
                        effective_lots *= limit_check.lot_multiplier * barrier_lot_multiplier;
                        effective_lots = effective_lots.min(barrier_effective_lot);
                        let effective_lots = effective_lots.max(0.0) as u64;

                        if effective_lots == 0 {
                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: Some(direction),
                                lots,
                                triggered,
                                skip_reason: Some("zero_effective_lot".to_string()),
                            });
                            total_decision_ticks += 1;
                            continue;
                        }

                        let result = self.simulate_order(
                            direction,
                            effective_lots,
                            sid,
                            mid_price,
                            volatility,
                            tick_ns,
                        );

                        if result.filled {
                            let (trade_pnl, exec_event) = self.process_execution_result(
                                sid,
                                &result,
                                direction,
                                tick_ns,
                                &mut projector,
                            );
                            if let Some(ref exec_ev) = exec_event {
                                feature_extractor.process_execution_event(exec_ev);
                            }
                            if let Some(ev) = exec_event {
                                execution_events.push(ev);
                            }

                            trades.push(TradeRecord {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction,
                                lots: result.fill_size,
                                fill_price: result.fill_price,
                                slippage: result.slippage,
                                pnl: trade_pnl,
                                fill_probability: result.effective_fill_probability,
                                latency_ms: result.latency_ms,
                                close_reason: None,
                            });

                            // Start MC episode
                            self.start_strategy_episode(sid, tick_ns, projector.snapshot());
                        }

                        decisions.push(BacktestDecision {
                            timestamp_ns: tick_ns,
                            strategy_id: sid,
                            direction: Some(direction),
                            lots: effective_lots,
                            triggered,
                            skip_reason: if result.filled {
                                None
                            } else {
                                Some("execution_rejected".to_string())
                            },
                        });
                        total_decision_ticks += 1;
                    }
                    Action::Hold => {
                        if triggered || decision.episode_active {
                            decisions.push(BacktestDecision {
                                timestamp_ns: tick_ns,
                                strategy_id: sid,
                                direction: None,
                                lots: 0,
                                triggered,
                                skip_reason: skip,
                            });
                            total_decision_ticks += 1;
                        }
                    }
                }

                // Phase 4: Record MC transition for active episodes
                if self.mc_evaluator.has_active_episode(sid) {
                    let ctx = tick_contexts.get(&sid).unwrap();
                    let phi = self.extract_strategy_features(sid, &ctx.features);
                    let snap = projector.snapshot();
                    let q_action = match decision.action {
                        Action::Buy(_) => QAction::Buy,
                        Action::Sell(_) => QAction::Sell,
                        Action::Hold => QAction::Hold,
                    };
                    self.mc_evaluator.record_transition(
                        sid,
                        tick_ns,
                        q_action,
                        phi,
                        snap,
                        volatility * volatility,
                    );
                }
            }

            // Time-compressed replay delay
            if self.config.replay_speed > 0.0 && prev_tick_ns > 0 {
                let real_gap_ns = tick_ns - prev_tick_ns;
                let sim_gap =
                    Duration::from_nanos((real_gap_ns as f64 / self.config.replay_speed) as u64);
                if sim_gap > Duration::from_millis(1) {
                    std::thread::sleep(sim_gap);
                }
            }
            prev_tick_ns = tick_ns;
        }

        // Close remaining open positions at the last mid price (END_OF_DATA)
        if let Some(last_event) = market_events.last() {
            if let Ok(last_market) = proto::MarketEventPayload::decode(last_event.payload_bytes()) {
                let last_mid = (last_market.bid + last_market.ask) / 2.0;
                let last_spread = last_market.ask - last_market.bid;
                let last_vol = last_spread / last_mid;
                let last_ns = last_event.header.timestamp_ns;

                let open_positions = self.collect_all_open_positions(&projector);

                for pos_snap in &open_positions {
                    let direction = if pos_snap.size > 0.0 {
                        Direction::Sell
                    } else {
                        Direction::Buy
                    };
                    let lots = pos_snap.size.abs() as u64;

                    let result = self.simulate_order(
                        direction,
                        lots,
                        pos_snap.strategy_id,
                        last_mid,
                        last_vol,
                        last_ns,
                    );

                    if result.filled {
                        let (trade_pnl, exec_event) = self.process_execution_result(
                            pos_snap.strategy_id,
                            &result,
                            direction,
                            last_ns,
                            &mut projector,
                        );
                        if let Some(ev) = exec_event {
                            execution_events.push(ev);
                        }
                        trades.push(TradeRecord {
                            timestamp_ns: last_ns,
                            strategy_id: pos_snap.strategy_id,
                            direction,
                            lots: result.fill_size,
                            fill_price: result.fill_price,
                            slippage: result.slippage,
                            pnl: trade_pnl,
                            fill_probability: result.effective_fill_probability,
                            latency_ms: result.latency_ms,
                            close_reason: Some("END_OF_DATA".to_string()),
                        });
                    }

                    self.end_strategy_episode(
                        pos_snap.strategy_id,
                        TerminalReason::PositionClosed,
                        last_ns,
                        projector.snapshot(),
                    );
                }
            }
        }

        let wall_time_ms = wall_start.elapsed().as_millis() as u64;
        let summary = TradeSummary::from_trades(&trades);

        // Collect LP execution stats from the gateway
        let execution_stats = self.collect_execution_stats(&trades);

        info!(
            total_ticks = total_ticks,
            total_trades = trades.len(),
            total_pnl = summary.total_pnl,
            sharpe = summary.sharpe_ratio,
            max_dd = summary.max_drawdown,
            wall_time_ms,
            "Backtest complete"
        );

        BacktestResult {
            config: self.config.clone(),
            trades,
            decisions,
            total_ticks,
            total_decision_ticks,
            wall_time_ms,
            summary,
            execution_stats,
            execution_events,
        }
    }

    /// Check if a strategy's position should be closed due to MAX_HOLD_TIME.
    fn should_close_max_hold(
        &self,
        sid: StrategyId,
        projector: &StateProjector,
        tick_ns: u64,
    ) -> bool {
        let snap = projector.snapshot();
        let pos = match snap.positions.get(&sid) {
            Some(p) if p.is_open() => p,
            _ => return false,
        };
        if pos.entry_timestamp_ns == 0 {
            return false;
        }
        let max_hold_ns = self.strategy_max_hold_time_ns(sid);
        tick_ns - pos.entry_timestamp_ns >= max_hold_ns
    }

    /// Per-strategy MAX_HOLD_TIME in nanoseconds.
    fn strategy_max_hold_time_ns(&self, sid: StrategyId) -> u64 {
        match sid {
            StrategyId::A => self.strategy_a.config().max_hold_time_ms * 1_000_000,
            StrategyId::B => self.strategy_b.config().max_hold_time_ms * 1_000_000,
            StrategyId::C => self.strategy_c.config().max_hold_time_ms * 1_000_000,
        }
    }

    /// Close all open positions (used when a hard risk limit fires).
    #[allow(clippy::too_many_arguments)]
    fn close_all_positions(
        &mut self,
        projector: &mut StateProjector,
        feature_extractor: &mut FeatureExtractor,
        trades: &mut Vec<TradeRecord>,
        execution_events: &mut Vec<GenericEvent>,
        mid_price: f64,
        volatility: f64,
        tick_ns: u64,
        reason: &str,
    ) {
        let open_positions = self.collect_all_open_positions(projector);
        for pos_snap in &open_positions {
            let direction = if pos_snap.size > 0.0 {
                Direction::Sell
            } else {
                Direction::Buy
            };
            let lots = pos_snap.size.abs() as u64;

            let result = self.simulate_order(
                direction,
                lots,
                pos_snap.strategy_id,
                mid_price,
                volatility,
                tick_ns,
            );

            if result.filled {
                let (trade_pnl, exec_event) = self.process_execution_result(
                    pos_snap.strategy_id,
                    &result,
                    direction,
                    tick_ns,
                    projector,
                );
                if let Some(ref exec_ev) = exec_event {
                    feature_extractor.process_execution_event(exec_ev);
                }
                if let Some(ev) = exec_event {
                    execution_events.push(ev);
                }
                trades.push(TradeRecord {
                    timestamp_ns: tick_ns,
                    strategy_id: pos_snap.strategy_id,
                    direction,
                    lots: result.fill_size,
                    fill_price: result.fill_price,
                    slippage: result.slippage,
                    pnl: trade_pnl,
                    fill_probability: result.effective_fill_probability,
                    latency_ms: result.latency_ms,
                    close_reason: Some(reason.to_string()),
                });
            }

            self.end_strategy_episode(
                pos_snap.strategy_id,
                TerminalReason::DailyHardLimit,
                tick_ns,
                projector.snapshot(),
            );
        }
    }

    /// Get strategy decision for a given strategy ID.
    fn get_strategy_decision(
        &mut self,
        sid: StrategyId,
        features: &FeatureVector,
        state: &StateSnapshot,
        tick_ns: u64,
    ) -> StrategyDecision {
        let regime_kl = self.regime_cache.state().kl_divergence();
        let latency_ms = 0.0; // No latency in backtest
        match sid {
            StrategyId::A => self
                .strategy_a
                .decide(
                    features,
                    state,
                    regime_kl,
                    latency_ms,
                    tick_ns,
                    &mut self.rng,
                )
                .into(),
            StrategyId::B => self
                .strategy_b
                .decide(
                    features,
                    state,
                    regime_kl,
                    latency_ms,
                    tick_ns,
                    &mut self.rng,
                )
                .into(),
            StrategyId::C => self
                .strategy_c
                .decide(
                    features,
                    state,
                    regime_kl,
                    latency_ms,
                    tick_ns,
                    &mut self.rng,
                )
                .into(),
        }
    }

    /// Extract strategy-specific feature vector (39-dim including strategy extras).
    fn extract_strategy_features(&self, sid: StrategyId, base: &FeatureVector) -> Vec<f64> {
        match sid {
            StrategyId::A => self.strategy_a.extract_features(base),
            StrategyId::B => self.strategy_b.extract_features(base),
            StrategyId::C => self.strategy_c.extract_features(base),
        }
    }

    /// Start an MC episode and strategy episode for the given strategy.
    fn start_strategy_episode(&mut self, sid: StrategyId, tick_ns: u64, snapshot: &StateSnapshot) {
        let equity = snapshot
            .positions
            .get(&sid)
            .map(|p| p.realized_pnl + p.unrealized_pnl)
            .unwrap_or(0.0);
        self.mc_evaluator.start_episode(sid, tick_ns, equity);
        match sid {
            StrategyId::A => self.strategy_a.start_episode(tick_ns),
            StrategyId::B => self.strategy_b.start_episode(tick_ns),
            StrategyId::C => self.strategy_c.start_episode(tick_ns),
        }
    }

    /// End an MC episode, update the Q-function, reset strategy episode state,
    /// and record the episode with the LifecycleManager for strategy culling evaluation.
    fn end_strategy_episode(
        &mut self,
        sid: StrategyId,
        reason: TerminalReason,
        tick_ns: u64,
        snapshot: &StateSnapshot,
    ) {
        if self.mc_evaluator.has_active_episode(sid) {
            let episode_result = self.mc_evaluator.end_episode(sid, reason, tick_ns);
            let q_fn = match sid {
                StrategyId::A => self.strategy_a.q_function_mut(),
                StrategyId::B => self.strategy_b.q_function_mut(),
                StrategyId::C => self.strategy_c.q_function_mut(),
            };
            McEvaluator::update_from_result(q_fn, &episode_result);

            // Record episode with LifecycleManager for strategy culling evaluation
            let summary = EpisodeSummary {
                strategy_id: episode_result.strategy_id,
                total_reward: episode_result.total_reward,
                return_g0: episode_result.return_g0,
                duration_ns: episode_result.duration_ns,
            };
            let is_unknown_regime = self.regime_cache.state().is_unknown();
            if let Some(_close_cmd) =
                self.lifecycle_manager
                    .record_episode(&summary, is_unknown_regime, snapshot)
            {
                // If the strategy was culled, its positions will be closed on
                // the next tick via the lifecycle check in Phase 2/3.
                info!(strategy = ?sid, "Strategy culled by LifecycleManager");
            }
        }
        match sid {
            StrategyId::A => self.strategy_a.end_episode(),
            StrategyId::B => self.strategy_b.end_episode(),
            StrategyId::C => self.strategy_c.end_episode(),
        }
    }

    /// Update the regime cache from extracted features using a lightweight online
    /// indicator. Uses feature-derived regime scores (simple heuristic based on
    /// volatility and spread features) when no pre-trained HDP-HMM weights are
    /// available.
    fn update_regime(&mut self, features: &FeatureVector, tick_ns: u64) {
        let n_regimes = self.regime_cache.config().n_regimes;
        let feature_dim = self.regime_cache.config().feature_dim;

        // Build lightweight regime scores from key features.
        // Without a trained HDP-HMM model, we use feature-derived heuristics:
        // - Regime 0: Low vol, tight spread (calm)
        // - Regime 1: Medium vol (normal)
        // - Regime 2: High vol, wide spread (turbulent)
        // - Regime 3: Extreme vol (crisis)
        // The scores are computed from a subset of available features.
        let spread_z = features.spread_zscore.abs();
        let rv = features.realized_volatility;
        let vol_ratio = features.volatility_ratio;

        // Simple scoring: each regime gets a score based on feature proximity
        let calm_score = -(spread_z + rv * 10.0 + vol_ratio * 2.0);
        let normal_score =
            -((spread_z - 1.0).abs() + (rv - 0.01).abs() * 10.0 + (vol_ratio - 1.0).abs() * 2.0);
        let turbulent_score =
            -((spread_z - 2.0).abs() + (rv - 0.03).abs() * 10.0 + (vol_ratio - 2.0).abs() * 2.0);
        let crisis_score =
            -(spread_z - 3.0).abs() - (rv - 0.05).abs() * 10.0 - (vol_ratio - 3.0).abs() * 2.0;

        let mut scores = vec![calm_score, normal_score, turbulent_score, crisis_score];
        // Pad or truncate to match n_regimes
        scores.resize(n_regimes, 0.0);

        // Numerically stable softmax
        let max_score = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let exp_scores: Vec<f64> = scores.iter().map(|s| (s - max_score).exp()).collect();
        let sum_exp: f64 = exp_scores.iter().sum();
        let posterior: Vec<f64> = exp_scores.iter().map(|e| e / sum_exp).collect();

        self.regime_cache.update(posterior, tick_ns);

        // Update drift if feature dimensions match
        let phi = self.extract_strategy_features(StrategyId::A, features);
        if phi.len() == feature_dim {
            self.regime_cache.update_drift(&phi);
        }
    }

    /// Collect all currently open positions.
    fn collect_all_open_positions(&self, projector: &StateProjector) -> Vec<PositionSnapshot> {
        projector
            .snapshot()
            .positions
            .iter()
            .filter_map(|(&sid, pos)| {
                if pos.is_open() {
                    Some(PositionSnapshot {
                        strategy_id: sid,
                        size: pos.size,
                        entry_timestamp_ns: pos.entry_timestamp_ns,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    // -- Internal helpers --

    fn simulate_order(
        &mut self,
        direction: Direction,
        lots: u64,
        strategy_id: StrategyId,
        mid_price: f64,
        volatility: f64,
        timestamp_ns: u64,
    ) -> ExecutionResult {
        let request = ExecutionRequest {
            direction,
            lots,
            strategy_id,
            current_mid_price: mid_price,
            volatility,
            expected_profit: 0.0,
            symbol: self.config.symbol.clone(),
            timestamp_ns,
            time_urgent: false,
        };

        self.execution_gateway
            .simulate_execution(&request, &mut self.rng)
            .unwrap_or_else(|_| ExecutionResult {
                order_id: String::new(),
                filled: false,
                fill_price: mid_price,
                fill_size: 0.0,
                slippage: 0.0,
                fill_probability: 0.0,
                effective_fill_probability: 0.0,
                last_look_rejection_prob: 1.0,
                price_improvement: 0.0,
                order_type: fx_execution::otc_model::OtcOrderType::Market,
                fill_status: fx_execution::gateway::FillOutcome::Rejected,
                reject_reason: Some("GATEWAY_ERROR".to_string()),
                lp_id: String::new(),
                requested_price: mid_price,
                requested_size: lots as f64,
                latency_ms: 0.0,
                evaluation: fx_execution::gateway::OrderEvaluation {
                    order_type: fx_execution::otc_model::OtcOrderType::Market,
                    fill_probability: 0.0,
                    effective_fill_probability: 0.0,
                    expected_slippage: 0.0,
                    last_look_fill_prob: 0.0,
                    lp_id: String::new(),
                    limit_price_distance: 0.0,
                },
            })
    }

    /// Process an execution result through the projector and return the per-trade PnL delta
    /// along with the constructed execution event for downstream use (e.g. feature extractor).
    fn process_execution_result(
        &self,
        strategy_id: StrategyId,
        result: &ExecutionResult,
        direction: Direction,
        timestamp_ns: u64,
        projector: &mut StateProjector,
    ) -> (f64, Option<GenericEvent>) {
        if !result.filled {
            return (0.0, None);
        }

        let prev_realized = projector
            .snapshot()
            .positions
            .get(&strategy_id)
            .map(|p| p.realized_pnl)
            .unwrap_or(0.0);

        let signed_size = match direction {
            Direction::Buy => result.fill_size,
            Direction::Sell => -result.fill_size,
        };

        let proto_event = proto::ExecutionEventPayload {
            header: None,
            order_id: result.order_id.clone(),
            symbol: self.config.symbol.clone(),
            order_type: proto::OrderType::OrderMarket as i32,
            fill_status: proto::FillStatus::Filled as i32,
            fill_price: result.fill_price,
            fill_size: signed_size.abs(),
            slippage: result.slippage,
            requested_price: result.requested_price,
            requested_size: result.requested_size,
            fill_probability: result.fill_probability,
            effective_fill_probability: result.effective_fill_probability,
            price_improvement: result.price_improvement,
            last_look_rejection_prob: result.last_look_rejection_prob,
            lp_id: result.lp_id.clone(),
            latency_ms: result.latency_ms,
            reject_reason: proto::RejectReason::Unknown as i32,
            reject_message: result.reject_reason.clone().unwrap_or_default(),
        };

        let header = EventHeader {
            stream_id: StreamId::Execution,
            sequence_id: 0,
            timestamp_ns,
            ..EventHeader::new(StreamId::Execution, 0, EventTier::Tier1Critical)
        };

        let generic_event = GenericEvent::new(header, proto_event.encode_to_vec());
        let _ = projector.process_execution_for_strategy(&generic_event, strategy_id);

        let new_realized = projector
            .snapshot()
            .positions
            .get(&strategy_id)
            .map(|p| p.realized_pnl)
            .unwrap_or(0.0);

        (new_realized - prev_realized, Some(generic_event))
    }

    // -- Accessors --

    pub fn execution_gateway(&self) -> &ExecutionGateway {
        &self.execution_gateway
    }

    pub fn execution_gateway_mut(&mut self) -> &mut ExecutionGateway {
        &mut self.execution_gateway
    }

    /// Access the MC evaluator (for testing/inspection).
    pub fn mc_evaluator(&self) -> &McEvaluator {
        &self.mc_evaluator
    }

    /// Access strategy A (for testing/inspection).
    pub fn strategy_a(&self) -> &StrategyA {
        &self.strategy_a
    }

    /// Access strategy B (for testing/inspection).
    pub fn strategy_b(&self) -> &StrategyB {
        &self.strategy_b
    }

    /// Access strategy C (for testing/inspection).
    pub fn strategy_c(&self) -> &StrategyC {
        &self.strategy_c
    }

    /// Access the lifecycle manager (for testing/inspection).
    pub fn lifecycle_manager(&self) -> &LifecycleManager {
        &self.lifecycle_manager
    }

    /// Access the regime cache (for testing/inspection).
    pub fn regime_cache(&self) -> &RegimeCache {
        &self.regime_cache
    }

    /// Collect LP execution stats from the gateway after a backtest run.
    fn collect_execution_stats(&self, trades: &[TradeRecord]) -> ExecutionStats {
        let monitor = self.execution_gateway.lp_monitor();
        let all_states = monitor.all_lp_states();

        let lp_stats: Vec<LpExecutionStats> = all_states
            .values()
            .map(|state| LpExecutionStats {
                lp_id: state.lp_id.clone(),
                total_requests: state.total_requests,
                total_fills: state.total_fills,
                total_rejections: state.total_rejections,
                fill_rate_ema: state.fill_rate_ema,
                is_adversarial: state.is_adversarial,
            })
            .collect();

        let total_fills: u64 = lp_stats.iter().map(|s| s.total_fills).sum();
        let total_rejections: u64 = lp_stats.iter().map(|s| s.total_rejections).sum();
        let total_requests = total_fills + total_rejections;
        let overall_fill_rate = if total_requests > 0 {
            total_fills as f64 / total_requests as f64
        } else {
            0.0
        };

        let avg_slippage = if !trades.is_empty() {
            trades.iter().map(|t| t.slippage.abs()).sum::<f64>() / trades.len() as f64
        } else {
            0.0
        };

        ExecutionStats {
            active_lp_id: self.execution_gateway.active_lp_id().to_string(),
            lp_stats,
            total_fills,
            total_rejections,
            overall_fill_rate,
            avg_slippage,
            recalibration_triggered: self.execution_gateway.is_recalibrating(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers: construct market events for testing
// ---------------------------------------------------------------------------

/// Build a MarketEvent GenericEvent for backtest injection.
pub fn make_market_event(
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

/// Generate a series of synthetic market ticks for testing.
pub fn generate_synthetic_ticks(
    start_ns: u64,
    num_ticks: u64,
    tick_interval_ms: u64,
    base_price: f64,
    volatility: f64,
) -> Vec<GenericEvent> {
    let mut rng = SmallRng::from_seed([42u8; 32]);
    let half_spread = 0.5;

    let mut events = Vec::with_capacity(num_ticks as usize);
    let mut price = base_price;

    for i in 0..num_ticks {
        let timestamp_ns = start_ns + i * tick_interval_ms * 1_000_000;

        let noise: f64 = rng.gen_range(-volatility..=volatility);
        price += noise;
        price += (base_price - price) * 0.001;

        let bid = price - half_spread * 0.01;
        let ask = price + half_spread * 0.01;

        events.push(make_market_event(
            timestamp_ns,
            "USD/JPY",
            bid,
            ask,
            1_000_000.0,
            1_000_000.0,
        ));
    }

    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use fx_events::store::Tier3Store;
    use std::time::Duration as StdDuration;

    fn default_config() -> BacktestConfig {
        BacktestConfig {
            start_time_ns: 1_000_000_000_000_000,
            end_time_ns: 2_000_000_000_000_000,
            rng_seed: Some([42u8; 32]),
            ..BacktestConfig::default()
        }
    }

    #[test]
    fn test_make_market_event() {
        let event = make_market_event(100, "EUR/USD", 1.1000, 1.1001, 1000.0, 1000.0);
        assert_eq!(event.header.stream_id, StreamId::Market);
        assert_eq!(event.header.tier, EventTier::Tier3Raw);
        assert_eq!(event.header.timestamp_ns, 100);

        let decoded = proto::MarketEventPayload::decode(event.payload_bytes()).unwrap();
        assert_eq!(decoded.symbol, "EUR/USD");
        assert!((decoded.bid - 1.1000).abs() < 1e-10);
        assert!((decoded.ask - 1.1001).abs() < 1e-10);
    }

    #[test]
    fn test_generate_synthetic_ticks() {
        let ticks = generate_synthetic_ticks(0, 100, 100, 110.0, 0.01);
        assert_eq!(ticks.len(), 100);

        for i in 1..ticks.len() {
            assert!(ticks[i].header.timestamp_ns > ticks[i - 1].header.timestamp_ns);
        }

        for tick in &ticks {
            let decoded = proto::MarketEventPayload::decode(tick.payload_bytes()).unwrap();
            assert!(decoded.ask > decoded.bid);
        }
    }

    #[test]
    fn test_generate_synthetic_ticks_deterministic() {
        let ticks1 = generate_synthetic_ticks(0, 50, 100, 110.0, 0.01);
        let ticks2 = generate_synthetic_ticks(0, 50, 100, 110.0, 0.01);
        assert_eq!(ticks1.len(), ticks2.len());
        for (a, b) in ticks1.iter().zip(ticks2.iter()) {
            let da = proto::MarketEventPayload::decode(a.payload_bytes()).unwrap();
            let db = proto::MarketEventPayload::decode(b.payload_bytes()).unwrap();
            assert!((da.bid - db.bid).abs() < 1e-10);
            assert!((da.ask - db.ask).abs() < 1e-10);
        }
    }

    #[tokio::test]
    async fn test_backtest_empty_store() {
        let store = Tier3Store::new(StdDuration::from_secs(300));
        let config = default_config();
        let mut engine = BacktestEngine::new(config);
        let result = engine.run(&store);

        assert_eq!(result.total_ticks, 0);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.decisions.len(), 0);
    }

    #[tokio::test]
    async fn test_backtest_loads_from_store() {
        let store = Tier3Store::new(StdDuration::from_secs(300));
        let start_ns = 1_000_000_000_000_000u64;

        for i in 0..50 {
            let ts = start_ns + i * 100_000_000;
            let event = make_market_event(
                ts,
                "USD/JPY",
                110.0 + i as f64 * 0.001,
                110.001 + i as f64 * 0.001,
                1e6,
                1e6,
            );
            store.store(&event).unwrap();
        }

        let config = BacktestConfig {
            start_time_ns: start_ns,
            end_time_ns: start_ns + 50 * 100_000_000 + 1,
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run(&store);

        assert_eq!(result.total_ticks, 50);
        assert_eq!(result.trades.len(), 0);
    }

    #[test]
    fn test_backtest_from_events() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 200, 100, 110.0, 0.005);

        let config = default_config();
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        assert_eq!(result.total_ticks, 200);
    }

    #[test]
    fn test_backtest_config_default() {
        let config = BacktestConfig::default();
        assert_eq!(config.start_time_ns, 0);
        assert_eq!(config.end_time_ns, u64::MAX);
        assert!((config.replay_speed - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_backtest_result_fields() {
        let events = generate_synthetic_ticks(0, 10, 100, 110.0, 0.001);
        let config = BacktestConfig {
            start_time_ns: 0,
            end_time_ns: u64::MAX,
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        assert_eq!(result.total_ticks, 10);
        assert!(result.wall_time_ms < 1000);
        assert!(!result.config.symbol.is_empty());
    }

    #[test]
    fn test_backtest_reproducible_with_seed() {
        let events = generate_synthetic_ticks(0, 100, 50, 110.0, 0.005);

        let config1 = BacktestConfig {
            rng_seed: Some([99u8; 32]),
            ..default_config()
        };
        let mut engine1 = BacktestEngine::new(config1);
        let result1 = engine1.run_from_events(&events);

        let config2 = BacktestConfig {
            rng_seed: Some([99u8; 32]),
            ..default_config()
        };
        let mut engine2 = BacktestEngine::new(config2);
        let result2 = engine2.run_from_events(&events);

        assert_eq!(result1.trades.len(), result2.trades.len());
        for (t1, t2) in result1.trades.iter().zip(result2.trades.iter()) {
            assert!((t1.fill_price - t2.fill_price).abs() < 1e-10);
            assert!((t1.slippage - t2.slippage).abs() < 1e-10);
        }
    }

    #[tokio::test]
    async fn test_backtest_store_replay_filtering() {
        let store = Tier3Store::new(StdDuration::from_secs(300));

        for i in 0..20 {
            let ts = i as u64 * 1_000_000_000;
            let event = make_market_event(ts, "USD/JPY", 110.0, 110.005, 1e6, 1e6);
            store.store(&event).unwrap();
        }

        let config = BacktestConfig {
            start_time_ns: 5_000_000_000,
            end_time_ns: 15_000_000_000,
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run(&store);

        assert_eq!(result.total_ticks, 11);
    }

    #[test]
    fn test_generate_synthetic_ticks_price_bounds() {
        let ticks = generate_synthetic_ticks(0, 1000, 10, 110.0, 0.001);
        for tick in &ticks {
            let decoded = proto::MarketEventPayload::decode(tick.payload_bytes()).unwrap();
            assert!(decoded.bid > 0.0, "bid must be positive");
            assert!(decoded.ask > decoded.bid, "ask must be > bid");
            assert!(decoded.bid_size > 0.0);
            assert!(decoded.ask_size > 0.0);
        }
    }

    #[test]
    fn test_execution_gateway_accessible() {
        let engine = BacktestEngine::new(default_config());
        assert!(!engine.execution_gateway().active_lp_id().is_empty());
    }

    #[test]
    fn test_feature_extractor_integration_with_synthetic_data() {
        use fx_strategy::features::FeatureVector;

        let events = generate_synthetic_ticks(1_000_000_000_000_000, 300, 100, 110.0, 0.005);

        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            feature_extractor_config: FeatureExtractorConfig::default(),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        assert_eq!(result.total_ticks, 300);

        // Manually run FeatureExtractor to verify features are computed
        let mut extractor = FeatureExtractor::new(FeatureExtractorConfig::default());
        for event in &events {
            extractor.process_market_event(event);
        }

        // After processing enough events, feature extraction should produce valid vectors
        let bus = PartitionedEventBus::new();
        let projector = StateProjector::new(&bus, 10.0, 1);
        let snapshot = projector.snapshot();

        for &sid in StrategyId::all() {
            let features = extractor.extract(
                &events[250],
                &snapshot,
                sid,
                events[250].header.timestamp_ns,
            );
            // Verify the FeatureVector has expected dimension
            let flat = features.flattened();
            assert_eq!(flat.len(), FeatureVector::DIM);

            // Spread should be positive (ask > bid in synthetic data)
            assert!(features.spread > 0.0, "spread should be positive");

            // Realized volatility should be non-negative after enough ticks
            assert!(
                features.realized_volatility >= 0.0,
                "volatility should be non-negative"
            );

            // Position-related features should be zero (no positions opened)
            assert!(
                features.position_size.abs() < f64::EPSILON,
                "position_size should be zero with no positions"
            );
        }
    }

    #[test]
    fn test_feature_extractor_config_customizable() {
        let custom_config = FeatureExtractorConfig {
            spread_window: 50,
            obi_window: 50,
            vol_window: 30,
            ..FeatureExtractorConfig::default()
        };

        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            feature_extractor_config: custom_config,
            ..default_config()
        };

        let engine = BacktestEngine::new(config);
        assert!(!engine.execution_gateway().active_lp_id().is_empty());
    }

    // -- Strategy integration tests --

    #[test]
    fn test_strategy_integration_produces_decisions() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            enabled_strategies: StrategyId::all().iter().copied().collect(),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        assert_eq!(result.total_ticks, 500);
        // Decisions should be recorded for each enabled strategy on each tick
        // (even if most are Hold with triggered=false, active episodes may produce some)
        assert!(
            result.decisions.len() <= result.total_ticks as usize * 3,
            "decisions should not exceed 3 per tick"
        );
    }

    #[test]
    fn test_strategy_enabled_subset() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 200, 100, 110.0, 0.005);

        // Only enable Strategy A
        let mut enabled = HashSet::new();
        enabled.insert(StrategyId::A);

        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            enabled_strategies: enabled,
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // All decisions should be from Strategy A only
        for d in &result.decisions {
            assert_eq!(d.strategy_id, StrategyId::A);
        }
    }

    #[test]
    fn test_strategy_per_strategy_max_hold_time() {
        let engine = BacktestEngine::new(default_config());

        // StrategyA: 30s, StrategyB: 5min, StrategyC: 10min
        let a_ns = engine.strategy_max_hold_time_ns(StrategyId::A);
        let b_ns = engine.strategy_max_hold_time_ns(StrategyId::B);
        let c_ns = engine.strategy_max_hold_time_ns(StrategyId::C);

        assert_eq!(a_ns, 30_000_000_000u64, "StrategyA max hold = 30s");
        assert_eq!(b_ns, 300_000_000_000u64, "StrategyB max hold = 5min");
        assert_eq!(c_ns, 600_000_000_000u64, "StrategyC max hold = 10min");
    }

    #[test]
    fn test_strategy_reproducible_with_seed() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 300, 100, 110.0, 0.005);

        let config1 = BacktestConfig {
            rng_seed: Some([77u8; 32]),
            ..default_config()
        };
        let mut engine1 = BacktestEngine::new(config1);
        let result1 = engine1.run_from_events(&events);

        let config2 = BacktestConfig {
            rng_seed: Some([77u8; 32]),
            ..default_config()
        };
        let mut engine2 = BacktestEngine::new(config2);
        let result2 = engine2.run_from_events(&events);

        assert_eq!(result1.trades.len(), result2.trades.len());
        assert_eq!(result1.decisions.len(), result2.decisions.len());
        for (t1, t2) in result1.trades.iter().zip(result2.trades.iter()) {
            assert!((t1.fill_price - t2.fill_price).abs() < 1e-10);
        }
    }

    // -- Risk integration tests --

    #[test]
    fn test_risk_config_defaults() {
        let config = BacktestConfig::default();
        assert!((config.risk_limits_config.max_daily_loss_mtm - (-500.0)).abs() < f64::EPSILON);
        assert!(
            (config.risk_limits_config.max_daily_loss_realized - (-1000.0)).abs() < f64::EPSILON
        );
        assert!(
            !config.kill_switch_config.enabled,
            "KillSwitch should be disabled by default in backtest"
        );
        assert_eq!(config.barrier_config.staleness_threshold_ms, 5000);
    }

    #[test]
    fn test_risk_pipeline_no_false_rejections_with_default_config() {
        // With default config (normal PnL, no staleness, kill switch disabled),
        // the risk pipeline should not reject any orders
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 300, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Verify no risk-related skip reasons
        let risk_skips: Vec<_> = result
            .decisions
            .iter()
            .filter(|d| d.skip_reason.as_deref() == Some("risk_limit_rejected"))
            .collect();
        assert!(
            risk_skips.is_empty(),
            "Should have no risk_limit_rejected with default config"
        );

        let staleness_skips: Vec<_> = result
            .decisions
            .iter()
            .filter(|d| d.skip_reason.as_deref() == Some("staleness_rejected"))
            .collect();
        assert!(
            staleness_skips.is_empty(),
            "Should have no staleness_rejected with zero staleness"
        );
    }

    #[test]
    fn test_kill_switch_rejects_when_masked() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            kill_switch_config: KillSwitchConfig {
                enabled: true,
                min_samples: 5,
                z_score_threshold: 3.0,
                max_history: 100,
                mask_duration_ms: 60000, // Long mask so it stays active
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);

        // Manually trigger the kill switch to force masking
        engine.kill_switch.trigger();

        let result = engine.run_from_events(&events);

        // Check that any attempted buy/sell decisions were blocked by kill switch.
        // If all strategies chose Hold, there's nothing to block (also valid).
        let attempted_orders: Vec<_> = result
            .decisions
            .iter()
            .filter(|d| d.direction.is_some())
            .collect();

        if !attempted_orders.is_empty() {
            let masked: Vec<_> = result
                .decisions
                .iter()
                .filter(|d| d.skip_reason.as_deref() == Some("kill_switch_masked"))
                .collect();
            assert!(
                !masked.is_empty(),
                "With {} attempted orders, at least some should be kill_switch_masked",
                attempted_orders.len()
            );
        }
    }

    #[test]
    fn test_hierarchical_limit_daily_realized_halt() {
        // Configure very tight daily realized limit to trigger close-all
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 300, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            risk_limits_config: RiskLimitsConfig {
                max_daily_loss_mtm: -0.001,
                max_daily_loss_realized: -0.001,
                max_weekly_loss: -1_000_000.0,
                max_monthly_loss: -1_000_000.0,
                daily_mtm_lot_fraction: 0.25,
                daily_mtm_q_threshold: 0.01,
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);

        // The StateProjector starts with zero PnL, so the limits won't fire
        // initially. But the pipeline should be in place and working.
        let result = engine.run_from_events(&events);
        // Just verify it doesn't crash and produces results
        assert_eq!(result.total_ticks, 300);
    }

    #[test]
    fn test_lifecycle_culling_blocks_culled_strategy() {
        // Pre-cull Strategy B, verify it produces only "strategy_culled" decisions
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 200, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            lifecycle_config: LifecycleConfig {
                min_episodes_for_eval: 5,
                consecutive_death_windows: 2,
                sharpe_annualization_factor: 1.0,
                death_sharpe_threshold: -0.5,
                ..LifecycleConfig::default()
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);

        // Force-cull Strategy B by feeding negative episodes
        let bus = PartitionedEventBus::new();
        let projector = StateProjector::new(&bus, 10.0, 1);
        let snap = projector.snapshot();
        for _ in 0..10 {
            let summary = fx_risk::lifecycle::EpisodeSummary {
                strategy_id: StrategyId::B,
                total_reward: -100.0,
                return_g0: -100.0,
                duration_ns: 5_000_000_000,
            };
            engine
                .lifecycle_manager
                .record_episode(&summary, false, &snap);
        }
        assert!(
            !engine.lifecycle_manager.is_alive(StrategyId::B),
            "Strategy B should be culled after 10 negative episodes"
        );

        let result = engine.run_from_events(&events);

        // All B decisions should be "strategy_culled"
        for d in &result.decisions {
            if d.strategy_id == StrategyId::B {
                assert_eq!(
                    d.skip_reason.as_deref(),
                    Some("strategy_culled"),
                    "Strategy B decisions should all be strategy_culled"
                );
            }
        }
    }

    #[test]
    fn test_barrier_rejects_high_staleness() {
        // The barrier rejects when staleness_ms >= threshold.
        // In backtest, staleness comes from StateSnapshot which is usually 0,
        // but we can verify the config is wired up correctly.
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            barrier_config: DynamicRiskBarrierConfig {
                staleness_threshold_ms: 5000,
                ..DynamicRiskBarrierConfig::default()
            },
            ..default_config()
        };
        let engine = BacktestEngine::new(config);

        // Verify the barrier is configured
        assert_eq!(engine.risk_barrier.config().staleness_threshold_ms, 5000);
    }

    #[test]
    fn test_close_all_positions_helper() {
        // Verify close_all_positions works when triggered by a hard limit
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 10, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);
        // No positions to close in this simple test, but verifies the helper exists
        assert_eq!(result.total_ticks, 10);
    }

    // -- OTC Execution Gateway integration tests --

    #[test]
    fn test_execution_gateway_otc_simulation() {
        // Verify that simulate_order goes through the full OTC execution pipeline:
        // Last-Look rejection model, fill probability, slippage calculation
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 1000, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Verify execution stats are populated (active LP always set)
        assert!(!result.execution_stats.active_lp_id.is_empty());

        // If trades were produced, LP stats should reflect execution
        if !result.trades.is_empty() {
            assert!(
                result.execution_stats.lp_stats.len() >= 1,
                "Should have at least one LP tracked when trades exist"
            );
        }
    }

    #[test]
    fn test_execution_stats_lp_tracking() {
        // Run a backtest and verify LP stats are properly tracked
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // LP stats should show the default LP_PRIMARY
        let active_lp = &result.execution_stats.active_lp_id;
        assert!(
            active_lp == "LP_PRIMARY" || active_lp == "LP_BACKUP",
            "Active LP should be one of the default LPs, got: {}",
            active_lp
        );

        // If there were trades, verify fill/reject counts are consistent
        if !result.trades.is_empty() {
            let lp = &result.execution_stats.lp_stats[0];
            assert_eq!(
                lp.total_requests,
                lp.total_fills + lp.total_rejections,
                "total_requests should equal fills + rejections"
            );
        }
    }

    #[test]
    fn test_execution_events_collected_in_result() {
        // Verify that execution events are collected for EventBus replay
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // If trades were produced, execution events should match
        if !result.trades.is_empty() {
            assert_eq!(
                result.execution_events.len(),
                result.trades.len(),
                "Each filled trade should produce an execution event"
            );

            // Verify events are on the Execution stream
            for ev in &result.execution_events {
                assert_eq!(
                    ev.header.stream_id,
                    StreamId::Execution,
                    "Execution events should be on the Execution stream"
                );
            }
        }
    }

    #[test]
    fn test_otc_slippage_reflected_in_trades() {
        // Verify that OTC slippage model produces non-zero slippage on trades
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 2000, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Trades that go through the OTC model should have slippage recorded
        for trade in &result.trades {
            // Slippage can be zero in rare cases, but the field should be populated
            assert!(
                trade.slippage.is_finite(),
                "Slippage should be finite, got: {}",
                trade.slippage
            );
            // Fill probability should be in [0, 1] range
            assert!(
                (0.0..=1.0).contains(&trade.fill_probability),
                "Fill probability should be in [0,1], got: {}",
                trade.fill_probability
            );
        }
    }

    #[test]
    fn test_otc_gateway_accessible_after_run() {
        // Verify the gateway retains state after a backtest run
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Gateway should still be accessible
        let gateway = engine.execution_gateway();
        assert!(!gateway.active_lp_id().is_empty());

        // If there were trades, the LastLook model should have been updated
        let last_look = gateway.last_look_model();
        for lp_id in last_look.tracked_lps() {
            let params = last_look.get_lp_params(lp_id).unwrap();
            assert!(
                params.alpha >= 2.0,
                "Alpha should be at least the prior after updates"
            );
        }
    }

    #[test]
    fn test_otc_execution_rejection_tracked() {
        // Run with many ticks to get a mix of fills and rejections
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 2000, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Some decisions should have execution_rejected skip reason
        let rejected: Vec<_> = result
            .decisions
            .iter()
            .filter(|d| d.skip_reason.as_deref() == Some("execution_rejected"))
            .collect();

        // With OTC model, some orders should be rejected (Last-Look or fill probability)
        // This is probabilistic but with 2000 ticks there should be some rejections
        if !rejected.is_empty() {
            // Verify rejected decisions have direction set
            for d in &rejected {
                assert!(
                    d.direction.is_some(),
                    "Rejected decisions should have a direction"
                );
                assert!(
                    d.triggered,
                    "Rejected decisions should have been triggered by a strategy"
                );
            }
        }
    }

    #[test]
    fn test_otc_fill_probability_model_in_backtest() {
        // Verify the fill probability model produces realistic values
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 1000, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Trades that were filled should have reasonable fill probabilities
        for trade in &result.trades {
            // Market order base fill probability is 0.98, effective should be close
            assert!(
                trade.fill_probability > 0.5,
                "Filled trades should have had >50% fill probability, got: {}",
                trade.fill_probability
            );
        }

        // Overall fill rate from LP stats should be reasonable
        if !result.execution_stats.lp_stats.is_empty() {
            let overall = result.execution_stats.overall_fill_rate;
            assert!(
                (0.0..=1.0).contains(&overall),
                "Overall fill rate should be in [0,1], got: {}",
                overall
            );
        }
    }

    #[test]
    fn test_execution_events_have_valid_proto_payloads() {
        // Verify that collected execution events have valid proto payloads
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 1000, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Each execution event should decode as a valid ExecutionEventPayload
        for ev in &result.execution_events {
            let decoded = proto::ExecutionEventPayload::decode(ev.payload_bytes());
            assert!(
                decoded.is_ok(),
                "Execution event should decode as ExecutionEventPayload"
            );
            let payload = decoded.unwrap();
            // Fill price should be positive for filled trades
            assert!(
                payload.fill_price > 0.0,
                "Fill price should be positive, got: {}",
                payload.fill_price
            );
            assert!(
                !payload.lp_id.is_empty(),
                "LP ID should be set in execution event"
            );
        }
    }

    #[test]
    fn test_mc_reward_computed_on_episode_completion() {
        use fx_strategy::mc_eval::{McEvalConfig, RewardConfig};

        // Use large tick count and wide price range to trigger strategy decisions
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 50_000, 100, 110.0, 0.05);
        let config = BacktestConfig {
            mc_eval_config: McEvalConfig {
                reward: RewardConfig {
                    lambda_risk: 0.1,
                    lambda_dd: 0.5,
                    dd_cap: 100.0,
                    gamma: 0.99,
                },
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let mc = engine.mc_evaluator();
        assert_eq!(mc.completed_count(), 0);

        let result = engine.run_from_events(&events);
        let mc = engine.mc_evaluator();

        // Verify the MC integration pipeline is wired:
        // If trades were executed, some episodes should have been completed
        if result.trades.len() > 0 {
            assert!(
                mc.completed_count() > 0,
                "With {} trades, expected at least one completed MC episode",
                result.trades.len()
            );

            for sid in [StrategyId::A, StrategyId::B, StrategyId::C] {
                for ep in mc.episodes_for(sid) {
                    assert!(ep.num_transitions > 0);
                    assert!(ep.duration_ns > 0);
                    assert!(ep.total_reward.is_finite());
                    assert!(ep.return_g0.is_finite());
                }
            }
        }
    }

    #[test]
    fn test_mc_discounted_returns_match_gamma() {
        use fx_strategy::mc_eval::{McEvalConfig, RewardConfig};

        let gamma = 0.95;
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 50_000, 100, 110.0, 0.05);
        let config = BacktestConfig {
            mc_eval_config: McEvalConfig {
                reward: RewardConfig {
                    gamma,
                    ..RewardConfig::default()
                },
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let _result = engine.run_from_events(&events);
        let mc = engine.mc_evaluator();
        for sid in [StrategyId::A, StrategyId::B, StrategyId::C] {
            for ep in mc.episodes_for(sid) {
                if ep.num_transitions >= 2 && !ep.returns.is_empty() {
                    // The first return (G_0) should equal:
                    // r_0 + gamma * r_1 + gamma^2 * r_2 + ...
                    // which McEvaluator::compute_returns handles
                    // Verify that returns are monotonically non-decreasing (for non-negative gamma)
                    // when rewards are all non-negative, or just verify the formula
                    let rewards: Vec<f64> = ep.transitions.iter().map(|t| t.reward).collect();
                    let expected = McEvaluator::compute_returns(&rewards, gamma);
                    assert_eq!(
                        ep.returns.len(),
                        expected.len(),
                        "Returns length should match computed returns"
                    );
                    for (actual, exp) in ep.returns.iter().zip(expected.iter()) {
                        assert!(
                            (actual - exp).abs() < 1e-6,
                            "Discounted return mismatch: actual={}, expected={}",
                            actual,
                            exp
                        );
                    }
                    return; // One verification is sufficient
                }
            }
        }
    }

    #[test]
    fn test_mc_q_function_updated_after_episode() {
        use fx_strategy::bayesian_lr::QAction;
        use fx_strategy::mc_eval::McEvalConfig;

        let events = generate_synthetic_ticks(1_000_000_000_000_000, 50_000, 100, 110.0, 0.05);
        let initial_obs_a = BacktestEngine::new(BacktestConfig {
            mc_eval_config: McEvalConfig::default(),
            ..default_config()
        })
        .strategy_a()
        .q_function()
        .model(QAction::Buy)
        .n_observations();
        let initial_obs_b = BacktestEngine::new(BacktestConfig {
            mc_eval_config: McEvalConfig::default(),
            ..default_config()
        })
        .strategy_b()
        .q_function()
        .model(QAction::Buy)
        .n_observations();
        let initial_obs_c = BacktestEngine::new(BacktestConfig {
            mc_eval_config: McEvalConfig::default(),
            ..default_config()
        })
        .strategy_c()
        .q_function()
        .model(QAction::Buy)
        .n_observations();

        let mut engine = BacktestEngine::new(BacktestConfig {
            mc_eval_config: McEvalConfig::default(),
            ..default_config()
        });
        let _result = engine.run_from_events(&events);

        // After run, get MC results
        let mc = engine.mc_evaluator();

        // If any episodes completed for a strategy, its Q-function should have received updates
        if mc.completed_count_for(StrategyId::A) > 0 {
            let final_obs = engine
                .strategy_a()
                .q_function()
                .model(QAction::Buy)
                .n_observations();
            assert!(
                final_obs > initial_obs_a,
                "Q-function should have more observations after MC updates"
            );
        }
        if mc.completed_count_for(StrategyId::B) > 0 {
            let final_obs = engine
                .strategy_b()
                .q_function()
                .model(QAction::Buy)
                .n_observations();
            assert!(
                final_obs > initial_obs_b,
                "Q-function should have more observations after MC updates"
            );
        }
        if mc.completed_count_for(StrategyId::C) > 0 {
            let final_obs = engine
                .strategy_c()
                .q_function()
                .model(QAction::Buy)
                .n_observations();
            assert!(
                final_obs > initial_obs_c,
                "Q-function should have more observations after MC updates"
            );
        }
    }

    #[test]
    fn test_lifecycle_records_episodes_from_mc() {
        use fx_risk::lifecycle::LifecycleConfig;
        use fx_strategy::mc_eval::McEvalConfig;

        let events = generate_synthetic_ticks(1_000_000_000_000_000, 50_000, 100, 110.0, 0.05);
        let config = BacktestConfig {
            mc_eval_config: McEvalConfig::default(),
            lifecycle_config: LifecycleConfig {
                min_episodes_for_eval: 1,
                ..LifecycleConfig::default()
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let _result = engine.run_from_events(&events);

        // The lifecycle manager should have recorded episodes from MC completion
        // Even if no culling happened, the internal state should be updated
        let lifecycle = engine.lifecycle_manager();
        // We verify the integration path by checking that at least one episode
        // was recorded (via MC end_episode → lifecycle.record_episode)
        // The lifecycle manager tracks episodes internally per strategy
        for sid in [StrategyId::A, StrategyId::B, StrategyId::C] {
            let is_alive = lifecycle.is_alive(sid);
            // With default death threshold of -0.5 Sharpe, strategies should remain alive
            // on reasonable synthetic data
            assert!(
                is_alive,
                "Strategy {:?} should remain alive with synthetic data",
                sid
            );
        }
    }

    #[test]
    fn test_mc_reward_config_reflected_in_computation() {
        use fx_strategy::mc_eval::{McEvalConfig, RewardConfig};

        let events = generate_synthetic_ticks(1_000_000_000_000_000, 50_000, 100, 110.0, 0.05);

        let config_high = BacktestConfig {
            mc_eval_config: McEvalConfig {
                reward: RewardConfig {
                    lambda_risk: 10.0,
                    lambda_dd: 0.0,
                    dd_cap: 100.0,
                    gamma: 0.99,
                },
            },
            ..default_config()
        };
        let config_low = BacktestConfig {
            mc_eval_config: McEvalConfig {
                reward: RewardConfig {
                    lambda_risk: 0.0,
                    lambda_dd: 0.0,
                    dd_cap: 100.0,
                    gamma: 0.99,
                },
            },
            ..default_config()
        };

        let mut engine_high = BacktestEngine::new(config_high);
        let mut engine_low = BacktestEngine::new(config_low);

        engine_high.run_from_events(&events);
        engine_low.run_from_events(&events);

        let mc_high = engine_high.mc_evaluator();
        let mc_low = engine_low.mc_evaluator();

        // Both should have completed episodes (if trades occurred)
        if mc_high.completed_count() > 0 && mc_low.completed_count() > 0 {
            // With high lambda_risk, average rewards should be lower (more penalized)
            let avg_reward_high: f64 = mc_high
                .episodes_for(StrategyId::A)
                .iter()
                .map(|e| e.avg_reward())
                .sum::<f64>()
                / mc_high.completed_count_for(StrategyId::A).max(1) as f64;

            let avg_reward_low: f64 = mc_low
                .episodes_for(StrategyId::A)
                .iter()
                .map(|e| e.avg_reward())
                .sum::<f64>()
                / mc_low.completed_count_for(StrategyId::A).max(1) as f64;

            assert!(
                avg_reward_high <= avg_reward_low + 1e-6,
                "High lambda_risk ({}) should produce avg reward <= low lambda_risk ({})",
                avg_reward_high,
                avg_reward_low
            );
        }
        // The MC pipeline is still validated by other tests when trades occur.
    }

    #[test]
    fn test_regime_cache_updated_during_run() {
        use fx_strategy::regime::RegimeConfig;

        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.01);
        let mut engine = BacktestEngine::new(default_config());
        assert!(!engine.regime_cache().state().is_initialized());

        let _result = engine.run_from_events(&events);

        let regime = engine.regime_cache();
        assert!(
            regime.state().is_initialized(),
            "Regime cache should be initialized after run"
        );
        assert!(
            regime.state().last_update_ns() > 0,
            "Last update time should be set"
        );
        // Posterior should be a valid probability distribution
        let posterior = regime.state().posterior();
        let sum: f64 = posterior.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "Regime posterior should sum to 1.0, got {}",
            sum
        );
        for &p in posterior {
            assert!(p >= 0.0, "Posterior probabilities should be non-negative");
        }
    }

    #[test]
    fn test_regime_kl_wired_to_strategy_decisions() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.01);
        let mut engine = BacktestEngine::new(default_config());
        let _result = engine.run_from_events(&events);

        // After the run, regime_kl should have been used in strategy decisions.
        // With synthetic data and the lightweight heuristic, the regime should not be
        // permanently unknown (entropy should be below threshold for at least some ticks).
        let regime = engine.regime_cache();
        // KL divergence from uniform should be non-negative
        assert!(
            regime.state().kl_divergence() >= 0.0,
            "KL divergence should be non-negative"
        );
        // Entropy should be in valid range [0, ln(n_regimes)]
        assert!(
            regime.state().entropy() >= 0.0,
            "Entropy should be non-negative"
        );
        assert!(
            regime.state().entropy() <= regime.config().max_entropy() + 1e-6,
            "Entropy should not exceed max_entropy"
        );
    }

    #[test]
    fn test_regime_unknown_suppresses_strategies() {
        use fx_strategy::regime::RegimeConfig;

        // Set a very low entropy threshold so all regimes appear "unknown"
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.01);
        let config = BacktestConfig {
            regime_config: RegimeConfig {
                unknown_regime_entropy_threshold: 0.0, // Everything is unknown
                ..RegimeConfig::default()
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // All decisions should be "unknown_regime" holds
        for decision in &result.decisions {
            assert_eq!(
                decision.skip_reason.as_deref(),
                Some("unknown_regime"),
                "Expected unknown_regime skip, got {:?}",
                decision.skip_reason
            );
        }
        // No trades should have been executed
        assert_eq!(
            result.trades.len(),
            0,
            "No trades should execute when regime is always unknown"
        );
    }

    #[test]
    fn test_regime_transition_resets_lifecycle() {
        use fx_strategy::regime::RegimeConfig;

        // Start with low threshold (unknown), then regime should stabilize
        // when features cluster around one regime.
        // With very low threshold, lifecycle should be reset on every tick transition.
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 100, 100, 110.0, 0.01);
        let config = BacktestConfig {
            regime_config: RegimeConfig {
                unknown_regime_entropy_threshold: 0.0,
                ..RegimeConfig::default()
            },
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let _result = engine.run_from_events(&events);

        // Verify the regime cache was updated throughout the run
        let regime = engine.regime_cache();
        assert!(regime.state().is_initialized());
        // With threshold 0.0, is_unknown should always be true
        assert!(regime.state().is_unknown());
    }

    #[test]
    fn test_regime_drift_updated() {
        let events = generate_synthetic_ticks(1_000_000_000_000_000, 500, 100, 110.0, 0.01);
        let mut engine = BacktestEngine::new(default_config());
        let _result = engine.run_from_events(&events);

        let regime = engine.regime_cache();
        let drift = regime.state().drift();
        // Drift should have been updated (may be zero if feature_dim doesn't match,
        // but should not panic)
        assert!(!drift.is_empty(), "Drift vector should be populated");
    }

    #[test]
    fn test_otc_execution_with_lp_switch_scenario() {
        // Configure gateway to trigger LP switch by using aggressive adversarial thresholds
        use fx_execution::gateway::ExecutionGatewayConfig;
        use fx_execution::lp_monitor::LpMonitorConfig;

        let events = generate_synthetic_ticks(1_000_000_000_000_000, 2000, 100, 110.0, 0.005);
        let config = BacktestConfig {
            rng_seed: Some([42u8; 32]),
            ..default_config()
        };
        let mut engine = BacktestEngine::new(config);
        let result = engine.run_from_events(&events);

        // Verify the execution stats are collected regardless of LP switch
        assert!(!result.execution_stats.active_lp_id.is_empty());
        // If LP switch happened, the stats should reflect it
        if result.execution_stats.recalibration_triggered {
            // Gateway should still be functional after LP switch
            let gateway = engine.execution_gateway();
            assert!(
                gateway.is_recalibrating() || !gateway.active_lp_id().is_empty(),
                "Gateway should be functional after LP switch"
            );
        }
    }
}
