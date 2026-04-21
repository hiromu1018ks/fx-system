"""Load dumped backtest features for Python-side regime training."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pandas as pd

FEATURE_VERSION = "feature_vector_v1_38"
METADATA_COLUMNS = ("timestamp_ns", "source_strategy", "feature_version")
FEATURE_COLUMNS = (
    "spread",
    "spread_zscore",
    "obi",
    "delta_obi",
    "depth_change_rate",
    "queue_position",
    "realized_volatility",
    "volatility_ratio",
    "volatility_decay_rate",
    "session_tokyo",
    "session_london",
    "session_ny",
    "session_sydney",
    "time_since_open_ms",
    "time_since_last_spike_ms",
    "holding_time_ms",
    "position_size",
    "position_direction",
    "entry_price",
    "pnl_unrealized",
    "trade_intensity",
    "signed_volume",
    "recent_fill_rate",
    "recent_slippage",
    "recent_reject_rate",
    "execution_drift_trend",
    "self_impact",
    "time_decay",
    "dynamic_cost",
    "p_revert",
    "p_continue",
    "p_trend",
    "spread_z_x_vol",
    "obi_x_session",
    "depth_drop_x_vol_spike",
    "position_size_x_vol",
    "obi_x_vol",
    "spread_z_x_self_impact",
)
EXPECTED_COLUMNS = METADATA_COLUMNS + FEATURE_COLUMNS


@dataclass(frozen=True)
class FeatureDataset:
    """Feature dump plus schema metadata."""

    frame: pd.DataFrame
    feature_version: str

    @property
    def feature_frame(self) -> pd.DataFrame:
        """Return only the numeric feature columns."""
        return self.frame.loc[:, FEATURE_COLUMNS].copy()


def load_feature_csv(
    path: str | Path,
    *,
    expected_version: str = FEATURE_VERSION,
) -> FeatureDataset:
    """Load a dumped feature CSV and validate its schema."""
    path = Path(path)
    if not path.exists():
        raise FileNotFoundError(f"Feature CSV not found: {path}")

    frame = pd.read_csv(path)
    actual_columns = tuple(frame.columns.tolist())
    if actual_columns != EXPECTED_COLUMNS:
        raise ValueError(
            "Feature CSV schema mismatch: "
            f"expected {EXPECTED_COLUMNS}, got {actual_columns}"
        )

    versions = frame["feature_version"].drop_duplicates().tolist()
    if versions != [expected_version]:
        raise ValueError(
            f"Unexpected feature_version values {versions}; expected [{expected_version!r}]"
        )

    frame["timestamp_ns"] = frame["timestamp_ns"].astype("int64")
    frame["source_strategy"] = frame["source_strategy"].astype("string")
    frame["feature_version"] = frame["feature_version"].astype("string")
    return FeatureDataset(frame=frame, feature_version=expected_version)
