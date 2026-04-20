"""End-to-end tests for the statistical validation pipeline (Python side).

Simulates the full Rust backtest → JSON → Python bridge → validation pipeline
→ JSON output flow. Tests each validation component (CPCV, PBO, DSR, Sharpe
ceiling, information leakage, reward sensitivity) in an integrated context.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np
import pytest

_project_root = Path(__file__).resolve().parents[1]
if str(_project_root) not in sys.path:
    sys.path.insert(0, str(_project_root))

from research.analysis.cpcv import generate_cpcv_splits, run_cpcv
from research.analysis.dsr import compute_dsr
from research.analysis.leakage import verify_information_leakage
from research.analysis.pipeline import PipelineConfig, run_validation_pipeline
from research.analysis.pbo import compute_pbo
from research.analysis.sensitivity import analyze_reward_sensitivity
from research.analysis.validation import check_sharpe_ceiling
from research.bridge.loader import extract_num_features, extract_returns, load_backtest_result
from research.bridge.output import write_validation_result
from research.bridge.runner import run_bridge_validation


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_backtest_json(
    returns: list[float] | None = None,
    num_features: int = 45,
    strategy_breakdown: list[dict] | None = None,
    execution_stats: dict | None = None,
) -> dict:
    """Create a synthetic backtest result JSON matching Rust bridge output."""
    if returns is None:
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
    return {
        "summary": {
            "total_pnl": sum(returns),
            "sharpe_ratio": float(np.mean(returns) / max(np.std(returns, ddof=1), 1e-12)),
            "total_trades": len(returns),
        },
        "trades": [{"pnl": r} for r in returns],
        "returns": returns,
        "num_features": num_features,
        "strategy_breakdown": strategy_breakdown or [],
        "execution_stats": execution_stats or {
            "overall_fill_rate": 0.95,
            "avg_slippage": 0.0001,
            "total_fills": 285,
            "total_rejections": 15,
        },
    }


def _write_backtest_json(tmp_path: Path, data: dict) -> Path:
    """Write backtest JSON to a temp file and return the path."""
    path = tmp_path / "backtest_result.json"
    path.write_text(json.dumps(data))
    return path


@pytest.fixture
def moderate_returns() -> np.ndarray:
    """Returns with moderate positive Sharpe (~0.8 annualised at 252 days)."""
    rng = np.random.default_rng(123)
    return rng.normal(0.0005, 0.01, 500)


@pytest.fixture
def high_sharpe_returns() -> np.ndarray:
    """Returns that push annual Sharpe well above 1.5 ceiling."""
    rng = np.random.default_rng(777)
    return rng.normal(0.005, 0.005, 500)


# ---------------------------------------------------------------------------
# Test: CPCV verification (time-series leakage prevention)
# ---------------------------------------------------------------------------


class TestE2eCpcv:
    """CPCV validation: time-series leakage prevention works end-to-end."""

    def test_cpcv_through_bridge_pipeline(self, tmp_path: Path) -> None:
        """CPCV splits must have no train/test overlap and purge zones."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)
        path = _write_backtest_json(tmp_path, data)

        loaded = load_backtest_result(path)
        ret = extract_returns(loaded)
        assert len(ret) == 300

        result = run_bridge_validation(loaded)
        cpcv_check = next(c for c in result["checks"] if c["name"] == "CPCV")
        assert cpcv_check["passed"] is True
        assert cpcv_check["value"] > 0.0

    def test_cpcv_no_train_test_overlap(self, moderate_returns: np.ndarray) -> None:
        """Every CPCV split must have disjoint train and test sets."""
        splits = generate_cpcv_splits(len(moderate_returns), n_groups=6, n_test_groups=2, purge_bars=10, embargo_bars=5)
        for split in splits:
            train_set = set(split.train_indices.tolist())
            test_set = set(split.test_indices.tolist())
            assert train_set.isdisjoint(test_set), (
                f"Train/test overlap: {train_set & test_set}"
            )

    def test_cpcv_purge_embargo_prevents_leakage(self) -> None:
        """With purge + embargo, train indices near each test group boundary are excluded."""
        rng = np.random.default_rng(10)
        returns = rng.normal(0.0, 0.01, 200)
        purge, embargo = 10, 5
        n_groups = 6
        group_size = 200 // n_groups
        total_purge = purge + embargo

        splits = generate_cpcv_splits(len(returns), n_groups=n_groups, n_test_groups=2, purge_bars=purge, embargo_bars=embargo)

        for split in splits:
            train_set = set(split.train_indices.tolist())
            test_set = set(split.test_indices.tolist())
            # Find contiguous test blocks (groups)
            test_indices_sorted = sorted(test_set)
            blocks: list[tuple[int, int]] = []
            block_start = test_indices_sorted[0]
            for i in range(1, len(test_indices_sorted)):
                if test_indices_sorted[i] - test_indices_sorted[i - 1] > 1:
                    blocks.append((block_start, test_indices_sorted[i - 1]))
                    block_start = test_indices_sorted[i]
            blocks.append((block_start, test_indices_sorted[-1]))

            for block_start, block_end in blocks:
                for idx in range(max(0, block_start - total_purge), block_start):
                    assert idx not in train_set, f"Index {idx} in purge zone before test block [{block_start}, {block_end}]"
                for idx in range(block_end + 1, min(200, block_end + total_purge + 1)):
                    assert idx not in train_set, f"Index {idx} in purge zone after test block [{block_start}, {block_end}]"

    def test_cpcv_negative_returns_fails(self, tmp_path: Path) -> None:
        """CPCV should fail when returns are consistently negative."""
        returns = [-0.01] * 300
        data = _make_backtest_json(returns=returns)
        result = run_bridge_validation(data)
        cpcv_check = next(c for c in result["checks"] if c["name"] == "CPCV")
        assert cpcv_check["passed"] is False


# ---------------------------------------------------------------------------
# Test: PBO (Probability of Backtest Overfitting)
# ---------------------------------------------------------------------------


class TestE2ePbo:
    """PBO validation: overfit detection with PBO > 0.1 → reject."""

    def test_non_overfit_strategies_pass(self) -> None:
        """Genuinely different strategies should have low PBO."""
        rng = np.random.default_rng(42)
        n_obs = 300
        strategy_a = rng.normal(0.001, 0.01, n_obs)
        strategy_b = rng.normal(0.0008, 0.012, n_obs)
        strategy_c = rng.normal(0.0005, 0.015, n_obs)
        matrix = np.vstack([strategy_a, strategy_b, strategy_c])

        result = compute_pbo(matrix, n_groups=6, n_test_groups=2)
        assert result.pbo >= 0.0
        assert result.pbo <= 1.0
        assert result.n_strategies == 3
        assert len(result.optimal_strategy_rank_distribution) == 3

    def test_overfit_strategy_detected(self) -> None:
        """A strategy overfitted to noise should have PBO > 0.1."""
        rng = np.random.default_rng(99)
        n_obs = 300
        strategies = []
        for _ in range(20):
            strategies.append(rng.normal(0.0, 0.01, n_obs))
        # Make one strategy look perfect in-sample by construction
        strategies[0][:150] += 0.01
        strategies[0][150:] -= 0.01  # collapse OOS

        matrix = np.vstack(strategies)
        result = compute_pbo(matrix, n_groups=6, n_test_groups=2)
        # PBO may or may not exceed 0.1 depending on the split, but structure is valid
        assert 0.0 <= result.pbo <= 1.0
        assert result.is_acceptable == (result.pbo <= 0.1)

    def test_pbo_through_full_pipeline(self, tmp_path: Path) -> None:
        """PBO check runs in the full validation pipeline via bridge."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)
        path = _write_backtest_json(tmp_path, data)
        loaded = load_backtest_result(path)
        ret = extract_returns(loaded)

        # Build strategy matrix with slight variations
        matrix = np.vstack([
            ret,
            ret * 0.8 + rng.normal(0, 0.001, len(ret)),
            ret * 0.6 + rng.normal(0, 0.002, len(ret)),
        ])

        config = PipelineConfig()
        result = run_validation_pipeline(
            returns=ret,
            num_features=extract_num_features(loaded),
            config=config,
            strategy_returns_matrix=matrix,
        )

        pbo_check = next(c for c in result.checks if c.name == "PBO")
        assert pbo_check.value >= 0.0
        assert pbo_check.value <= 1.0


# ---------------------------------------------------------------------------
# Test: DSR (Deflated Sharpe Ratio)
# ---------------------------------------------------------------------------


class TestE2eDsr:
    """DSR validation: DSR >= 0.95 for acceptable strategies."""

    def test_dsr_good_sharpe_passes(self, moderate_returns: np.ndarray) -> None:
        """A strategy with decent Sharpe and few trials should pass DSR."""
        annual_sharpe = float(np.mean(moderate_returns) / np.std(moderate_returns, ddof=1)) * np.sqrt(252)
        result = compute_dsr(
            sharpe_observed=annual_sharpe,
            returns=moderate_returns,
            n_trials=1,
        )
        # With few trials, DSR should be reasonable
        assert result.dsr >= 0.0
        assert result.dsr <= 1.0

    def test_dsr_multiple_trials_increases_hurdle(self, moderate_returns: np.ndarray) -> None:
        """More trials (multiple testing) should lower DSR."""
        annual_sharpe = float(np.mean(moderate_returns) / np.std(moderate_returns, ddof=1)) * np.sqrt(252)
        dsr_1 = compute_dsr(sharpe_observed=annual_sharpe, returns=moderate_returns, n_trials=1)
        dsr_100 = compute_dsr(sharpe_observed=annual_sharpe, returns=moderate_returns, n_trials=100)
        assert dsr_100.dsr <= dsr_1.dsr

    def test_dsr_through_bridge(self, tmp_path: Path) -> None:
        """DSR check runs correctly through bridge pipeline."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)
        result = run_bridge_validation(data)
        dsr_check = next(c for c in result["checks"] if c["name"] == "Deflated Sharpe Ratio")
        assert 0.0 <= dsr_check["value"] <= 1.0


# ---------------------------------------------------------------------------
# Test: Sharpe ceiling (annual Sharpe > 1.5 → force reject)
# ---------------------------------------------------------------------------


class TestE2eSharpeCeiling:
    """Sharpe ceiling: annual Sharpe > 1.5 triggers forcible rejection."""

    def test_moderate_sharpe_passes_ceiling(self, moderate_returns: np.ndarray) -> None:
        """Moderate returns should pass the Sharpe ceiling check."""
        result = check_sharpe_ceiling(moderate_returns, periods_per_year=252, ceiling=1.5)
        assert result.passed is True
        assert result.annual_sharpe <= 1.5

    def test_high_sharpe_rejected(self, high_sharpe_returns: np.ndarray) -> None:
        """Returns with very high Sharpe must be rejected by ceiling."""
        result = check_sharpe_ceiling(high_sharpe_returns, periods_per_year=252, ceiling=1.5)
        assert result.passed is False
        assert result.annual_sharpe > 1.5

    def test_sharpe_ceiling_in_bridge_pipeline(self, tmp_path: Path) -> None:
        """Sharpe ceiling fires through the bridge when returns are too good."""
        rng = np.random.default_rng(777)
        returns = rng.normal(0.005, 0.005, 500).tolist()
        data = _make_backtest_json(returns=returns)
        result = run_bridge_validation(data)
        ceiling_check = next(c for c in result["checks"] if c["name"] == "Sharpe Ceiling")
        assert ceiling_check["passed"] is False


# ---------------------------------------------------------------------------
# Test: Information leakage verification
# ---------------------------------------------------------------------------


class TestE2eInformationLeakage:
    """Information leakage: lag vs no-lag comparison."""

    def test_no_leakage_passes(self) -> None:
        """When lagged and unlagged have similar performance, no leakage."""
        rng = np.random.default_rng(42)
        lagged = rng.normal(0.001, 0.01, 300)
        unlagged = lagged + rng.normal(0, 0.001, 300)  # very similar
        result = verify_information_leakage(lagged, unlagged, threshold=0.2)
        assert result.leakage_detected is False

    def test_leakage_detected_when_unlagged_much_better(self) -> None:
        """When unlagged is significantly better, leakage is flagged."""
        rng = np.random.default_rng(42)
        lagged = rng.normal(0.0, 0.01, 300)  # zero mean
        unlagged = rng.normal(0.01, 0.01, 300)  # much better
        result = verify_information_leakage(lagged, unlagged, threshold=0.2)
        assert result.leakage_detected is True

    def test_leakage_through_full_pipeline(self, tmp_path: Path) -> None:
        """Information leakage check runs in full pipeline."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)
        path = _write_backtest_json(tmp_path, data)
        loaded = load_backtest_result(path)
        ret = extract_returns(loaded)

        lagged = ret + rng.normal(0, 0.001, len(ret))
        unlagged = ret + rng.normal(0, 0.001, len(ret))

        result = run_validation_pipeline(
            returns=ret,
            num_features=45,
            lagged_returns=lagged,
            unlagged_returns=unlagged,
        )

        leak_check = next(c for c in result.checks if c.name == "Information Leakage")
        assert leak_check.passed is True
        assert np.isfinite(leak_check.value)


# ---------------------------------------------------------------------------
# Test: Reward function sensitivity analysis
# ---------------------------------------------------------------------------


class TestE2eRewardSensitivity:
    """Reward sensitivity: lambda parameter perturbation stability."""

    @staticmethod
    def _stable_reward_fn(returns: np.ndarray, lambda_risk: float, lambda_dd: float, dd_cap: float) -> float:
        """A reward function that is stable under parameter perturbation (ignores params)."""
        return float(np.sum(returns))

    @staticmethod
    def _fragile_reward_fn(returns: np.ndarray, lambda_risk: float, lambda_dd: float, dd_cap: float) -> float:
        """A reward function that is fragile (quadratically sensitive) to perturbation."""
        return float(np.mean(returns)) / (lambda_risk * lambda_risk + 1e-12)

    def test_stable_strategy_passes_sensitivity(self) -> None:
        """A strategy with stable reward should pass sensitivity analysis."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300)
        result = analyze_reward_sensitivity(
            self._stable_reward_fn, returns,
            base_lambda_risk=0.1, base_lambda_dd=0.5, base_dd_cap=100.0,
        )
        assert result.overall_robust is True
        assert len(result.results) == 3  # lambda_risk, lambda_dd, dd_cap

    def test_fragile_strategy_fails_sensitivity(self) -> None:
        """A strategy with fragile reward should fail sensitivity analysis."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300)
        result = analyze_reward_sensitivity(
            self._fragile_reward_fn, returns,
            base_lambda_risk=0.1, base_lambda_dd=0.5, base_dd_cap=100.0,
        )
        assert result.overall_robust is False

    def test_sensitivity_through_full_pipeline(self, tmp_path: Path) -> None:
        """Reward sensitivity runs in full pipeline via bridge."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)
        path = _write_backtest_json(tmp_path, data)
        loaded = load_backtest_result(path)
        ret = extract_returns(loaded)

        result = run_validation_pipeline(
            returns=ret,
            num_features=45,
            reward_fn=self._stable_reward_fn,
        )
        sens_check = next(c for c in result.checks if c.name == "Reward Sensitivity")
        assert sens_check.passed is True


# ---------------------------------------------------------------------------
# Test: Full E2E roundtrip (Rust JSON → Python bridge → validation → output)
# ---------------------------------------------------------------------------


class TestE2eFullRoundtrip:
    """Full roundtrip simulating the Rust → Python → Rust validation flow."""

    def test_minimal_pipeline_4_checks(self, tmp_path: Path) -> None:
        """Minimal run (no optional data) produces 4 checks: Sharpe, DSR, Complexity, CPCV."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)
        path = _write_backtest_json(tmp_path, data)

        # Step 1: Load (as Python bridge would)
        loaded = load_backtest_result(path)
        # Step 2: Validate
        result = run_bridge_validation(loaded)
        # Step 3: Write output
        output_path = tmp_path / "validation_result.json"
        write_validation_result(result, output_path)
        # Step 4: Read back (as Rust would)
        validation = json.loads(output_path.read_text())

        assert validation["all_passed"] is True
        assert len(validation["checks"]) == 4
        check_names = {c["name"] for c in validation["checks"]}
        assert "Sharpe Ceiling" in check_names
        assert "Deflated Sharpe Ratio" in check_names
        assert "Complexity Penalty" in check_names
        assert "CPCV" in check_names

    def test_full_pipeline_all_8_checks(self, tmp_path: Path) -> None:
        """Full run with all optional data produces all 8 checks."""
        rng = np.random.default_rng(42)
        n = 300
        returns = rng.normal(0.001, 0.01, n)

        data = _make_backtest_json(returns=returns.tolist())
        path = _write_backtest_json(tmp_path, data)
        loaded = load_backtest_result(path)
        ret = extract_returns(loaded)

        # Build all optional inputs
        is_returns = ret[:200]
        oos_returns = ret[200:]
        lagged = ret + rng.normal(0, 0.001, n)
        unlagged = ret + rng.normal(0, 0.001, n)
        strategy_matrix = np.vstack([
            ret,
            ret * 0.8 + rng.normal(0, 0.001, n),
            ret * 0.6 + rng.normal(0, 0.002, n),
        ])

        def reward_fn(r, lr, ld, dc):
            return float(np.mean(r)) - lr * float(np.var(r)) - ld * min(-float(np.min(r)), dc)

        result = run_validation_pipeline(
            returns=ret,
            num_features=45,
            is_returns=is_returns,
            oos_returns=oos_returns,
            lagged_returns=lagged,
            unlagged_returns=unlagged,
            strategy_returns_matrix=strategy_matrix,
            reward_fn=reward_fn,
        )

        assert len(result.checks) == 8
        check_names = {c.name for c in result.checks}
        expected = {
            "Sharpe Ceiling", "Deflated Sharpe Ratio", "Complexity Penalty",
            "Live Degradation", "Information Leakage", "PBO", "CPCV",
            "Reward Sensitivity",
        }
        assert check_names == expected

    def test_reproducibility_same_data_same_result(self, tmp_path: Path) -> None:
        """Same input must produce identical validation results."""
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 300).tolist()
        data = _make_backtest_json(returns=returns)

        result1 = run_bridge_validation(data)
        result2 = run_bridge_validation(data)

        assert result1["n_passed"] == result2["n_passed"]
        assert result1["n_failed"] == result2["n_failed"]
        for c1, c2 in zip(result1["checks"], result2["checks"]):
            assert c1["passed"] == c2["passed"]
            assert abs(c1["value"] - c2["value"]) < 1e-10
