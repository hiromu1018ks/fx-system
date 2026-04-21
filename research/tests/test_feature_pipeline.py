"""Tests for dumped feature loading and preprocessing."""

from __future__ import annotations

import pandas as pd
import pytest

from research.features.loader import (
    EXPECTED_COLUMNS,
    FEATURE_COLUMNS,
    FEATURE_VERSION,
    load_feature_csv,
)
from research.features.preprocess import (
    TIME_SINCE_LAST_SPIKE_CAP_MS,
    apply_standardization,
    fit_standardization_stats,
    load_standardization_stats,
    sanitize_feature_frame,
    split_train_validation,
    write_standardization_stats,
)


def make_feature_frame(rows: int = 4) -> pd.DataFrame:
    data = {
        "timestamp_ns": [1_000 + i for i in range(rows)],
        "source_strategy": ["A"] * rows,
        "feature_version": [FEATURE_VERSION] * rows,
    }
    for idx, column in enumerate(FEATURE_COLUMNS):
        data[column] = [float(idx + row) for row in range(rows)]
    return pd.DataFrame(data, columns=EXPECTED_COLUMNS)


def test_load_feature_csv_validates_schema_and_preserves_row_order(tmp_path):
    frame = make_feature_frame(rows=3)
    frame["timestamp_ns"] = [300, 100, 200]
    path = tmp_path / "features.csv"
    frame.to_csv(path, index=False)

    dataset = load_feature_csv(path)
    assert dataset.feature_version == FEATURE_VERSION
    assert dataset.frame["timestamp_ns"].tolist() == [300, 100, 200]
    assert list(dataset.feature_frame.columns) == list(FEATURE_COLUMNS)


def test_load_feature_csv_rejects_missing_columns(tmp_path):
    frame = make_feature_frame(rows=2).drop(columns=["spread"])
    path = tmp_path / "features.csv"
    frame.to_csv(path, index=False)

    with pytest.raises(ValueError, match="schema mismatch"):
        load_feature_csv(path)


def test_load_feature_csv_rejects_unexpected_version(tmp_path):
    frame = make_feature_frame(rows=2)
    frame["feature_version"] = ["bad_schema", "bad_schema"]
    path = tmp_path / "features.csv"
    frame.to_csv(path, index=False)

    with pytest.raises(ValueError, match="Unexpected feature_version"):
        load_feature_csv(path)


def test_sanitize_feature_frame_handles_non_finite_and_caps_time_fields():
    frame = make_feature_frame(rows=2).loc[:, FEATURE_COLUMNS].copy()
    frame.loc[0, "spread"] = float("nan")
    frame.loc[0, "time_since_open_ms"] = -1.0
    frame.loc[0, "time_since_last_spike_ms"] = float("inf")
    frame.loc[0, "holding_time_ms"] = -5.0
    frame.loc[0, "entry_price"] = float("-inf")

    sanitized = sanitize_feature_frame(frame)
    assert sanitized.loc[0, "spread"] == 0.0
    assert sanitized.loc[0, "time_since_open_ms"] == 0.0
    assert sanitized.loc[0, "time_since_last_spike_ms"] == TIME_SINCE_LAST_SPIKE_CAP_MS
    assert sanitized.loc[0, "holding_time_ms"] == 0.0
    assert sanitized.loc[0, "entry_price"] == 0.0


def test_split_train_validation_is_deterministic(tmp_path):
    path = tmp_path / "features.csv"
    make_feature_frame(rows=10).to_csv(path, index=False)
    dataset = load_feature_csv(path)
    train, validation = split_train_validation(dataset, validation_fraction=0.3)

    assert len(train.frame) == 7
    assert len(validation.frame) == 3
    assert train.frame["timestamp_ns"].tolist() == [1_000 + i for i in range(7)]
    assert validation.frame["timestamp_ns"].tolist() == [1_007, 1_008, 1_009]


def test_standardization_stats_roundtrip_and_application(tmp_path):
    path = tmp_path / "features.csv"
    make_feature_frame(rows=5).to_csv(path, index=False)
    dataset = load_feature_csv(path)
    stats = fit_standardization_stats(dataset.feature_frame)
    stats_path = write_standardization_stats(stats, tmp_path / "stats.json")
    loaded = load_standardization_stats(stats_path)

    transformed = apply_standardization(dataset.feature_frame, loaded)
    assert transformed.shape == (5, len(FEATURE_COLUMNS))
    assert loaded.feature_version == FEATURE_VERSION
    assert abs(float(transformed["spread"].mean())) < 1e-9
