"""Tests for HDP-HMM regime inference engine."""

import numpy as np
import pytest
from research.models.hdp_hmm import (
    HdpHmmParams,
    compute_drift,
    compute_regime_entropy,
    compute_regime_kl_divergence,
    compute_regime_posterior,
    export_hdp_hmm_to_onnx,
    initialize_hdp_hmm_params,
    train_hdp_hmm_online,
)


@pytest.fixture
def simple_params():
    return HdpHmmParams(n_regimes=3, feature_dim=4)


@pytest.fixture
def features():
    return np.array([0.1, -0.2, 0.3, 0.0])


class TestHdpHmmParams:
    def test_default_creation(self):
        params = HdpHmmParams(n_regimes=3, feature_dim=4)
        assert params.n_regimes == 3
        assert params.feature_dim == 4
        assert params.weights.shape == (3, 4)
        assert params.bias.shape == (3,)
        assert params.transition_matrix.shape == (3, 3)

    def test_custom_weights(self):
        w = np.ones((2, 3))
        b = np.array([1.0, 2.0])
        params = HdpHmmParams(n_regimes=2, feature_dim=3, initial_weights=w, initial_bias=b)
        np.testing.assert_array_equal(params.weights, w)
        np.testing.assert_array_equal(params.bias, b)

    def test_transition_matrix_uniform(self):
        params = HdpHmmParams(n_regimes=3, feature_dim=2)
        expected = np.full((3, 3), 1.0 / 3)
        np.testing.assert_allclose(params.transition_matrix, expected)


class TestComputeRegimePosterior:
    def test_output_sums_to_one(self, simple_params, features):
        posterior = compute_regime_posterior(features, simple_params.weights, simple_params.bias)
        np.testing.assert_allclose(np.sum(posterior), 1.0)

    def test_output_non_negative(self, simple_params, features):
        posterior = compute_regime_posterior(features, simple_params.weights, simple_params.bias)
        assert np.all(posterior >= 0)

    def test_shape(self, simple_params, features):
        posterior = compute_regime_posterior(features, simple_params.weights, simple_params.bias)
        assert posterior.shape == (3,)

    def test_dominant_regime(self):
        w = np.zeros((2, 3))
        w[0] = [10.0, 0.0, 0.0]
        w[1] = [0.0, 0.0, 0.0]
        b = np.array([0.0, -100.0])
        x = np.array([1.0, 0.0, 0.0])
        posterior = compute_regime_posterior(x, w, b)
        assert posterior[0] > 0.99

    def test_equal_weights_give_uniform(self):
        w = np.zeros((2, 1))
        b = np.array([0.0, 0.0])
        x = np.array([0.0])
        posterior = compute_regime_posterior(x, w, b)
        np.testing.assert_allclose(posterior, [0.5, 0.5])

    def test_2d_input(self, simple_params):
        x_2d = np.array([[0.1, -0.2, 0.3, 0.0]])
        posterior = compute_regime_posterior(x_2d, simple_params.weights, simple_params.bias)
        assert posterior.shape == (3,)


class TestComputeRegimeEntropy:
    def test_uniform_max_entropy(self):
        p = np.array([0.25, 0.25, 0.25, 0.25])
        entropy = compute_regime_entropy(p)
        np.testing.assert_allclose(entropy, np.log(4))

    def test_deterministic_zero_entropy(self):
        p = np.array([1.0, 0.0, 0.0, 0.0])
        entropy = compute_regime_entropy(p)
        np.testing.assert_allclose(entropy, 0.0)

    def test_skewed_entropy(self):
        p = np.array([0.9, 0.1])
        entropy = compute_regime_entropy(p)
        assert 0.0 < entropy < np.log(2)

    def test_single_regime(self):
        p = np.array([1.0])
        entropy = compute_regime_entropy(p)
        np.testing.assert_allclose(entropy, 0.0)


class TestComputeRegimeKlDivergence:
    def test_identical_distributions(self):
        p = np.array([0.5, 0.3, 0.2])
        kl = compute_regime_kl_divergence(p, p)
        np.testing.assert_allclose(kl, 0.0)

    def test_kl_from_uniform(self):
        p = np.array([0.9, 0.05, 0.05])
        kl = compute_regime_kl_divergence(p)
        assert kl > 0.0

    def test_kl_with_reference(self):
        p = np.array([0.8, 0.1, 0.1])
        q = np.array([0.33, 0.33, 0.34])
        kl = compute_regime_kl_divergence(p, q)
        assert kl > 0.0

    def test_kl_non_negative(self):
        rng = np.random.RandomState(42)
        for _ in range(10):
            p = rng.dirichlet(np.ones(4))
            q = rng.dirichlet(np.ones(4))
            kl = compute_regime_kl_divergence(p, q)
            assert kl >= 0.0


class TestComputeDrift:
    def test_zero_prev_drift(self):
        posterior = np.array([0.5, 0.5])
        prev_drift = np.zeros(4)
        features = np.ones(4)
        drift = compute_drift(posterior, prev_drift, features)
        np.testing.assert_allclose(drift, np.zeros(4))

    def test_decay(self):
        posterior = np.array([1.0, 0.0])
        prev_drift = np.ones(4)
        features = np.zeros(4)
        drift = compute_drift(posterior, prev_drift, features, regime_ar_coeff=0.9)
        np.testing.assert_allclose(drift, 0.9 * np.ones(4))

    def test_weighted_drift(self):
        posterior = np.array([0.3, 0.7])
        prev_drift = np.array([1.0, 0.0])
        features = np.zeros(2)
        drift = compute_drift(posterior, prev_drift, features, regime_ar_coeff=1.0)
        expected = 0.3 * 1.0 + 0.7 * 0.0
        np.testing.assert_allclose(drift, np.array([expected, 0.0]))

    def test_shape(self):
        posterior = np.array([0.5, 0.5])
        prev_drift = np.ones(10)
        features = np.ones(10)
        drift = compute_drift(posterior, prev_drift, features)
        assert drift.shape == (10,)


class TestInitializeHdpHmmParams:
    def test_sticky_transition_matrix(self):
        params = initialize_hdp_hmm_params(feature_dim=4, n_regimes=3, seed=42)
        for k in range(3):
            assert params.transition_matrix[k, k] > 1.0 / 3
            for j in range(3):
                if j != k:
                    assert params.transition_matrix[k, j] < 1.0 / 3

    def test_rows_sum_to_one(self):
        params = initialize_hdp_hmm_params(feature_dim=4, n_regimes=4, seed=42)
        for k in range(4):
            np.testing.assert_allclose(np.sum(params.transition_matrix[k]), 1.0)

    def test_seed_reproducibility(self):
        p1 = initialize_hdp_hmm_params(feature_dim=4, n_regimes=3, seed=42)
        p2 = initialize_hdp_hmm_params(feature_dim=4, n_regimes=3, seed=42)
        np.testing.assert_array_equal(p1.weights, p2.weights)


class TestTrainHdpHmmOnline:
    def test_empty_sequence(self):
        params = HdpHmmParams(n_regimes=2, feature_dim=3)
        result = train_hdp_hmm_online(params, [])
        np.testing.assert_array_equal(result.weights, params.weights)

    def test_weights_change_after_training(self):
        params = HdpHmmParams(n_regimes=2, feature_dim=3)
        original_weights = params.weights.copy()
        features_seq = [np.random.randn(3) for _ in range(10)]
        result = train_hdp_hmm_online(params, features_seq, learning_rate=0.1)
        assert not np.allclose(result.weights, original_weights)

    def test_convergence_to_dominant_regime(self):
        params = HdpHmmParams(n_regimes=2, feature_dim=3)
        x = np.array([1.0, 0.0, 0.0])
        features_seq = [x] * 100
        result = train_hdp_hmm_online(params, features_seq, learning_rate=0.01)
        posterior = compute_regime_posterior(x, result.weights, result.bias)
        assert np.max(posterior) > 0.6


class TestExportHdpHmmToOnnx:
    def test_export_creates_valid_model(self, simple_params):
        model = export_hdp_hmm_to_onnx(simple_params)
        assert model is not None
        assert model.graph is not None

    def test_export_input_output_names(self, simple_params):
        import onnx

        model = export_hdp_hmm_to_onnx(simple_params)
        input_names = [inp.name for inp in model.graph.input]
        output_names = [out.name for out in model.graph.output]
        assert "features" in input_names
        assert "regime_posterior" in output_names

    def test_export_consistent_with_python(self, simple_params, features):
        import onnxruntime as ort

        model = export_hdp_hmm_to_onnx(simple_params)
        import tempfile, os
        with tempfile.NamedTemporaryFile(suffix=".onnx", delete=False) as f:
            path = f.name
        try:
            onnx.save(model, path)
            sess = ort.InferenceSession(path)
            x = features.astype(np.float32).reshape(1, -1)
            result = sess.run(None, {"features": x})[0].flatten()
            expected = compute_regime_posterior(features, simple_params.weights, simple_params.bias)
            np.testing.assert_allclose(result, expected, atol=1e-5)
        finally:
            os.unlink(path)
