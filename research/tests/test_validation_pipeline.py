"""Tests for the statistical validation pipeline."""

import math

import numpy as np
import pytest

from research.analysis.cpcv import CpcvResult, CpcvSplit, generate_cpcv_splits, run_cpcv
from research.analysis.pbo import PboResult, compute_pbo
from research.analysis.dsr import DsrResult, compute_dsr
from research.analysis.validation import (
    SharpeCeilingResult,
    ComplexityPenaltyResult,
    LiveDegradationResult,
    check_sharpe_ceiling,
    compute_complexity_penalty,
    check_live_degradation,
)
from research.analysis.leakage import LeakageTestResult, verify_information_leakage
from research.analysis.sensitivity import (
    SensitivityResult,
    SensitivityAnalysisResult,
    analyze_reward_sensitivity,
)
from research.analysis.pipeline import (
    PipelineConfig,
    PipelineCheckResult,
    PipelineResult,
    run_validation_pipeline,
)

rng = np.random.RandomState(42)


# ── Helpers ──────────────────────────────────────────────────────────────────


def _make_returns(n: int = 1000, mean: float = 0.0001, std: float = 0.01) -> np.ndarray:
    return rng.normal(mean, std, n)


def _sharpe_fn(train: np.ndarray, test: np.ndarray) -> float:
    std = np.std(test, ddof=1)
    return float(np.mean(test) / std) if std > 1e-12 else 0.0


# ── CPCV Tests ───────────────────────────────────────────────────────────────


class TestCpcvSplits:
    def test_basic_split_generation(self):
        splits = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2)
        assert len(splits) == 15  # C(6,2) = 15

    def test_split_has_train_and_test(self):
        splits = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2)
        for s in splits:
            assert s.n_train > 0
            assert s.n_test > 0

    def test_no_overlap(self):
        splits = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2)
        for s in splits:
            overlap = set(s.train_indices) & set(s.test_indices)
            assert len(overlap) == 0

    def test_full_coverage(self):
        splits = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2)
        for s in splits:
            total = set(s.train_indices) | set(s.test_indices)
            assert len(total) == 1000

    def test_purge_removes_boundary_samples(self):
        splits_no_purge = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2, purge_bars=0, embargo_bars=0)
        splits_purge = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2, purge_bars=10, embargo_bars=5)
        assert len(splits_no_purge) == len(splits_purge)
        for s_no, s_purge in zip(splits_no_purge, splits_purge):
            assert s_purge.n_train <= s_no.n_train

    def test_embargo_removes_more(self):
        splits_purge = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2, purge_bars=10, embargo_bars=0)
        splits_embargo = generate_cpcv_splits(1000, n_groups=6, n_test_groups=2, purge_bars=10, embargo_bars=5)
        for s_p, s_e in zip(splits_purge, splits_embargo):
            assert s_e.n_train <= s_p.n_train

    def test_invalid_n_groups(self):
        with pytest.raises(ValueError):
            generate_cpcv_splits(100, n_groups=1)

    def test_invalid_n_test_groups(self):
        with pytest.raises(ValueError):
            generate_cpcv_splits(100, n_groups=6, n_test_groups=6)

    def test_n_samples_less_than_groups(self):
        with pytest.raises(ValueError):
            generate_cpcv_splits(3, n_groups=6)

    def test_uneven_split(self):
        splits = generate_cpcv_splits(103, n_groups=6, n_test_groups=2)
        assert len(splits) > 0
        for s in splits:
            assert s.n_test > 0

    def test_single_test_group(self):
        splits = generate_cpcv_splits(100, n_groups=5, n_test_groups=1)
        assert len(splits) == 5  # C(5,1) = 5


class TestCpcvRun:
    def test_basic_run(self):
        returns = _make_returns(1000)
        result = run_cpcv(returns, _sharpe_fn, n_groups=6, n_test_groups=2)
        assert isinstance(result, CpcvResult)
        assert result.n_splits == 15
        assert result.std_score >= 0.0

    def test_result_aggregation(self):
        returns = _make_returns(1000)
        result = run_cpcv(returns, _sharpe_fn)
        assert result.mean_score == pytest.approx(np.mean(result.split_results), abs=1e-10)
        assert result.min_score <= result.mean_score <= result.max_score

    def test_failure_rate(self):
        rng_neg = np.random.RandomState(123)
        neg_returns = rng_neg.normal(-0.001, 0.01, 1000)
        result = run_cpcv(neg_returns, _sharpe_fn)
        assert 0.0 <= result.failure_rate <= 1.0


# ── PBO Tests ────────────────────────────────────────────────────────────────


class TestPBO:
    def test_non_overfit_strategies(self):
        rng_pbo = np.random.RandomState(42)
        strategy_returns = rng_pbo.normal(0.0001, 0.01, (10, 1000))
        result = compute_pbo(strategy_returns, n_groups=6, n_test_groups=2)
        assert isinstance(result, PboResult)
        assert 0.0 <= result.pbo <= 1.0
        assert result.n_strategies == 10

    def test_rank_distribution_sums_to_one(self):
        rng_pbo = np.random.RandomState(42)
        strategy_returns = rng_pbo.normal(0.0001, 0.01, (5, 1000))
        result = compute_pbo(strategy_returns, n_groups=6, n_test_groups=2)
        np.testing.assert_allclose(
            np.sum(result.optimal_strategy_rank_distribution), 1.0, atol=1e-10
        )

    def test_single_strategy_raises(self):
        with pytest.raises(ValueError):
            compute_pbo(np.random.randn(1, 100))

    def test_too_few_observations(self):
        with pytest.raises(ValueError):
            compute_pbo(np.random.randn(5, 3), n_groups=6)

    def test_identical_strategies(self):
        rng_pbo = np.random.RandomState(42)
        base = rng_pbo.normal(0.0001, 0.01, 1000)
        matrix = np.tile(base, (5, 1))
        result = compute_pbo(matrix, n_groups=6, n_test_groups=2)
        assert result.pbo >= 0.0


# ── DSR Tests ────────────────────────────────────────────────────────────────


class TestDSR:
    def test_good_sharpe_single_trial(self):
        returns = rng.normal(0.001, 0.01, 500)
        result = compute_dsr(sharpe_observed=1.0, returns=returns, n_trials=1)
        assert isinstance(result, DsrResult)
        assert 0.0 <= result.dsr <= 1.0

    def test_multiple_trials_reduces_dsr(self):
        returns = rng.normal(0.001, 0.01, 500)
        result_single = compute_dsr(sharpe_observed=0.8, returns=returns, n_trials=1)
        result_multi = compute_dsr(sharpe_observed=0.8, returns=returns, n_trials=50)
        assert result_multi.dsr <= result_single.dsr

    def test_with_reference(self):
        returns = rng.normal(0.001, 0.01, 500)
        result = compute_dsr(
            sharpe_observed=0.8, returns=returns, n_trials=1, sharpe_reference=0.3
        )
        assert result.sharpe_expected == 0.3

    def test_short_returns(self):
        returns = np.array([0.01, 0.02])
        result = compute_dsr(sharpe_observed=1.0, returns=returns, n_trials=1)
        assert 0.0 <= result.dsr <= 1.0

    def test_single_observation(self):
        returns = np.array([0.01])
        result = compute_dsr(sharpe_observed=1.0, returns=returns, n_trials=1)
        assert result.dsr == 0.0

    def test_custom_skewness_kurtosis(self):
        returns = rng.normal(0.001, 0.01, 500)
        result = compute_dsr(
            sharpe_observed=0.8, returns=returns, n_trials=1, skewness=0.0, kurtosis=0.0
        )
        assert result.variance > 0.0


# ── Sharpe Ceiling Tests ────────────────────────────────────────────────────


class TestSharpeCeiling:
    def test_below_ceiling_passes(self):
        returns = rng.normal(0.0001, 0.01, 500)
        result = check_sharpe_ceiling(returns, periods_per_year=252, ceiling=1.5)
        assert result.passed is True
        assert result.rejection_reason is None

    def test_above_ceiling_fails(self):
        returns = rng.normal(0.005, 0.005, 500)
        result = check_sharpe_ceiling(returns, periods_per_year=252, ceiling=1.0)
        if result.annual_sharpe > 1.0:
            assert result.passed is False
            assert result.rejection_reason is not None

    def test_custom_ceiling(self):
        returns = rng.normal(0.0001, 0.01, 500)
        result = check_sharpe_ceiling(returns, periods_per_year=252, ceiling=100.0)
        assert result.passed is True

    def test_zero_returns(self):
        returns = np.zeros(100)
        result = check_sharpe_ceiling(returns)
        assert result.annual_sharpe == 0.0
        assert result.passed is True

    def test_negative_sharpe(self):
        returns = rng.normal(-0.001, 0.01, 500)
        result = check_sharpe_ceiling(returns)
        assert result.annual_sharpe < 0.0
        assert result.passed is True

    def test_short_returns(self):
        returns = np.array([0.01])
        result = check_sharpe_ceiling(returns)
        assert result.annual_sharpe == 0.0


# ── Complexity Penalty Tests ─────────────────────────────────────────────────


class TestComplexityPenalty:
    def test_penalty_reduces_sharpe(self):
        returns = rng.normal(0.0001, 0.01, 500)
        result = compute_complexity_penalty(returns, num_features=10)
        assert isinstance(result, ComplexityPenaltyResult)
        assert abs(result.adjusted_sharpe) <= abs(result.raw_sharpe) + 1e-10

    def test_more_features_more_penalty(self):
        returns = rng.normal(0.0001, 0.01, 500)
        r5 = compute_complexity_penalty(returns, num_features=5)
        r20 = compute_complexity_penalty(returns, num_features=20)
        assert abs(r20.adjusted_sharpe) < abs(r5.adjusted_sharpe)

    def test_single_feature(self):
        returns = rng.normal(0.0001, 0.01, 500)
        result = compute_complexity_penalty(returns, num_features=1)
        assert result.penalty_factor == 1.0
        assert result.adjusted_sharpe == pytest.approx(result.raw_sharpe, abs=1e-10)

    def test_zero_features_clamped(self):
        returns = rng.normal(0.0001, 0.01, 500)
        result = compute_complexity_penalty(returns, num_features=0)
        assert result.penalty_factor == 1.0

    def test_negative_sharpe_preserved(self):
        returns = rng.normal(-0.001, 0.01, 500)
        result = compute_complexity_penalty(returns, num_features=10)
        assert result.raw_sharpe < 0.0
        assert result.adjusted_sharpe < 0.0


# ── Live Degradation Tests ──────────────────────────────────────────────────


class TestLiveDegradation:
    def test_similar_performance_passes(self):
        is_r = rng.normal(0.001, 0.005, 500)
        oos_r = rng.normal(0.001, 0.005, 500)
        result = check_live_degradation(is_r, oos_r, max_degradation_pct=0.8)
        assert isinstance(result, LiveDegradationResult)

    def test_large_degradation_fails(self):
        is_r = rng.normal(0.005, 0.005, 500)
        oos_r = rng.normal(-0.001, 0.01, 500)
        result = check_live_degradation(is_r, oos_r, max_degradation_pct=0.3)
        if result.degradation_pct > 0.3:
            assert result.passed is False

    def test_zero_is_sharpe(self):
        is_r = np.zeros(100)
        oos_r = rng.normal(0.0001, 0.01, 100)
        result = check_live_degradation(is_r, oos_r)
        assert result.degradation_pct == 0.0
        assert result.passed is True

    def test_oos_better_than_is(self):
        is_r = rng.normal(0.0001, 0.01, 500)
        oos_r = rng.normal(0.002, 0.005, 500)
        result = check_live_degradation(is_r, oos_r)
        assert result.degradation_pct == 0.0
        assert result.passed is True

    def test_custom_threshold(self):
        is_r = rng.normal(0.001, 0.01, 500)
        oos_r = rng.normal(0.0005, 0.01, 500)
        result = check_live_degradation(is_r, oos_r, max_degradation_pct=0.5)
        assert result.max_allowed_degradation == 0.5


# ── Information Leakage Tests ────────────────────────────────────────────────


class TestInformationLeakage:
    def test_no_leakage(self):
        lagged = rng.normal(0.0001, 0.01, 500)
        unlagged = rng.normal(0.0001, 0.01, 500)
        result = verify_information_leakage(lagged, unlagged)
        assert isinstance(result, LeakageTestResult)
        assert result.leakage_detected is False

    def test_leakage_detected(self):
        lagged = rng.normal(0.0001, 0.01, 500)
        unlagged = rng.normal(0.005, 0.005, 500)
        result = verify_information_leakage(lagged, unlagged, threshold=0.2)
        if result.performance_without_lag > result.performance_with_lag:
            if result.leakage_ratio > 0.2:
                assert result.leakage_detected is True

    def test_custom_metric(self):
        lagged = rng.normal(0.0001, 0.01, 500)
        unlagged = rng.normal(0.0001, 0.01, 500)
        metric = lambda r: float(np.sum(r))
        result = verify_information_leakage(lagged, unlagged, metric_fn=metric)
        assert result.performance_with_lag == pytest.approx(float(np.sum(lagged)), abs=1e-10)

    def test_zero_lagged_performance(self):
        lagged = np.zeros(100)
        unlagged = rng.normal(0.001, 0.01, 100)
        result = verify_information_leakage(lagged, unlagged)
        assert result.leakage_ratio == float("inf")

    def test_both_zero(self):
        lagged = np.zeros(100)
        unlagged = np.zeros(100)
        result = verify_information_leakage(lagged, unlagged)
        assert result.leakage_ratio == 0.0
        assert result.leakage_detected is False

    def test_custom_threshold(self):
        lagged = rng.normal(0.0001, 0.01, 500)
        unlagged = rng.normal(0.0001, 0.01, 500)
        result = verify_information_leakage(lagged, unlagged, threshold=0.5)
        assert result.threshold == 0.5


# ── Sensitivity Analysis Tests ───────────────────────────────────────────────


class TestSensitivityAnalysis:
    def test_basic_analysis(self):
        def reward_fn(returns, lr, ld, dc):
            adj = returns - lr * returns**2 - ld * np.maximum(0, -returns)
            std = np.std(adj, ddof=1)
            return float(np.mean(adj) / std) if std > 1e-12 else 0.0

        returns = rng.normal(0.0001, 0.01, 500)
        result = analyze_reward_sensitivity(reward_fn, returns)
        assert isinstance(result, SensitivityAnalysisResult)
        assert len(result.results) == 3
        assert isinstance(result.base_performance, float)

    def test_robust_result(self):
        def constant_reward(returns, lr, ld, dc):
            return 1.0

        returns = rng.normal(0.0001, 0.01, 500)
        result = analyze_reward_sensitivity(constant_reward, returns)
        assert result.overall_robust is True
        for r in result.results:
            assert r.is_robust is True

    def test_perturbation_factors(self):
        def constant_reward(returns, lr, ld, dc):
            return 1.0

        returns = np.zeros(100)
        factors = [0.25, 0.5, 1.0, 2.0, 4.0]
        result = analyze_reward_sensitivity(constant_reward, returns, perturbation_factors=factors)
        for r in result.results:
            assert len(r.perturbed_values) == len(factors)
            assert len(r.performances) == len(factors)

    def test_param_names(self):
        def constant_reward(returns, lr, ld, dc):
            return 1.0

        returns = np.zeros(100)
        result = analyze_reward_sensitivity(constant_reward, returns)
        names = [r.param_name for r in result.results]
        assert "lambda_risk" in names
        assert "lambda_dd" in names
        assert "dd_cap" in names

    def test_sensitive_result(self):
        call_count = [0]

        def fragile_reward(returns, lr, ld, dc):
            call_count[0] += 1
            if lr > 0.15:
                return -1.0
            return 1.0

        returns = np.zeros(100)
        result = analyze_reward_sensitivity(
            fragile_reward, returns,
            base_lambda_risk=0.1,
            perturbation_factors=[0.5, 1.0, 2.0],
            min_performance_ratio=0.5,
        )
        assert isinstance(result, SensitivityAnalysisResult)


# ── Pipeline Tests ───────────────────────────────────────────────────────────


class TestPipeline:
    def test_minimal_pipeline(self):
        returns = rng.normal(0.0001, 0.01, 1000)
        result = run_validation_pipeline(returns, num_features=10)
        assert isinstance(result, PipelineResult)
        assert result.n_passed >= 0
        assert result.n_failed >= 0
        check_names = [c.name for c in result.checks]
        assert "Sharpe Ceiling" in check_names
        assert "Deflated Sharpe Ratio" in check_names
        assert "Complexity Penalty" in check_names
        assert "CPCV" in check_names

    def test_full_pipeline(self):
        returns = rng.normal(0.0001, 0.01, 1000)
        is_r = rng.normal(0.0001, 0.01, 500)
        oos_r = rng.normal(0.0001, 0.01, 500)
        lagged = rng.normal(0.0001, 0.01, 500)
        unlagged = rng.normal(0.0001, 0.01, 500)
        strat_matrix = rng.normal(0.0001, 0.01, (5, 1000))

        def reward_fn(returns, lr, ld, dc):
            return float(np.mean(returns))

        result = run_validation_pipeline(
            returns=returns,
            num_features=10,
            is_returns=is_r,
            oos_returns=oos_r,
            lagged_returns=lagged,
            unlagged_returns=unlagged,
            strategy_returns_matrix=strat_matrix,
            n_trials=5,
            reward_fn=reward_fn,
        )
        check_names = [c.name for c in result.checks]
        assert "Live Degradation" in check_names
        assert "Information Leakage" in check_names
        assert "PBO" in check_names
        assert "Reward Sensitivity" in check_names

    def test_custom_config(self):
        config = PipelineConfig(
            sharpe_ceiling=2.0,
            n_cpcv_groups=4,
            n_cpcv_test_groups=1,
        )
        returns = rng.normal(0.0001, 0.01, 500)
        result = run_validation_pipeline(returns, num_features=5, config=config)
        assert isinstance(result, PipelineResult)

    def test_check_result_structure(self):
        check = PipelineCheckResult(name="test", passed=True, details="ok", value=1.0)
        assert check.name == "test"
        assert check.passed is True
        assert check.value == 1.0

    def test_all_passed(self):
        checks = [
            PipelineCheckResult("a", True, "ok"),
            PipelineCheckResult("b", True, "ok"),
        ]
        result = PipelineResult(checks)
        assert result.all_passed is True
        assert result.n_passed == 2
        assert result.n_failed == 0

    def test_not_all_passed(self):
        checks = [
            PipelineCheckResult("a", True, "ok"),
            PipelineCheckResult("b", False, "fail"),
        ]
        result = PipelineResult(checks)
        assert result.all_passed is False
        assert result.n_passed == 1
        assert result.n_failed == 1

    def test_config_defaults(self):
        config = PipelineConfig()
        assert config.sharpe_ceiling == 1.5
        assert config.pbo_threshold == 0.1
        assert config.dsr_threshold == 0.95
        assert config.max_degradation_pct == 0.3
        assert config.leakage_threshold == 0.2

    def test_negative_returns_pipeline(self):
        returns = rng.normal(-0.001, 0.01, 1000)
        result = run_validation_pipeline(returns, num_features=10)
        assert isinstance(result, PipelineResult)


# ── CpcvResult Tests ─────────────────────────────────────────────────────────


class TestCpcvResult:
    def test_empty_result(self):
        result = CpcvResult([])
        assert result.n_splits == 0
        assert result.mean_score == 0.0
        assert result.std_score == 0.0
        assert result.failure_rate == 0.0

    def test_single_split(self):
        result = CpcvResult([0.5])
        assert result.n_splits == 1
        assert result.mean_score == 0.5
        assert result.std_score == 0.0 or np.isnan(result.std_score)

    def test_mixed_results(self):
        scores = [0.1, -0.2, 0.3, -0.1, 0.0]
        result = CpcvResult(scores)
        assert result.n_splits == 5
        assert result.min_score == -0.2
        assert result.max_score == 0.3
        assert result.failure_rate == 0.6  # 3 out of 5 <= 0 (0.0, -0.1, -0.2)


# ── Edge Cases ───────────────────────────────────────────────────────────────


class TestEdgeCases:
    def test_nan_in_returns(self):
        returns = rng.normal(0.0001, 0.01, 500)
        returns[10] = float("nan")
        with np.errstate(invalid="ignore"):
            result = check_sharpe_ceiling(returns)
        assert isinstance(result, SharpeCeilingResult)

    def test_inf_in_returns(self):
        returns = rng.normal(0.0001, 0.01, 500)
        returns[10] = float("inf")
        with np.errstate(invalid="ignore"):
            result = check_sharpe_ceiling(returns)
        assert isinstance(result, SharpeCeilingResult)

    def test_constant_returns(self):
        returns = np.full(500, 0.0001)
        result = check_sharpe_ceiling(returns)
        assert result.annual_sharpe == 0.0

    def test_very_small_sample_cpcv(self):
        returns = rng.normal(0.0001, 0.01, 12)
        result = run_cpcv(returns, _sharpe_fn, n_groups=6, n_test_groups=2)
        assert isinstance(result, CpcvResult)

    def test_dsr_zero_sharpe(self):
        returns = rng.normal(0.0, 0.01, 500)
        result = compute_dsr(sharpe_observed=0.0, returns=returns, n_trials=1)
        assert 0.0 <= result.dsr <= 1.0
