use std::collections::VecDeque;

use fx_core::types::StrategyId;
use fx_events::event::Event;
use fx_events::projector::StateSnapshot;
use fx_events::proto;
use prost::Message;

use crate::features::FeatureVector;

/// Rolling window for online statistical computations.
struct RollingWindow {
    values: VecDeque<f64>,
    capacity: usize,
    sum: f64,
    sum_sq: f64,
}

impl RollingWindow {
    fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    fn push(&mut self, v: f64) {
        if self.values.len() >= self.capacity {
            if let Some(old) = self.values.pop_front() {
                self.sum -= old;
                self.sum_sq -= old * old;
            }
        }
        self.sum += v;
        self.sum_sq += v * v;
        self.values.push_back(v);
    }

    fn mean(&self) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.sum / self.values.len() as f64
    }

    fn variance(&self) -> f64 {
        let n = self.values.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.mean();
        (self.sum_sq - n as f64 * mean * mean) / (n - 1) as f64
    }

    fn std(&self) -> f64 {
        self.variance().sqrt().max(0.0)
    }

    fn z_score(&self, v: f64) -> f64 {
        let std = self.std();
        if std < f64::EPSILON {
            return 0.0;
        }
        (v - self.mean()) / std
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn latest(&self) -> Option<f64> {
        self.values.back().copied()
    }

    fn prev(&self) -> Option<f64> {
        if self.values.len() >= 2 {
            self.values.get(self.values.len() - 2).copied()
        } else {
            None
        }
    }
}

/// Lagged execution stats for information leakage prevention.
#[derive(Debug, Clone, Default)]
struct LaggedExecutionStats {
    fill_rate_ema: f64,
    slippage_ema: f64,
    reject_rate_ema: f64,
    drift_ema: f64,
    trade_count_window: VecDeque<(u64, i32)>,
    volume_sum_window: VecDeque<(u64, f64)>,
    first_execution_ns: Option<u64>,
}

impl LaggedExecutionStats {
    fn new() -> Self {
        Self {
            fill_rate_ema: 0.0,
            slippage_ema: 0.0,
            reject_rate_ema: 0.0,
            drift_ema: 0.0,
            trade_count_window: VecDeque::with_capacity(1000),
            volume_sum_window: VecDeque::with_capacity(1000),
            first_execution_ns: None,
        }
    }

    fn update(
        &mut self,
        fill_status: i32,
        slippage: f64,
        size: f64,
        execution_drift: f64,
        timestamp_ns: u64,
    ) {
        let alpha = 0.05;
        let is_fill = fill_status == proto::FillStatus::Filled as i32
            || fill_status == proto::FillStatus::PartialFill as i32;

        if self.first_execution_ns.is_none() {
            self.first_execution_ns = Some(timestamp_ns);
        }

        let instant_fill_rate = if is_fill { 1.0 } else { 0.0 };
        let instant_reject_rate = if is_fill { 0.0 } else { 1.0 };
        self.fill_rate_ema += alpha * (instant_fill_rate - self.fill_rate_ema);
        self.slippage_ema += alpha * (slippage.abs() - self.slippage_ema);
        self.reject_rate_ema += alpha * (instant_reject_rate - self.reject_rate_ema);
        self.drift_ema += alpha * (execution_drift - self.drift_ema);

        let count_delta: i32 = if is_fill { 1 } else { 0 };
        self.trade_count_window
            .push_back((timestamp_ns, count_delta));
        self.volume_sum_window
            .push_back((timestamp_ns, size * if is_fill { 1.0 } else { 0.0 }));
    }

    fn prune_before(&mut self, cutoff_ns: u64) {
        while self
            .trade_count_window
            .front()
            .map(|(ts, _)| *ts < cutoff_ns)
            .unwrap_or(false)
        {
            self.trade_count_window.pop_front();
        }
        while self
            .volume_sum_window
            .front()
            .map(|(ts, _)| *ts < cutoff_ns)
            .unwrap_or(false)
        {
            self.volume_sum_window.pop_front();
        }
    }

    fn trade_intensity(&self) -> f64 {
        self.trade_count_window.iter().map(|(_, c)| *c as f64).sum()
    }

    fn signed_volume(&self) -> f64 {
        self.volume_sum_window.iter().map(|(_, v)| *v).sum()
    }
}

/// Configuration for the feature extractor.
#[derive(Debug, Clone)]
pub struct FeatureExtractorConfig {
    /// Spread rolling window size (for z-score computation).
    pub spread_window: usize,
    /// OBI rolling window size.
    pub obi_window: usize,
    /// Realized volatility window in ticks.
    pub vol_window: usize,
    /// Long-term volatility window for ratio computation.
    pub vol_long_window: usize,
    /// Trade intensity lookback window in nanoseconds.
    pub trade_intensity_window_ns: u64,
    /// Forced lag in nanoseconds for execution-related features.
    pub execution_lag_ns: u64,
    /// Default decay rate for time_decay (λ = 1/τ, τ in ms).
    pub default_decay_rate: f64,
    /// Typical lot size for self_impact normalization.
    pub typical_lot_size: f64,
    /// Maximum holding time in ms for time_decay computation.
    pub max_hold_time_ms: f64,
    /// Session transition timestamps (UTC hour) [Tokyo open, London open, NY open, Sydney open].
    pub session_hours_utc: [u8; 4],
}

impl Default for FeatureExtractorConfig {
    fn default() -> Self {
        Self {
            spread_window: 200,
            obi_window: 200,
            vol_window: 100,
            vol_long_window: 500,
            trade_intensity_window_ns: 60_000_000_000, // 60 seconds
            execution_lag_ns: 500_000_000,             // 500ms forced lag
            default_decay_rate: 0.01,
            typical_lot_size: 1000.0,
            max_hold_time_ms: 60_000.0,
            session_hours_utc: [0, 8, 13, 22], // Tokyo, London, NY, Sydney
        }
    }
}

/// FX session enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Tokyo,
    London,
    NewYork,
    Sydney,
}

impl Session {
    fn from_utc_hour(hour: u8, session_hours: &[u8; 4]) -> Self {
        // Tokyo: 0:00-8:00 UTC, London: 8:00-13:00 UTC, NY: 13:00-22:00 UTC, Sydney: 22:00-24:00 UTC
        if hour >= session_hours[3] {
            Session::Sydney
        } else if hour >= session_hours[2] {
            Session::NewYork
        } else if hour >= session_hours[1] {
            Session::London
        } else {
            Session::Tokyo
        }
    }

    fn one_hot(&self) -> (f64, f64, f64, f64) {
        match self {
            Session::Tokyo => (1.0, 0.0, 0.0, 0.0),
            Session::London => (0.0, 1.0, 0.0, 0.0),
            Session::NewYork => (0.0, 0.0, 1.0, 0.0),
            Session::Sydney => (0.0, 0.0, 0.0, 1.0),
        }
    }

    fn numeric(&self) -> f64 {
        match self {
            Session::Tokyo => 0.0,
            Session::London => 1.0,
            Session::NewYork => 2.0,
            Session::Sydney => 3.0,
        }
    }
}

/// Internal state for volatility tracking.
struct VolatilityState {
    mid_prices: VecDeque<f64>,
    prev_realized_vol: f64,
    vol_ema_long: f64,
}

impl VolatilityState {
    fn new() -> Self {
        Self {
            mid_prices: VecDeque::with_capacity(600),
            prev_realized_vol: 0.0,
            vol_ema_long: 0.0,
        }
    }

    fn push_mid(&mut self, mid: f64) {
        self.mid_prices.push_back(mid);
        if self.mid_prices.len() > 600 {
            self.mid_prices.pop_front();
        }
    }

    fn realized_volatility(&self, window: usize) -> f64 {
        if self.mid_prices.len() < 2 {
            return 0.0;
        }
        let n = window.min(self.mid_prices.len() - 1);
        let start = self.mid_prices.len() - 1 - n;
        let prices: Vec<f64> = self.mid_prices.iter().skip(start).copied().collect();

        let log_returns: Vec<f64> = prices
            .windows(2)
            .map(|w| if w[0] > 0.0 { (w[1] / w[0]).ln() } else { 0.0 })
            .collect();

        if log_returns.is_empty() {
            return 0.0;
        }

        let mean = log_returns.iter().sum::<f64>() / log_returns.len() as f64;
        let variance =
            log_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / log_returns.len() as f64;
        variance.sqrt()
    }
}

/// Main feature extractor. Consumes market/execution events and produces
/// FeatureVector for Q-function evaluation.
///
/// **Information leakage prevention:** Execution-related features
/// (`recent_fill_rate`, `recent_slippage`, `trade_intensity`, `signed_volume`)
/// and `pnl_unrealized` use forced lag. Execution stats are only visible after
/// `execution_lag_ns` has elapsed since the execution event timestamp.
pub struct FeatureExtractor {
    config: FeatureExtractorConfig,
    spread_window: RollingWindow,
    obi_window: RollingWindow,
    vol_state: VolatilityState,
    lagged_exec: LaggedExecutionStats,
    prev_obi: f64,
    last_vol_spike_ns: u64,
    last_depth_total: f64,
    prev_depth_total: f64,
    session_open_ns: u64,
    gap_hold: bool,
}

impl FeatureExtractor {
    pub fn new(config: FeatureExtractorConfig) -> Self {
        Self {
            spread_window: RollingWindow::new(config.spread_window),
            obi_window: RollingWindow::new(config.obi_window),
            vol_state: VolatilityState::new(),
            lagged_exec: LaggedExecutionStats::new(),
            prev_obi: 0.0,
            last_vol_spike_ns: 0,
            last_depth_total: 0.0,
            prev_depth_total: 0.0,
            session_open_ns: 0,
            gap_hold: false,
            config,
        }
    }

    /// Process a market event to update internal state.
    pub fn process_market_event(&mut self, event: &fx_events::event::GenericEvent) {
        let market = match proto::MarketEventPayload::decode(event.payload_bytes()) {
            Ok(m) => m,
            Err(_) => return,
        };

        let bid = market.bid;
        let ask = market.ask;
        let mid = (bid + ask) / 2.0;
        let spread = ask - bid;
        let bid_size = market.bid_size;
        let ask_size = market.ask_size;

        // Update spread
        self.spread_window.push(spread);

        // Compute OBI (Order Book Imbalance): (ask_vol - bid_vol) / (ask_vol + bid_vol)
        let total_depth = bid_size + ask_size;
        let obi = if total_depth > 0.0 {
            (ask_size - bid_size) / total_depth
        } else {
            0.0
        };
        self.obi_window.push(obi);
        self.prev_obi = self.obi_window.prev().unwrap_or(obi);

        // Depth change rate (computed lazily in extract)
        self.prev_depth_total = self.last_depth_total;
        self.last_depth_total = total_depth;

        // Update volatility state
        self.vol_state.push_mid(mid);

        // Detect volatility spike
        if self.spread_window.len() >= 10 {
            let current_vol = self
                .vol_state
                .realized_volatility(self.config.vol_window.min(20));
            if current_vol > self.vol_state.vol_ema_long * 3.0 && self.vol_state.vol_ema_long > 0.0
            {
                self.last_vol_spike_ns = market.timestamp_ns;
            }
        }

        // Session open tracking (simple heuristic: track from first event)
        if self.session_open_ns == 0 {
            self.session_open_ns = market.timestamp_ns;
        }

        // Prune lagged execution stats
        let cutoff = market
            .timestamp_ns
            .saturating_sub(self.config.trade_intensity_window_ns);
        self.lagged_exec.prune_before(cutoff);
    }

    /// Process an execution event to update lagged execution statistics.
    pub fn process_execution_event(&mut self, event: &fx_events::event::GenericEvent) {
        let execution = match proto::ExecutionEventPayload::decode(event.payload_bytes()) {
            Ok(e) => e,
            Err(_) => return,
        };

        let timestamp_ns = execution
            .header
            .as_ref()
            .map(|h| h.timestamp_ns)
            .unwrap_or(event.header.timestamp_ns);

        self.lagged_exec.update(
            execution.fill_status,
            execution.slippage,
            execution.fill_size,
            execution.execution_drift_trend,
            timestamp_ns,
        );
    }

    /// Set gap hold state. When true, features are held (return last computed values).
    pub fn set_gap_hold(&mut self, hold: bool) {
        self.gap_hold = hold;
    }

    /// Extract the full feature vector at the current market state.
    pub fn extract(
        &self,
        market_event: &fx_events::event::GenericEvent,
        state: &StateSnapshot,
        strategy_id: StrategyId,
        now_ns: u64,
    ) -> FeatureVector {
        let market = proto::MarketEventPayload::decode(market_event.payload_bytes())
            .unwrap_or_else(|_| proto::MarketEventPayload::default());

        let bid = market.bid;
        let ask = market.ask;
        let spread = ask - bid;
        let bid_size = market.bid_size;
        let ask_size = market.ask_size;
        let timestamp_ns = market.timestamp_ns;

        // --- Microstructure features ---
        let spread_zscore = self.spread_window.z_score(spread);

        let total_depth = bid_size + ask_size;
        let obi = if total_depth > 0.0 {
            (ask_size - bid_size) / total_depth
        } else {
            0.0
        };
        let delta_obi = obi - self.prev_obi;

        let depth_change_rate = if self.prev_depth_total > 0.0 {
            (total_depth - self.prev_depth_total) / self.prev_depth_total
        } else {
            0.0
        };

        // Queue position estimate: position of our order in the queue
        // Simplified: ratio of our position to total depth on our side
        let position = state.positions.get(&strategy_id);
        let pos_size = position.map(|p| p.size.abs()).unwrap_or(0.0);
        let queue_position = if bid_size > 0.0 {
            (pos_size / self.config.typical_lot_size / (bid_size / self.config.typical_lot_size))
                .min(1.0)
        } else {
            0.0
        };

        // --- Volatility features ---
        let realized_vol = self.vol_state.realized_volatility(self.config.vol_window);
        let realized_vol_long = self
            .vol_state
            .realized_volatility(self.config.vol_long_window);
        let volatility_ratio = if realized_vol_long > f64::EPSILON {
            realized_vol / realized_vol_long
        } else {
            1.0
        };
        let volatility_decay_rate = if self.vol_state.prev_realized_vol > f64::EPSILON {
            (realized_vol - self.vol_state.prev_realized_vol) / self.vol_state.prev_realized_vol
        } else {
            0.0
        };

        // --- Time features ---
        let session = Session::from_utc_hour(
            ((timestamp_ns / 3_600_000_000_000) % 24) as u8,
            &self.config.session_hours_utc,
        );
        let (session_tokyo, session_london, session_ny, session_sydney) = session.one_hot();

        let time_since_open_ms = if self.session_open_ns > 0 && timestamp_ns > self.session_open_ns
        {
            ((timestamp_ns - self.session_open_ns) / 1_000_000) as f64
        } else {
            0.0
        };

        let time_since_last_spike_ms =
            if self.last_vol_spike_ns > 0 && timestamp_ns > self.last_vol_spike_ns {
                ((timestamp_ns - self.last_vol_spike_ns) / 1_000_000) as f64
            } else {
                f64::MAX
            };

        let holding_time_ms = position
            .map(|p| p.holding_time_ms(now_ns) as f64)
            .unwrap_or(0.0);

        // --- Position state features ---
        let position_size = position.map(|p| p.size).unwrap_or(0.0);
        let position_direction = if position_size.abs() < f64::EPSILON {
            0.0
        } else {
            position_size.signum()
        };
        let entry_price = position.map(|p| p.entry_price).unwrap_or(0.0);

        // pnl_unrealized: uses lagged mid-price (not current) for information leakage prevention.
        // We use the state's unrealized_pnl which is computed by StateProjector using
        // the mid-price at the last market event time, which is inherently lagged by
        // one tick relative to the decision time.
        let pnl_unrealized = position.map(|p| p.unrealized_pnl).unwrap_or(0.0);

        // --- Order flow / execution features (LAGGED) ---
        // Only use execution stats after execution_lag_ns has elapsed since first execution
        let lag_expired = self
            .lagged_exec
            .first_execution_ns
            .map(|first| now_ns.saturating_sub(first) >= self.config.execution_lag_ns)
            .unwrap_or(false);
        let recent_fill_rate = if lag_expired {
            self.lagged_exec.fill_rate_ema
        } else {
            0.0
        };
        let recent_slippage = if lag_expired {
            self.lagged_exec.slippage_ema
        } else {
            0.0
        };
        let recent_reject_rate = if lag_expired {
            self.lagged_exec.reject_rate_ema
        } else {
            0.0
        };
        let execution_drift_trend = if lag_expired {
            self.lagged_exec.drift_ema
        } else {
            0.0
        };
        let trade_intensity = self.lagged_exec.trade_intensity();
        let signed_volume = self.lagged_exec.signed_volume();

        // --- Nonlinear transformation terms ---
        let self_impact = self.compute_self_impact(position_size, realized_vol);
        let time_decay = self.compute_time_decay(holding_time_ms);
        let dynamic_cost = self.compute_dynamic_cost(spread, realized_vol, obi);
        let p_revert = self.compute_p_revert(spread_zscore, obi, depth_change_rate);
        let p_continue = self.compute_p_continue(volatility_ratio, trade_intensity);
        let p_trend = self.compute_p_trend(session, obi);

        // --- Interaction terms ---
        let spread_z_x_vol = spread_zscore * realized_vol;
        let obi_x_session = obi * session.numeric();
        let depth_drop_x_vol_spike = depth_change_rate
            * if time_since_last_spike_ms < 5000.0 {
                1.0
            } else {
                0.0
            };
        let position_size_x_vol = position_size.abs() * realized_vol;
        let obi_x_vol = obi * realized_vol;
        let spread_z_x_self_impact = spread_zscore * self_impact;

        FeatureVector {
            spread,
            spread_zscore,
            obi,
            delta_obi,
            depth_change_rate,
            queue_position,
            realized_volatility: realized_vol,
            volatility_ratio,
            volatility_decay_rate,
            session_tokyo,
            session_london,
            session_ny,
            session_sydney,
            time_since_open_ms,
            time_since_last_spike_ms,
            holding_time_ms,
            position_size,
            position_direction,
            entry_price,
            pnl_unrealized,
            trade_intensity,
            signed_volume,
            recent_fill_rate,
            recent_slippage,
            recent_reject_rate,
            execution_drift_trend,
            self_impact,
            time_decay,
            dynamic_cost,
            p_revert,
            p_continue,
            p_trend,
            spread_z_x_vol,
            obi_x_session,
            depth_drop_x_vol_spike,
            position_size_x_vol,
            obi_x_vol,
            spread_z_x_self_impact,
        }
    }

    fn compute_self_impact(&self, position_size: f64, vol: f64) -> f64 {
        // Self-impact: larger positions in low-vol environments have higher market impact
        // |pos| * vol / typical_lot (simplified Kyle's lambda)
        let normalized_pos = position_size.abs() / self.config.typical_lot_size;
        normalized_pos * vol
    }

    fn compute_time_decay(&self, holding_time_ms: f64) -> f64 {
        // Exponential decay: exp(-λ * t), where λ = default_decay_rate
        if holding_time_ms <= 0.0 {
            1.0
        } else {
            (-self.config.default_decay_rate * holding_time_ms).exp()
        }
    }

    fn compute_dynamic_cost(&self, spread: f64, vol: f64, obi: f64) -> f64 {
        // Dynamic cost: base spread + adverse OBI premium + volatility premium
        // Higher OBI (more sell-side imbalance when buying) increases cost
        let obi_premium = (obi.abs() * spread * 0.5).max(0.0);
        let vol_premium = vol * spread * 10.0;
        spread + obi_premium + vol_premium
    }

    fn compute_p_revert(&self, spread_zscore: f64, obi: f64, depth_change_rate: f64) -> f64 {
        // P(revert): probability of mean reversion
        // Higher spread z-score and adverse OBI suggest reversion more likely
        // Depth drop also suggests reversion (liquidity shock)
        let spread_signal = (spread_zscore.abs() / 3.0).min(1.0);
        let obi_signal = (obi.abs() * 2.0).min(1.0);
        let depth_signal = if depth_change_rate < -0.2 { 1.0 } else { 0.0 };
        // Combine: weighted average clamped to [0, 1]
        (spread_signal * 0.4 + obi_signal * 0.3 + depth_signal * 0.3).clamp(0.0, 1.0)
    }

    fn compute_p_continue(&self, vol_ratio: f64, trade_intensity: f64) -> f64 {
        // P(continue): probability of momentum continuation
        // Higher vol ratio (>1 = current vol > long vol) suggests momentum
        // Higher trade intensity supports continuation
        let vol_signal = if vol_ratio > 1.0 {
            ((vol_ratio - 1.0) * 2.0).min(1.0)
        } else {
            0.0
        };
        let intensity_signal = (trade_intensity / 20.0).min(1.0);
        (vol_signal * 0.6 + intensity_signal * 0.4).clamp(0.0, 1.0)
    }

    fn compute_p_trend(&self, session: Session, obi: f64) -> f64 {
        // P(trend): probability of directional trend
        // Strong OBI in certain sessions suggests trend
        // NY and London sessions tend to have stronger trends
        let session_bias = match session {
            Session::London => 0.1,
            Session::NewYork => 0.15,
            Session::Tokyo => -0.05,
            Session::Sydney => 0.0,
        };
        (0.5 + session_bias + obi * 0.3).clamp(0.0, 1.0)
    }

    /// Current OBI value (for strategy-specific feature access).
    pub fn current_obi(&self) -> f64 {
        self.obi_window.latest().unwrap_or(0.0)
    }

    /// Current spread z-score.
    pub fn current_spread_zscore(&self) -> f64 {
        self.spread_window
            .latest()
            .map(|s| self.spread_window.z_score(s))
            .unwrap_or(0.0)
    }

    /// Current realized volatility.
    pub fn current_realized_vol(&self) -> f64 {
        self.vol_state.realized_volatility(self.config.vol_window)
    }

    /// Whether gap hold is active.
    pub fn is_gap_hold(&self) -> bool {
        self.gap_hold
    }

    /// Reset session open timestamp (e.g., on session change).
    pub fn reset_session_open(&mut self, timestamp_ns: u64) {
        self.session_open_ns = timestamp_ns;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use fx_core::types::{EventTier, StreamId};
    use fx_events::event::GenericEvent;
    use fx_events::header::EventHeader;
    use fx_events::projector::{LimitStateData, Position};
    use uuid::Uuid;

    use super::*;

    const NS_BASE: u64 = 1_000_000_000_000_000;
    const TICK_MS: u64 = 100;

    fn make_header(stream_id: StreamId, timestamp_ns: u64, seq: u64) -> EventHeader {
        EventHeader {
            event_id: Uuid::now_v7(),
            parent_event_id: None,
            stream_id,
            sequence_id: seq,
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

    fn make_market_event(
        ts_offset_ms: u64,
        bid: f64,
        ask: f64,
        bid_size: f64,
        ask_size: f64,
        seq: u64,
    ) -> GenericEvent {
        let ts = NS_BASE + ts_offset_ms * 1_000_000;
        let payload = proto::MarketEventPayload {
            header: None,
            symbol: "USD/JPY".to_string(),
            bid,
            ask,
            bid_size,
            ask_size,
            timestamp_ns: ts,
            bid_levels: vec![],
            ask_levels: vec![],
            latency_ms: 0.5,
        }
        .encode_to_vec();
        GenericEvent::new(make_header(StreamId::Market, ts, seq), payload)
    }

    fn make_execution_event(
        ts_offset_ms: u64,
        fill_status: i32,
        slippage: f64,
        fill_size: f64,
        seq: u64,
    ) -> GenericEvent {
        let ts = NS_BASE + ts_offset_ms * 1_000_000;
        let payload = proto::ExecutionEventPayload {
            header: None,
            order_id: "ord-001".to_string(),
            symbol: "USD/JPY".to_string(),
            order_type: proto::OrderType::OrderMarket as i32,
            fill_status,
            fill_price: 110.0,
            fill_size,
            slippage,
            requested_price: 110.0,
            requested_size: fill_size.abs(),
            fill_probability: 0.95,
            effective_fill_probability: 0.90,
            price_improvement: 0.0,
            last_look_rejection_prob: 0.05,
            lp_id: "LP1".to_string(),
            latency_ms: 1.0,
            reject_reason: proto::RejectReason::Unspecified as i32,
            reject_message: String::new(),
            execution_drift_trend: slippage,
            ..Default::default()
        }
        .encode_to_vec();
        GenericEvent::new(make_header(StreamId::Execution, ts, seq), payload)
    }

    fn make_state_snapshot() -> StateSnapshot {
        StateSnapshot {
            positions: HashMap::new(),
            global_position: 0.0,
            global_position_limit: 10.0,
            total_unrealized_pnl: 0.0,
            total_realized_pnl: 0.0,
            limit_state: LimitStateData::default(),
            state_version: 0,
            staleness_ms: 0,
            state_hash: "test".to_string(),
            lot_multiplier: 1.0,
            last_market_data_ns: NS_BASE,
        }
    }

    fn make_state_with_position(
        size: f64,
        entry_price: f64,
        unrealized_pnl: f64,
        entry_ns: u64,
    ) -> StateSnapshot {
        let mut positions = HashMap::new();
        positions.insert(
            StrategyId::A,
            Position {
                strategy_id: StrategyId::A,
                size,
                entry_price,
                unrealized_pnl,
                realized_pnl: 0.0,
                entry_timestamp_ns: entry_ns,
            },
        );
        StateSnapshot {
            positions,
            global_position: size,
            global_position_limit: 10.0,
            total_unrealized_pnl: unrealized_pnl,
            total_realized_pnl: 0.0,
            limit_state: LimitStateData::default(),
            state_version: 1,
            staleness_ms: 0,
            state_hash: "test".to_string(),
            lot_multiplier: 1.0,
            last_market_data_ns: NS_BASE,
        }
    }

    fn default_extractor() -> FeatureExtractor {
        FeatureExtractor::new(FeatureExtractorConfig {
            spread_window: 50,
            obi_window: 50,
            vol_window: 20,
            vol_long_window: 50,
            trade_intensity_window_ns: 10_000_000_000, // 10 seconds for tests
            execution_lag_ns: 100_000_000,             // 100ms for tests
            ..Default::default()
        })
    }

    // --- Microstructure feature tests ---

    #[test]
    fn test_spread_computed_correctly() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1_000_000.0, 1_000_000.0, 1);
        ext.process_market_event(&event);

        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.spread - 0.005).abs() < 1e-10);
    }

    #[test]
    fn test_spread_zscore_converges() {
        let mut ext = default_extractor();
        // Feed constant spread events to build statistics
        for i in 0..60u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        // Constant spread → z-score should be near 0
        let event = make_market_event(60 * TICK_MS, 110.0, 110.005, 1e6, 1e6, 61);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 60 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.spread_zscore.abs() < 0.5,
            "z-score should be near 0 for constant spread, got {}",
            fv.spread_zscore
        );
    }

    #[test]
    fn test_spread_zscore_detects_widening() {
        let mut ext = default_extractor();
        // Feed normal spread
        for i in 0..60u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        // Widened spread
        let event = make_market_event(60 * TICK_MS, 110.0, 110.020, 1e6, 1e6, 61);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 60 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.spread_zscore > 1.0,
            "z-score should be positive for widened spread, got {}",
            fv.spread_zscore
        );
    }

    #[test]
    fn test_obi_balanced() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1_000_000.0, 1_000_000.0, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.obi - 0.0).abs() < 1e-10, "balanced book → OBI = 0");
    }

    #[test]
    fn test_obi_sell_imbalance() {
        let mut ext = default_extractor();
        // More bid size → sell imbalance (more sellers on bid side)
        let event = make_market_event(0, 110.0, 110.005, 2_000_000.0, 500_000.0, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        let expected_obi = (500_000.0 - 2_000_000.0) / 2_500_000.0;
        assert!((fv.obi - expected_obi).abs() < 1e-10);
        assert!(fv.obi < 0.0, "sell imbalance → OBI < 0");
    }

    #[test]
    fn test_delta_obi_initial() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!(
            (fv.delta_obi - 0.0).abs() < 1e-10,
            "first tick → delta_obi = 0"
        );
    }

    #[test]
    fn test_delta_obi_changes() {
        let mut ext = default_extractor();
        let event1 = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event1);
        let event2 = make_market_event(TICK_MS, 110.0, 110.005, 2e6, 500_000.0, 2);
        ext.process_market_event(&event2);
        let fv = ext.extract(
            &event2,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + TICK_MS * 1_000_000,
        );
        assert!(
            (fv.delta_obi - (-0.6)).abs() < 1e-10,
            "OBI shifted from 0.0 to -0.6, delta should be -0.6, got {}",
            fv.delta_obi
        );
    }

    #[test]
    fn test_depth_change_rate() {
        let mut ext = default_extractor();
        let event1 = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event1);
        let event2 = make_market_event(TICK_MS, 110.0, 110.005, 500_000.0, 500_000.0, 2);
        ext.process_market_event(&event2);
        let fv = ext.extract(
            &event2,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + TICK_MS * 1_000_000,
        );
        // First event sets last_depth_total=2e6, second computes change from 2e6 to 1e6
        let expected = (1_000_000.0 - 2_000_000.0) / 2_000_000.0;
        assert!(
            (fv.depth_change_rate - expected).abs() < 1e-10,
            "depth should halve, got {}",
            fv.depth_change_rate
        );
    }

    #[test]
    fn test_queue_position_zero_when_no_position() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.queue_position - 0.0).abs() < 1e-10);
    }

    // --- Volatility feature tests ---

    #[test]
    fn test_realized_volatility_zero_for_constant_price() {
        let mut ext = default_extractor();
        for i in 0..30u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(30 * TICK_MS, 110.0, 110.005, 1e6, 1e6, 31);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.realized_volatility.abs() < 1e-12,
            "constant price → vol ≈ 0, got {}",
            fv.realized_volatility
        );
    }

    #[test]
    fn test_realized_volatility_increases_with_movement() {
        let mut ext = default_extractor();
        // Feed small price movements
        for i in 0..30u64 {
            let bid = 110.0 + (i as f64) * 0.001;
            let ask = bid + 0.005;
            let event = make_market_event(i * TICK_MS, bid, ask, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(30 * TICK_MS, 110.03, 110.035, 1e6, 1e6, 31);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.realized_volatility > 0.0,
            "price movement → vol > 0, got {}",
            fv.realized_volatility
        );
    }

    #[test]
    fn test_volatility_ratio() {
        let mut ext = default_extractor();
        for i in 0..60u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(60 * TICK_MS, 110.0, 110.005, 1e6, 1e6, 61);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 60 * TICK_MS * 1_000_000,
        );
        // Constant price → both short and long vol are 0 → ratio should be 1.0 (default)
        assert!(
            (fv.volatility_ratio - 1.0).abs() < 1e-10,
            "constant price → vol_ratio = 1.0, got {}",
            fv.volatility_ratio
        );
    }

    #[test]
    fn test_volatility_decay_rate() {
        let mut ext = default_extractor();
        // Feed constant prices
        for i in 0..30u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(30 * TICK_MS, 110.0, 110.005, 1e6, 1e6, 31);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.volatility_decay_rate.abs() < 1e-10,
            "constant price → decay_rate ≈ 0, got {}",
            fv.volatility_decay_rate
        );
    }

    // --- Time feature tests ---

    #[test]
    fn test_session_one_hot() {
        let (t, l, n, s) = Session::Tokyo.one_hot();
        assert!((t - 1.0).abs() < 1e-10);
        assert!((l - 0.0).abs() < 1e-10);
        assert!((n - 0.0).abs() < 1e-10);
        assert!((s - 0.0).abs() < 1e-10);

        let (t, l, _n, _s) = Session::London.one_hot();
        assert!((t - 0.0).abs() < 1e-10);
        assert!((l - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_session_from_utc_hour() {
        let config = FeatureExtractorConfig::default();
        assert_eq!(
            Session::from_utc_hour(3, &config.session_hours_utc),
            Session::Tokyo
        );
        assert_eq!(
            Session::from_utc_hour(10, &config.session_hours_utc),
            Session::London
        );
        assert_eq!(
            Session::from_utc_hour(15, &config.session_hours_utc),
            Session::NewYork
        );
        assert_eq!(
            Session::from_utc_hour(23, &config.session_hours_utc),
            Session::Sydney
        );
    }

    #[test]
    fn test_time_since_open() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);

        let event2 = make_market_event(5000, 110.0, 110.005, 1e6, 1e6, 2);
        ext.process_market_event(&event2);
        let fv = ext.extract(
            &event2,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 5000 * 1_000_000,
        );
        assert!(
            (fv.time_since_open_ms - 5000.0).abs() < 1e-5,
            "time_since_open should be 5000ms, got {}",
            fv.time_since_open_ms
        );
    }

    #[test]
    fn test_time_since_last_spike_initial() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert_eq!(fv.time_since_last_spike_ms, f64::MAX, "no spike → MAX");
    }

    #[test]
    fn test_holding_time_no_position() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.holding_time_ms - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_holding_time_with_position() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let entry_ns = NS_BASE;
        let state = make_state_with_position(1000.0, 110.0, 0.0, entry_ns);
        let now_ns = NS_BASE + 10_000_000; // 10ms later
        let fv = ext.extract(&event, &state, StrategyId::A, now_ns);
        assert!((fv.holding_time_ms - 10.0).abs() < 1e-5);
    }

    // --- Position state feature tests ---

    #[test]
    fn test_position_features_no_position() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.position_size - 0.0).abs() < 1e-10);
        assert!((fv.position_direction - 0.0).abs() < 1e-10);
        assert!((fv.entry_price - 0.0).abs() < 1e-10);
        assert!((fv.pnl_unrealized - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_position_features_long() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let state = make_state_with_position(1000.0, 110.0, 5.0, NS_BASE);
        let fv = ext.extract(&event, &state, StrategyId::A, NS_BASE);
        assert!((fv.position_size - 1000.0).abs() < 1e-10);
        assert!((fv.position_direction - 1.0).abs() < 1e-10);
        assert!((fv.entry_price - 110.0).abs() < 1e-10);
        assert!((fv.pnl_unrealized - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_position_features_short() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let state = make_state_with_position(-500.0, 110.0, -3.0, NS_BASE);
        let fv = ext.extract(&event, &state, StrategyId::A, NS_BASE);
        assert!((fv.position_size - (-500.0)).abs() < 1e-10);
        assert!((fv.position_direction - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_position_features_wrong_strategy() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        // State has position for strategy A, but we query strategy B
        let state = make_state_with_position(1000.0, 110.0, 5.0, NS_BASE);
        let fv = ext.extract(&event, &state, StrategyId::B, NS_BASE);
        assert!((fv.position_size - 0.0).abs() < 1e-10);
    }

    // --- Order flow / execution feature tests ---

    #[test]
    fn test_execution_lag_fill_rate_initial() {
        let ext = default_extractor();
        // Before any events and before lag window
        assert!((ext.lagged_exec.fill_rate_ema - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_execution_lag_fill_rate_after_fill() {
        let mut ext = default_extractor();
        let exec_event =
            make_execution_event(0, proto::FillStatus::Filled as i32, 0.001, 1000.0, 1);
        ext.process_execution_event(&exec_event);

        let market_event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&market_event);

        // now_ns is within lag window → should return 0
        let fv = ext.extract(
            &market_event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE,
        );
        assert!(
            (fv.recent_fill_rate - 0.0).abs() < 1e-10,
            "within lag window → fill_rate = 0, got {}",
            fv.recent_fill_rate
        );
    }

    #[test]
    fn test_execution_lag_fill_rate_after_lag_window() {
        let mut ext = default_extractor();
        let exec_event =
            make_execution_event(0, proto::FillStatus::Filled as i32, 0.001, 1000.0, 1);
        ext.process_execution_event(&exec_event);

        let market_event = make_market_event(200, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&market_event);

        // now_ns = NS_BASE + 200ms > execution_lag_ns (100ms) → should return EMA
        let now_ns = NS_BASE + 200 * 1_000_000;
        let fv = ext.extract(&market_event, &make_state_snapshot(), StrategyId::A, now_ns);
        assert!(
            fv.recent_fill_rate > 0.0,
            "after lag window → fill_rate > 0, got {}",
            fv.recent_fill_rate
        );
    }

    #[test]
    fn test_trade_intensity_counts_fills() {
        let mut ext = default_extractor();
        for i in 0..5u64 {
            let exec = make_execution_event(
                i * TICK_MS,
                proto::FillStatus::Filled as i32,
                0.001,
                100.0,
                i + 1,
            );
            ext.process_execution_event(&exec);
        }
        let market_event = make_market_event(500, 110.0, 110.005, 1e6, 1e6, 10);
        ext.process_market_event(&market_event);
        let now_ns = NS_BASE + 500 * 1_000_000;
        let fv = ext.extract(&market_event, &make_state_snapshot(), StrategyId::A, now_ns);
        assert!(
            (fv.trade_intensity - 5.0).abs() < 1e-10,
            "5 fills → trade_intensity = 5, got {}",
            fv.trade_intensity
        );
    }

    #[test]
    fn test_signed_volume() {
        let mut ext = default_extractor();
        let exec1 = make_execution_event(0, proto::FillStatus::Filled as i32, 0.001, 1000.0, 1);
        ext.process_execution_event(&exec1);
        let exec2 =
            make_execution_event(TICK_MS, proto::FillStatus::Filled as i32, 0.002, 500.0, 2);
        ext.process_execution_event(&exec2);

        let market_event = make_market_event(200, 110.0, 110.005, 1e6, 1e6, 10);
        ext.process_market_event(&market_event);
        let now_ns = NS_BASE + 200 * 1_000_000;
        let fv = ext.extract(&market_event, &make_state_snapshot(), StrategyId::A, now_ns);
        assert!(
            (fv.signed_volume - 1500.0).abs() < 1e-10,
            "1000 + 500 = 1500, got {}",
            fv.signed_volume
        );
    }

    #[test]
    fn test_rejected_execution_not_counted_in_intensity() {
        let mut ext = default_extractor();
        let exec = make_execution_event(0, proto::FillStatus::Rejected as i32, 0.0, 1000.0, 1);
        ext.process_execution_event(&exec);

        let market_event = make_market_event(200, 110.0, 110.005, 1e6, 1e6, 10);
        ext.process_market_event(&market_event);
        let now_ns = NS_BASE + 200 * 1_000_000;
        let fv = ext.extract(&market_event, &make_state_snapshot(), StrategyId::A, now_ns);
        assert!(
            (fv.trade_intensity - 0.0).abs() < 1e-10,
            "rejected → trade_intensity = 0, got {}",
            fv.trade_intensity
        );
    }

    // --- Nonlinear transformation tests ---

    #[test]
    fn test_self_impact_zero_no_position() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.self_impact - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_self_impact_scales_with_position() {
        let mut ext = default_extractor();
        for i in 0..30u64 {
            let bid = 110.0 + (i as f64) * 0.001;
            let event = make_market_event(i * TICK_MS, bid, bid + 0.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(30 * TICK_MS, 110.03, 110.035, 1e6, 1e6, 31);
        ext.process_market_event(&event);

        let state = make_state_with_position(2000.0, 110.0, 0.0, NS_BASE);
        let fv = ext.extract(
            &event,
            &state,
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );

        // Larger position should have higher self_impact
        let state_small = make_state_with_position(500.0, 110.0, 0.0, NS_BASE);
        let fv_small = ext.extract(
            &event,
            &state_small,
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.self_impact > fv_small.self_impact,
            "larger position → higher self_impact: {} vs {}",
            fv.self_impact,
            fv_small.self_impact
        );
    }

    #[test]
    fn test_time_decay_at_zero() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let state = make_state_with_position(1000.0, 110.0, 0.0, NS_BASE);
        let fv = ext.extract(&event, &state, StrategyId::A, NS_BASE);
        assert!(
            (fv.time_decay - 1.0).abs() < 1e-10,
            "zero holding time → decay = 1.0, got {}",
            fv.time_decay
        );
    }

    #[test]
    fn test_time_decay_decreases() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);

        // Long holding time
        let state = make_state_with_position(1000.0, 110.0, 0.0, NS_BASE);
        let now_ns = NS_BASE + 50_000_000_000; // 50 seconds
        let fv = ext.extract(&event, &state, StrategyId::A, now_ns);
        assert!(
            fv.time_decay < 1.0,
            "long holding → decay < 1.0, got {}",
            fv.time_decay
        );
        assert!(
            fv.time_decay > 0.0,
            "decay should be positive, got {}",
            fv.time_decay
        );
    }

    #[test]
    fn test_dynamic_cost_includes_spread() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.010, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!(
            fv.dynamic_cost >= 0.010,
            "dynamic_cost should include spread (0.010), got {}",
            fv.dynamic_cost
        );
    }

    #[test]
    fn test_p_revert_clamped() {
        let mut ext = default_extractor();
        // Extreme spread z-score
        for i in 0..60u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(60 * TICK_MS, 110.0, 110.100, 1e6, 1e6, 61);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 60 * TICK_MS * 1_000_000,
        );
        assert!(
            fv.p_revert >= 0.0 && fv.p_revert <= 1.0,
            "p_revert should be in [0, 1], got {}",
            fv.p_revert
        );
    }

    #[test]
    fn test_p_continue_low_vol_constant() {
        let mut ext = default_extractor();
        for i in 0..30u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(30 * TICK_MS, 110.0, 110.005, 1e6, 1e6, 31);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );
        assert!(
            (fv.p_continue - 0.0).abs() < 1e-10,
            "constant price, no trades → p_continue = 0, got {}",
            fv.p_continue
        );
    }

    #[test]
    fn test_p_trend_depends_on_session() {
        let config = FeatureExtractorConfig::default();
        let ext = FeatureExtractor::new(config);
        let p_tokyo = ext.compute_p_trend(Session::Tokyo, 0.0);
        let p_ny = ext.compute_p_trend(Session::NewYork, 0.0);
        assert!(
            p_ny > p_tokyo,
            "NY should have higher trend prob than Tokyo: {} vs {}",
            p_ny,
            p_tokyo
        );
    }

    // --- Interaction term tests ---

    #[test]
    fn test_interaction_spread_z_x_vol() {
        let mut ext = default_extractor();
        for i in 0..60u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(60 * TICK_MS, 110.0, 110.020, 1e6, 1e6, 61);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 60 * TICK_MS * 1_000_000,
        );
        let expected = fv.spread_zscore * fv.realized_volatility;
        assert!(
            (fv.spread_z_x_vol - expected).abs() < 1e-12,
            "spread_z_x_vol = spread_zscore * vol, expected {}, got {}",
            expected,
            fv.spread_z_x_vol
        );
    }

    #[test]
    fn test_interaction_obi_x_session() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        // Balanced book → OBI = 0 → interaction = 0
        assert!((fv.obi_x_session - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_interaction_depth_drop_x_vol_spike_recent() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);

        // Simulate recent vol spike by manipulating last_vol_spike_ns
        // We can't directly set it, but we can verify the interaction when spike is old
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        // No spike → depth_drop_x_vol_spike should be 0 (time_since_spike = MAX > 5000)
        assert!(
            (fv.depth_drop_x_vol_spike - 0.0).abs() < 1e-10,
            "no recent spike → interaction = 0, got {}",
            fv.depth_drop_x_vol_spike
        );
    }

    #[test]
    fn test_interaction_position_size_x_vol() {
        let mut ext = default_extractor();
        for i in 0..30u64 {
            let bid = 110.0 + (i as f64) * 0.001;
            let event = make_market_event(i * TICK_MS, bid, bid + 0.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(30 * TICK_MS, 110.03, 110.035, 1e6, 1e6, 31);
        ext.process_market_event(&event);

        let state = make_state_with_position(1000.0, 110.0, 0.0, NS_BASE);
        let fv = ext.extract(
            &event,
            &state,
            StrategyId::A,
            NS_BASE + 30 * TICK_MS * 1_000_000,
        );
        let expected = 1000.0 * fv.realized_volatility;
        assert!(
            (fv.position_size_x_vol - expected).abs() < 1e-12,
            "expected {}, got {}",
            expected,
            fv.position_size_x_vol
        );
    }

    // --- Information leakage tests ---

    #[test]
    fn test_information_leakage_execution_lag_enforced() {
        let mut ext = default_extractor();
        // Execution at t=0
        let exec = make_execution_event(0, proto::FillStatus::Filled as i32, 0.001, 1000.0, 1);
        ext.process_execution_event(&exec);

        let market_event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&market_event);

        // Query at t=0 (same time as execution, within lag window)
        let fv = ext.extract(
            &market_event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE,
        );
        assert!(
            (fv.recent_fill_rate - 0.0).abs() < 1e-10,
            "fill rate must be 0 within lag window (leakage prevention), got {}",
            fv.recent_fill_rate
        );
        assert!(
            (fv.recent_slippage - 0.0).abs() < 1e-10,
            "slippage must be 0 within lag window (leakage prevention), got {}",
            fv.recent_slippage
        );
        assert!(
            (fv.recent_reject_rate - 0.0).abs() < 1e-10,
            "reject rate must be 0 within lag window (leakage prevention), got {}",
            fv.recent_reject_rate
        );
        assert!(
            (fv.execution_drift_trend - 0.0).abs() < 1e-10,
            "execution drift must be 0 within lag window (leakage prevention), got {}",
            fv.execution_drift_trend
        );
    }

    #[test]
    fn test_information_leakage_fill_rate_visible_after_lag() {
        let mut ext = default_extractor();
        let exec = make_execution_event(0, proto::FillStatus::Filled as i32, 0.001, 1000.0, 1);
        ext.process_execution_event(&exec);

        let market_event = make_market_event(500, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&market_event);

        // Query at t=500ms (well past 100ms lag)
        let now_ns = NS_BASE + 500 * 1_000_000;
        let fv = ext.extract(&market_event, &make_state_snapshot(), StrategyId::A, now_ns);
        assert!(
            fv.recent_fill_rate > 0.0,
            "fill rate should be visible after lag window, got {}",
            fv.recent_fill_rate
        );
        assert!(
            fv.recent_slippage > 0.0,
            "slippage should be visible after lag window, got {}",
            fv.recent_slippage
        );
        assert!(
            fv.execution_drift_trend > 0.0,
            "execution drift should be visible after lag window, got {}",
            fv.execution_drift_trend
        );
    }

    #[test]
    fn test_information_leakage_reject_rate_visible_after_lag() {
        let mut ext = default_extractor();
        let exec = make_execution_event(0, proto::FillStatus::Rejected as i32, 0.0, 1000.0, 1);
        ext.process_execution_event(&exec);

        let market_event = make_market_event(500, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&market_event);

        let now_ns = NS_BASE + 500 * 1_000_000;
        let fv = ext.extract(&market_event, &make_state_snapshot(), StrategyId::A, now_ns);
        assert!(
            fv.recent_reject_rate > 0.0,
            "reject rate should be visible after lag window, got {}",
            fv.recent_reject_rate
        );
    }

    #[test]
    fn test_information_leakage_unrealized_pnl_uses_state_snapshot() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 1e6, 1e6, 1);
        ext.process_market_event(&event);

        // State has unrealized PnL from a previous mid-price computation
        let state = make_state_with_position(1000.0, 110.0, 5.0, NS_BASE);
        let fv = ext.extract(&event, &state, StrategyId::A, NS_BASE);
        // Should use the state's unrealized PnL, not compute from current mid
        assert!(
            (fv.pnl_unrealized - 5.0).abs() < 1e-10,
            "pnl_unrealized should come from state snapshot (lagged), got {}",
            fv.pnl_unrealized
        );
    }

    // --- Gap hold tests ---

    #[test]
    fn test_gap_hold_flag() {
        let mut ext = default_extractor();
        assert!(!ext.is_gap_hold());
        ext.set_gap_hold(true);
        assert!(ext.is_gap_hold());
        ext.set_gap_hold(false);
        assert!(!ext.is_gap_hold());
    }

    // --- Flattened feature vector tests ---

    #[test]
    fn test_extract_produces_valid_flattened_vector() {
        let mut ext = default_extractor();
        for i in 0..10u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(10 * TICK_MS, 110.0, 110.005, 1e6, 1e6, 11);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 10 * TICK_MS * 1_000_000,
        );
        let flat = fv.flattened();
        assert_eq!(flat.len(), FeatureVector::DIM);

        // Verify roundtrip
        let restored = FeatureVector::from_flattened(&flat).unwrap();
        assert!((restored.spread - fv.spread).abs() < 1e-15);
        assert!((restored.obi - fv.obi).abs() < 1e-15);
        assert!((restored.realized_volatility - fv.realized_volatility).abs() < 1e-15);
    }

    #[test]
    fn test_all_features_finite() {
        let mut ext = default_extractor();
        for i in 0..30u64 {
            let bid = 110.0 + (i as f64) * 0.001;
            let event = make_market_event(i * TICK_MS, bid, bid + 0.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let exec = make_execution_event(0, proto::FillStatus::Filled as i32, 0.001, 1000.0, 1);
        ext.process_execution_event(&exec);

        let event = make_market_event(30 * TICK_MS, 110.03, 110.035, 1e6, 1e6, 31);
        ext.process_market_event(&event);

        let state = make_state_with_position(1000.0, 110.0, 5.0, NS_BASE);
        let now_ns = NS_BASE + 500 * 1_000_000;
        let fv = ext.extract(&event, &state, StrategyId::A, now_ns);
        let flat = fv.flattened();
        for (i, &v) in flat.iter().enumerate() {
            assert!(v.is_finite(), "feature[{}] = {} is not finite", i, v);
        }
    }

    // --- Rolling window unit tests ---

    #[test]
    fn test_rolling_window_basic() {
        let mut w = RollingWindow::new(5);
        w.push(1.0);
        w.push(2.0);
        w.push(3.0);
        assert!((w.mean() - 2.0).abs() < 1e-10);
        assert!((w.std() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_rolling_window_capacity() {
        let mut w = RollingWindow::new(3);
        w.push(1.0);
        w.push(2.0);
        w.push(3.0);
        w.push(4.0); // evicts 1.0
        assert_eq!(w.len(), 3);
        assert!((w.mean() - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_rolling_window_z_score() {
        let mut w = RollingWindow::new(100);
        for i in 0..50_i32 {
            w.push(10.0 + (i % 5) as f64 * 0.1); // varying values for non-zero std
        }
        let z = w.z_score(15.0);
        assert!(
            z > 0.0,
            "value above mean should have positive z-score, got {}",
            z
        );
    }

    #[test]
    fn test_rolling_window_latest_prev() {
        let mut w = RollingWindow::new(10);
        w.push(1.0);
        w.push(2.0);
        w.push(3.0);
        assert_eq!(w.latest(), Some(3.0));
        assert_eq!(w.prev(), Some(2.0));
    }

    #[test]
    fn test_rolling_window_single_element() {
        let mut w = RollingWindow::new(10);
        w.push(5.0);
        assert_eq!(w.mean(), 5.0);
        assert_eq!(w.std(), 0.0);
        assert_eq!(w.prev(), None);
    }

    // --- Lagged execution stats tests ---

    #[test]
    fn test_lagged_exec_pruning() {
        let mut stats = LaggedExecutionStats::new();
        stats.update(proto::FillStatus::Filled as i32, 0.001, 100.0, 0.001, 1000);
        stats.update(proto::FillStatus::Filled as i32, 0.002, 200.0, 0.002, 2000);
        stats.update(proto::FillStatus::Filled as i32, 0.003, 300.0, 0.003, 3000);
        assert_eq!(stats.trade_count_window.len(), 3);
        stats.prune_before(2500);
        // Events at 1000 and 2000 are pruned (ts < 2500), only 3000 remains
        assert_eq!(stats.trade_count_window.len(), 1);
    }

    #[test]
    fn test_lagged_exec_reject_not_counted() {
        let mut stats = LaggedExecutionStats::new();
        stats.update(proto::FillStatus::Rejected as i32, 0.0, 1000.0, 0.0, 1000);
        stats.update(proto::FillStatus::Filled as i32, 0.001, 500.0, 0.001, 2000);
        assert!((stats.trade_intensity() - 1.0).abs() < 1e-10);
        assert!((stats.signed_volume() - 500.0).abs() < 1e-10);
    }

    // --- Volatility state tests ---

    #[test]
    fn test_vol_state_realized_vol_empty() {
        let vs = VolatilityState::new();
        assert!((vs.realized_volatility(10) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_vol_state_realized_vol_single_price() {
        let mut vs = VolatilityState::new();
        vs.push_mid(110.0);
        assert!((vs.realized_volatility(10) - 0.0).abs() < 1e-10);
    }

    // --- Session tests ---

    #[test]
    fn test_session_numeric() {
        assert!((Session::Tokyo.numeric() - 0.0).abs() < 1e-10);
        assert!((Session::London.numeric() - 1.0).abs() < 1e-10);
        assert!((Session::NewYork.numeric() - 2.0).abs() < 1e-10);
        assert!((Session::Sydney.numeric() - 3.0).abs() < 1e-10);
    }

    // --- Edge case tests ---

    #[test]
    fn test_zero_bid_ask_sizes() {
        let mut ext = default_extractor();
        let event = make_market_event(0, 110.0, 110.005, 0.0, 0.0, 1);
        ext.process_market_event(&event);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        assert!((fv.obi - 0.0).abs() < 1e-10, "zero sizes → OBI = 0");
        assert!(fv.obi.is_finite());
        assert!(fv.queue_position.is_finite());
    }

    #[test]
    fn test_very_large_spread() {
        let mut ext = default_extractor();
        for i in 0..60u64 {
            let event = make_market_event(i * TICK_MS, 110.0, 110.005, 1e6, 1e6, i + 1);
            ext.process_market_event(&event);
        }
        let event = make_market_event(60 * TICK_MS, 100.0, 120.0, 1e6, 1e6, 61);
        ext.process_market_event(&event);
        let fv = ext.extract(
            &event,
            &make_state_snapshot(),
            StrategyId::A,
            NS_BASE + 60 * TICK_MS * 1_000_000,
        );
        assert!(fv.spread > 0.0);
        assert!(fv.spread_zscore.is_finite());
        assert!(fv.dynamic_cost.is_finite());
    }

    #[test]
    fn test_decode_error_handled_gracefully() {
        let mut ext = default_extractor();
        // Create a GenericEvent with invalid payload
        let header = make_header(StreamId::Market, NS_BASE, 1);
        let event = GenericEvent::new(header, vec![0xFF, 0xFE]); // invalid protobuf
        ext.process_market_event(&event); // should not panic
        assert!(!ext.is_gap_hold());
    }

    #[test]
    fn test_execution_decode_error_handled_gracefully() {
        let mut ext = default_extractor();
        let header = make_header(StreamId::Execution, NS_BASE, 1);
        let event = GenericEvent::new(header, vec![0xFF, 0xFE]);
        ext.process_execution_event(&event); // should not panic
    }

    #[test]
    fn test_extract_with_decode_error_returns_defaults() {
        let ext = default_extractor();
        let header = make_header(StreamId::Market, NS_BASE, 1);
        let event = GenericEvent::new(header, vec![0xFF, 0xFE]);
        let fv = ext.extract(&event, &make_state_snapshot(), StrategyId::A, NS_BASE);
        // Should return default/zero values since decode failed
        let flat = fv.flattened();
        for v in &flat {
            assert!(
                v.is_finite(),
                "all features should be finite even on decode error"
            );
        }
    }
}
