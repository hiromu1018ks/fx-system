"""Generate a trained HDP-HMM regime model and export to ONNX.

Usage:
    python -m research.models.generate_regime_model [--output PATH]

Creates research/models/onnx/regime_v1.onnx by default.
"""

import argparse
import json
import os
import sys

import numpy as np


def generate_model(
    n_regimes: int = 4,
    feature_dim: int = 36,
    n_samples: int = 500,
    learning_rate: float = 0.01,
    seed: int = 42,
):
    """Train HDP-HMM on synthetic data and export to ONNX."""
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

    from research.models.hdp_hmm import (
        HdpHmmParams,
        compute_regime_posterior,
        export_hdp_hmm_to_onnx,
        initialize_hdp_hmm_params,
        train_hdp_hmm_online,
    )

    rng = np.random.RandomState(seed)

    params = initialize_hdp_hmm_params(
        feature_dim=feature_dim, n_regimes=n_regimes, seed=seed
    )

    features_seq = [rng.randn(feature_dim) for _ in range(n_samples)]
    trained = train_hdp_hmm_online(
        params, features_seq, learning_rate=learning_rate
    )

    model = export_hdp_hmm_to_onnx(trained)

    test_features = rng.randn(feature_dim)
    expected_posterior = compute_regime_posterior(
        test_features, trained.weights, trained.bias
    )

    return model, test_features, expected_posterior.tolist()


def main():
    parser = argparse.ArgumentParser(description="Generate ONNX regime model")
    parser.add_argument(
        "--output",
        default=os.path.join(
            os.path.dirname(__file__), "onnx", "regime_v1.onnx"
        ),
        help="Output ONNX model path",
    )
    args = parser.parse_args()

    import onnx

    model, test_features, expected_posterior = generate_model()

    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    onnx.save(model, args.output)
    print(f"Model saved to {args.output}")

    meta_path = os.path.join(
        os.path.dirname(args.output), "regime_v1_meta.json"
    )
    meta = {
        "test_features": test_features.tolist(),
        "expected_posterior": expected_posterior,
        "feature_dim": 36,
        "n_regimes": 4,
    }
    with open(meta_path, "w") as f:
        json.dump(meta, f, indent=2)
    print(f"Metadata saved to {meta_path}")

    return args.output, meta_path


if __name__ == "__main__":
    main()
