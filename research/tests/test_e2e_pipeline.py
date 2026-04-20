"""E2E test: HDP-HMM training → ONNX export → verification."""

import json
import os
import tempfile

import numpy as np
import pytest

from research.models.hdp_hmm import (
    compute_regime_posterior,
    export_hdp_hmm_to_onnx,
    initialize_hdp_hmm_params,
    train_hdp_hmm_online,
)


@pytest.fixture
def trained_model_and_data():
    """Train HDP-HMM on synthetic data and export to ONNX."""
    rng = np.random.RandomState(42)
    n_regimes = 4
    feature_dim = 36
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
        params = initialize_hdp_hmm_params(feature_dim=36, n_regimes=4, seed=42)
        original_weights = params.weights.copy()
        rng = np.random.RandomState(42)
        features_seq = [rng.randn(36) for _ in range(200)]
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
            np.testing.assert_allclose(np.sum(result), 1.0)
        finally:
            os.unlink(path)

    def test_export_with_correct_input_output_shapes(self, trained_model_and_data):
        model, _, _, trained = trained_model_and_data

        assert len(model.graph.input) == 1
        assert model.graph.input[0].type.tensor_type.shape.dim[0].dim_value == 1
        assert model.graph.input[0].type.tensor_type.shape.dim[1].dim_value == 36

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
                x = rng.randn(36).astype(np.float32).reshape(1, -1)
                result = sess.run(None, {"features": x})[0].flatten()
                np.testing.assert_allclose(np.sum(result), 1.0, atol=1e-6)
                assert np.all(result >= 0.0)
        finally:
            os.unlink(path)

    def test_generate_model_script_creates_valid_files(self):
        """Verify the generate_regime_model.py script produces correct output."""
        import importlib
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

            assert meta["feature_dim"] == 36
            assert meta["n_regimes"] == 4
            assert len(meta["test_features"]) == 36
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
