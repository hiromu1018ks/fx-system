"""Run the validation pipeline on backtest result data."""

from __future__ import annotations

import sys
from typing import Any

import numpy as np

from research.analysis.pipeline import PipelineConfig, run_validation_pipeline

from .loader import extract_num_features, extract_returns


def run_bridge_validation(
    data: dict[str, Any],
    config: PipelineConfig | None = None,
) -> dict[str, Any]:
    """Run the full validation pipeline on loaded backtest result data.

    Args:
        data: Parsed backtest result JSON dict.
        config: Pipeline configuration. Uses defaults if None.

    Returns:
        Dict with 'all_passed', 'n_passed', 'n_failed', 'checks'.
    """
    returns = extract_returns(data)
    num_features = extract_num_features(data)

    if len(returns) < 2:
        return _insufficient_data_result(len(returns))

    result = run_validation_pipeline(
        returns=returns,
        num_features=num_features,
        config=config,
    )

    checks = [
        {
            "name": c.name,
            "passed": c.passed,
            "details": c.details,
            "value": float(c.value) if _is_finite(c.value) else 0.0,
        }
        for c in result.checks
    ]

    return {
        "all_passed": result.all_passed,
        "n_passed": result.n_passed,
        "n_failed": result.n_failed,
        "checks": checks,
    }


def _insufficient_data_result(n_returns: int) -> dict[str, Any]:
    return {
        "all_passed": False,
        "n_passed": 0,
        "n_failed": 1,
        "checks": [
            {
                "name": "Data Sufficiency",
                "passed": False,
                "details": f"Insufficient data: {n_returns} returns (need >= 2)",
                "value": float(n_returns),
            }
        ],
    }


def _is_finite(v: float) -> bool:
    try:
        return np.isfinite(v)
    except (TypeError, ValueError):
        return False
