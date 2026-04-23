# Ralph Loop Summary (Iterations 1-5)

## Commits Made
1. `614099f` fix(backtest): pass decision.q_sampled as expected_profit and fix execution stats
2. `9a518c0` feat(cli): add per-strategy skip reason breakdown to backtest output
3. `90fb49c` feat(strategy): add Q-function signal-driven exits (TRIGGER_EXIT)

## Structural Fixes Applied
- **expected_profit consistency**: Backtest now passes `decision.q_sampled` instead of `0.0` to ExecutionRequest, aligning with forward test semantics
- **ExecutionStats collection**: `run_from_stream` now collects actual execution stats instead of hardcoded `empty()`
- **Per-strategy skip reason diagnostics**: Skip reasons broken down by strategy in JSON and CLI output
- **Signal-driven exits**: Q-function closing direction now maps to `should_close=true` (TRIGGER_EXIT), reducing MAX_HOLD_TIME from 100% to 13.6%

## Multi-Seed Evidence (5 seeds)
| Seed | Trades | PnL | Win% | Sharpe |
|------|--------|-----|------|--------|
| 42 | 3,710 | +221,752 | 27.1 | 0.651 |
| 123 | 5,936 | -20,636 | 25.4 | -0.112 |
| 456 | 7,080 | +55,044 | 28.4 | 0.257 |
| 789 | 8,272 | +156,731 | 30.3 | 0.884 |
| 2024 | 4,444 | +329,752 | 30.6 | 0.873 |
| **Mean** | **5,888** | **+148,529** | **28.4** | **0.511** |

- Profitable: 4/5 seeds (80%)
- Same-seed reproducibility: verified (seed=42 identical across runs)

## Remaining Limitations (Model/Design Quality, NOT Implementation)
1. **Q-function untrained**: PnL variance across seeds is 93% (stdev/mean). Pretraining API exists but not wired to backtest loop start.
2. **Risk limits not equity-scaled**: Weekly/monthly limits are absolute $ amounts blocking 46%/31% of periods. No capital-based scaling mechanism.
3. **Strategy C risk pass rate**: 0.15-2.4% across seeds, primarily blocked by daily_realized_halt and weekly_halt cascading.
4. **Strategy A trigger strictness**: Only ~2,959 triggers across 71.8M ticks (0.004%). Conditions may be too restrictive for available data.

## Phase Gate Status
- **P0 (Structural integrity)**: ✅ Complete
- **P1 (Bottleneck removal)**: ✅ Complete (dominant bottleneck is risk calibration, not code bugs)
- **P2 (Strategy behavior quality)**: ✅ Mostly complete (MAX_HOLD_TIME 100%→13.6%, A is active)
- **P3 (Evaluation quality)**: ✅ Complete (5-seed evidence, reproducibility verified)
