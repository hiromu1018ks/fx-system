//! Bayesian Linear Regression for Q-function weight estimation.
//!
//! Posterior: w ~ N(ŵ, Σ̂) where Σ̂ = σ²_noise,t · (Φ^T Φ + λ_reg I)^{-1}
//! Adaptive noise: σ²_noise,t = EMA_variance(residuals, halflife=500)
//!
//! σ_model is ONLY reflected through Thompson Sampling, NEVER in point estimates.

use anyhow::{bail, Result};
use std::collections::HashMap;

use nalgebra::{Cholesky, DMatrix, DVector};
use rand::Rng;
use rand_distr::StandardNormal;
use serde::{Deserialize, Serialize};

use crate::q_state::{
    ActionQStateSnapshot, BayesianLinearRegressionSnapshot, MatrixSnapshot, QFunctionSnapshot,
    VectorSnapshot,
};

/// Result of a single Bayesian update.
#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub residual: f64,
    pub divergence_ratio: f64,
    pub diverged: bool,
}

/// Bayesian Linear Regression for a single action's weight vector.
///
/// Maintains posterior w ~ N(ŵ, Σ̂) with online Sherman-Morrison updates
/// and adaptive noise variance estimation.
#[derive(Debug, Clone)]
pub struct BayesianLinearRegression {
    dim: usize,
    /// Inverse of regularized precision: (Φ^T Φ + λ_reg I)^{-1}
    a_inv: DMatrix<f64>,
    /// Accumulated Φ^T y
    b: DVector<f64>,
    /// Posterior mean ŵ = A_inv · b
    w_hat: DVector<f64>,
    /// Adaptive noise variance σ²_noise
    sigma2_noise: f64,
    /// EMA decay factor
    ema_alpha: f64,
    /// Running EMA of squared residuals
    residual_var_ema: f64,
    /// Regularization strength λ_reg
    lambda_reg: f64,
    /// Weight norm before last update
    prev_w_norm: f64,
    /// Number of observations processed
    n_observations: usize,
    /// Divergence detection threshold
    divergence_threshold: f64,
}

impl BayesianLinearRegression {
    pub fn new(dim: usize, lambda_reg: f64, halflife: usize, initial_sigma2: f64) -> Self {
        assert!(lambda_reg > 0.0, "lambda_reg must be positive");
        assert!(halflife > 0, "halflife must be positive");
        assert!(initial_sigma2 > 0.0, "initial_sigma2 must be positive");

        let ema_alpha = 1.0 - (-std::f64::consts::LN_2 / halflife as f64).exp();

        Self {
            dim,
            a_inv: DMatrix::identity(dim, dim) / lambda_reg,
            b: DVector::zeros(dim),
            w_hat: DVector::zeros(dim),
            sigma2_noise: initial_sigma2,
            ema_alpha,
            residual_var_ema: initial_sigma2,
            lambda_reg,
            prev_w_norm: 0.0,
            n_observations: 0,
            divergence_threshold: 2.0,
        }
    }

    /// Online Bayesian update with observation (φ, y).
    ///
    /// Uses Sherman-Morrison for efficient A_inv update.
    pub fn update(&mut self, phi: &[f64], y: f64) -> UpdateResult {
        assert_eq!(phi.len(), self.dim, "Feature dimension mismatch");

        let x = DVector::from_column_slice(phi);

        // Residual with current weights (before update)
        let y_pred = self.w_hat.dot(&x);
        let residual = y - y_pred;

        // Update adaptive noise variance via EMA
        let sq_residual = residual * residual;
        self.residual_var_ema =
            self.ema_alpha * sq_residual + (1.0 - self.ema_alpha) * self.residual_var_ema;
        self.sigma2_noise = self.residual_var_ema.max(1e-10);

        // Store previous norm for divergence check
        self.prev_w_norm = self.w_hat.norm();

        // Sherman-Morrison: A_inv_new = A_inv - (A_inv x)(A_inv x)^T / (1 + x^T A_inv x)
        let a_inv_x = &self.a_inv * &x;
        let x_t_a_inv_x = x.dot(&a_inv_x);
        let denom = 1.0 + x_t_a_inv_x;
        self.a_inv -= (&a_inv_x * a_inv_x.transpose()) / denom;

        // b <- b + x * y
        self.b += &x * y;

        // ŵ = A_inv · b
        self.w_hat = &self.a_inv * &self.b;
        self.n_observations += 1;

        // Divergence check: ||w_new|| / ||w_old|| > threshold
        // Skip check for the first few observations — posterior hasn't stabilized yet
        let new_w_norm = self.w_hat.norm();
        let (divergence_ratio, diverged) = if self.n_observations < 5 {
            (1.0, false)
        } else if self.prev_w_norm > 1e-10 {
            let ratio = new_w_norm / self.prev_w_norm;
            (ratio, ratio > self.divergence_threshold)
        } else {
            (1.0, false)
        };

        UpdateResult {
            residual,
            divergence_ratio,
            diverged,
        }
    }

    /// Q-value (point estimate): Q(s, a) = ŵ^T φ(s).
    /// Does NOT include σ_model — only used for monitoring.
    pub fn predict(&self, phi: &[f64]) -> f64 {
        assert_eq!(phi.len(), self.dim, "Feature dimension mismatch");
        let x = DVector::from_column_slice(phi);
        self.w_hat.dot(&x)
    }

    /// Posterior variance: σ²_model(s) = σ²_noise · φ(s)^T A_inv φ(s)
    pub fn posterior_variance(&self, phi: &[f64]) -> f64 {
        assert_eq!(phi.len(), self.dim, "Feature dimension mismatch");
        let x = DVector::from_column_slice(phi);
        let a_inv_x = &self.a_inv * &x;
        (self.sigma2_noise * x.dot(&a_inv_x)).max(0.0)
    }

    /// Posterior std: σ_model(s) = √(posterior_variance)
    pub fn posterior_std(&self, phi: &[f64]) -> f64 {
        self.posterior_variance(phi).sqrt()
    }

    /// Sample weights from posterior: w̃ ~ N(ŵ, Σ̂)
    ///
    /// When `n_observations < dim`, uses diagonal approximation:
    /// `diag(sqrt(σ² · diag(A_inv))) · z` instead of full Cholesky.
    pub fn sample_weights(&self, rng: &mut impl Rng) -> DVector<f64> {
        if self.n_observations < self.dim {
            // Diagonal approximation: independent sampling per dimension
            let sigma = self.sigma2_noise.sqrt();
            let mut w_sampled = self.w_hat.clone();
            for i in 0..self.dim {
                let std_i = sigma * self.a_inv[(i, i)].sqrt().max(0.0);
                w_sampled[i] += std_i * sample_standard_normal_scalar(rng);
            }
            return w_sampled;
        }

        match Cholesky::new(self.a_inv.clone()) {
            Some(chol) => {
                let sigma = self.sigma2_noise.sqrt();
                let z_vec = sample_standard_normal(self.dim, rng);
                self.w_hat.clone() + sigma * chol.l() * z_vec
            }
            None => {
                tracing::warn!(
                    "Cholesky decomposition failed (dim={}, obs={}), using point estimate",
                    self.dim,
                    self.n_observations
                );
                self.w_hat.clone()
            }
        }
    }

    /// Sampled Q-value for Thompson Sampling: Q̃(s, a) = w̃^T φ(s)
    pub fn sample_predict(&self, phi: &[f64], rng: &mut impl Rng) -> f64 {
        let w_sampled = self.sample_weights(rng);
        let x = DVector::from_column_slice(phi);
        w_sampled.dot(&x)
    }

    /// Reset posterior state (keep noise estimate).
    pub fn reset(&mut self) {
        self.a_inv = DMatrix::identity(self.dim, self.dim) / self.lambda_reg;
        self.b = DVector::zeros(self.dim);
        self.w_hat = DVector::zeros(self.dim);
        self.prev_w_norm = 0.0;
        self.n_observations = 0;
    }

    /// Full reset including noise estimate.
    pub fn reset_full(&mut self, initial_sigma2: f64) {
        self.reset();
        self.sigma2_noise = initial_sigma2;
        self.residual_var_ema = initial_sigma2;
    }

    /// Inflate posterior covariance by factor (hold degradation prevention).
    pub fn inflate_covariance(&mut self, factor: f64) {
        assert!(factor >= 1.0, "Inflation factor must be >= 1.0");
        self.a_inv *= factor;
    }

    /// Apply optimistic initialization bias.
    ///
    /// Sets b so that ŵ = A_inv · b = bias · ones.
    pub fn apply_optimistic_bias(&mut self, bias: f64) {
        for i in 0..self.dim {
            self.b[i] += self.lambda_reg * bias;
        }
        self.w_hat = &self.a_inv * &self.b;
    }

    pub fn weights(&self) -> &DVector<f64> {
        &self.w_hat
    }
    pub fn noise_variance(&self) -> f64 {
        self.sigma2_noise
    }
    pub fn n_observations(&self) -> usize {
        self.n_observations
    }
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn snapshot(&self) -> BayesianLinearRegressionSnapshot {
        BayesianLinearRegressionSnapshot {
            dim: self.dim,
            posterior_mean: VectorSnapshot::from_vec(self.w_hat.iter().copied().collect()),
            posterior_covariance: MatrixSnapshot::from_column_major(
                self.dim,
                self.dim,
                (self.a_inv.clone() * self.sigma2_noise).as_slice().to_vec(),
            ),
            precision_inverse: MatrixSnapshot::from_column_major(
                self.dim,
                self.dim,
                self.a_inv.as_slice().to_vec(),
            ),
            b_vector: VectorSnapshot::from_vec(self.b.iter().copied().collect()),
            sigma2_noise: self.sigma2_noise,
            ema_alpha: self.ema_alpha,
            residual_var_ema: self.residual_var_ema,
            lambda_reg: self.lambda_reg,
            prev_w_norm: self.prev_w_norm,
            n_observations: self.n_observations,
            divergence_threshold: self.divergence_threshold,
        }
    }

    pub fn from_snapshot(snapshot: &BayesianLinearRegressionSnapshot) -> Result<Self> {
        if snapshot.dim == 0 {
            bail!("BayesianLinearRegression snapshot dimension must be positive");
        }
        if snapshot.lambda_reg <= 0.0 {
            bail!("BayesianLinearRegression snapshot lambda_reg must be positive");
        }
        if snapshot.sigma2_noise <= 0.0 {
            bail!("BayesianLinearRegression snapshot sigma2_noise must be positive");
        }
        if snapshot.residual_var_ema <= 0.0 {
            bail!("BayesianLinearRegression snapshot residual_var_ema must be positive");
        }
        if !(0.0 < snapshot.ema_alpha && snapshot.ema_alpha <= 1.0) {
            bail!("BayesianLinearRegression snapshot ema_alpha must be in (0, 1]");
        }
        if snapshot.divergence_threshold <= 0.0 {
            bail!("BayesianLinearRegression snapshot divergence_threshold must be positive");
        }

        let posterior_mean =
            vector_from_snapshot(&snapshot.posterior_mean, snapshot.dim, "posterior_mean")?;
        let posterior_covariance = matrix_from_snapshot(
            &snapshot.posterior_covariance,
            snapshot.dim,
            snapshot.dim,
            "posterior_covariance",
        )?;
        let precision_inverse = matrix_from_snapshot(
            &snapshot.precision_inverse,
            snapshot.dim,
            snapshot.dim,
            "precision_inverse",
        )?;
        let b = vector_from_snapshot(&snapshot.b_vector, snapshot.dim, "b_vector")?;

        let expected_covariance = precision_inverse.clone() * snapshot.sigma2_noise;
        let covariance_diff = max_abs_matrix_diff(&expected_covariance, &posterior_covariance);
        if covariance_diff > 1e-9 {
            bail!(
                "BayesianLinearRegression snapshot posterior_covariance does not match precision_inverse * sigma2_noise (max diff: {covariance_diff})"
            );
        }

        let expected_w_hat = &precision_inverse * &b;
        let mean_diff = max_abs_vector_diff(&expected_w_hat, &posterior_mean);
        if mean_diff > 1e-9 {
            bail!(
                "BayesianLinearRegression snapshot posterior_mean does not match precision_inverse * b_vector (max diff: {mean_diff})"
            );
        }

        Ok(Self {
            dim: snapshot.dim,
            a_inv: precision_inverse,
            b,
            w_hat: posterior_mean,
            sigma2_noise: snapshot.sigma2_noise,
            ema_alpha: snapshot.ema_alpha,
            residual_var_ema: snapshot.residual_var_ema,
            lambda_reg: snapshot.lambda_reg,
            prev_w_norm: snapshot.prev_w_norm,
            n_observations: snapshot.n_observations,
            divergence_threshold: snapshot.divergence_threshold,
        })
    }
}

fn sample_standard_normal(dim: usize, rng: &mut impl Rng) -> DVector<f64> {
    let mut v = DVector::zeros(dim);
    for i in 0..dim {
        v[i] = rng.sample(StandardNormal);
    }
    v
}

fn sample_standard_normal_scalar(rng: &mut impl Rng) -> f64 {
    rng.sample(StandardNormal)
}

/// Action types for Q-function (without lot sizes — those are determined by execution layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QAction {
    Buy,
    Sell,
    Hold,
}

impl QAction {
    pub fn all() -> &'static [QAction] {
        &[QAction::Buy, QAction::Sell, QAction::Hold]
    }
}

/// Q-function: Q(s, a) = w_a^T φ(s).
///
/// Manages separate BayesianLinearRegression for each action.
/// Thompson Sampling is the sole action selection mechanism;
/// σ_model is ONLY reflected through posterior sampling.
#[derive(Debug, Clone)]
pub struct QFunction {
    models: HashMap<QAction, BayesianLinearRegression>,
    dim: usize,
    initial_sigma2: f64,
    optimistic_bias: f64,
}

impl QFunction {
    pub fn new(
        dim: usize,
        lambda_reg: f64,
        halflife: usize,
        initial_sigma2: f64,
        optimistic_bias: f64,
    ) -> Self {
        let mut models = HashMap::new();
        for &action in QAction::all() {
            let mut model =
                BayesianLinearRegression::new(dim, lambda_reg, halflife, initial_sigma2);
            if action != QAction::Hold {
                model.apply_optimistic_bias(optimistic_bias);
            }
            models.insert(action, model);
        }

        Self {
            models,
            dim,
            initial_sigma2,
            optimistic_bias,
        }
    }

    /// Point estimate Q (monitoring only, NOT for action selection).
    pub fn q_value(&self, action: QAction, phi: &[f64]) -> f64 {
        self.models[&action].predict(phi)
    }

    /// Point estimate Q (alias).
    pub fn q_point(&self, action: QAction, phi: &[f64]) -> f64 {
        self.q_value(action, phi)
    }

    /// Posterior std for diagnostics.
    pub fn posterior_std(&self, action: QAction, phi: &[f64]) -> f64 {
        self.models[&action].posterior_std(phi)
    }

    /// Sampled Q-value for Thompson Sampling: Q̃(s, a) = w̃_a^T φ(s)
    pub fn sample_q_value(&self, action: QAction, phi: &[f64], rng: &mut impl Rng) -> f64 {
        self.models[&action].sample_predict(phi, rng)
    }

    /// Sample weights from posterior.
    pub fn sample_weights(&self, action: QAction, rng: &mut impl Rng) -> DVector<f64> {
        self.models[&action].sample_weights(rng)
    }

    /// On-policy update for a specific action.
    pub fn update(&mut self, action: QAction, phi: &[f64], target: f64) -> UpdateResult {
        self.models.get_mut(&action).unwrap().update(phi, target)
    }

    /// All Q-values for monitoring.
    pub fn q_values(&self, phi: &[f64]) -> HashMap<QAction, f64> {
        QAction::all()
            .iter()
            .map(|&a| (a, self.q_value(a, phi)))
            .collect()
    }

    /// All posterior stds for diagnostics.
    pub fn posterior_stds(&self, phi: &[f64]) -> HashMap<QAction, f64> {
        QAction::all()
            .iter()
            .map(|&a| (a, self.posterior_std(a, phi)))
            .collect()
    }

    /// Reset a specific action (preserving optimistic init for buy/sell).
    pub fn reset_action(&mut self, action: QAction) {
        let model = self.models.get_mut(&action).unwrap();
        model.reset();
        if action != QAction::Hold {
            model.apply_optimistic_bias(self.optimistic_bias);
        }
    }

    /// Reset all actions.
    pub fn reset_all(&mut self) {
        for &action in QAction::all() {
            self.reset_action(action);
        }
    }

    /// Full reset including noise estimates.
    pub fn reset_all_full(&mut self) {
        for &action in QAction::all() {
            let model = self.models.get_mut(&action).unwrap();
            model.reset_full(self.initial_sigma2);
            if action != QAction::Hold {
                model.apply_optimistic_bias(self.optimistic_bias);
            }
        }
    }

    /// Inflate all covariances (hold degradation prevention).
    pub fn inflate_covariance(&mut self, factor: f64) {
        for model in self.models.values_mut() {
            model.inflate_covariance(factor);
        }
    }

    pub fn model(&self, action: QAction) -> &BayesianLinearRegression {
        &self.models[&action]
    }

    pub fn model_mut(&mut self, action: QAction) -> &mut BayesianLinearRegression {
        self.models.get_mut(&action).unwrap()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn snapshot(&self) -> QFunctionSnapshot {
        let actions = QAction::all()
            .iter()
            .map(|&action| ActionQStateSnapshot {
                action,
                model: self.models[&action].snapshot(),
            })
            .collect();

        QFunctionSnapshot {
            dim: self.dim,
            initial_sigma2: self.initial_sigma2,
            optimistic_bias: self.optimistic_bias,
            actions,
        }
    }

    pub fn from_snapshot(snapshot: &QFunctionSnapshot) -> Result<Self> {
        if snapshot.dim == 0 {
            bail!("QFunction snapshot dimension must be positive");
        }
        if snapshot.initial_sigma2 <= 0.0 {
            bail!("QFunction snapshot initial_sigma2 must be positive");
        }
        if snapshot.actions.len() != QAction::all().len() {
            bail!(
                "QFunction snapshot must contain {} actions, found {}",
                QAction::all().len(),
                snapshot.actions.len()
            );
        }

        let mut models = HashMap::new();
        for action_snapshot in &snapshot.actions {
            if models.contains_key(&action_snapshot.action) {
                bail!(
                    "QFunction snapshot contains duplicate action {:?}",
                    action_snapshot.action
                );
            }

            let model = BayesianLinearRegression::from_snapshot(&action_snapshot.model)?;
            if model.dim() != snapshot.dim {
                bail!(
                    "QFunction snapshot action {:?} has dimension {}, expected {}",
                    action_snapshot.action,
                    model.dim(),
                    snapshot.dim
                );
            }
            models.insert(action_snapshot.action, model);
        }

        for &action in QAction::all() {
            if !models.contains_key(&action) {
                bail!("QFunction snapshot is missing action {:?}", action);
            }
        }

        Ok(Self {
            models,
            dim: snapshot.dim,
            initial_sigma2: snapshot.initial_sigma2,
            optimistic_bias: snapshot.optimistic_bias,
        })
    }
}

fn vector_from_snapshot(
    snapshot: &VectorSnapshot,
    expected_len: usize,
    field_name: &str,
) -> Result<DVector<f64>> {
    if snapshot.len != expected_len {
        bail!(
            "Snapshot field '{field_name}' has length {}, expected {}",
            snapshot.len,
            expected_len
        );
    }
    if snapshot.data.len() != expected_len {
        bail!(
            "Snapshot field '{field_name}' stores {} values, expected {}",
            snapshot.data.len(),
            expected_len
        );
    }
    Ok(DVector::from_column_slice(&snapshot.data))
}

fn matrix_from_snapshot(
    snapshot: &MatrixSnapshot,
    expected_rows: usize,
    expected_cols: usize,
    field_name: &str,
) -> Result<DMatrix<f64>> {
    if snapshot.rows != expected_rows || snapshot.cols != expected_cols {
        bail!(
            "Snapshot field '{field_name}' has shape {}x{}, expected {}x{}",
            snapshot.rows,
            snapshot.cols,
            expected_rows,
            expected_cols
        );
    }
    if snapshot.data.len() != expected_rows * expected_cols {
        bail!(
            "Snapshot field '{field_name}' stores {} values, expected {}",
            snapshot.data.len(),
            expected_rows * expected_cols
        );
    }
    Ok(DMatrix::from_column_slice(
        expected_rows,
        expected_cols,
        &snapshot.data,
    ))
}

fn max_abs_vector_diff(lhs: &DVector<f64>, rhs: &DVector<f64>) -> f64 {
    lhs.iter()
        .zip(rhs.iter())
        .map(|(left, right)| (left - right).abs())
        .fold(0.0_f64, f64::max)
}

fn max_abs_matrix_diff(lhs: &DMatrix<f64>, rhs: &DMatrix<f64>) -> f64 {
    lhs.iter()
        .zip(rhs.iter())
        .map(|(left, right)| (left - right).abs())
        .fold(0.0_f64, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::FeatureVector;
    use rand::thread_rng;
    use rand::SeedableRng;

    const DIM: usize = 5;
    const LAMBDA_REG: f64 = 1.0;
    const HALFLIFE: usize = 500;
    const INITIAL_SIGMA2: f64 = 0.01;

    fn make_model() -> BayesianLinearRegression {
        BayesianLinearRegression::new(DIM, LAMBDA_REG, HALFLIFE, INITIAL_SIGMA2)
    }

    fn phi_ones() -> Vec<f64> {
        vec![1.0; DIM]
    }

    // === BayesianLinearRegression tests ===

    #[test]
    fn test_creation() {
        let m = make_model();
        assert_eq!(m.dim(), DIM);
        assert_eq!(m.n_observations(), 0);
        assert!((m.noise_variance() - INITIAL_SIGMA2).abs() < 1e-15);
        assert_eq!(m.weights().len(), DIM);
        for i in 0..DIM {
            assert!((m.weights()[i] - 0.0).abs() < 1e-15);
        }
    }

    #[test]
    fn test_predict_zero_initial() {
        let m = make_model();
        let phi = vec![0.5, -0.3, 1.0, 0.0, 2.0];
        assert!((m.predict(&phi) - 0.0).abs() < 1e-15);
    }

    #[test]
    fn test_single_update() {
        let mut m = make_model();
        let phi = phi_ones();
        let result = m.update(&phi, 1.0);

        assert_eq!(m.n_observations(), 1);
        assert!(!result.diverged);

        // After update with phi=[1,...,1], y=1.0:
        // A = I/λ_reg + 1*1^T, b = 1*1.0
        // ŵ should be positive (pulling toward 1.0)
        let q = m.predict(&phi);
        assert!(
            q > 0.0,
            "Q-value should be positive after positive update: {}",
            q
        );
    }

    #[test]
    fn test_convergence_to_known_weights() {
        // True model: y = 2*x0 + 3*x1 + (-1)*x2 + 0*x3 + 1*x4 + noise
        let true_w = vec![2.0, 3.0, -1.0, 0.0, 1.0];
        let mut rng = rand::thread_rng();
        let mut m = BayesianLinearRegression::new(DIM, 0.01, HALFLIFE, 0.1);

        // Train with many observations
        for _ in 0..500 {
            let phi: Vec<f64> = (0..DIM).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let y = true_w
                .iter()
                .zip(phi.iter())
                .map(|(w, x)| w * x)
                .sum::<f64>()
                + rng.gen_range(-0.01..0.01);
            m.update(&phi, y);
        }

        // Weights should be close to true weights
        for i in 0..DIM {
            assert!(
                (m.weights()[i] - true_w[i]).abs() < 0.5,
                "w[{}] = {}, expected ~{}",
                i,
                m.weights()[i],
                true_w[i]
            );
        }
    }

    #[test]
    fn test_posterior_std_decreases_with_data() {
        let mut m = make_model();
        let phi = phi_ones();

        let std_before = m.posterior_std(&phi);

        for i in 0..50 {
            m.update(&phi, 1.0 + (i as f64) * 0.01);
        }

        let std_after = m.posterior_std(&phi);
        assert!(
            std_after < std_before,
            "Posterior std should decrease: before={}, after={}",
            std_before,
            std_after
        );
    }

    #[test]
    fn test_posterior_std_non_negative() {
        let mut m = make_model();
        let phi = vec![100.0, -50.0, 0.0, 1.0, -1.0];

        for _ in 0..10 {
            m.update(&phi, 1.0);
        }

        let var = m.posterior_variance(&phi);
        assert!(
            var >= 0.0,
            "Posterior variance must be non-negative: {}",
            var
        );
        assert!(!var.is_nan(), "Posterior variance must not be NaN");
    }

    #[test]
    fn test_adaptive_noise_variance() {
        let mut m = BayesianLinearRegression::new(DIM, 0.01, HALFLIFE, 0.1);

        // True linear model: y = 2*x0 + 1*x1 + noise(σ=0.1)
        let mut rng = thread_rng();
        for _ in 0..2000 {
            let phi: Vec<f64> = (0..DIM).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let y = 2.0 * phi[0] + 1.0 * phi[1] + rng.gen_range(-0.1..0.1);
            m.update(&phi, y);
        }

        // Noise variance should be approximately 0.1² = 0.01
        let noise_var = m.noise_variance();
        assert!(
            noise_var > 0.001 && noise_var < 0.1,
            "Adaptive noise should be near 0.01, got {}",
            noise_var
        );
    }

    #[test]
    fn test_divergence_detection() {
        let mut m = make_model();

        // First update with small features
        let phi_small = vec![0.001; DIM];
        m.update(&phi_small, 0.0);

        // Second update with huge target — should cause large weight change
        let phi = phi_ones();
        let result = m.update(&phi, 1e6);

        // With only 2 observations, a massive target can cause divergence
        if result.diverged {
            assert!(result.divergence_ratio > 2.0);
        }
    }

    #[test]
    fn test_divergence_ratio_no_false_positive() {
        let mut m = make_model();
        let mut rng = thread_rng();

        // Normal updates should not trigger divergence
        for _ in 0..100 {
            let phi: Vec<f64> = (0..DIM).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let y: f64 = rng.gen_range(-1.0..1.0);
            let result = m.update(&phi, y);
            if m.n_observations() > 1 {
                assert!(
                    !result.diverged,
                    "False positive divergence: ratio={}",
                    result.divergence_ratio
                );
            }
        }
    }

    #[test]
    fn test_reset() {
        let mut m = make_model();
        let phi = phi_ones();
        m.update(&phi, 1.0);
        assert!(m.n_observations() > 0);

        m.reset();
        assert_eq!(m.n_observations(), 0);
        assert!((m.predict(&phi) - 0.0).abs() < 1e-15);
        // Noise estimate preserved
        assert!(m.noise_variance() > 0.0);
    }

    #[test]
    fn test_reset_full() {
        let mut m = make_model();
        let phi = phi_ones();
        for _ in 0..10 {
            m.update(&phi, 5.0);
        }
        let noise_after_updates = m.noise_variance();

        m.reset_full(INITIAL_SIGMA2);
        assert_eq!(m.n_observations(), 0);
        assert!((m.noise_variance() - INITIAL_SIGMA2).abs() < 1e-15);
        assert_ne!(noise_after_updates, INITIAL_SIGMA2);
    }

    #[test]
    fn test_covariance_inflation() {
        let mut m = make_model();
        let phi = phi_ones();

        for _ in 0..10 {
            m.update(&phi, 1.0);
        }
        let std_before = m.posterior_std(&phi);

        m.inflate_covariance(2.0);
        let std_after = m.posterior_std(&phi);

        // σ_model = √(σ²_noise · φ^T A_inv φ)
        // A_inv *= 2 → posterior_std *= √2
        let ratio = std_after / std_before;
        assert!(
            (ratio - 2.0_f64.sqrt()).abs() < 0.01,
            "Inflation by 2.0 should increase std by √2: ratio={}",
            ratio
        );
    }

    #[test]
    fn test_covariance_inflation_factor_validation() {
        let mut m = make_model();
        m.inflate_covariance(1.5); // OK
        m.inflate_covariance(1.0); // OK
    }

    #[test]
    #[should_panic(expected = "Inflation factor must be >= 1.0")]
    fn test_covariance_inflation_rejects_below_one() {
        let mut m = make_model();
        m.inflate_covariance(0.9);
    }

    #[test]
    fn test_sample_weights_distribution() {
        let mut m = make_model();
        let phi = phi_ones();

        // Set known weights
        for _ in 0..10 {
            m.update(&phi, 1.0);
        }

        let w_hat = m.weights().clone();
        let mut rng = thread_rng();

        let n_samples = 2000;
        let mut sum = DVector::zeros(DIM);
        let mut sum_sq = DVector::zeros(DIM);

        for _ in 0..n_samples {
            let w_sampled = m.sample_weights(&mut rng);
            sum += &w_sampled;
            sum_sq += w_sampled.component_mul(&w_sampled);
        }

        let mean = sum / n_samples as f64;
        let variance = sum_sq / n_samples as f64 - mean.component_mul(&mean);

        // Sample mean should be close to w_hat
        for i in 0..DIM {
            assert!(
                (mean[i] - w_hat[i]).abs() < 0.1,
                "Sample mean[{}] = {}, expected ~{}",
                i,
                mean[i],
                w_hat[i]
            );
        }

        // Sample variance should be positive
        for i in 0..DIM {
            assert!(
                variance[i] > 0.0,
                "Sample variance[{}] should be positive",
                i
            );
        }
    }

    #[test]
    fn test_sample_predict_differs_from_point() {
        let mut m = make_model();
        let phi = phi_ones();
        for _ in 0..5 {
            m.update(&phi, 1.0);
        }

        let point_q = m.predict(&phi);
        let mut rng = thread_rng();
        let mut any_different = false;

        for _ in 0..50 {
            let sampled_q = m.sample_predict(&phi, &mut rng);
            if (sampled_q - point_q).abs() > 1e-10 {
                any_different = true;
                break;
            }
        }
        assert!(any_different, "Sampled Q should differ from point estimate");
    }

    #[test]
    fn test_optimistic_initialization() {
        let mut m = make_model();
        let bias = 0.1;
        m.apply_optimistic_bias(bias);

        // ŵ = bias * ones
        let phi = phi_ones();
        let q = m.predict(&phi);
        assert!(
            (q - bias * DIM as f64).abs() < 1e-10,
            "Optimistic bias: Q = {}, expected {}",
            q,
            bias * DIM as f64
        );
    }

    #[test]
    fn test_optimistic_bias_diluted_after_updates() {
        let mut m = make_model();
        m.apply_optimistic_bias(1.0);
        let biased_q = m.predict(&phi_ones());

        // Feed observations consistent with w=0: y = 0 for all phi
        let mut rng = thread_rng();
        for _ in 0..100 {
            let phi: Vec<f64> = (0..DIM).map(|_| rng.gen_range(-1.0..1.0)).collect();
            m.update(&phi, 0.0);
        }

        // After many zero-target observations, Q should have moved toward 0
        let current_q = m.predict(&phi_ones());
        assert!(
            current_q < biased_q,
            "Optimistic bias should dilute: biased={}, current={}",
            biased_q,
            current_q
        );
    }

    #[test]
    fn test_sherman_morrison_equivalence() {
        // Compare online (Sherman-Morrison) vs batch (direct inverse)
        let mut m_online = make_model();
        let mut rng = thread_rng();

        let observations: Vec<(Vec<f64>, f64)> = (0..20)
            .map(|_| {
                let phi: Vec<f64> = (0..DIM).map(|_| rng.gen_range(-1.0..1.0)).collect();
                let y: f64 = rng.gen_range(-1.0..1.0);
                (phi, y)
            })
            .collect();

        for (phi, y) in &observations {
            m_online.update(phi, *y);
        }

        // Batch computation
        let n = observations.len();
        let mut phi_matrix = DMatrix::zeros(DIM, n);
        let mut y_vec = DVector::zeros(n);
        for (j, (phi, y)) in observations.iter().enumerate() {
            for (i, v) in phi.iter().enumerate() {
                phi_matrix[(i, j)] = *v;
            }
            y_vec[j] = *y;
        }
        let a_batch =
            &phi_matrix * phi_matrix.transpose() + LAMBDA_REG * DMatrix::identity(DIM, DIM);
        let b_batch = &phi_matrix * &y_vec;
        let w_batch = a_batch.lu().solve(&b_batch).unwrap();

        for i in 0..DIM {
            assert!(
                (m_online.weights()[i] - w_batch[i]).abs() < 1e-8,
                "Online vs batch w[{}]: online={}, batch={}",
                i,
                m_online.weights()[i],
                w_batch[i]
            );
        }
    }

    #[test]
    fn test_update_result_residual() {
        let mut m = make_model();
        let phi = phi_ones();

        // Initially w=0, so y_pred=0, residual=y
        let result = m.update(&phi, 2.5);
        assert!((result.residual - 2.5).abs() < 1e-10);
    }

    // === QFunction tests ===

    fn make_qfunction() -> QFunction {
        QFunction::new(DIM, LAMBDA_REG, HALFLIFE, INITIAL_SIGMA2, 0.1)
    }

    #[test]
    fn test_qfunction_creation() {
        let qf = make_qfunction();
        assert_eq!(qf.dim(), DIM);

        // Buy and Sell should have optimistic bias
        let phi = phi_ones();
        let q_buy = qf.q_value(QAction::Buy, &phi);
        let q_sell = qf.q_value(QAction::Sell, &phi);
        let q_hold = qf.q_value(QAction::Hold, &phi);

        assert!(
            q_buy > q_hold,
            "Buy Q={} should be > Hold Q={}",
            q_buy,
            q_hold
        );
        assert!(
            q_sell > q_hold,
            "Sell Q={} should be > Hold Q={}",
            q_sell,
            q_hold
        );
        assert!((q_hold - 0.0).abs() < 1e-10, "Hold Q should be 0");
    }

    #[test]
    fn test_qfunction_update_single_action() {
        let mut qf = make_qfunction();
        let phi = phi_ones();

        qf.update(QAction::Buy, &phi, 1.0);

        // Only Buy model should have changed
        assert!(qf.model(QAction::Buy).n_observations() == 1);
        assert!(qf.model(QAction::Sell).n_observations() == 0);
        assert!(qf.model(QAction::Hold).n_observations() == 0);
    }

    #[test]
    fn test_qfunction_q_values_all() {
        let mut qf = make_qfunction();
        let phi = phi_ones();

        qf.update(QAction::Buy, &phi, 5.0);

        let q_map = qf.q_values(&phi);
        assert_eq!(q_map.len(), 3);
        assert!(q_map.contains_key(&QAction::Buy));
        assert!(q_map.contains_key(&QAction::Sell));
        assert!(q_map.contains_key(&QAction::Hold));
    }

    #[test]
    fn test_qfunction_sample_q_value_varies() {
        let qf = make_qfunction();
        let phi = phi_ones();
        let mut rng = thread_rng();

        let mut values = std::collections::HashSet::new();
        for _ in 0..20 {
            let q = qf.sample_q_value(QAction::Buy, &phi, &mut rng);
            values.insert((q * 1e6) as i64); // discretize for comparison
        }
        assert!(values.len() > 1, "Sampled Q-values should vary");
    }

    #[test]
    fn test_qfunction_posterior_stds() {
        let qf = make_qfunction();
        let phi = phi_ones();

        let stds = qf.posterior_stds(&phi);
        assert_eq!(stds.len(), 3);
        for (_, std_val) in &stds {
            assert!(*std_val >= 0.0);
        }
    }

    #[test]
    fn test_qfunction_reset_action() {
        let mut qf = make_qfunction();
        let phi = phi_ones();

        qf.update(QAction::Buy, &phi, 1.0);
        assert!(qf.model(QAction::Buy).n_observations() == 1);

        qf.reset_action(QAction::Buy);
        assert!(qf.model(QAction::Buy).n_observations() == 0);

        // Optimistic bias should be restored for Buy
        let q_buy = qf.q_value(QAction::Buy, &phi);
        let q_hold = qf.q_value(QAction::Hold, &phi);
        assert!(
            q_buy > q_hold,
            "Optimistic bias should be restored after reset"
        );
    }

    #[test]
    fn test_qfunction_reset_all() {
        let mut qf = make_qfunction();
        let phi = phi_ones();

        qf.update(QAction::Buy, &phi, 1.0);
        qf.update(QAction::Sell, &phi, 2.0);
        qf.update(QAction::Hold, &phi, 3.0);

        qf.reset_all();

        for &action in QAction::all() {
            assert_eq!(qf.model(action).n_observations(), 0);
        }

        // Optimistic bias restored for buy/sell
        assert!(qf.q_value(QAction::Buy, &phi) > qf.q_value(QAction::Hold, &phi));
        assert!(qf.q_value(QAction::Sell, &phi) > qf.q_value(QAction::Hold, &phi));
    }

    #[test]
    fn test_qfunction_reset_all_full() {
        let mut qf = make_qfunction();
        let phi = phi_ones();

        for _ in 0..50 {
            qf.update(QAction::Buy, &phi, 10.0);
        }
        let noise_after = qf.model(QAction::Buy).noise_variance();

        qf.reset_all_full();
        let noise_after_reset = qf.model(QAction::Buy).noise_variance();

        assert!((noise_after_reset - INITIAL_SIGMA2).abs() < 1e-15);
        // Noise changed after updates
        assert!((noise_after - INITIAL_SIGMA2).abs() > 1e-6);
    }

    #[test]
    fn test_qfunction_inflate_covariance() {
        let mut qf = make_qfunction();
        let phi = phi_ones();

        for _ in 0..10 {
            qf.update(QAction::Buy, &phi, 1.0);
        }

        let std_before = qf.posterior_std(QAction::Buy, &phi);
        qf.inflate_covariance(3.0);
        let std_after = qf.posterior_std(QAction::Buy, &phi);

        let ratio = std_after / std_before;
        assert!(
            (ratio - 3.0_f64.sqrt()).abs() < 0.01,
            "Ratio should be √3: {}",
            ratio
        );
    }

    #[test]
    fn test_qfunction_point_equals_q_value() {
        let qf = make_qfunction();
        let phi = phi_ones();

        for &action in QAction::all() {
            assert!(
                (qf.q_value(action, &phi) - qf.q_point(action, &phi)).abs() < 1e-15,
                "q_value should equal q_point"
            );
        }
    }

    #[test]
    fn test_qfunction_with_feature_vector_dim() {
        let qf = QFunction::new(
            FeatureVector::DIM,
            LAMBDA_REG,
            HALFLIFE,
            INITIAL_SIGMA2,
            0.1,
        );
        assert_eq!(qf.dim(), FeatureVector::DIM);

        let fv = FeatureVector::zero();
        let phi = fv.flattened();

        // Should not panic
        let _ = qf.q_value(QAction::Buy, &phi);
        let _ = qf.posterior_std(QAction::Hold, &phi);
        let _ = qf.q_values(&phi);
    }

    #[test]
    fn test_noise_variance_floor() {
        let mut m = BayesianLinearRegression::new(DIM, LAMBDA_REG, HALFLIFE, 0.001);

        // Feed many zero-residual observations
        let phi_zero = vec![0.0; DIM];
        for _ in 0..5000 {
            m.update(&phi_zero, 0.0);
        }

        // Noise variance should not go below floor
        assert!(m.noise_variance() >= 1e-10);
    }

    #[test]
    fn test_sample_weights_diagonal_approximation_no_panic() {
        // n_observations=0 < dim=5 → diagonal approximation should not panic
        let m = BayesianLinearRegression::new(DIM, LAMBDA_REG, HALFLIFE, INITIAL_SIGMA2);
        assert_eq!(m.n_observations(), 0);

        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let w = m.sample_weights(&mut rng);

        // Should produce a vector of correct dimension
        assert_eq!(w.len(), DIM);
        // All values should be finite
        for i in 0..DIM {
            assert!(
                w[i].is_finite(),
                "sampled weight[{}] = {} is not finite",
                i,
                w[i]
            );
        }
        // Should differ from pure point estimate (diagonal adds variance)
        let w_hat = m.weights();
        let all_same = (0..DIM).all(|i| (w[i] - w_hat[i]).abs() < 1e-15);
        assert!(
            !all_same,
            "diagonal approximation should add variance, not just return point estimate"
        );
    }

    #[test]
    fn test_sample_weights_diagonal_then_cholesky_transition() {
        let mut m = BayesianLinearRegression::new(DIM, LAMBDA_REG, HALFLIFE, INITIAL_SIGMA2);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        // Before enough observations: diagonal approximation
        for i in 0..(DIM - 1) {
            let phi = vec![1.0; DIM];
            m.update(&phi, (i as f64) * 0.1);
        }
        assert!(m.n_observations() < DIM);
        let w_diag = m.sample_weights(&mut rng);
        assert_eq!(w_diag.len(), DIM);

        // After enough observations: full Cholesky
        let phi = vec![1.0; DIM];
        m.update(&phi, 1.0);
        assert!(m.n_observations() >= DIM);
        let w_chol = m.sample_weights(&mut rng);
        assert_eq!(w_chol.len(), DIM);

        // Both should produce finite values
        for i in 0..DIM {
            assert!(w_diag[i].is_finite());
            assert!(w_chol[i].is_finite());
        }
    }

    #[test]
    fn test_bayesian_lr_snapshot_roundtrip() {
        let mut model = make_model();
        let phi = vec![1.0, -0.5, 0.25, 0.75, -1.25];
        for target in [0.5, -0.25, 1.5, 0.8] {
            model.update(&phi, target);
        }

        let snapshot = model.snapshot();
        let restored = BayesianLinearRegression::from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.dim(), model.dim());
        assert_eq!(restored.n_observations(), model.n_observations());
        assert!((restored.noise_variance() - model.noise_variance()).abs() < 1e-12);
        for (restored_weight, original_weight) in
            restored.weights().iter().zip(model.weights().iter())
        {
            assert!((restored_weight - original_weight).abs() < 1e-12);
        }
        assert!((restored.predict(&phi) - model.predict(&phi)).abs() < 1e-12);
        assert!((restored.posterior_std(&phi) - model.posterior_std(&phi)).abs() < 1e-12);
    }

    #[test]
    fn test_qfunction_snapshot_roundtrip() {
        let mut q_function = make_qfunction();
        let phi = vec![0.5, -1.0, 0.25, 0.75, 1.25];

        q_function.update(QAction::Buy, &phi, 1.0);
        q_function.update(QAction::Sell, &phi, -0.7);
        q_function.update(QAction::Hold, &phi, 0.1);

        let snapshot = q_function.snapshot();
        let restored = QFunction::from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.dim(), q_function.dim());
        for &action in QAction::all() {
            assert_eq!(
                restored.model(action).n_observations(),
                q_function.model(action).n_observations()
            );
            assert!(
                (restored.q_value(action, &phi) - q_function.q_value(action, &phi)).abs() < 1e-12
            );
        }
    }

    #[test]
    fn test_qfunction_snapshot_rejects_missing_action() {
        let mut snapshot = make_qfunction().snapshot();
        snapshot.actions.pop();

        let err = QFunction::from_snapshot(&snapshot).unwrap_err().to_string();
        assert!(err.contains("must contain"));
    }
}
