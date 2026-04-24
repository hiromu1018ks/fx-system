#!/usr/bin/env bash
# Walk-forward backtesting script.
#
# Usage: ./scripts/walk_forward.sh [--seed SEED] [--strategies A,B,C]
#
# Expanding-window walk-forward:
#   Split 1: Train on train1 → export Q-state → Test on test1
#   Split 2: Train on train2 → export Q-state → Test on test2
#   Split 3: Train on train3 → export Q-state → Test on test3
#
# For each test split, it runs three modes:
#   1. fresh-learning: no imported Q-state, learning ON
#   2. warm-continuing: imported Q-state, learning ON
#   3. warm-frozen: imported Q-state, learning OFF
#
# Outputs a human-readable comparison summary to stdout.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLI="$PROJECT_ROOT/target/release/fx-cli"

DATA_DIR="${WF_DATA_DIR:-/tmp/wf-data}"
OUTPUT_DIR="${WF_OUTPUT_DIR:-/tmp/wf-results}"
SEED="${WF_SEED:-42}"
STRATEGIES="${WF_STRATEGIES:-}"

while [ $# -gt 0 ]; do
    case "$1" in
        --seed)
            SEED="$2"
            shift 2
            ;;
        --strategies)
            STRATEGIES="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

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

STRATEGY_ARGS=()
if [ -n "$STRATEGIES" ]; then
    STRATEGY_ARGS+=(--strategies "$STRATEGIES")
fi

run_backtest() {
    local out_dir="$1"
    shift
    RUST_LOG=off "$CLI" backtest \
        "$@" \
        --seed "$SEED" \
        "${STRATEGY_ARGS[@]}" \
        --output "$out_dir" 2>/dev/null
}

echo "========================================" >&2
echo "Walk-Forward Backtest" >&2
echo "  seed: $SEED" >&2
echo "  data: $DATA_DIR" >&2
if [ -n "$STRATEGIES" ]; then
    echo "  strategies: $STRATEGIES" >&2
fi
echo "========================================" >&2

SPLITS=3
declare -a TRAIN_FILES=("train1.csv" "train2.csv" "train3.csv")
declare -a TEST_FILES=("test1.csv" "test2.csv" "test3.csv")

for i in $(seq 1 $SPLITS); do
    TRAIN="$DATA_DIR/${TRAIN_FILES[$((i-1))]}"
    TEST="$DATA_DIR/${TEST_FILES[$((i-1))]}"
    QSTATE="$OUTPUT_DIR/qstate-split${i}.json"
    OUT_TRAIN="$OUTPUT_DIR/train-split${i}"
    OUT_FRESH="$OUTPUT_DIR/fresh-learning-split${i}"
    OUT_WARM_CONT="$OUTPUT_DIR/warm-continuing-split${i}"
    OUT_WARM_FROZEN="$OUTPUT_DIR/warm-frozen-split${i}"

    echo "" >&2
    echo "--- Split $i ---" >&2
    echo "  Training on $(wc -l < "$TRAIN") rows..." >&2
    run_backtest "$OUT_TRAIN" \
        --data "$TRAIN" \
        --export-q-state "$QSTATE"

    echo "  Fresh learning on test ($(wc -l < "$TEST") rows)..." >&2
    run_backtest "$OUT_FRESH" \
        --data "$TEST"
    extract_metrics "$OUT_FRESH/backtest_result.json" "Split $i fresh-learning"

    echo "  Warm continuing on test (imported Q-state, learning ON)..." >&2
    run_backtest "$OUT_WARM_CONT" \
        --data "$TEST" \
        --import-q-state "$QSTATE"
    extract_metrics "$OUT_WARM_CONT/backtest_result.json" "Split $i warm-continuing"

    echo "  Warm frozen on test (imported Q-state, learning OFF)..." >&2
    run_backtest "$OUT_WARM_FROZEN" \
        --data "$TEST" \
        --import-q-state "$QSTATE" \
        --no-learn
    extract_metrics "$OUT_WARM_FROZEN/backtest_result.json" "Split $i warm-frozen"
done

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

rows = [
    ("fresh-learning", "fresh-learning"),
    ("warm-continuing", "warm-continuing"),
    ("warm-frozen", "warm-frozen"),
]

for i in range(1, splits + 1):
    for mode, label in rows:
        path = f"{output_dir}/{mode}-split{i}/backtest_result.json"
        try:
            d = json.load(open(path))
            print(f"  {i:<6} {label:<20} {d['total_pnl']:>10.2f} {d['total_trades']:>8} {d['close_trades']:>8} {d['execution_fill_rate']:>10.3f} {d['sharpe_ratio']:>10.3f} {d['max_drawdown']:>10.2f}")
        except Exception:
            print(f"  {i:<6} {label:<20} {'ERROR':>10}")

PYEOF
