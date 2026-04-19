"""Deflated Sharpe Ratio (DSR).

Implements the DSR from Bailey & Lopez de Prado (2014).
DSR >= 0.95 is required to confirm a strategy's Sharpe ratio is not
due to multiple testing / selection bias.
"""

from __future__ import annotations

import math

import numpy as np
from scipy.stats import norm


class DsrResult:
    """Result of a Deflated Sharpe Ratio evaluation."""

    __slots__ = ("dsr", "sharpe_observed", "sharpe_expected", "variance", "n_trials", "is_acceptable")

    def __init__(
        self,
        dsr: float,
        sharpe_observed: float,
        sharpe_expected: float,
        variance: float,
        n_trials: int,
    ) -> None:
        self.dsr = float(dsr)
        self.sharpe_observed = float(sharpe_observed)
        self.sharpe_expected = float(sharpe_expected)
        self.variance = float(variance)
        self.n_trials = n_trials
        self.is_acceptable = dsr >= 0.95


def compute_dsr(
    sharpe_observed: float,
    returns: np.ndarray,
    n_trials: int = 1,
    sharpe_reference: float | None = None,
    skewness: float | None = None,
    kurtosis: float | None = None,
) -> DsrResult:
    """Compute the Deflated Sharpe Ratio.

    Adjusts the observed Sharpe ratio for the number of independent trials
    (multiple testing), return distribution properties (skewness, kurtosis),
    and an optional reference Sharpe from a benchmark strategy.

    Args:
        sharpe_observed: The annualized Sharpe ratio of the strategy being tested.
        returns: 1D array of returns used to estimate distribution properties.
        n_trials: Number of independent strategy trials (for multiple testing correction).
        sharpe_reference: Benchmark/baseline Sharpe ratio. Defaults to 0.0.
        skewness: Return skewness. Estimated from data if None.
        kurtosis: Return excess kurtosis. Estimated from data if None.

    Returns:
        DsrResult with the deflated Sharpe probability and diagnostics.
    """
    if sharpe_reference is None:
        sharpe_reference = 0.0

    n = len(returns)
    if n < 2:
        return DsrResult(
            dsr=0.0,
            sharpe_observed=sharpe_observed,
            sharpe_expected=sharpe_reference,
            variance=0.0,
            n_trials=n_trials,
        )

    if skewness is None:
        skewness = float(_skew(returns))
    if kurtosis is None:
        kurtosis = float(_kurtosis(returns))

    sr_diff = sharpe_observed - sharpe_reference

    hat_sr = sharpe_observed
    hat_var = (1.0 - skewness * hat_sr + (kurtosis - 1.0) / 4.0 * hat_sr**2) / (n - 1)

    hat_var = max(hat_var, 1e-12)

    z_score = (sr_diff * math.sqrt(n)) / math.sqrt(hat_var)

    cdf_val = float(norm.cdf(z_score))
    cdf_val = max(min(cdf_val, 1.0 - 1e-15), 1e-15)
    one_minus_cdf = max(1.0 - cdf_val, 1e-15)

    log_p_value = math.log(cdf_val) + (n_trials - 1) * math.log(one_minus_cdf)
    log_p_value = max(log_p_value, -1e12)

    dsr_value = math.exp(log_p_value)
    dsr_value = min(dsr_value, 1.0)

    return DsrResult(
        dsr=dsr_value,
        sharpe_observed=sharpe_observed,
        sharpe_expected=sharpe_reference,
        variance=hat_var,
        n_trials=n_trials,
    )


def _skew(x: np.ndarray) -> float:
    """Compute sample skewness."""
    n = len(x)
    if n < 3:
        return 0.0
    mean = np.mean(x)
    std = np.std(x, ddof=1)
    if std < 1e-12:
        return 0.0
    return float(np.mean(((x - mean) / std) ** 3))


def _kurtosis(x: np.ndarray) -> float:
    """Compute sample excess kurtosis."""
    n = len(x)
    if n < 4:
        return 0.0
    mean = np.mean(x)
    std = np.std(x, ddof=1)
    if std < 1e-12:
        return 0.0
    return float(np.mean(((x - mean) / std) ** 4) - 3.0)
