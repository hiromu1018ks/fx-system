#!/usr/bin/env bash
# Walk-forward backtesting script.
#
# Usage: ./scripts/walk_forward.sh [--seed SEED] [--strategies A,B,C]
#
# Expanding-window walk-forward:
#   Split 1: Train on train1 → export Q-state → Test on test1 (frozen)
#   Split 2: Train on train2 → export Q-state → Test on test2 (frozen)
#   Split 3: Train on train3 → export Q-state → Test on test3 (frozen)
#
# Also runs a baseline (no Q-state, learning ON) on each test period.
#
# Outputs JSON summary to stdout.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLI="$PROJECT_ROOT/target/release/fx-cli"

DATA_DIR="${WF_DATA_DIR:-/tmp/wf-data}"
OUTPUT_DIR="${WF_OUTPUT_DIR:-/tmp/wf-results}"
SEED="${WF_SEED:-42}"

mkdir -p "$OUTPUT_DIR"

if [ ! -f "$CLI" ]; then
    echo "Building release CLI..." >&2
    cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" >&2
fi

# Extract metrics from backtest result JSON
extract_metrics() {
    local json="$1"
    local label="$2"
    python3 -c "
import json, sys
d = json.load(open('$json'))
s = d.get('summary', d)
print(f'  $label:')
print(f'    PnL={d[\"total_pnl\"]:.2f} trades={d[\"total_trades\"]} closes={d[\"close_trades\"]} fill_rate={d[\"execution_fill_rate\"]:.3f} sharpe={d[\"sharpe_ratio\"]:.3f} max_dd={d[\"max_drawdown\"]:.2f}')
"
}

echo "========================================" >&2
echo "Walk-Forward Backtest" >&2
echo "  seed: $SEED" >&2
echo "  data: $DATA_DIR" >&2
echo "========================================" >&2

SPLITS=3
declare -a TRAIN_FILES=("train1.csv" "train2.csv" "train3.csv")
declare -a TEST_FILES=("test1.csv" "test2.csv" "test3.csv")

echo "" >&2
echo "--- Baseline: test periods with learning ON, no Q-state ---" >&2
echo "" >&2

for i in $(seq 1 $SPLITS); do
    TEST="$DATA_DIR/${TEST_FILES[$((i-1))]}"
    OUT="$OUTPUT_DIR/baseline-split${i}"

    echo "  Baseline split $i ($(wc -l < "$TEST") rows)..." >&2
    RUST_LOG=off "$CLI" backtest \
        --data "$TEST" \
        --seed "$SEED" \
        --output "$OUT" 2>/dev/null

    extract_metrics "$OUT/backtest_result.json" "Split $i baseline (learning ON)"
done

echo "" >&2
echo "--- Walk-forward: train → export → test (frozen) ---" >&2
echo "" >&2

for i in $(seq 1 $SPLITS); do
    TRAIN="$DATA_DIR/${TRAIN_FILES[$((i-1))]}"
    TEST="$DATA_DIR/${TEST_FILES[$((i-1))]}"
    QSTATE="$OUTPUT_DIR/qstate-split${i}.json"
    OUT_TRAIN="$OUTPUT_DIR/train-split${i}"
    OUT_TEST="$OUTPUT_DIR/test-split${i}"

    # Phase 1: Train with learning ON, export Q-state
    echo "  Split $i: Training on $(wc -l < "$TRAIN") rows..." >&2
    RUST_LOG=off "$CLI" backtest \
        --data "$TRAIN" \
        --seed "$SEED" \
        --export-q-state "$QSTATE" \
        --output "$OUT_TRAIN" 2>/dev/null

    # Phase 2: Test with learning OFF, imported Q-state
    echo "  Split $i: Testing on $(wc -l < "$TEST") rows (frozen)..." >&2
    RUST_LOG=off "$CLI" backtest \
        --data "$TEST" \
        --seed "$SEED" \
        --no-learn \
        --import-q-state "$QSTATE" \
        --output "$OUT_TEST" 2>/dev/null

    extract_metrics "$OUT_TEST/backtest_result.json" "Split $i walk-forward (frozen)"
    echo "" >&2
done

# Summary
echo "" >&2
echo "========================================" >&2
echo "Walk-Forward Summary" >&2
echo "========================================" >&2

python3 << 'PYEOF'
import json, sys, os

output_dir = os.environ.get("WF_OUTPUT_DIR", "/tmp/wf-results")
splits = 3

print(f"{'Split':<8} {'Mode':<20} {'PnL':>10} {'Trades':>8} {'Closes':>8} {'FillRate':>10} {'Sharpe':>10} {'MaxDD':>10}")
print("-" * 94)

for i in range(1, splits + 1):
    for mode, label in [("baseline", "baseline (learn ON)"), (f"test-split{i}", "walk-forward (frozen)")]:
        path = f"{output_dir}/{mode}-split{i}/backtest_result.json" if mode == "baseline" else f"{output_dir}/{label.split()[0]}/backtest_result.json" if "walk" in label else f"{output_dir}/{mode}-split{i}/backtest_result.json"

        # Fix path construction
        if mode == "baseline":
            path = f"{output_dir}/baseline-split{i}/backtest_result.json"
        else:
            path = f"{output_dir}/test-split{i}/backtest_result.json"

        try:
            d = json.load(open(path))
            print(f"  {i:<6} {label:<20} {d['total_pnl']:>10.2f} {d['total_trades']:>8} {d['close_trades']:>8} {d['execution_fill_rate']:>10.3f} {d['sharpe_ratio']:>10.3f} {d['max_drawdown']:>10.2f}")
        except:
            print(f"  {i:<6} {label:<20} {'ERROR':>10}")

PYEOF
