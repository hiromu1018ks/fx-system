"""Sharpe ceiling, complexity penalty, and live degradation tests.

Implements validation checks:
- Sharpe ceiling: Annual Sharpe > 1.5 is forcibly rejected (overfitting signal).
- Complexity penalty: Adjusted Sharpe = Sharpe / sqrt(num_features).
- Live Degradation Test: OOS Sharpe decline must be within 30%.
"""

from __future__ import annotations

import numpy as np


class SharpeCeilingResult:
    """Result of Sharpe ceiling validation."""

    __slots__ = ("annual_sharpe", "ceiling", "passed", "rejection_reason")

    def __init__(self, annual_sharpe: float, ceiling: float = 1.5) -> None:
        self.annual_sharpe = annual_sharpe
        self.ceiling = ceiling
        self.passed = annual_sharpe <= ceiling
        self.rejection_reason = (
            f"Annual Sharpe {annual_sharpe:.4f} exceeds ceiling {ceiling}"
            if not self.passed
            else None
        )


def check_sharpe_ceiling(
    returns: np.ndarray,
    periods_per_year: int = 252,
    ceiling: float = 1.5,
) -> SharpeCeilingResult:
    """Check if annualized Sharpe ratio exceeds the overfitting ceiling.

    Args:
        returns: 1D array of periodic returns.
        periods_per_year: Number of return periods per year for annualization.
        ceiling: Maximum acceptable annualized Sharpe ratio.

    Returns:
        SharpeCeilingResult with pass/fail status.
    """
    annual_sharpe = _compute_annual_sharpe(returns, periods_per_year)
    return SharpeCeilingResult(annual_sharpe, ceiling)


class ComplexityPenaltyResult:
    """Result of complexity penalty adjustment."""

    __slots__ = ("raw_sharpe", "adjusted_sharpe", "num_features", "penalty_factor")

    def __init__(
        self,
        raw_sharpe: float,
        adjusted_sharpe: float,
        num_features: int,
        penalty_factor: float,
    ) -> None:
        self.raw_sharpe = raw_sharpe
        self.adjusted_sharpe = adjusted_sharpe
        self.num_features = num_features
        self.penalty_factor = penalty_factor


def compute_complexity_penalty(
    returns: np.ndarray,
    num_features: int,
    periods_per_year: int = 252,
) -> ComplexityPenaltyResult:
    """Apply complexity penalty to Sharpe ratio.

    Adjusted Sharpe = Annual Sharpe / sqrt(num_features).
    Penalizes strategies that use too many features relative to their
    information content.

    Args:
        returns: 1D array of periodic returns.
        num_features: Number of features used by the strategy.
        periods_per_year: Number of return periods per year.

    Returns:
        ComplexityPenaltyResult with raw and adjusted Sharpe.
    """
    raw_sharpe = _compute_annual_sharpe(returns, periods_per_year)
    penalty_factor = 1.0 / math_sqrt(max(num_features, 1))
    adjusted = raw_sharpe * penalty_factor
    return ComplexityPenaltyResult(raw_sharpe, adjusted, num_features, penalty_factor)


class LiveDegradationResult:
    """Result of live degradation test."""

    __slots__ = (
        "is_sharpe",
        "oos_sharpe",
        "degradation_pct",
        "max_allowed_degradation",
        "passed",
    )

    def __init__(
        self,
        is_sharpe: float,
        oos_sharpe: float,
        degradation_pct: float,
        max_allowed_degradation: float,
    ) -> None:
        self.is_sharpe = is_sharpe
        self.oos_sharpe = oos_sharpe
        self.degradation_pct = degradation_pct
        self.max_allowed_degradation = max_allowed_degradation
        self.passed = degradation_pct <= max_allowed_degradation


def check_live_degradation(
    is_returns: np.ndarray,
    oos_returns: np.ndarray,
    periods_per_year: int = 252,
    max_degradation_pct: float = 0.3,
) -> LiveDegradationResult:
    """Test whether OOS performance degrades more than allowed.

    Compares in-sample Sharpe to out-of-sample Sharpe. The OOS Sharpe
    decline as a percentage of IS Sharpe must not exceed max_degradation_pct.

    Args:
        is_returns: 1D array of in-sample returns.
        oos_returns: 1D array of out-of-sample returns.
        periods_per_year: Number of return periods per year.
        max_degradation_pct: Maximum allowed degradation (0.3 = 30%).

    Returns:
        LiveDegradationResult with pass/fail status.
    """
    is_sharpe = _compute_annual_sharpe(is_returns, periods_per_year)
    oos_sharpe = _compute_annual_sharpe(oos_returns, periods_per_year)

    if abs(is_sharpe) < 1e-12:
        return LiveDegradationResult(
            is_sharpe=is_sharpe,
            oos_sharpe=oos_sharpe,
            degradation_pct=0.0,
            max_allowed_degradation=max_degradation_pct,
        )

    degradation = max(0.0, (is_sharpe - oos_sharpe) / abs(is_sharpe))

    return LiveDegradationResult(
        is_sharpe=is_sharpe,
        oos_sharpe=oos_sharpe,
        degradation_pct=degradation,
        max_allowed_degradation=max_degradation_pct,
    )


def _compute_annual_sharpe(returns: np.ndarray, periods_per_year: int) -> float:
    """Compute annualized Sharpe ratio from periodic returns."""
    if len(returns) < 2:
        return 0.0
    mean_r = np.mean(returns)
    std_r = np.std(returns, ddof=1)
    if std_r < 1e-12:
        return 0.0
    return float(mean_r / std_r * math_sqrt(periods_per_year))


def math_sqrt(x: float) -> float:
    return x**0.5
