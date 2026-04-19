"""Statistical Validation Pipeline.

Orchestrates all validation checks for a trading strategy:
CPCV, PBO, DSR, Sharpe ceiling, complexity penalty, live degradation,
information leakage verification, and reward sensitivity analysis.
"""

from __future__ import annotations

import numpy as np
from typing import Callable


class PipelineConfig:
    """Configuration for the full validation pipeline."""

    __slots__ = (
        "n_cpcv_groups",
        "n_cpcv_test_groups",
        "purge_bars",
        "embargo_bars",
        "sharpe_ceiling",
        "pbo_threshold",
        "dsr_threshold",
        "max_degradation_pct",
        "leakage_threshold",
        "min_complexity_ratio",
        "periods_per_year",
    )

    def __init__(
        self,
        n_cpcv_groups: int = 6,
        n_cpcv_test_groups: int = 2,
        purge_bars: int = 10,
        embargo_bars: int = 5,
        sharpe_ceiling: float = 1.5,
        pbo_threshold: float = 0.1,
        dsr_threshold: float = 0.95,
        max_degradation_pct: float = 0.3,
        leakage_threshold: float = 0.2,
        min_complexity_ratio: float = 0.5,
        periods_per_year: int = 252,
    ) -> None:
        self.n_cpcv_groups = n_cpcv_groups
        self.n_cpcv_test_groups = n_cpcv_test_groups
        self.purge_bars = purge_bars
        self.embargo_bars = embargo_bars
        self.sharpe_ceiling = sharpe_ceiling
        self.pbo_threshold = pbo_threshold
        self.dsr_threshold = dsr_threshold
        self.max_degradation_pct = max_degradation_pct
        self.leakage_threshold = leakage_threshold
        self.min_complexity_ratio = min_complexity_ratio
        self.periods_per_year = periods_per_year


class PipelineCheckResult:
    """Result of a single pipeline check."""

    __slots__ = ("name", "passed", "details", "value")

    def __init__(self, name: str, passed: bool, details: str, value: float = float("nan")) -> None:
        self.name = name
        self.passed = passed
        self.details = details
        self.value = value


class PipelineResult:
    """Aggregated result of the full validation pipeline."""

    __slots__ = ("checks", "all_passed", "n_passed", "n_failed")

    def __init__(self, checks: list[PipelineCheckResult]) -> None:
        self.checks = checks
        self.all_passed = all(c.passed for c in checks)
        self.n_passed = sum(1 for c in checks if c.passed)
        self.n_failed = sum(1 for c in checks if not c.passed)


def run_validation_pipeline(
    returns: np.ndarray,
    num_features: int,
    config: PipelineConfig | None = None,
    is_returns: np.ndarray | None = None,
    oos_returns: np.ndarray | None = None,
    lagged_returns: np.ndarray | None = None,
    unlagged_returns: np.ndarray | None = None,
    strategy_returns_matrix: np.ndarray | None = None,
    n_trials: int = 1,
    reward_fn: Callable[[np.ndarray, float, float, float], float] | None = None,
) -> PipelineResult:
    """Run the full statistical validation pipeline.

    Args:
        returns: 1D array of strategy returns (IS or full sample).
        num_features: Number of features used by the strategy.
        config: Pipeline configuration. Uses defaults if None.
        is_returns: In-sample returns for degradation test.
        oos_returns: Out-of-sample returns for degradation test.
        lagged_returns: Returns with lag for leakage test.
        unlagged_returns: Returns without lag for leakage test.
        strategy_returns_matrix: 2D array (n_strategies, n_obs) for PBO.
        n_trials: Number of independent trials for DSR.
        reward_fn: Reward function for sensitivity analysis.

    Returns:
        PipelineResult with all check results.
    """
    if config is None:
        config = PipelineConfig()

    checks: list[PipelineCheckResult] = []

    # 1. Sharpe Ceiling
    from .validation import check_sharpe_ceiling

    ceiling_result = check_sharpe_ceiling(returns, config.periods_per_year, config.sharpe_ceiling)
    checks.append(
        PipelineCheckResult(
            name="Sharpe Ceiling",
            passed=ceiling_result.passed,
            details=ceiling_result.rejection_reason or f"Sharpe={ceiling_result.annual_sharpe:.4f} <= {config.sharpe_ceiling}",
            value=ceiling_result.annual_sharpe,
        )
    )

    # 2. DSR
    from .dsr import compute_dsr

    dsr_result = compute_dsr(
        sharpe_observed=ceiling_result.annual_sharpe,
        returns=returns,
        n_trials=n_trials,
    )
    checks.append(
        PipelineCheckResult(
            name="Deflated Sharpe Ratio",
            passed=dsr_result.is_acceptable,
            details=f"DSR={dsr_result.dsr:.4f} (threshold={config.dsr_threshold})",
            value=dsr_result.dsr,
        )
    )

    # 3. Complexity Penalty
    from .validation import compute_complexity_penalty

    cp_result = compute_complexity_penalty(returns, num_features, config.periods_per_year)
    complexity_ok = cp_result.adjusted_sharpe > 0.0 or cp_result.raw_sharpe <= 0.0
    checks.append(
        PipelineCheckResult(
            name="Complexity Penalty",
            passed=complexity_ok,
            details=f"Raw={cp_result.raw_sharpe:.4f}, Adjusted={cp_result.adjusted_sharpe:.4f}, Features={num_features}",
            value=cp_result.adjusted_sharpe,
        )
    )

    # 4. Live Degradation Test (if OOS data provided)
    if is_returns is not None and oos_returns is not None:
        from .validation import check_live_degradation

        deg_result = check_live_degradation(
            is_returns, oos_returns, config.periods_per_year, config.max_degradation_pct
        )
        checks.append(
            PipelineCheckResult(
                name="Live Degradation",
                passed=deg_result.passed,
                details=f"IS={deg_result.is_sharpe:.4f}, OOS={deg_result.oos_sharpe:.4f}, Degradation={deg_result.degradation_pct:.2%}",
                value=deg_result.degradation_pct,
            )
        )

    # 5. Information Leakage (if lag/unlag data provided)
    if lagged_returns is not None and unlagged_returns is not None:
        from .leakage import verify_information_leakage

        leak_result = verify_information_leakage(
            lagged_returns, unlagged_returns, threshold=config.leakage_threshold
        )
        checks.append(
            PipelineCheckResult(
                name="Information Leakage",
                passed=not leak_result.leakage_detected,
                details=f"Lagged={leak_result.performance_with_lag:.4f}, Unlagged={leak_result.performance_without_lag:.4f}, Ratio={leak_result.leakage_ratio:.4f}",
                value=leak_result.leakage_ratio,
            )
        )

    # 6. PBO (if strategy matrix provided)
    if strategy_returns_matrix is not None:
        from .pbo import compute_pbo

        pbo_result = compute_pbo(
            strategy_returns_matrix,
            n_groups=config.n_cpcv_groups,
            n_test_groups=config.n_cpcv_test_groups,
        )
        checks.append(
            PipelineCheckResult(
                name="PBO",
                passed=pbo_result.is_acceptable,
                details=f"PBO={pbo_result.pbo:.4f} (threshold={config.pbo_threshold})",
                value=pbo_result.pbo,
            )
        )

    # 7. CPCV
    from .cpcv import run_cpcv

    def oos_sharpe_fn(train: np.ndarray, test: np.ndarray) -> float:
        std = np.std(test, ddof=1)
        return float(np.mean(test) / std) if std > 1e-12 else 0.0

    cpcv_result = run_cpcv(
        returns,
        oos_sharpe_fn,
        n_groups=config.n_cpcv_groups,
        n_test_groups=config.n_cpcv_test_groups,
        purge_bars=config.purge_bars,
        embargo_bars=config.embargo_bars,
    )
    cpcv_ok = cpcv_result.mean_score > 0.0 and cpcv_result.failure_rate < 0.5
    checks.append(
        PipelineCheckResult(
            name="CPCV",
            passed=cpcv_ok,
            details=f"Mean OOS Sharpe={cpcv_result.mean_score:.4f}, Failure Rate={cpcv_result.failure_rate:.2%}, Splits={cpcv_result.n_splits}",
            value=cpcv_result.mean_score,
        )
    )

    # 8. Reward Sensitivity (if reward_fn provided)
    if reward_fn is not None:
        from .sensitivity import analyze_reward_sensitivity

        sens_result = analyze_reward_sensitivity(reward_fn, returns)
        robust_names = [r.param_name for r in sens_result.results if r.is_robust]
        fragile_names = [r.param_name for r in sens_result.results if not r.is_robust]
        details_parts = [f"Robust: {robust_names}" if robust_names else ""]
        if fragile_names:
            details_parts.append(f"Fragile: {fragile_names}")
        checks.append(
            PipelineCheckResult(
                name="Reward Sensitivity",
                passed=sens_result.overall_robust,
                details=f"Base Perf={sens_result.base_performance:.4f}, {'; '.join(filter(None, details_parts))}",
                value=1.0 if sens_result.overall_robust else 0.0,
            )
        )

    return PipelineResult(checks)
