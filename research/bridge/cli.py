"""CLI entry point for the Rust-Python validation bridge.

Usage:
    python -m research.bridge.cli --input <backtest_result.json> --output <validation_result.json> [--num-features N]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

# Ensure project root is on sys.path for `research.*` imports
_project_root = Path(__file__).resolve().parents[2]
if str(_project_root) not in sys.path:
    sys.path.insert(0, str(_project_root))

from research.analysis.pipeline import PipelineConfig
from research.bridge.loader import load_backtest_result
from research.bridge.output import write_validation_result
from research.bridge.runner import run_bridge_validation


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Validate a backtest result using the statistical pipeline."
    )
    parser.add_argument(
        "--input", required=True, help="Path to backtest result JSON from Rust."
    )
    parser.add_argument(
        "--output", required=True, help="Path to write validation result JSON."
    )
    parser.add_argument(
        "--num-features",
        type=int,
        default=None,
        help="Number of features (overrides JSON value).",
    )
    args = parser.parse_args()

    data = load_backtest_result(args.input)

    if args.num_features is not None:
        data["num_features"] = args.num_features

    config = PipelineConfig()
    result = run_bridge_validation(data, config)
    output_path = write_validation_result(result, args.output)

    status = "PASSED" if result["all_passed"] else "FAILED"
    print(f"Validation {status}: {result['n_passed']}/{result['n_passed'] + result['n_failed']} checks passed")
    print(f"Results written to {output_path}")


if __name__ == "__main__":
    main()
