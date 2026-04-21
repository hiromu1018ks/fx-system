"""E2E test: HDP-HMM training → ONNX export → verification."""

import json
import os
from pathlib import Path
import tempfile

import numpy as np
import pandas as pd
import pytest

from research.features.loader import FEATURE_COLUMNS, FEATURE_VERSION
from research.models.hdp_hmm import (
    compute_regime_posterior,
    export_hdp_hmm_to_onnx,
    initialize_hdp_hmm_params,
    train_hdp_hmm_online,
)

FEATURE_DIM = 38


@pytest.fixture
def trained_model_and_data():
    """Train HDP-HMM on synthetic data and export to ONNX."""
    rng = np.random.RandomState(42)
    n_regimes = 4
    feature_dim = FEATURE_DIM
    n_samples = 500

    params = initialize_hdp_hmm_params(
        feature_dim=feature_dim, n_regimes=n_regimes, seed=42
    )

    features_seq = [rng.randn(feature_dim) for _ in range(n_samples)]
    trained = train_hdp_hmm_online(
        params, features_seq, learning_rate=0.01
    )

    model = export_hdp_hmm_to_onnx(trained)

    test_features = rng.randn(feature_dim)
    expected_posterior = compute_regime_posterior(
        test_features, trained.weights, trained.bias
    )

    return model, test_features, expected_posterior, trained


class TestE2EPipeline:
    def test_train_produces_different_weights(self):
        params = initialize_hdp_hmm_params(feature_dim=FEATURE_DIM, n_regimes=4, seed=42)
        original_weights = params.weights.copy()
        rng = np.random.RandomState(42)
        features_seq = [rng.randn(FEATURE_DIM) for _ in range(200)]
        trained = train_hdp_hmm_online(params, features_seq, learning_rate=0.01)

        assert not np.allclose(trained.weights, original_weights)

    def test_export_matches_python_inference(self, trained_model_and_data):
        import onnx
        import onnxruntime as ort

        model, test_features, expected_posterior, trained = trained_model_and_data

        with tempfile.NamedTemporaryFile(suffix=".onnx", delete=False) as f:
            path = f.name
        try:
            onnx.save(model, path)
            sess = ort.InferenceSession(path)
            x = test_features.astype(np.float32).reshape(1, -1)
            result = sess.run(None, {"features": x})[0].flatten()

            assert result.shape == (4,)
            np.testing.assert_allclose(result, expected_posterior, atol=1e-5)
            assert np.all(result >= 0.0)
            np.testing.assert_allclose(np.sum(result), 1.0, atol=1e-6)
        finally:
            os.unlink(path)

    def test_export_with_correct_input_output_shapes(self, trained_model_and_data):
        model, _, _, trained = trained_model_and_data

        assert len(model.graph.input) == 1
        assert model.graph.input[0].type.tensor_type.shape.dim[0].dim_value == 1
        assert model.graph.input[0].type.tensor_type.shape.dim[1].dim_value == FEATURE_DIM

        assert len(model.graph.output) == 1
        assert model.graph.output[0].type.tensor_type.shape.dim[0].dim_value == 1
        assert model.graph.output[0].type.tensor_type.shape.dim[1].dim_value == 4

    def test_posterior_sums_to_one_for_multiple_inputs(self, trained_model_and_data):
        import onnx
        import onnxruntime as ort

        model, _, _, _ = trained_model_and_data
        rng = np.random.RandomState(123)

        with tempfile.NamedTemporaryFile(suffix=".onnx", delete=False) as f:
            path = f.name
        try:
            onnx.save(model, path)
            sess = ort.InferenceSession(path)

            for _ in range(10):
                x = rng.randn(FEATURE_DIM).astype(np.float32).reshape(1, -1)
                result = sess.run(None, {"features": x})[0].flatten()
                np.testing.assert_allclose(np.sum(result), 1.0, atol=1e-6)
                assert np.all(result >= 0.0)
        finally:
            os.unlink(path)

    def test_generate_model_script_creates_valid_files(self):
        """Verify the generate_regime_model.py script produces correct output."""
        import subprocess

        script_path = os.path.join(
            os.path.dirname(__file__), "..", "models", "generate_regime_model.py"
        )
        script_path = os.path.abspath(script_path)

        with tempfile.TemporaryDirectory() as tmpdir:
            output_path = os.path.join(tmpdir, "regime_v1.onnx")
            env = os.environ.copy()
            env["PYTHONPATH"] = os.path.abspath(
                os.path.join(os.path.dirname(__file__), "..", "..")
            )
            result = subprocess.run(
                ["python", script_path, "--output", output_path],
                capture_output=True,
                text=True,
                env=env,
            )
            assert result.returncode == 0, f"Script failed: {result.stderr}"

            assert os.path.exists(output_path)

            meta_path = os.path.join(tmpdir, "regime_v1_meta.json")
            assert os.path.exists(meta_path)

            with open(meta_path) as f:
                meta = json.load(f)

            assert meta["feature_dim"] == FEATURE_DIM
            assert meta["n_regimes"] == 4
            assert len(meta["test_features"]) == FEATURE_DIM
            assert len(meta["expected_posterior"]) == 4
            np.testing.assert_allclose(
                np.sum(meta["expected_posterior"]), 1.0, atol=1e-6
            )

            import onnxruntime as ort

            sess = ort.InferenceSession(output_path)
            x = np.array(meta["test_features"], dtype=np.float32).reshape(1, -1)
            result = sess.run(None, {"features": x})[0].flatten()
            np.testing.assert_allclose(
                result, meta["expected_posterior"], atol=1e-5
            )

    def test_train_regime_script_creates_model_from_feature_csv(self, tmp_path):
        import subprocess

        script_path = Path(__file__).resolve().parents[1] / "models" / "train_regime.py"
        features_path = tmp_path / "features.csv"
        output_dir = tmp_path / "artifacts"

        frame = pd.DataFrame(
            {
                "timestamp_ns": [1_000 + i for i in range(16)],
                "source_strategy": ["A"] * 16,
                "feature_version": [FEATURE_VERSION] * 16,
                **{
                    column: np.linspace(idx, idx + 1.5, 16)
                    for idx, column in enumerate(FEATURE_COLUMNS)
                },
            }
        )
        frame.to_csv(features_path, index=False)

        env = os.environ.copy()
        env["PYTHONPATH"] = str(Path(__file__).resolve().parents[2])
        result = subprocess.run(
            [
                "python",
                str(script_path),
                "--features",
                str(features_path),
                "--output",
                str(output_dir),
            ],
            capture_output=True,
            text=True,
            env=env,
        )
        assert result.returncode == 0, f"Script failed: {result.stderr}"

        model_path = output_dir / "regime_v1.onnx"
        meta_path = output_dir / "regime_v1_meta.json"
        assert model_path.exists()
        assert meta_path.exists()

        meta = json.loads(meta_path.read_text())
        assert meta["feature_version"] == FEATURE_VERSION
        assert meta["feature_dim"] == FEATURE_DIM
        assert len(meta["feature_columns"]) == FEATURE_DIM
        assert meta["train_rows"] > 0
        assert meta["validation_rows"] > 0

        import onnxruntime as ort

        sess = ort.InferenceSession(str(model_path))
        x = np.array(meta["test_features"], dtype=np.float32).reshape(1, -1)
        result = sess.run(None, {"features": x})[0].flatten()
        np.testing.assert_allclose(result, meta["expected_posterior"], atol=1e-5)
