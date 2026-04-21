"""Preprocessing helpers for dumped regime-model features."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pandas as pd

from .loader import FEATURE_COLUMNS, FEATURE_VERSION, FeatureDataset

TIME_SINCE_LAST_SPIKE_CAP_MS = 86_400_000.0


@dataclass(frozen=True)
class FeatureStandardizationStats:
    """Per-feature normalization statistics for a fixed schema version."""

    feature_version: str
    means: dict[str, float]
    scales: dict[str, float]

    def to_dict(self) -> dict[str, object]:
        return {
            "feature_version": self.feature_version,
            "means": self.means,
            "scales": self.scales,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, object]) -> "FeatureStandardizationStats":
        return cls(
            feature_version=str(payload["feature_version"]),
            means={k: float(v) for k, v in dict(payload["means"]).items()},
            scales={k: float(v) for k, v in dict(payload["scales"]).items()},
        )


def sanitize_feature_frame(frame: pd.DataFrame) -> pd.DataFrame:
    """Apply the finite-value contract used by regime-model training."""
    sanitized = frame.loc[:, FEATURE_COLUMNS].astype(np.float64).copy()
    spike_time = sanitized["time_since_last_spike_ms"].copy()
    spike_time = spike_time.mask(~np.isfinite(spike_time), TIME_SINCE_LAST_SPIKE_CAP_MS)
    spike_time = spike_time.clip(lower=0.0, upper=TIME_SINCE_LAST_SPIKE_CAP_MS)

    sanitized = sanitized.replace([np.inf, -np.inf], np.nan)
    sanitized = sanitized.fillna(0.0)

    for column in ("time_since_open_ms", "holding_time_ms"):
        sanitized[column] = sanitized[column].clip(lower=0.0)

    sanitized["time_since_last_spike_ms"] = spike_time

    return sanitized


def split_train_validation(
    dataset: FeatureDataset,
    *,
    validation_fraction: float = 0.2,
) -> tuple[FeatureDataset, FeatureDataset]:
    """Split a feature dataset into deterministic train/validation partitions."""
    if not 0.0 < validation_fraction < 1.0:
        raise ValueError(
            f"validation_fraction must be in (0, 1), got {validation_fraction}"
        )

    n_rows = len(dataset.frame)
    if n_rows < 2:
        raise ValueError("Need at least two rows to split train/validation data")

    split_at = int(np.floor(n_rows * (1.0 - validation_fraction)))
    split_at = min(max(split_at, 1), n_rows - 1)

    train = dataset.frame.iloc[:split_at].reset_index(drop=True)
    validation = dataset.frame.iloc[split_at:].reset_index(drop=True)
    return (
        FeatureDataset(frame=train, feature_version=dataset.feature_version),
        FeatureDataset(frame=validation, feature_version=dataset.feature_version),
    )


def fit_standardization_stats(
    frame: pd.DataFrame,
    *,
    feature_version: str = FEATURE_VERSION,
) -> FeatureStandardizationStats:
    """Fit per-feature mean/std statistics on sanitized features."""
    sanitized = sanitize_feature_frame(frame)
    means = {
        column: float(sanitized[column].mean())
        for column in FEATURE_COLUMNS
    }
    scales = {
        column: max(float(sanitized[column].std(ddof=0)), 1.0)
        for column in FEATURE_COLUMNS
    }
    return FeatureStandardizationStats(
        feature_version=feature_version,
        means=means,
        scales=scales,
    )


def apply_standardization(
    frame: pd.DataFrame,
    stats: FeatureStandardizationStats,
) -> pd.DataFrame:
    """Apply fitted normalization statistics to sanitized features."""
    sanitized = sanitize_feature_frame(frame)
    if stats.feature_version != FEATURE_VERSION:
        raise ValueError(
            f"Stats feature_version {stats.feature_version!r} != {FEATURE_VERSION!r}"
        )

    scaled = sanitized.copy()
    for column in FEATURE_COLUMNS:
        scaled[column] = (scaled[column] - stats.means[column]) / stats.scales[column]
    return scaled


def write_standardization_stats(
    stats: FeatureStandardizationStats,
    path: str | Path,
) -> Path:
    """Persist normalization statistics as JSON."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(stats.to_dict(), indent=2))
    return path


def load_standardization_stats(path: str | Path) -> FeatureStandardizationStats:
    """Load normalization statistics from JSON."""
    payload = json.loads(Path(path).read_text())
    return FeatureStandardizationStats.from_dict(payload)
