use serde::{Deserialize, Serialize};

/// Named feature vector with indexed access for the Q-function φ(s).
///
/// Feature layout is deterministic and versioned. Strategy-specific features
/// are appended after the common base. The `flattened()` method produces the
/// `repeated double feature_vector` field used in DecisionEventPayload proto.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureVector {
    pub spread: f64,
    pub spread_zscore: f64,
    pub obi: f64,
    pub delta_obi: f64,
    pub depth_change_rate: f64,
    pub queue_position: f64,
    pub realized_volatility: f64,
    pub volatility_ratio: f64,
    pub volatility_decay_rate: f64,
    pub session_tokyo: f64,
    pub session_london: f64,
    pub session_ny: f64,
    pub session_sydney: f64,
    pub time_since_open_ms: f64,
    pub time_since_last_spike_ms: f64,
    pub holding_time_ms: f64,
    pub position_size: f64,
    pub position_direction: f64,
    pub entry_price: f64,
    pub pnl_unrealized: f64,
    pub trade_intensity: f64,
    pub signed_volume: f64,
    pub recent_fill_rate: f64,
    pub recent_slippage: f64,
    pub recent_reject_rate: f64,
    pub execution_drift_trend: f64,
    pub self_impact: f64,
    pub time_decay: f64,
    pub dynamic_cost: f64,
    pub p_revert: f64,
    pub p_continue: f64,
    pub p_trend: f64,
    pub spread_z_x_vol: f64,
    pub obi_x_session: f64,
    pub depth_drop_x_vol_spike: f64,
    pub position_size_x_vol: f64,
    pub obi_x_vol: f64,
    pub spread_z_x_self_impact: f64,
}

impl FeatureVector {
    pub const DIM: usize = 38;
    pub const SCHEMA_VERSION: &str = "feature_vector_v1_38";
    pub const TIME_SINCE_LAST_SPIKE_CAP_MS: f64 = 86_400_000.0;
    pub const HEADER_NAMES: [&str; Self::DIM] = [
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
    ];

    pub fn flattened(&self) -> Vec<f64> {
        vec![
            self.spread,
            self.spread_zscore,
            self.obi,
            self.delta_obi,
            self.depth_change_rate,
            self.queue_position,
            self.realized_volatility,
            self.volatility_ratio,
            self.volatility_decay_rate,
            self.session_tokyo,
            self.session_london,
            self.session_ny,
            self.session_sydney,
            self.time_since_open_ms,
            self.time_since_last_spike_ms,
            self.holding_time_ms,
            self.position_size,
            self.position_direction,
            self.entry_price,
            self.pnl_unrealized,
            self.trade_intensity,
            self.signed_volume,
            self.recent_fill_rate,
            self.recent_slippage,
            self.recent_reject_rate,
            self.execution_drift_trend,
            self.self_impact,
            self.time_decay,
            self.dynamic_cost,
            self.p_revert,
            self.p_continue,
            self.p_trend,
            self.spread_z_x_vol,
            self.obi_x_session,
            self.depth_drop_x_vol_spike,
            self.position_size_x_vol,
            self.obi_x_vol,
            self.spread_z_x_self_impact,
        ]
    }

    pub fn header_names() -> [&'static str; Self::DIM] {
        Self::HEADER_NAMES
    }

    /// Regime-model input must be finite because Rust feeds these values directly
    /// into ONNX float32 inference. Keep the ordering identical to `flattened()`
    /// and apply only contract-level sanitization here.
    pub fn flattened_for_regime_model(&self) -> Vec<f64> {
        let mut values = self.flattened();
        for (idx, value) in values.iter_mut().enumerate() {
            *value = match idx {
                13 | 15 => {
                    if value.is_finite() && *value >= 0.0 {
                        *value
                    } else {
                        0.0
                    }
                }
                14 => {
                    if value.is_finite() {
                        value.clamp(0.0, Self::TIME_SINCE_LAST_SPIKE_CAP_MS)
                    } else {
                        Self::TIME_SINCE_LAST_SPIKE_CAP_MS
                    }
                }
                _ => {
                    if value.is_finite() {
                        *value
                    } else {
                        0.0
                    }
                }
            };
        }
        values
    }

    pub fn from_flattened(values: &[f64]) -> Option<Self> {
        if values.len() != Self::DIM {
            return None;
        }
        Some(Self {
            spread: values[0],
            spread_zscore: values[1],
            obi: values[2],
            delta_obi: values[3],
            depth_change_rate: values[4],
            queue_position: values[5],
            realized_volatility: values[6],
            volatility_ratio: values[7],
            volatility_decay_rate: values[8],
            session_tokyo: values[9],
            session_london: values[10],
            session_ny: values[11],
            session_sydney: values[12],
            time_since_open_ms: values[13],
            time_since_last_spike_ms: values[14],
            holding_time_ms: values[15],
            position_size: values[16],
            position_direction: values[17],
            entry_price: values[18],
            pnl_unrealized: values[19],
            trade_intensity: values[20],
            signed_volume: values[21],
            recent_fill_rate: values[22],
            recent_slippage: values[23],
            recent_reject_rate: values[24],
            execution_drift_trend: values[25],
            self_impact: values[26],
            time_decay: values[27],
            dynamic_cost: values[28],
            p_revert: values[29],
            p_continue: values[30],
            p_trend: values[31],
            spread_z_x_vol: values[32],
            obi_x_session: values[33],
            depth_drop_x_vol_spike: values[34],
            position_size_x_vol: values[35],
            obi_x_vol: values[36],
            spread_z_x_self_impact: values[37],
        })
    }

    /// Zero-initialized feature vector.
    pub fn zero() -> Self {
        Self {
            spread: 0.0,
            spread_zscore: 0.0,
            obi: 0.0,
            delta_obi: 0.0,
            depth_change_rate: 0.0,
            queue_position: 0.0,
            realized_volatility: 0.0,
            volatility_ratio: 0.0,
            volatility_decay_rate: 0.0,
            session_tokyo: 0.0,
            session_london: 0.0,
            session_ny: 0.0,
            session_sydney: 0.0,
            time_since_open_ms: 0.0,
            time_since_last_spike_ms: 0.0,
            holding_time_ms: 0.0,
            position_size: 0.0,
            position_direction: 0.0,
            entry_price: 0.0,
            pnl_unrealized: 0.0,
            trade_intensity: 0.0,
            signed_volume: 0.0,
            recent_fill_rate: 0.0,
            recent_slippage: 0.0,
            recent_reject_rate: 0.0,
            execution_drift_trend: 0.0,
            self_impact: 0.0,
            time_decay: 0.0,
            dynamic_cost: 0.0,
            p_revert: 0.5,
            p_continue: 0.5,
            p_trend: 0.5,
            spread_z_x_vol: 0.0,
            obi_x_session: 0.0,
            depth_drop_x_vol_spike: 0.0,
            position_size_x_vol: 0.0,
            obi_x_vol: 0.0,
            spread_z_x_self_impact: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flattened_roundtrip() {
        let mut fv = FeatureVector::zero();
        fv.spread = 0.5;
        fv.obi = -0.3;
        fv.session_tokyo = 1.0;
        fv.recent_reject_rate = 0.2;
        fv.execution_drift_trend = 0.01;
        fv.p_revert = 0.8;

        let flat = fv.flattened();
        assert_eq!(flat.len(), FeatureVector::DIM);

        let restored = FeatureVector::from_flattened(&flat).unwrap();
        assert!((restored.spread - 0.5).abs() < 1e-15);
        assert!((restored.obi - (-0.3)).abs() < 1e-15);
        assert!((restored.session_tokyo - 1.0).abs() < 1e-15);
        assert!((restored.recent_reject_rate - 0.2).abs() < 1e-15);
        assert!((restored.execution_drift_trend - 0.01).abs() < 1e-15);
        assert!((restored.p_revert - 0.8).abs() < 1e-15);
    }

    #[test]
    fn test_from_flattened_wrong_length() {
        assert!(FeatureVector::from_flattened(&[1.0, 2.0]).is_none());
        assert!(FeatureVector::from_flattened(&[]).is_none());
    }

    #[test]
    fn test_zero_initialized() {
        let fv = FeatureVector::zero();
        let flat = fv.flattened();
        // Most fields are 0.0, probability fields default to 0.5
        assert!((flat[0] - 0.0).abs() < 1e-15);
        assert!((flat[29] - 0.5).abs() < 1e-15); // p_revert
        assert!((flat[30] - 0.5).abs() < 1e-15); // p_continue
        assert!((flat[31] - 0.5).abs() < 1e-15); // p_trend
    }

    #[test]
    fn test_dim_constant() {
        assert_eq!(FeatureVector::DIM, FeatureVector::zero().flattened().len());
    }

    #[test]
    fn test_header_names_match_dimension_and_layout() {
        let headers = FeatureVector::header_names();
        assert_eq!(headers.len(), FeatureVector::DIM);
        assert_eq!(headers[0], "spread");
        assert_eq!(headers[14], "time_since_last_spike_ms");
        assert_eq!(headers[37], "spread_z_x_self_impact");
    }

    #[test]
    fn test_regime_model_input_sanitizes_non_finite_values() {
        let mut fv = FeatureVector::zero();
        fv.spread = f64::NAN;
        fv.time_since_open_ms = -1.0;
        fv.time_since_last_spike_ms = f64::INFINITY;
        fv.holding_time_ms = -5.0;
        fv.entry_price = f64::NEG_INFINITY;

        let flat = fv.flattened_for_regime_model();
        assert_eq!(flat[0], 0.0);
        assert_eq!(flat[13], 0.0);
        assert_eq!(flat[14], FeatureVector::TIME_SINCE_LAST_SPIKE_CAP_MS);
        assert_eq!(flat[15], 0.0);
        assert_eq!(flat[18], 0.0);
    }

    #[test]
    fn test_regime_model_input_preserves_finite_values() {
        let mut fv = FeatureVector::zero();
        fv.spread = 0.25;
        fv.time_since_last_spike_ms = 2_500.0;
        fv.dynamic_cost = 1.5;

        let flat = fv.flattened_for_regime_model();
        assert_eq!(flat[0], 0.25);
        assert_eq!(flat[14], 2_500.0);
        assert_eq!(flat[28], 1.5);
    }
}
