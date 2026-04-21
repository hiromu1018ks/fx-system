"""Train a v1 regime classifier from dumped backtest features and export ONNX."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import TYPE_CHECKING

import numpy as np
import pandas as pd

from research.features.loader import (
    FEATURE_COLUMNS,
    FEATURE_VERSION,
    FeatureDataset,
    load_feature_csv,
)
from research.features.preprocess import (
    apply_standardization,
    fit_standardization_stats,
    sanitize_feature_frame,
    split_train_validation,
)
from research.models.hdp_hmm import (
    compute_regime_posterior,
    export_hdp_hmm_to_onnx,
    initialize_hdp_hmm_params,
    train_hdp_hmm_online,
)
from research.models.onnx_export import save_model

if TYPE_CHECKING:
    import onnx


def make_synthetic_feature_dataset(
    n_samples: int = 500,
    *,
    seed: int = 42,
) -> FeatureDataset:
    """Build a synthetic dataset with the same schema as dumped backtest features."""
    rng = np.random.RandomState(seed)
    data: dict[str, object] = {
        'timestamp_ns': np.arange(n_samples, dtype=np.int64),
        'source_strategy': ['A'] * n_samples,
        'feature_version': [FEATURE_VERSION] * n_samples,
    }
    for column in FEATURE_COLUMNS:
        if column in {'time_since_open_ms', 'time_since_last_spike_ms', 'holding_time_ms'}:
            data[column] = rng.uniform(0.0, 10_000.0, size=n_samples)
        elif column in {'session_tokyo', 'session_london', 'session_ny', 'session_sydney'}:
            data[column] = rng.randint(0, 2, size=n_samples).astype(np.float64)
        elif column in {'p_revert', 'p_continue', 'p_trend'}:
            data[column] = rng.uniform(0.0, 1.0, size=n_samples)
        else:
            data[column] = rng.randn(n_samples)
    frame = pd.DataFrame(data)
    ordered = frame.loc[:, ['timestamp_ns', 'source_strategy', 'feature_version', *FEATURE_COLUMNS]]
    return FeatureDataset(frame=ordered, feature_version=FEATURE_VERSION)


def train_regime_model(
    dataset: FeatureDataset,
    *,
    n_regimes: int = 4,
    validation_fraction: float = 0.2,
    learning_rate: float = 0.01,
    seed: int = 42,
) -> tuple['onnx.ModelProto', dict[str, object]]:
    """Train the v1 regime classifier and return export metadata."""
    train_dataset, validation_dataset = split_train_validation(
        dataset,
        validation_fraction=validation_fraction,
    )
    stats = fit_standardization_stats(train_dataset.feature_frame)
    train_features = apply_standardization(train_dataset.feature_frame, stats)
    train_sequence = [row for row in train_features.to_numpy(dtype=np.float64)]

    params = initialize_hdp_hmm_params(
        feature_dim=len(FEATURE_COLUMNS),
        n_regimes=n_regimes,
        seed=seed,
    )
    trained = train_hdp_hmm_online(
        params,
        train_sequence,
        learning_rate=learning_rate,
    )

    model = export_hdp_hmm_to_onnx(
        trained,
        feature_means=np.array(
            [stats.means[column] for column in FEATURE_COLUMNS],
            dtype=np.float64,
        ),
        feature_scales=np.array(
            [stats.scales[column] for column in FEATURE_COLUMNS],
            dtype=np.float64,
        ),
    )

    validation_raw = sanitize_feature_frame(validation_dataset.feature_frame)
    if validation_raw.empty:
        validation_raw = sanitize_feature_frame(train_dataset.feature_frame.iloc[[-1]])
    sample_raw = validation_raw.iloc[0].to_numpy(dtype=np.float64)
    sample_standardized = apply_standardization(
        pd.DataFrame([sample_raw], columns=FEATURE_COLUMNS),
        stats,
    ).iloc[0].to_numpy(dtype=np.float64)
    expected_posterior = compute_regime_posterior(
        sample_standardized,
        trained.weights,
        trained.bias,
    )

    metadata: dict[str, object] = {
        'feature_version': FEATURE_VERSION,
        'feature_dim': len(FEATURE_COLUMNS),
        'n_regimes': n_regimes,
        'feature_columns': list(FEATURE_COLUMNS),
        'standardization_means': stats.means,
        'standardization_scales': stats.scales,
        'train_rows': len(train_dataset.frame),
        'validation_rows': len(validation_dataset.frame),
        'test_features': sample_raw.tolist(),
        'expected_posterior': expected_posterior.tolist(),
    }
    return model, metadata


def write_regime_artifacts(
    model: 'onnx.ModelProto',
    metadata: dict[str, object],
    output_model_path: str | Path,
) -> tuple[Path, Path]:
    """Write ONNX model and sidecar metadata JSON."""
    output_model_path = Path(output_model_path)
    model_path = save_model(model, output_model_path)
    meta_path = model_path.with_name(f'{model_path.stem}_meta.json')
    meta_path.write_text(json.dumps(metadata, indent=2))
    return model_path, meta_path


def main() -> tuple[Path, Path]:
    parser = argparse.ArgumentParser(description='Train a regime classifier from dumped features')
    parser.add_argument('--features', required=True, help='Path to dumped feature CSV')
    parser.add_argument('--output', required=True, help='Output directory for exported artifacts')
    parser.add_argument('--n-regimes', type=int, default=4, help='Number of regimes to model')
    parser.add_argument(
        '--validation-fraction',
        type=float,
        default=0.2,
        help='Fraction of rows reserved for validation',
    )
    parser.add_argument(
        '--learning-rate',
        type=float,
        default=0.01,
        help='Learning rate for the online trainer',
    )
    parser.add_argument('--seed', type=int, default=42, help='Random seed')
    args = parser.parse_args()

    dataset = load_feature_csv(args.features)
    model, metadata = train_regime_model(
        dataset,
        n_regimes=args.n_regimes,
        validation_fraction=args.validation_fraction,
        learning_rate=args.learning_rate,
        seed=args.seed,
    )
    output_dir = Path(args.output)
    model_path, meta_path = write_regime_artifacts(
        model,
        metadata,
        output_dir / 'regime_v1.onnx',
    )
    print(f'Model saved to {model_path}')
    print(f'Metadata saved to {meta_path}')
    return model_path, meta_path


if __name__ == '__main__':
    main()
