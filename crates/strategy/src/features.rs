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
    pub const DIM: usize = 36;

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
            self_impact: values[24],
            time_decay: values[25],
            dynamic_cost: values[26],
            p_revert: values[27],
            p_continue: values[28],
            p_trend: values[29],
            spread_z_x_vol: values[30],
            obi_x_session: values[31],
            depth_drop_x_vol_spike: values[32],
            position_size_x_vol: values[33],
            obi_x_vol: values[34],
            spread_z_x_self_impact: values[35],
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
        fv.p_revert = 0.8;

        let flat = fv.flattened();
        assert_eq!(flat.len(), FeatureVector::DIM);

        let restored = FeatureVector::from_flattened(&flat).unwrap();
        assert!((restored.spread - 0.5).abs() < 1e-15);
        assert!((restored.obi - (-0.3)).abs() < 1e-15);
        assert!((restored.session_tokyo - 1.0).abs() < 1e-15);
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
        assert!((flat[27] - 0.5).abs() < 1e-15); // p_revert
        assert!((flat[28] - 0.5).abs() < 1e-15); // p_continue
        assert!((flat[29] - 0.5).abs() < 1e-15); // p_trend
    }

    #[test]
    fn test_dim_constant() {
        assert_eq!(FeatureVector::DIM, FeatureVector::zero().flattened().len());
    }
}
