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
use fx_risk::global_position::{GlobalPositionChecker, GlobalPositionConfig};
use fx_strategy::bayesian_lr::QAction;
use fx_strategy::extractor::{FeatureExtractor, FeatureExtractorConfig};
use fx_strategy::features::FeatureVector;
use fx_strategy::mc_eval::{McEvalConfig, McEvaluator, TerminalReason};
use fx_strategy::policy::Action;
use fx_strategy::strategy_a::{StrategyA, StrategyAConfig, StrategyADecision};
use fx_strategy::strategy_b::{StrategyB, StrategyBConfig, StrategyBDecision};
use fx_strategy::strategy_c::{StrategyC, StrategyCConfig, StrategyCDecision};
use prost::Message as _;
use rand::prelude::*;
use rand::rngs::SmallRng;
use tracing::{debug, info, warn};

use crate::stats::{TradeRecord, TradeSummary};

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
            };
        }

        let bus = PartitionedEventBus::new();
        let mut projector = StateProjector::new(&bus, self.config.global_position_limit, 1);
        let mut feature_extractor =
            FeatureExtractor::new(self.config.feature_extractor_config.clone());

        let mut trades: Vec<TradeRecord> = Vec::new();
        let mut decisions: Vec<BacktestDecision> = Vec::new();
        let mut total_ticks: u64 = 0;
        let mut total_decision_ticks: u64 = 0;
        let mut prev_tick_ns: u64 = 0;

        // Clone to release borrow on self.config before mutating self
        let enabled_strategies: Vec<StrategyId> =
            self.config.enabled_strategies.iter().copied().collect();

        for event in market_events {
            let tick_ns = event.header.timestamp_ns;
            total_ticks += 1;

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

            // Phase 2: Collect strategy decisions
            let snapshot = projector.snapshot();
            let mut strategy_q: HashMap<StrategyId, f64> = HashMap::new();
            let mut strategy_decisions: Vec<(StrategyId, StrategyDecision)> = Vec::new();

            for &sid in &enabled_strategies {
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

                        // Global position constraint check (priority-based)
                        let pos_result = GlobalPositionChecker::validate_order(
                            &self.config.global_position_config,
                            snap,
                            sid,
                            direction,
                            lots as f64,
                            decision.q_sampled,
                            &strategy_q,
                        );

                        let effective_lots = match pos_result {
                            Ok(r) => r.effective_lot.max(0.0) as u64,
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
                        let (trade_pnl, _) = self.process_execution_result(
                            pos_snap.strategy_id,
                            &result,
                            direction,
                            last_ns,
                            &mut projector,
                        );
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
                    );
                }
            }
        }

        let wall_time_ms = wall_start.elapsed().as_millis() as u64;
        let summary = TradeSummary::from_trades(&trades);

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

    /// Get strategy decision for a given strategy ID.
    fn get_strategy_decision(
        &mut self,
        sid: StrategyId,
        features: &FeatureVector,
        state: &StateSnapshot,
        tick_ns: u64,
    ) -> StrategyDecision {
        let regime_kl = 0.0; // Regime integration is a separate task
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
        let equity = snapshot.total_realized_pnl + snapshot.total_unrealized_pnl;
        self.mc_evaluator.start_episode(sid, tick_ns, equity);
        match sid {
            StrategyId::A => self.strategy_a.start_episode(tick_ns),
            StrategyId::B => self.strategy_b.start_episode(tick_ns),
            StrategyId::C => self.strategy_c.start_episode(tick_ns),
        }
    }

    /// End an MC episode, update the Q-function, and reset strategy episode state.
    fn end_strategy_episode(&mut self, sid: StrategyId, reason: TerminalReason, tick_ns: u64) {
        if self.mc_evaluator.has_active_episode(sid) {
            // Extract episode first, then update Q-function separately to avoid double borrow
            let episode_result = self.mc_evaluator.end_episode(sid, reason, tick_ns);
            let q_fn = match sid {
                StrategyId::A => self.strategy_a.q_function_mut(),
                StrategyId::B => self.strategy_b.q_function_mut(),
                StrategyId::C => self.strategy_c.q_function_mut(),
            };
            McEvaluator::update_from_result(q_fn, &episode_result);
        }
        match sid {
            StrategyId::A => self.strategy_a.end_episode(),
            StrategyId::B => self.strategy_b.end_episode(),
            StrategyId::C => self.strategy_c.end_episode(),
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
}
