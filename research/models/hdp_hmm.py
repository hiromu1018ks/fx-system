"""Hierarchical Dirichlet Process Hidden Markov Model for regime detection.

Online regime inference engine that estimates posterior regime probabilities
from market feature vectors. Designed for ONNX export to Rust-side inference.
"""

import numpy as np
from numpy.typing import NDArray


class HdpHmmParams:
    """HDP-HMM model parameters for a fixed number of regimes K."""

    def __init__(
        self,
        n_regimes: int,
        feature_dim: int,
        initial_weights: NDArray[np.float64] | None = None,
        initial_bias: NDArray[np.float64] | None = None,
        concentration_alpha: float = 1.0,
        concentration_gamma: float = 1.0,
    ):
        self.n_regimes = n_regimes
        self.feature_dim = feature_dim
        self.concentration_alpha = concentration_alpha
        self.concentration_gamma = concentration_gamma

        if initial_weights is None:
            self.weights = np.zeros((n_regimes, feature_dim), dtype=np.float64)
        else:
            self.weights = np.asarray(initial_weights, dtype=np.float64)

        if initial_bias is None:
            self.bias = np.zeros(n_regimes, dtype=np.float64)
        else:
            self.bias = np.asarray(initial_bias, dtype=np.float64)

        self.transition_matrix = np.full(
            (n_regimes, n_regimes), 1.0 / n_regimes, dtype=np.float64
        )


def compute_regime_posterior(
    features: NDArray[np.float64],
    weights: NDArray[np.float64],
    bias: NDArray[np.float64],
) -> NDArray[np.float64]:
    """Compute regime posterior probabilities using softmax over regime scores.

    Each regime k computes a score: s_k = w_k^T x + b_k
    Posterior is softmax(s).

    Args:
        features: Shape (feature_dim,) or (1, feature_dim).
        weights: Shape (n_regimes, feature_dim).
        bias: Shape (n_regimes,).

    Returns:
        Shape (n_regimes,) posterior probabilities summing to 1.
    """
    x = np.asarray(features, dtype=np.float64).ravel()
    scores = weights @ x + bias
    scores_shifted = scores - np.max(scores)
    exp_scores = np.exp(scores_shifted)
    return exp_scores / np.sum(exp_scores)


def compute_regime_entropy(posterior: NDArray[np.float64]) -> float:
    """Compute Shannon entropy H(p) = -sum(p * log(p)).

    Returns:
        Entropy in nats. Maximum is log(n_regimes) for uniform distribution.
    """
    p = np.asarray(posterior, dtype=np.float64).ravel()
    mask = p > 1e-12
    return -float(np.sum(p[mask] * np.log(p[mask])))


def compute_regime_kl_divergence(
    posterior: NDArray[np.float64],
    reference: NDArray[np.float64] | None = None,
) -> float:
    """Compute KL(p || q) = sum(p * log(p/q)).

    If reference is None, uses uniform distribution over same number of regimes.

    Args:
        posterior: Current regime posterior.
        reference: Reference distribution (uniform if None).

    Returns:
        KL divergence in nats. Higher = more different from reference.
    """
    p = np.asarray(posterior, dtype=np.float64).ravel()
    if reference is None:
        q = np.ones_like(p) / len(p)
    else:
        q = np.asarray(reference, dtype=np.float64).ravel()

    mask = (p > 1e-12) & (q > 1e-12)
    return float(np.sum(p[mask] * np.log(p[mask] / q[mask])))


def compute_drift(
    posterior: NDArray[np.float64],
    prev_drift: NDArray[np.float64],
    features: NDArray[np.float64],
    regime_ar_coeff: float = 0.9,
) -> NDArray[np.float64]:
    """Compute per-regime drift: drift_t = sum_k(pi_k * f_k(drift_{t-1}, X_t)).

    Each regime's drift evolves with AR(1) dynamics toward current features:
        drift_k = ar * prev_drift_k + (1 - ar) * X_t

    Returns per-regime drift vectors for state tracking. Aggregate with:
        drift_aggregated = posterior @ result

    Args:
        posterior: Shape (n_regimes,) regime posterior probabilities.
        prev_drift: Shape (n_regimes, feature_dim) per-regime drift vectors.
        features: Shape (feature_dim,) current feature vector.
        regime_ar_coeff: AR(1) autoregressive coefficient per regime.

    Returns:
        Shape (n_regimes, feature_dim) updated per-regime drift vectors.
    """
    p = np.asarray(posterior, dtype=np.float64).ravel()
    d = np.asarray(prev_drift, dtype=np.float64)
    x = np.asarray(features, dtype=np.float64).ravel()

    return regime_ar_coeff * d + (1.0 - regime_ar_coeff) * x[np.newaxis, :]


def aggregate_drift(
    posterior: NDArray[np.float64],
    per_regime_drift: NDArray[np.float64],
) -> NDArray[np.float64]:
    """Aggregate per-regime drifts into a single drift vector.

    drift = sum_k(posterior_k * drift_k)

    Args:
        posterior: Shape (n_regimes,) regime posterior probabilities.
        per_regime_drift: Shape (n_regimes, feature_dim) per-regime drift vectors.

    Returns:
        Shape (feature_dim,) aggregated drift vector.
    """
    p = np.asarray(posterior, dtype=np.float64).ravel()
    return p @ per_regime_drift


def initialize_hdp_hmm_params(
    feature_dim: int,
    n_regimes: int = 4,
    seed: int | None = 42,
) -> HdpHmmParams:
    """Create HDP-HMM parameters with optimistic initialization.

    Regimes are initialized with small random weights and zero bias,
    with the transition matrix set to near-uniform (sticky HMM).

    Args:
        feature_dim: Number of input features.
        n_regimes: Number of regimes K.
        seed: Random seed for reproducibility.

    Returns:
        HdpHmmParams with initialized weights.
    """
    rng = np.random.RandomState(seed)
    weights = rng.randn(n_regimes, feature_dim) * 0.01
    bias = np.zeros(n_regimes, dtype=np.float64)

    params = HdpHmmParams(
        n_regimes=n_regimes,
        feature_dim=feature_dim,
        initial_weights=weights,
        initial_bias=bias,
    )

    sticky = 0.7
    off_diag = (1.0 - sticky) / (n_regimes - 1) if n_regimes > 1 else 0.0
    for k in range(n_regimes):
        for j in range(n_regimes):
            params.transition_matrix[k, j] = sticky if k == j else off_diag

    return params


def train_hdp_hmm_online(
    params: HdpHmmParams,
    features_sequence: list[NDArray[np.float64]],
    learning_rate: float = 0.001,
) -> HdpHmmParams:
    """Simple online training via gradient ascent on the regime posterior log-likelihood.

    This is a simplified training procedure. In production, a full Gibbs sampler
    or variational inference would be used in the Python research pipeline,
    with the learned parameters exported to ONNX.

    Args:
        params: Initial HDP-HMM parameters.
        features_sequence: List of feature vectors for training.
        learning_rate: Gradient step size.

    Returns:
        Updated HdpHmmParams.
    """
    n = len(features_sequence)
    if n == 0:
        return params

    for x in features_sequence:
        posterior = compute_regime_posterior(x, params.weights, params.bias)
        winner = int(np.argmax(posterior))
        for k in range(params.n_regimes):
            gradient = (1.0 if k == winner else 0.0) - posterior[k]
            params.weights[k] += learning_rate * gradient * x
            params.bias[k] += learning_rate * gradient

    return params


def export_hdp_hmm_to_onnx(
    params: HdpHmmParams,
    opset_version: int = 17,
) -> "onnx.ModelProto":
    """Export HDP-HMM inference to ONNX for Rust-side deployment.

    Exports two graphs in a single model:
    - Input: features (1, feature_dim)
    - Output: regime_posterior (1, n_regimes)

    The graph computes: posterior = softmax(W @ x + b)

    Args:
        params: Trained HDP-HMM parameters.
        opset_version: ONNX opset version.

    Returns:
        ONNX ModelProto.
    """
    import onnx
    from onnx import TensorProto, helper

    X = helper.make_tensor_value_info(
        "features", TensorProto.FLOAT, [1, params.feature_dim]
    )
    Y = helper.make_tensor_value_info(
        "regime_posterior", TensorProto.FLOAT, [1, params.n_regimes]
    )

    nodes = []
    initializers = []

    w_init = helper.make_tensor(
        "regime_weights",
        TensorProto.FLOAT,
        [params.feature_dim, params.n_regimes],
        params.weights.T.astype(np.float32).flatten().tolist(),
    )
    initializers.append(w_init)

    b_init = helper.make_tensor(
        "regime_bias",
        TensorProto.FLOAT,
        [params.n_regimes],
        params.bias.astype(np.float32).tolist(),
    )
    initializers.append(b_init)

    matmul_node = helper.make_node(
        "MatMul", ["features", "regime_weights"], ["scores"], name="regime_matmul"
    )
    nodes.append(matmul_node)

    add_node = helper.make_node(
        "Add", ["scores", "regime_bias"], ["biased_scores"], name="regime_add_bias"
    )
    nodes.append(add_node)

    softmax_node = helper.make_node(
        "Softmax", ["biased_scores"], ["regime_posterior"], name="regime_softmax", axis=1
    )
    nodes.append(softmax_node)

    graph = helper.make_graph(
        nodes, "hdp_hmm_inference", [X], [Y], initializer=initializers
    )
    model = helper.make_model(
        graph, opset_imports=[helper.make_opsetid("", opset_version)]
    )
    model.ir_version = 8

    onnx.checker.check_model(model)
    return model
