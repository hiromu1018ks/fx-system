"""Combinatorial Purged Cross-Validation (CPCV) for time series.

Implements the CPCV method from Marcos Lopez de Prado's "Advances in Financial
Machine Learning" to prevent information leakage in backtesting through
purged combinatorial cross-validation.
"""

from __future__ import annotations

import numpy as np
from itertools import combinations
from typing import Callable


class CpcvSplit:
    """A single train/test split from a CPCV combinatorial partition."""

    __slots__ = ("train_indices", "test_indices", "n_train", "n_test")

    def __init__(self, train_indices: np.ndarray, test_indices: np.ndarray) -> None:
        self.train_indices = train_indices
        self.test_indices = test_indices
        self.n_train = len(train_indices)
        self.n_test = len(test_indices)


class CpcvResult:
    """Aggregated results from a full CPCV evaluation."""

    __slots__ = (
        "split_results",
        "mean_score",
        "std_score",
        "min_score",
        "max_score",
        "n_splits",
        "failure_rate",
    )

    def __init__(
        self,
        split_results: list[float],
    ) -> None:
        self.split_results = np.array(split_results, dtype=np.float64)
        self.n_splits = len(split_results)
        if self.n_splits > 0:
            self.mean_score = float(np.mean(self.split_results))
            self.std_score = float(np.std(self.split_results, ddof=1))
            self.min_score = float(np.min(self.split_results))
            self.max_score = float(np.max(self.split_results))
        else:
            self.mean_score = 0.0
            self.std_score = 0.0
            self.min_score = 0.0
            self.max_score = 0.0
        self.failure_rate = float(np.mean(self.split_results <= 0.0)) if self.n_splits > 0 else 0.0


def generate_cpcv_splits(
    n_samples: int,
    n_groups: int = 6,
    n_test_groups: int = 2,
    purge_bars: int = 0,
    embargo_bars: int = 0,
) -> list[CpcvSplit]:
    """Generate combinatorial purged cross-validation splits.

    Divides the sample into `n_groups` contiguous groups, then generates all
    C(n_groups, n_test_groups) combinations where test groups are selected and
    remaining groups form the training set. Purge and embargo bars are removed
    from training data adjacent to test boundaries to prevent information leakage.

    Args:
        n_samples: Total number of observations.
        n_groups: Number of contiguous groups to partition into.
        n_test_groups: Number of groups used as test set per split.
        purge_bars: Number of bars to purge between train and test boundaries.
        embargo_bars: Additional embargo bars after purge zone.

    Returns:
        List of CpcvSplit objects with train/test index arrays.

    Raises:
        ValueError: If parameters are invalid.
    """
    if n_groups < 2:
        raise ValueError("n_groups must be >= 2")
    if n_test_groups < 1 or n_test_groups >= n_groups:
        raise ValueError(f"n_test_groups must be in [1, {n_groups - 1}]")
    if n_samples < n_groups:
        raise ValueError("n_samples must be >= n_groups")

    group_size = n_samples // n_groups
    remainder = n_samples % n_groups
    boundaries = np.zeros(n_groups + 1, dtype=np.int64)
    for i in range(n_groups):
        boundaries[i + 1] = boundaries[i] + group_size + (1 if i < remainder else 0)

    group_ranges = [(int(boundaries[i]), int(boundaries[i + 1])) for i in range(n_groups)]

    splits: list[CpcvSplit] = []
    total_purge = purge_bars + embargo_bars

    for test_combo in combinations(range(n_groups), n_test_groups):
        test_set = set(test_combo)
        train_indices_list: list[np.ndarray] = []
        test_indices_list: list[np.ndarray] = []

        for g_idx in range(n_groups):
            start, end = group_ranges[g_idx]
            group_indices = np.arange(start, end, dtype=np.int64)

            if g_idx in test_set:
                test_indices_list.append(group_indices)
            else:
                if total_purge > 0:
                    purge_start = start
                    purge_end = end

                    for t_idx in test_combo:
                        t_start, t_end = group_ranges[t_idx]
                        if t_start > start:
                            purge_end = min(purge_end, t_start - total_purge)
                        if t_end <= end:
                            purge_start = max(purge_start, t_end + total_purge)

                    purge_end = max(purge_end, purge_start)
                    train_indices_list.append(np.arange(purge_start, purge_end, dtype=np.int64))
                else:
                    train_indices_list.append(group_indices)

        train_idx = np.concatenate(train_indices_list) if train_indices_list else np.array([], dtype=np.int64)
        test_idx = np.concatenate(test_indices_list) if test_indices_list else np.array([], dtype=np.int64)

        if len(train_idx) > 0 and len(test_idx) > 0:
            splits.append(CpcvSplit(train_idx, test_idx))

    return splits


def run_cpcv(
    returns: np.ndarray,
    strategy_fn: Callable[[np.ndarray, np.ndarray], float],
    n_groups: int = 6,
    n_test_groups: int = 2,
    purge_bars: int = 10,
    embargo_bars: int = 5,
) -> CpcvResult:
    """Run full CPCV evaluation on returns data.

    Args:
        returns: 1D array of strategy returns (or features for custom scoring).
        strategy_fn: Function(train_returns, test_returns) -> performance_score.
                     Must accept 1D numpy arrays and return a float (e.g. Sharpe).
        n_groups: Number of groups for partitioning.
        n_test_groups: Number of test groups per split.
        purge_bars: Bars to purge between train/test.
        embargo_bars: Embargo bars after purge.

    Returns:
        CpcvResult with aggregated performance across all splits.
    """
    splits = generate_cpcv_splits(len(returns), n_groups, n_test_groups, purge_bars, embargo_bars)
    split_scores: list[float] = []

    for split in splits:
        train_returns = returns[split.train_indices]
        test_returns = returns[split.test_indices]
        if len(train_returns) > 0 and len(test_returns) > 0:
            score = strategy_fn(train_returns, test_returns)
            split_scores.append(score)

    return CpcvResult(split_scores)
