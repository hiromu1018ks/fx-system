"""Information leakage verification.

Compares strategy performance with and without enforced lags on execution-related
data. If performance is significantly better without lags, information leakage
is present and the strategy must be rejected.
"""

from __future__ import annotations

import numpy as np
from typing import Callable


class LeakageTestResult:
    """Result of information leakage verification."""

    __slots__ = (
        "performance_with_lag",
        "performance_without_lag",
        "leakage_ratio",
        "leakage_detected",
        "threshold",
    )

    def __init__(
        self,
        performance_with_lag: float,
        performance_without_lag: float,
        leakage_ratio: float,
        leakage_detected: bool,
        threshold: float = 0.2,
    ) -> None:
        self.performance_with_lag = performance_with_lag
        self.performance_without_lag = performance_without_lag
        self.leakage_ratio = leakage_ratio
        self.leakage_detected = leakage_detected
        self.threshold = threshold


def verify_information_leakage(
    returns_with_lag: np.ndarray,
    returns_without_lag: np.ndarray,
    metric_fn: Callable[[np.ndarray], float] | None = None,
    threshold: float = 0.2,
) -> LeakageTestResult:
    """Verify that enforced lags do not significantly degrade performance.

    Computes a performance metric on both lagged and unlagged return series.
    If the unlagged version is significantly better (exceeds threshold),
    information leakage is likely present.

    Args:
        returns_with_lag: 1D array of strategy returns with execution data lags enforced.
        returns_without_lag: 1D array of strategy returns without lag enforcement.
        metric_fn: Function to compute performance metric. Defaults to Sharpe ratio.
        threshold: Relative improvement threshold to flag leakage (default 20%).

    Returns:
        LeakageTestResult with diagnostics.
    """
    if metric_fn is None:
        metric_fn = _default_metric

    perf_lagged = metric_fn(returns_with_lag)
    perf_unlagged = metric_fn(returns_without_lag)

    if abs(perf_lagged) < 1e-12:
        leakage_ratio = 0.0 if abs(perf_unlagged) < 1e-12 else float("inf")
    else:
        leakage_ratio = (perf_unlagged - perf_lagged) / abs(perf_lagged)

    leakage_detected = leakage_ratio > threshold

    return LeakageTestResult(
        performance_with_lag=perf_lagged,
        performance_without_lag=perf_unlagged,
        leakage_ratio=leakage_ratio,
        leakage_detected=leakage_detected,
        threshold=threshold,
    )


def _default_metric(returns: np.ndarray) -> float:
    """Default metric: Sharpe-like ratio."""
    if len(returns) < 2:
        return 0.0
    std = np.std(returns, ddof=1)
    if std < 1e-12:
        return 0.0
    return float(np.mean(returns) / std)
