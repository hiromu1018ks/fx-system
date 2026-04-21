use std::ffi::OsStr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use fx_core::types::StrategyId;
use serde::{Deserialize, Serialize};

use crate::bayesian_lr::QAction;
use crate::features::FeatureVector;

pub const Q_STATE_SCHEMA_VERSION: u32 = 1;

fn sanitize_f64(v: f64) -> f64 {
    if v.is_nan() || v.is_infinite() { 0.0 } else { v }
}

fn sanitize_vec(data: Vec<f64>) -> Vec<f64> {
    data.into_iter().map(sanitize_f64).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VectorSnapshot {
    pub len: usize,
    pub data: Vec<f64>,
}

impl VectorSnapshot {
    pub fn from_vec(data: Vec<f64>) -> Self {
        Self {
            len: data.len(),
            data: sanitize_vec(data),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MatrixSnapshot {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<f64>,
}

impl MatrixSnapshot {
    pub fn from_column_major(rows: usize, cols: usize, data: Vec<f64>) -> Self {
        Self { rows, cols, data: sanitize_vec(data) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BayesianLinearRegressionSnapshot {
    pub dim: usize,
    pub posterior_mean: VectorSnapshot,
    pub posterior_covariance: MatrixSnapshot,
    pub precision_inverse: MatrixSnapshot,
    pub b_vector: VectorSnapshot,
    pub sigma2_noise: f64,
    pub ema_alpha: f64,
    pub residual_var_ema: f64,
    pub lambda_reg: f64,
    pub prev_w_norm: f64,
    pub n_observations: usize,
    pub divergence_threshold: f64,
}

impl BayesianLinearRegressionSnapshot {
    pub fn sanitize(&mut self) {
        self.sigma2_noise = sanitize_f64(self.sigma2_noise).max(1e-10);
        self.ema_alpha = sanitize_f64(self.ema_alpha);
        self.residual_var_ema = sanitize_f64(self.residual_var_ema).max(1e-10);
        self.lambda_reg = sanitize_f64(self.lambda_reg).max(1e-10);
        self.prev_w_norm = sanitize_f64(self.prev_w_norm);
        self.divergence_threshold = sanitize_f64(self.divergence_threshold);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionQStateSnapshot {
    pub action: QAction,
    pub model: BayesianLinearRegressionSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QFunctionSnapshot {
    pub dim: usize,
    pub initial_sigma2: f64,
    pub optimistic_bias: f64,
    pub actions: Vec<ActionQStateSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrategyQStateSnapshot {
    pub strategy_id: StrategyId,
    pub q_function: QFunctionSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrategySetQStateSnapshot {
    pub schema_version: u32,
    pub feature_schema_version: String,
    pub strategies: Vec<StrategyQStateSnapshot>,
}

impl StrategySetQStateSnapshot {
    pub fn new(mut strategies: Vec<StrategyQStateSnapshot>) -> Self {
        strategies.sort_by_key(|snapshot| strategy_sort_key(snapshot.strategy_id));
        Self {
            schema_version: Q_STATE_SCHEMA_VERSION,
            feature_schema_version: FeatureVector::SCHEMA_VERSION.to_string(),
            strategies,
        }
    }

    pub fn sanitize(&mut self) {
        for strategy in &mut self.strategies {
            for action in &mut strategy.q_function.actions {
                action.model.sanitize();
            }
        }
    }

    pub fn strategy(&self, strategy_id: StrategyId) -> Option<&StrategyQStateSnapshot> {
        self.strategies
            .iter()
            .find(|snapshot| snapshot.strategy_id == strategy_id)
    }

    pub fn write_to_path<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        self.sanitize();
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create q-state parent directory: {}",
                    parent.display()
                )
            })?;
        }

        match QStateFileFormat::from_path(path)? {
            QStateFileFormat::Json => {
                let content = serde_json::to_vec_pretty(self)
                    .context("Failed to serialize q-state snapshot to JSON")?;
                std::fs::write(path, content)
                    .with_context(|| format!("Failed to write q-state file: {}", path.display()))?;
            }
            QStateFileFormat::Binary => {
                let content = bincode::serialize(self)
                    .context("Failed to serialize q-state snapshot to binary")?;
                std::fs::write(path, content)
                    .with_context(|| format!("Failed to write q-state file: {}", path.display()))?;
            }
        }

        Ok(())
    }

    pub fn read_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read(path)
            .with_context(|| format!("Failed to read q-state file: {}", path.display()))?;

        let snapshot = match QStateFileFormat::from_path(path)? {
            QStateFileFormat::Json => serde_json::from_slice(&content)
                .context("Failed to deserialize q-state snapshot JSON")?,
            QStateFileFormat::Binary => bincode::deserialize(&content)
                .context("Failed to deserialize q-state snapshot binary")?,
        };

        Ok(snapshot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QStateFileFormat {
    Json,
    Binary,
}

impl QStateFileFormat {
    fn from_path(path: &Path) -> Result<Self> {
        match path
            .extension()
            .and_then(OsStr::to_str)
            .map(|ext| ext.to_ascii_lowercase())
        {
            None => Ok(Self::Json),
            Some(ext) if ext == "json" => Ok(Self::Json),
            Some(ext) if ext == "bin" || ext == "bincode" => Ok(Self::Binary),
            Some(ext) => {
                bail!("Unsupported q-state file extension '.{ext}'. Use .json, .bin, or .bincode.")
            }
        }
    }
}

fn strategy_sort_key(strategy_id: StrategyId) -> u8 {
    match strategy_id {
        StrategyId::A => 0,
        StrategyId::B => 1,
        StrategyId::C => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bayesian_lr::{QAction, QFunction};

    fn make_snapshot() -> StrategySetQStateSnapshot {
        let mut q_function = QFunction::new(3, 1.0, 100, 0.05, 0.1);
        let phi = vec![1.0, 0.5, -0.25];
        q_function.update(QAction::Buy, &phi, 1.0);
        q_function.update(QAction::Sell, &phi, -0.5);

        StrategySetQStateSnapshot::new(vec![StrategyQStateSnapshot {
            strategy_id: StrategyId::A,
            q_function: q_function.snapshot(),
        }])
    }

    fn temp_snapshot_path(ext: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("fx-q-state-{suffix}.{ext}"))
    }

    #[test]
    fn test_q_state_json_roundtrip() {
        let path = temp_snapshot_path("json");
        let snapshot = make_snapshot();
        let phi = vec![1.0, 0.5, -0.25];

        snapshot.write_to_path(&path).unwrap();
        let loaded = StrategySetQStateSnapshot::read_from_path(&path).unwrap();

        assert_eq!(loaded.schema_version, snapshot.schema_version);
        assert_eq!(
            loaded.feature_schema_version,
            snapshot.feature_schema_version
        );
        assert_eq!(loaded.strategies.len(), snapshot.strategies.len());

        let original_q = QFunction::from_snapshot(&snapshot.strategies[0].q_function).unwrap();
        let loaded_q = QFunction::from_snapshot(&loaded.strategies[0].q_function).unwrap();
        for &action in QAction::all() {
            assert!(
                (original_q.q_value(action, &phi) - loaded_q.q_value(action, &phi)).abs() < 1e-12
            );
            assert_eq!(
                original_q.model(action).n_observations(),
                loaded_q.model(action).n_observations()
            );
        }

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_q_state_binary_roundtrip() {
        let path = temp_snapshot_path("bin");
        let snapshot = make_snapshot();

        snapshot.write_to_path(&path).unwrap();
        let loaded = StrategySetQStateSnapshot::read_from_path(&path).unwrap();

        assert_eq!(loaded, snapshot);

        std::fs::remove_file(path).ok();
    }
}
