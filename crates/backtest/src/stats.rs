use fx_core::types::{Direction, StrategyId};

// ---------------------------------------------------------------------------
// LP Execution Stats
// ---------------------------------------------------------------------------

/// Per-LP execution statistics tracked during a backtest run.
#[derive(Debug, Clone)]
pub struct LpExecutionStats {
    pub lp_id: String,
    pub total_requests: u64,
    pub total_fills: u64,
    pub total_rejections: u64,
    pub fill_rate_ema: f64,
    pub is_adversarial: bool,
}

/// Aggregate execution statistics across all LPs.
#[derive(Debug, Clone)]
pub struct ExecutionStats {
    pub lp_stats: Vec<LpExecutionStats>,
    pub active_lp_id: String,
    pub total_fills: u64,
    pub total_rejections: u64,
    pub overall_fill_rate: f64,
    pub avg_slippage: f64,
    pub recalibration_triggered: bool,
}

impl ExecutionStats {
    pub fn empty() -> Self {
        Self {
            lp_stats: Vec::new(),
            active_lp_id: String::new(),
            total_fills: 0,
            total_rejections: 0,
            overall_fill_rate: 0.0,
            avg_slippage: 0.0,
            recalibration_triggered: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Trade Record
// ---------------------------------------------------------------------------

/// A single recorded trade (fill) from backtest execution.
#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub timestamp_ns: u64,
    pub strategy_id: StrategyId,
    pub direction: Direction,
    pub lots: f64,
    pub fill_price: f64,
    pub slippage: f64,
    pub pnl: f64,
    pub fill_probability: f64,
    pub latency_ms: f64,
    /// Reason for closing if this is a close trade.
    pub close_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Trade Summary / Performance Statistics
// ---------------------------------------------------------------------------

/// Aggregated performance statistics from a backtest run.
#[derive(Debug, Clone)]
pub struct TradeSummary {
    pub total_pnl: f64,
    pub realized_pnl: f64,
    pub total_trades: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub win_rate: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub profit_factor: f64,
    pub max_drawdown: f64,
    pub max_drawdown_duration_ns: u64,
    pub sharpe_ratio: f64,
    pub sortino_ratio: f64,
    pub avg_slippage: f64,
    pub avg_fill_probability: f64,
    pub avg_latency_ms: f64,
    pub max_consecutive_wins: u64,
    pub max_consecutive_losses: u64,
    pub avg_trade_duration_ns: u64,
}

impl TradeSummary {
    pub fn empty() -> Self {
        Self {
            total_pnl: 0.0,
            realized_pnl: 0.0,
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            win_rate: 0.0,
            avg_win: 0.0,
            avg_loss: 0.0,
            profit_factor: 0.0,
            max_drawdown: 0.0,
            max_drawdown_duration_ns: 0,
            sharpe_ratio: 0.0,
            sortino_ratio: 0.0,
            avg_slippage: 0.0,
            avg_fill_probability: 0.0,
            avg_latency_ms: 0.0,
            max_consecutive_wins: 0,
            max_consecutive_losses: 0,
            avg_trade_duration_ns: 0,
        }
    }

    /// Compute summary statistics from a list of trade records.
    pub fn from_trades(trades: &[TradeRecord]) -> Self {
        if trades.is_empty() {
            return Self::empty();
        }

        let total_trades = trades.len() as u64;
        let total_pnl: f64 = trades.iter().map(|t| t.pnl).sum();
        let realized_pnl = total_pnl; // In backtest, all PnL is realized

        let winning_trades = trades.iter().filter(|t| t.pnl > 0.0).count() as u64;
        let losing_trades = trades.iter().filter(|t| t.pnl < 0.0).count() as u64;
        let win_rate = winning_trades as f64 / total_trades as f64;

        let total_win_pnl: f64 = trades.iter().filter(|t| t.pnl > 0.0).map(|t| t.pnl).sum();
        let total_loss_pnl: f64 = trades.iter().filter(|t| t.pnl < 0.0).map(|t| t.pnl).sum();

        let avg_win = if winning_trades > 0 {
            total_win_pnl / winning_trades as f64
        } else {
            0.0
        };
        let avg_loss = if losing_trades > 0 {
            total_loss_pnl / losing_trades as f64
        } else {
            0.0
        };

        let profit_factor = if total_loss_pnl.abs() > f64::EPSILON {
            total_win_pnl / total_loss_pnl.abs()
        } else if total_win_pnl > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };

        // Max drawdown (peak-to-trough in equity curve)
        let mut max_dd = 0.0;
        let mut peak = 0.0;
        let mut equity = 0.0;
        for trade in trades {
            equity += trade.pnl;
            if equity > peak {
                peak = equity;
            }
            let dd = peak - equity;
            if dd > max_dd {
                max_dd = dd;
            }
        }

        // Max drawdown duration (time between peak and recovery)
        let (max_dd_duration_ns, _) = compute_max_drawdown_duration(trades);

        // Sharpe ratio (per-trade basis, annualized)
        let sharpe_ratio = compute_sharpe_ratio(trades);

        // Sortino ratio (downside deviation only)
        let sortino_ratio = compute_sortino_ratio(trades);

        // Average metrics
        let avg_slippage =
            trades.iter().map(|t| t.slippage.abs()).sum::<f64>() / total_trades as f64;
        let avg_fill_probability =
            trades.iter().map(|t| t.fill_probability).sum::<f64>() / total_trades as f64;
        let avg_latency_ms = trades.iter().map(|t| t.latency_ms).sum::<f64>() / total_trades as f64;

        // Consecutive wins/losses
        let (max_consec_wins, max_consec_losses) = compute_consecutive_streaks(trades);

        // Average trade duration (time between trades)
        let avg_trade_duration_ns = if trades.len() > 1 {
            let mut durations = Vec::with_capacity(trades.len() - 1);
            for i in 1..trades.len() {
                durations.push(trades[i].timestamp_ns - trades[i - 1].timestamp_ns);
            }
            durations.iter().sum::<u64>() / durations.len() as u64
        } else {
            0
        };

        Self {
            total_pnl,
            realized_pnl,
            total_trades,
            winning_trades,
            losing_trades,
            win_rate,
            avg_win,
            avg_loss,
            profit_factor,
            max_drawdown: max_dd,
            max_drawdown_duration_ns: max_dd_duration_ns,
            sharpe_ratio,
            sortino_ratio,
            avg_slippage,
            avg_fill_probability,
            avg_latency_ms,
            max_consecutive_wins: max_consec_wins,
            max_consecutive_losses: max_consec_losses,
            avg_trade_duration_ns,
        }
    }
}

// ---------------------------------------------------------------------------
// Computation helpers
// ---------------------------------------------------------------------------

/// Sharpe ratio computed on per-trade returns.
/// Annualized assuming ~252 trading days with the average trade frequency.
fn compute_sharpe_ratio(trades: &[TradeRecord]) -> f64 {
    if trades.len() < 2 {
        return 0.0;
    }

    let pnls: Vec<f64> = trades.iter().map(|t| t.pnl).collect();
    let n = pnls.len() as f64;
    let mean = pnls.iter().sum::<f64>() / n;

    let variance = pnls.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std = variance.sqrt();

    if std < f64::EPSILON {
        return 0.0;
    }

    // Annualization: assume trades occur ~every 100ms on average during
    // 24h forex market → ~864,000 trades/year
    let trades_per_year = 864_000.0_f64;
    let annualization_factor = trades_per_year.sqrt();

    (mean / std) * annualization_factor
}

/// Sortino ratio using downside deviation only.
fn compute_sortino_ratio(trades: &[TradeRecord]) -> f64 {
    if trades.len() < 2 {
        return 0.0;
    }

    let pnls: Vec<f64> = trades.iter().map(|t| t.pnl).collect();
    let n = pnls.len() as f64;
    let mean = pnls.iter().sum::<f64>() / n;

    // Downside deviation: only consider returns below mean
    let downside_var: f64 = pnls
        .iter()
        .filter(|&&x| x < mean)
        .map(|x| (x - mean).powi(2))
        .sum::<f64>()
        / n;
    let downside_std = downside_var.sqrt();

    if downside_std < f64::EPSILON {
        return 0.0;
    }

    let trades_per_year = 864_000.0_f64;
    let annualization_factor = trades_per_year.sqrt();

    (mean / downside_std) * annualization_factor
}

/// Compute max drawdown duration in nanoseconds.
/// Returns (max_duration_ns, max_dd_value).
fn compute_max_drawdown_duration(trades: &[TradeRecord]) -> (u64, f64) {
    if trades.len() < 2 {
        return (0, 0.0);
    }

    let mut peak_ns = trades[0].timestamp_ns;
    let mut peak_equity = 0.0_f64;
    let mut equity = 0.0_f64;
    let mut max_dd_duration_ns = 0u64;
    let mut max_dd = 0.0_f64;
    let mut in_drawdown = false;
    let mut dd_start_ns = 0u64;

    for trade in trades {
        equity += trade.pnl;
        let ts = trade.timestamp_ns;

        if equity >= peak_equity {
            // New peak
            if in_drawdown {
                let dd_duration = ts - dd_start_ns;
                if dd_duration > max_dd_duration_ns {
                    max_dd_duration_ns = dd_duration;
                }
                in_drawdown = false;
            }
            peak_equity = equity;
            peak_ns = ts;
        } else {
            let dd = peak_equity - equity;
            if dd > max_dd {
                max_dd = dd;
            }
            if !in_drawdown {
                in_drawdown = true;
                dd_start_ns = peak_ns;
            }
        }
    }

    // If still in drawdown at end, measure to last trade
    if in_drawdown && !trades.is_empty() {
        let dd_duration = trades.last().unwrap().timestamp_ns - dd_start_ns;
        if dd_duration > max_dd_duration_ns {
            max_dd_duration_ns = dd_duration;
        }
    }

    (max_dd_duration_ns, max_dd)
}

/// Compute max consecutive wins and losses.
fn compute_consecutive_streaks(trades: &[TradeRecord]) -> (u64, u64) {
    let mut max_wins = 0u64;
    let mut max_losses = 0u64;
    let mut current_wins = 0u64;
    let mut current_losses = 0u64;

    for trade in trades {
        if trade.pnl > 0.0 {
            current_wins += 1;
            current_losses = 0;
            if current_wins > max_wins {
                max_wins = current_wins;
            }
        } else if trade.pnl < 0.0 {
            current_losses += 1;
            current_wins = 0;
            if current_losses > max_losses {
                max_losses = current_losses;
            }
        } else {
            // Break-even: reset both
            current_wins = 0;
            current_losses = 0;
        }
    }

    (max_wins, max_losses)
}

// ---------------------------------------------------------------------------
// Equity Curve
// ---------------------------------------------------------------------------

/// A point on the equity curve.
#[derive(Debug, Clone)]
pub struct EquityPoint {
    pub timestamp_ns: u64,
    pub equity: f64,
    pub drawdown: f64,
}

/// Compute the equity curve from trade records.
pub fn compute_equity_curve(trades: &[TradeRecord]) -> Vec<EquityPoint> {
    let mut curve = Vec::with_capacity(trades.len());
    let mut equity = 0.0;
    let mut peak = 0.0;

    for trade in trades {
        equity += trade.pnl;
        if equity > peak {
            peak = equity;
        }
        let dd = peak - equity;
        curve.push(EquityPoint {
            timestamp_ns: trade.timestamp_ns,
            equity,
            drawdown: dd,
        });
    }

    curve
}

// ---------------------------------------------------------------------------
// Per-Strategy Breakdown
// ---------------------------------------------------------------------------

/// Performance breakdown per strategy.
#[derive(Debug, Clone)]
pub struct StrategyBreakdown {
    pub strategy_id: StrategyId,
    pub total_trades: u64,
    pub total_pnl: f64,
    pub win_rate: f64,
    pub avg_pnl: f64,
}

/// Compute per-strategy performance breakdown.
pub fn compute_strategy_breakdown(trades: &[TradeRecord]) -> Vec<StrategyBreakdown> {
    let mut by_strategy: std::collections::HashMap<StrategyId, Vec<&TradeRecord>> =
        std::collections::HashMap::new();

    for trade in trades {
        by_strategy
            .entry(trade.strategy_id)
            .or_default()
            .push(trade);
    }

    let mut breakdowns: Vec<StrategyBreakdown> = by_strategy
        .into_iter()
        .map(|(sid, strat_trades)| {
            let n = strat_trades.len() as u64;
            let total_pnl: f64 = strat_trades.iter().map(|t| t.pnl).sum();
            let wins = strat_trades.iter().filter(|t| t.pnl > 0.0).count() as u64;

            StrategyBreakdown {
                strategy_id: sid,
                total_trades: n,
                total_pnl,
                win_rate: if n > 0 { wins as f64 / n as f64 } else { 0.0 },
                avg_pnl: if n > 0 { total_pnl / n as f64 } else { 0.0 },
            }
        })
        .collect();

    breakdowns.sort_by_key(|b| b.strategy_id as u8);
    breakdowns
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trade(timestamp_ns: u64, pnl: f64, strategy_id: StrategyId) -> TradeRecord {
        TradeRecord {
            timestamp_ns,
            strategy_id,
            direction: if pnl >= 0.0 {
                Direction::Buy
            } else {
                Direction::Sell
            },
            lots: 1000.0,
            fill_price: 110.0,
            slippage: 0.0001,
            pnl,
            fill_probability: 0.95,
            latency_ms: 1.0,
            close_reason: None,
        }
    }

    fn make_trade_with_slippage(timestamp_ns: u64, pnl: f64, slippage: f64) -> TradeRecord {
        TradeRecord {
            timestamp_ns,
            strategy_id: StrategyId::A,
            direction: Direction::Buy,
            lots: 1000.0,
            fill_price: 110.0,
            slippage,
            pnl,
            fill_probability: 0.9,
            latency_ms: 1.5,
            close_reason: None,
        }
    }

    // -- TradeSummary::empty --
    #[test]
    fn test_empty_summary() {
        let summary = TradeSummary::empty();
        assert_eq!(summary.total_trades, 0);
        assert_eq!(summary.total_pnl, 0.0);
        assert_eq!(summary.sharpe_ratio, 0.0);
        assert_eq!(summary.max_drawdown, 0.0);
        assert_eq!(summary.win_rate, 0.0);
    }

    #[test]
    fn test_from_trades_empty() {
        let summary = TradeSummary::from_trades(&[]);
        assert_eq!(summary.total_trades, 0);
    }

    // -- Basic statistics --
    #[test]
    fn test_single_trade() {
        let trades = vec![make_trade(1000, 5.0, StrategyId::A)];
        let summary = TradeSummary::from_trades(&trades);

        assert_eq!(summary.total_trades, 1);
        assert!((summary.total_pnl - 5.0).abs() < 1e-10);
        assert_eq!(summary.winning_trades, 1);
        assert_eq!(summary.losing_trades, 0);
        assert!((summary.win_rate - 1.0).abs() < 1e-10);
        assert!((summary.avg_win - 5.0).abs() < 1e-10);
        assert_eq!(summary.avg_loss, 0.0);
    }

    #[test]
    fn test_mixed_trades() {
        let trades = vec![
            make_trade(1000, 10.0, StrategyId::A),
            make_trade(2000, -3.0, StrategyId::A),
            make_trade(3000, 5.0, StrategyId::A),
            make_trade(4000, -2.0, StrategyId::A),
            make_trade(5000, 8.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);

        assert_eq!(summary.total_trades, 5);
        assert!((summary.total_pnl - 18.0).abs() < 1e-10);
        assert_eq!(summary.winning_trades, 3);
        assert_eq!(summary.losing_trades, 2);
        assert!((summary.win_rate - 0.6).abs() < 1e-10);
        assert!((summary.avg_win - (10.0 + 5.0 + 8.0) / 3.0).abs() < 1e-10);
        assert!((summary.avg_loss - (-5.0 / 2.0)).abs() < 1e-10);
    }

    #[test]
    fn test_profit_factor() {
        let trades = vec![
            make_trade(1000, 10.0, StrategyId::A),
            make_trade(2000, 5.0, StrategyId::A),
            make_trade(3000, -3.0, StrategyId::A),
            make_trade(4000, -2.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        // PF = 15 / 5 = 3.0
        assert!((summary.profit_factor - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_profit_factor_no_losses() {
        let trades = vec![
            make_trade(1000, 5.0, StrategyId::A),
            make_trade(2000, 3.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!(summary.profit_factor.is_infinite());
    }

    #[test]
    fn test_profit_factor_no_wins() {
        let trades = vec![
            make_trade(1000, -5.0, StrategyId::A),
            make_trade(2000, -3.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert_eq!(summary.profit_factor, 0.0);
    }

    // -- Max drawdown --
    #[test]
    fn test_max_drawdown_no_drawdown() {
        let trades = vec![
            make_trade(1000, 5.0, StrategyId::A),
            make_trade(2000, 5.0, StrategyId::A),
            make_trade(3000, 5.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!((summary.max_drawdown - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_max_drawdown_with_recovery() {
        let trades = vec![
            make_trade(1000, 10.0, StrategyId::A),
            make_trade(2000, -5.0, StrategyId::A), // dd = 5
            make_trade(3000, -3.0, StrategyId::A), // dd = 8
            make_trade(4000, 6.0, StrategyId::A),  // recovery
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!((summary.max_drawdown - 8.0).abs() < 1e-10);
    }

    #[test]
    fn test_max_drawdown_deep() {
        let trades = vec![
            make_trade(1000, 20.0, StrategyId::A),
            make_trade(2000, -15.0, StrategyId::A), // dd = 15
            make_trade(3000, -10.0, StrategyId::A), // dd = 25
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!((summary.max_drawdown - 25.0).abs() < 1e-10);
    }

    // -- Sharpe ratio --
    #[test]
    fn test_sharpe_ratio_positive() {
        let trades = vec![
            make_trade(1000, 1.0, StrategyId::A),
            make_trade(2000, 2.0, StrategyId::A),
            make_trade(3000, 1.5, StrategyId::A),
            make_trade(4000, 2.5, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        // All positive → positive Sharpe
        assert!(summary.sharpe_ratio > 0.0);
    }

    #[test]
    fn test_sharpe_ratio_single_trade() {
        let trades = vec![make_trade(1000, 5.0, StrategyId::A)];
        let summary = TradeSummary::from_trades(&trades);
        // Need at least 2 trades for Sharpe
        assert_eq!(summary.sharpe_ratio, 0.0);
    }

    #[test]
    fn test_sharpe_ratio_constant_returns() {
        // All same returns → std = 0 → Sharpe = 0
        let trades = vec![
            make_trade(1000, 1.0, StrategyId::A),
            make_trade(2000, 1.0, StrategyId::A),
            make_trade(3000, 1.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert_eq!(summary.sharpe_ratio, 0.0);
    }

    // -- Sortino ratio --
    #[test]
    fn test_sortino_ratio_positive() {
        let trades = vec![
            make_trade(1000, 2.0, StrategyId::A),
            make_trade(2000, 3.0, StrategyId::A),
            make_trade(3000, -0.5, StrategyId::A),
            make_trade(4000, 4.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!(summary.sortino_ratio > 0.0);
    }

    #[test]
    fn test_sortino_single_trade() {
        let trades = vec![make_trade(1000, 5.0, StrategyId::A)];
        let summary = TradeSummary::from_trades(&trades);
        assert_eq!(summary.sortino_ratio, 0.0);
    }

    // -- Average metrics --
    #[test]
    fn test_avg_slippage() {
        let trades = vec![
            make_trade_with_slippage(1000, 1.0, 0.0001),
            make_trade_with_slippage(2000, -1.0, 0.0003),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!((summary.avg_slippage - 0.0002).abs() < 1e-10);
    }

    #[test]
    fn test_avg_fill_probability() {
        let trades = vec![
            TradeRecord {
                timestamp_ns: 1000,
                strategy_id: StrategyId::A,
                direction: Direction::Buy,
                lots: 1000.0,
                fill_price: 110.0,
                slippage: 0.0001,
                pnl: 1.0,
                fill_probability: 0.8,
                latency_ms: 1.0,
                close_reason: None,
            },
            TradeRecord {
                timestamp_ns: 2000,
                strategy_id: StrategyId::A,
                direction: Direction::Buy,
                lots: 1000.0,
                fill_price: 110.0,
                slippage: 0.0001,
                pnl: -1.0,
                fill_probability: 0.9,
                latency_ms: 2.0,
                close_reason: None,
            },
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert!((summary.avg_fill_probability - 0.85).abs() < 1e-10);
        assert!((summary.avg_latency_ms - 1.5).abs() < 1e-10);
    }

    // -- Consecutive streaks --
    #[test]
    fn test_consecutive_streaks() {
        let trades = vec![
            make_trade(1000, 1.0, StrategyId::A),
            make_trade(2000, 1.0, StrategyId::A),
            make_trade(3000, -1.0, StrategyId::A),
            make_trade(4000, -1.0, StrategyId::A),
            make_trade(5000, -1.0, StrategyId::A),
            make_trade(6000, 1.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        assert_eq!(summary.max_consecutive_wins, 2);
        assert_eq!(summary.max_consecutive_losses, 3);
    }

    #[test]
    fn test_consecutive_all_wins() {
        let trades: Vec<TradeRecord> = (0..5)
            .map(|i| make_trade((i + 1) as u64 * 1000, 1.0, StrategyId::A))
            .collect();
        let summary = TradeSummary::from_trades(&trades);
        assert_eq!(summary.max_consecutive_wins, 5);
        assert_eq!(summary.max_consecutive_losses, 0);
    }

    // -- Average trade duration --
    #[test]
    fn test_avg_trade_duration() {
        let trades = vec![
            make_trade(1000, 1.0, StrategyId::A),
            make_trade(3000, 1.0, StrategyId::A),
            make_trade(5000, 1.0, StrategyId::A),
        ];
        let summary = TradeSummary::from_trades(&trades);
        // Durations: 2000, 2000 → avg = 2000
        assert_eq!(summary.avg_trade_duration_ns, 2000);
    }

    #[test]
    fn test_avg_trade_duration_single() {
        let trades = vec![make_trade(1000, 1.0, StrategyId::A)];
        let summary = TradeSummary::from_trades(&trades);
        assert_eq!(summary.avg_trade_duration_ns, 0);
    }

    // -- Equity curve --
    #[test]
    fn test_equity_curve() {
        let trades = vec![
            make_trade(1000, 10.0, StrategyId::A),
            make_trade(2000, -3.0, StrategyId::A),
            make_trade(3000, 5.0, StrategyId::A),
        ];
        let curve = compute_equity_curve(&trades);

        assert_eq!(curve.len(), 3);
        assert!((curve[0].equity - 10.0).abs() < 1e-10);
        assert!((curve[0].drawdown - 0.0).abs() < 1e-10);
        assert!((curve[1].equity - 7.0).abs() < 1e-10);
        assert!((curve[1].drawdown - 3.0).abs() < 1e-10);
        assert!((curve[2].equity - 12.0).abs() < 1e-10);
        assert!((curve[2].drawdown - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_equity_curve_empty() {
        let curve = compute_equity_curve(&[]);
        assert!(curve.is_empty());
    }

    // -- Max drawdown duration --
    #[test]
    fn test_max_drawdown_duration() {
        let trades = vec![
            make_trade(1000, 10.0, StrategyId::A),
            make_trade(2000, -3.0, StrategyId::A),
            make_trade(3000, -2.0, StrategyId::A),
            make_trade(4000, 5.0, StrategyId::A),
        ];
        let (duration, dd) = compute_max_drawdown_duration(&trades);
        assert_eq!(duration, 3000); // from tick 1000 (peak) to tick 4000 (recovery)
        assert!((dd - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_max_drawdown_duration_no_recovery() {
        let trades = vec![
            make_trade(1000, 10.0, StrategyId::A),
            make_trade(2000, -3.0, StrategyId::A),
            make_trade(3000, -2.0, StrategyId::A),
        ];
        let (duration, _) = compute_max_drawdown_duration(&trades);
        assert_eq!(duration, 2000); // from peak to last trade
    }

    // -- Per-strategy breakdown --
    #[test]
    fn test_strategy_breakdown() {
        let trades = vec![
            make_trade(1000, 5.0, StrategyId::A),
            make_trade(2000, -2.0, StrategyId::A),
            make_trade(3000, 3.0, StrategyId::B),
            make_trade(4000, 1.0, StrategyId::B),
            make_trade(5000, -1.0, StrategyId::C),
        ];
        let breakdowns = compute_strategy_breakdown(&trades);

        assert_eq!(breakdowns.len(), 3);

        let a = &breakdowns[0];
        assert_eq!(a.strategy_id, StrategyId::A);
        assert_eq!(a.total_trades, 2);
        assert!((a.total_pnl - 3.0).abs() < 1e-10);
        assert!((a.win_rate - 0.5).abs() < 1e-10);

        let b = &breakdowns[1];
        assert_eq!(b.strategy_id, StrategyId::B);
        assert_eq!(b.total_trades, 2);
        assert!((b.total_pnl - 4.0).abs() < 1e-10);
        assert!((b.win_rate - 1.0).abs() < 1e-10);

        let c = &breakdowns[2];
        assert_eq!(c.strategy_id, StrategyId::C);
        assert_eq!(c.total_trades, 1);
        assert!((c.total_pnl - (-1.0)).abs() < 1e-10);
        assert!((c.win_rate - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_strategy_breakdown_empty() {
        let breakdowns = compute_strategy_breakdown(&[]);
        assert!(breakdowns.is_empty());
    }

    // -- Consecutive streaks helper --
    #[test]
    fn test_consecutive_streaks_helper() {
        let trades = vec![
            make_trade(1000, 1.0, StrategyId::A),
            make_trade(2000, -1.0, StrategyId::A),
            make_trade(3000, 0.0, StrategyId::A), // break-even resets
            make_trade(4000, 1.0, StrategyId::A),
            make_trade(5000, 1.0, StrategyId::A),
        ];
        let (wins, losses) = compute_consecutive_streaks(&trades);
        assert_eq!(wins, 2);
        assert_eq!(losses, 1);
    }
}
