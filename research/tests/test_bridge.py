"""Tests for the Rust-Python validation bridge."""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

import numpy as np
import pytest

# Ensure project root is on sys.path
_project_root = Path(__file__).resolve().parents[1]
if str(_project_root) not in sys.path:
    sys.path.insert(0, str(_project_root))

from research.bridge.loader import extract_num_features, extract_returns, load_backtest_result
from research.bridge.output import write_validation_result
from research.bridge.runner import run_bridge_validation


def _make_backtest_json(returns: list[float] | None = None, num_features: int = 45) -> dict:
    """Create a minimal backtest result dict for testing."""
    if returns is None:
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 100).tolist()
    return {
        "summary": {"total_pnl": sum(returns), "sharpe_ratio": 1.0, "total_trades": len(returns)},
        "trades": [{"pnl": r} for r in returns],
        "returns": returns,
        "num_features": num_features,
        "strategy_breakdown": [],
        "execution_stats": {
            "overall_fill_rate": 0.95,
            "avg_slippage": 0.01,
            "total_fills": 95,
            "total_rejections": 5,
        },
    }


class TestLoader:
    def test_load_from_file(self, tmp_path: Path) -> None:
        data = _make_backtest_json()
        path = tmp_path / "result.json"
        path.write_text(json.dumps(data))
        loaded = load_backtest_result(path)
        assert loaded["num_features"] == 45

    def test_load_missing_file_raises(self) -> None:
        with pytest.raises(FileNotFoundError):
            load_backtest_result("/nonexistent/file.json")

    def test_extract_returns_from_field(self) -> None:
        data = _make_backtest_json(returns=[1.0, -0.5, 2.0])
        returns = extract_returns(data)
        np.testing.assert_array_almost_equal(returns, [1.0, -0.5, 2.0])

    def test_extract_returns_from_trades(self) -> None:
        data = {"trades": [{"pnl": 1.0}, {"pnl": -0.5}, {"pnl": 2.0}]}
        returns = extract_returns(data)
        np.testing.assert_array_almost_equal(returns, [1.0, -0.5, 2.0])

    def test_extract_returns_no_data_returns_empty(self) -> None:
        returns = extract_returns({"trades": []})
        assert len(returns) == 0

    def test_extract_num_features(self) -> None:
        assert extract_num_features({"num_features": 30}) == 30
        assert extract_num_features({}) == 45


class TestRunner:
    def test_run_bridge_validation_passes(self) -> None:
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 200).tolist()
        data = _make_backtest_json(returns=returns)
        result = run_bridge_validation(data)
        assert "all_passed" in result
        assert "checks" in result
        assert len(result["checks"]) >= 3  # At least Sharpe, DSR, Complexity, CPCV

    def test_run_bridge_insufficient_data(self) -> None:
        data = _make_backtest_json(returns=[0.5])
        result = run_bridge_validation(data)
        assert result["all_passed"] is False
        assert result["checks"][0]["name"] == "Data Sufficiency"

    def test_run_bridge_empty_returns(self) -> None:
        data = _make_backtest_json(returns=[])
        result = run_bridge_validation(data)
        assert result["all_passed"] is False

    def test_checks_structure(self) -> None:
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 200).tolist()
        data = _make_backtest_json(returns=returns)
        result = run_bridge_validation(data)
        for check in result["checks"]:
            assert "name" in check
            assert "passed" in check
            assert "details" in check
            assert "value" in check
            assert isinstance(check["passed"], bool)
            assert isinstance(check["value"], float)


class TestOutput:
    def test_write_validation_result(self, tmp_path: Path) -> None:
        result = {
            "all_passed": True,
            "n_passed": 2,
            "n_failed": 0,
            "checks": [
                {"name": "A", "passed": True, "details": "ok", "value": 1.0},
                {"name": "B", "passed": True, "details": "ok", "value": 0.5},
            ],
        }
        path = tmp_path / "subdir" / "result.json"
        written = write_validation_result(result, path)
        assert written.exists()
        loaded = json.loads(path.read_text())
        assert loaded["all_passed"] is True
        assert len(loaded["checks"]) == 2


class TestEndToEnd:
    def test_full_bridge_roundtrip(self, tmp_path: Path) -> None:
        """Simulate the full Rust -> Python -> Rust data flow."""
        # 1. Create backtest result JSON (as Rust would)
        rng = np.random.default_rng(42)
        returns = rng.normal(0.001, 0.01, 200).tolist()
        data = _make_backtest_json(returns=returns)
        input_path = tmp_path / "backtest_result.json"
        input_path.write_text(json.dumps(data))

        # 2. Load and validate (as Python bridge would)
        loaded = load_backtest_result(input_path)
        result = run_bridge_validation(loaded)

        # 3. Write validation result (as Python bridge would)
        output_path = tmp_path / "validation_result.json"
        write_validation_result(result, output_path)

        # 4. Read back validation result (as Rust would)
        validation = json.loads(output_path.read_text())
        assert "all_passed" in validation
        assert "n_passed" in validation
        assert "n_failed" in validation
        assert "checks" in validation
        for check in validation["checks"]:
            assert isinstance(check["name"], str)
            assert isinstance(check["passed"], bool)
