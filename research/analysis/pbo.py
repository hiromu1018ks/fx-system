"""Probability of Backtest Overfitting (PBO).

Implements the PBO metric from Marcos Lopez de Prado's framework.
PBO > 0.1 indicates the backtest is likely overfit and should be discarded.
"""

from __future__ import annotations

import numpy as np
from typing import Callable


class PboResult:
    """Result of a PBO evaluation."""

    __slots__ = (
        "pbo",
        "optimal_strategy_rank_distribution",
        "n_strategies",
        "n_splits",
        "is_acceptable",
    )

    def __init__(
        self,
        pbo: float,
        optimal_rank_distribution: np.ndarray,
        n_strategies: int,
        n_splits: int,
    ) -> None:
        self.pbo = float(pbo)
        self.optimal_strategy_rank_distribution = optimal_rank_distribution
        self.n_strategies = n_strategies
        self.n_splits = n_splits
        self.is_acceptable = pbo <= 0.1


def compute_pbo(
    strategy_returns: np.ndarray,
    n_groups: int = 6,
    n_test_groups: int = 2,
) -> PboResult:
    """Compute the Probability of Backtest Overfitting.

    For each CPCV split, rank strategies by their in-sample (train) performance.
    Track the out-of-sample rank of the strategy that was ranked #1 in-sample.
    PBO = fraction of splits where the in-sample optimal strategy performs
    below median out-of-sample.

    Args:
        strategy_returns: 2D array of shape (n_strategies, n_observations).
                          Each row is a strategy's return series.
        n_groups: Number of CPCV groups.
        n_test_groups: Number of test groups per split.

    Returns:
        PboResult with the overfitting probability and diagnostics.
    """
    from .cpcv import generate_cpcv_splits

    n_strategies, n_obs = strategy_returns.shape
    if n_strategies < 2:
        raise ValueError("Need at least 2 strategies to compute PBO")
    if n_obs < n_groups:
        raise ValueError(f"n_obs ({n_obs}) must be >= n_groups ({n_groups})")

    splits = generate_cpcv_splits(n_obs, n_groups, n_test_groups, purge_bars=0, embargo_bars=0)

    rank_distribution = np.zeros(n_strategies, dtype=np.float64)

    for split in splits:
        train_returns = strategy_returns[:, split.train_indices]
        test_returns = strategy_returns[:, split.test_indices]

        if len(split.test_indices) == 0:
            continue

        train_perf = np.array([_safe_sharpe(r) for r in train_returns])
        test_perf = np.array([_safe_sharpe(r) for r in test_returns])

        best_in_sample_idx = int(np.argmax(train_perf))

        test_ranks = np.argsort(np.argsort(-test_perf))
        rank_distribution[test_ranks[best_in_sample_idx]] += 1.0

    if len(splits) > 0:
        rank_distribution /= len(splits)

    pbo_value = float(np.sum(rank_distribution[n_strategies // 2:]))

    return PboResult(
        pbo=pbo_value,
        optimal_rank_distribution=rank_distribution,
        n_strategies=n_strategies,
        n_splits=len(splits),
    )


def _safe_sharpe(returns: np.ndarray) -> float:
    """Compute Sharpe ratio, returning 0.0 for degenerate cases."""
    if len(returns) < 2:
        return 0.0
    std = np.std(returns, ddof=1)
    if std < 1e-12:
        return 0.0
    return float(np.mean(returns) / std)
