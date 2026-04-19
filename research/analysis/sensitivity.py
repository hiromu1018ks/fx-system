"""Reward function sensitivity analysis.

Perturbs reward function hyperparameters (lambda_risk, lambda_dd, DD_cap)
to assess strategy robustness. A robust strategy should maintain positive
performance across a range of parameter settings.
"""

from __future__ import annotations

import numpy as np
from typing import Callable


class SensitivityResult:
    """Result of sensitivity analysis for a single parameter."""

    __slots__ = ("param_name", "base_value", "perturbed_values", "performances", "is_robust")

    def __init__(
        self,
        param_name: str,
        base_value: float,
        perturbed_values: list[float],
        performances: list[float],
        is_robust: bool,
    ) -> None:
        self.param_name = param_name
        self.base_value = base_value
        self.perturbed_values = perturbed_values
        self.performances = performances
        self.is_robust = is_robust


class SensitivityAnalysisResult:
    """Aggregated results from reward function sensitivity analysis."""

    __slots__ = ("results", "overall_robust", "base_performance")

    def __init__(
        self,
        results: list[SensitivityResult],
        overall_robust: bool,
        base_performance: float,
    ) -> None:
        self.results = results
        self.overall_robust = overall_robust
        self.base_performance = base_performance


def analyze_reward_sensitivity(
    reward_fn: Callable[[np.ndarray, float, float, float], float],
    returns: np.ndarray,
    base_lambda_risk: float = 0.1,
    base_lambda_dd: float = 0.5,
    base_dd_cap: float = 100.0,
    perturbation_factors: list[float] | None = None,
    min_performance_ratio: float = 0.5,
) -> SensitivityAnalysisResult:
    """Analyze strategy sensitivity to reward function parameters.

    Perturbs each reward parameter independently by a set of factors
    (e.g., 0.5x, 0.75x, 1.25x, 1.5x, 2.0x base value) and measures
    the resulting performance. A strategy is robust if performance
    remains above min_performance_ratio of the base performance across
    all perturbations.

    Args:
        reward_fn: Function(returns, lambda_risk, lambda_dd, dd_cap) -> performance_score.
                  Returns the strategy's performance metric for given reward params.
        returns: 1D array of raw returns/rewards.
        base_lambda_risk: Base risk penalty coefficient.
        base_lambda_dd: Base drawdown penalty coefficient.
        base_dd_cap: Base drawdown cap value.
        perturbation_factors: Multipliers for parameter perturbation.
        min_performance_ratio: Minimum performance ratio for robustness (0.5 = 50%).

    Returns:
        SensitivityAnalysisResult with per-parameter and overall robustness.
    """
    if perturbation_factors is None:
        perturbation_factors = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0]

    base_perf = reward_fn(returns, base_lambda_risk, base_lambda_dd, base_dd_cap)

    params = [
        ("lambda_risk", base_lambda_risk),
        ("lambda_dd", base_lambda_dd),
        ("dd_cap", base_dd_cap),
    ]

    results: list[SensitivityResult] = []

    for param_name, base_val in params:
        perturbed_perfs: list[float] = []
        perturbed_vals: list[float] = []

        for factor in perturbation_factors:
            new_val = base_val * factor
            perturbed_vals.append(new_val)

            if param_name == "lambda_risk":
                perf = reward_fn(returns, new_val, base_lambda_dd, base_dd_cap)
            elif param_name == "lambda_dd":
                perf = reward_fn(returns, base_lambda_risk, new_val, base_dd_cap)
            else:
                perf = reward_fn(returns, base_lambda_risk, base_lambda_dd, new_val)

            perturbed_perfs.append(perf)

        is_robust = True
        if abs(base_perf) > 1e-12:
            for p in perturbed_perfs:
                if p / base_perf < min_performance_ratio:
                    is_robust = False
                    break

        results.append(
            SensitivityResult(
                param_name=param_name,
                base_value=base_val,
                perturbed_values=perturbed_vals,
                performances=perturbed_perfs,
                is_robust=is_robust,
            )
        )

    overall = all(r.is_robust for r in results)

    return SensitivityAnalysisResult(
        results=results,
        overall_robust=overall,
        base_performance=base_perf,
    )
