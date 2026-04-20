"""Load backtest result JSON produced by Rust CLI."""

from __future__ import annotations

import json
import numpy as np
from pathlib import Path
from typing import Any


def load_backtest_result(path: str | Path) -> dict[str, Any]:
    """Load a backtest result JSON file.

    Returns the parsed JSON as a dict. The expected structure has:
      - summary: aggregate statistics
      - trades: list of trade records
      - returns: 1D array of per-trade PnL
      - strategy_breakdown: per-strategy stats
      - num_features: feature count for complexity penalty
      - execution_stats: fill/slippage stats
    """
    path = Path(path)
    if not path.exists():
        raise FileNotFoundError(f"Backtest result not found: {path}")
    with open(path) as f:
        data = json.load(f)
    return data


def extract_returns(data: dict[str, Any]) -> np.ndarray:
    """Extract the returns array from backtest result data.

    Uses the 'returns' field if present (trade-level PnLs).
    Falls back to computing from 'trades[].pnl' if available.
    Returns empty array if no data found.
    """
    if "returns" in data:
        return np.array(data["returns"], dtype=np.float64)

    if "trades" in data:
        pnls = [t["pnl"] for t in data["trades"] if "pnl" in t]
        if pnls:
            return np.array(pnls, dtype=np.float64)

    return np.array([], dtype=np.float64)


def extract_num_features(data: dict[str, Any]) -> int:
    """Extract the number of features for complexity penalty."""
    return data.get("num_features", 45)
