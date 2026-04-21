"""Generate a synthetic v1 regime model and export to ONNX.

Usage:
    python -m research.models.generate_regime_model [--output PATH]

Creates research/models/onnx/regime_v1.onnx by default.
"""

import argparse
from pathlib import Path

from research.models.train_regime import (
    make_synthetic_feature_dataset,
    train_regime_model,
    write_regime_artifacts,
)


def main():
    parser = argparse.ArgumentParser(description="Generate ONNX regime model")
    parser.add_argument(
        "--output",
        default=str(Path(__file__).resolve().parent / 'onnx' / 'regime_v1.onnx'),
        help="Output ONNX model path",
    )
    parser.add_argument('--n-samples', type=int, default=500, help='Synthetic sample count')
    parser.add_argument('--seed', type=int, default=42, help='Random seed')
    args = parser.parse_args()

    dataset = make_synthetic_feature_dataset(args.n_samples, seed=args.seed)
    model, metadata = train_regime_model(dataset, seed=args.seed)
    model_path, meta_path = write_regime_artifacts(model, metadata, args.output)
    print(f"Model saved to {model_path}")
    print(f"Metadata saved to {meta_path}")
    return model_path, meta_path


if __name__ == "__main__":
    main()
