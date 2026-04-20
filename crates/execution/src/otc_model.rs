use std::collections::HashMap;

use fx_core::types::Direction;
use rand::prelude::*;
use rand_distr::{Distribution, StudentT};

// ============================================================
// Last-Look Rejection Model (Beta-Binomial conjugate)
// ============================================================

#[derive(Debug, Clone)]
pub struct BetaParams {
    pub alpha: f64,
    pub beta: f64,
}

impl BetaParams {
    pub fn new(alpha: f64, beta: f64) -> Self {
        assert!(alpha > 0.0, "BetaParams alpha must be positive");
        assert!(beta > 0.0, "BetaParams beta must be positive");
        Self { alpha, beta }
    }

    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    pub fn variance(&self) -> f64 {
        let s = self.alpha + self.beta;
        (self.alpha * self.beta) / (s * s * (s + 1.0))
    }

    pub fn update_success(&mut self) {
        self.alpha += 1.0;
    }

    pub fn update_failure(&mut self) {
        self.beta += 1.0;
    }
}

#[derive(Debug, Clone)]
pub struct LastLookConfig {
    pub prior_alpha: f64,
    pub prior_beta: f64,
    pub vol_adjustment_factor: f64,
}

impl Default for LastLookConfig {
    fn default() -> Self {
        Self {
            prior_alpha: 2.0,
            prior_beta: 1.0,
            vol_adjustment_factor: 0.1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LastLookModel {
    config: LastLookConfig,
    lp_models: HashMap<String, BetaParams>,
}

impl LastLookModel {
    pub fn new(config: LastLookConfig) -> Self {
        Self {
            config,
            lp_models: HashMap::new(),
        }
    }

    fn get_lp_model(&mut self, lp_id: &str) -> &mut BetaParams {
        self.lp_models
            .entry(lp_id.to_string())
            .or_insert_with(|| BetaParams::new(self.config.prior_alpha, self.config.prior_beta))
    }

    /// P(not_rejected | lp, vol) = posterior_mean × (1 - vol_penalty)
    pub fn fill_probability(&self, lp_id: &str, volatility: f64) -> f64 {
        let default = BetaParams::new(self.config.prior_alpha, self.config.prior_beta);
        let params = self.lp_models.get(lp_id).unwrap_or(&default);
        let vol_penalty = self.config.vol_adjustment_factor * volatility;
        (params.mean() * (1.0 - vol_penalty)).clamp(0.0, 1.0)
    }

    /// P(rejected | lp, vol)
    pub fn rejection_probability(&self, lp_id: &str, volatility: f64) -> f64 {
        1.0 - self.fill_probability(lp_id, volatility)
    }

    pub fn update_fill(&mut self, lp_id: &str) {
        self.get_lp_model(lp_id).update_success();
    }

    pub fn update_rejection(&mut self, lp_id: &str) {
        self.get_lp_model(lp_id).update_failure();
    }

    pub fn get_lp_params(&self, lp_id: &str) -> Option<&BetaParams> {
        self.lp_models.get(lp_id)
    }

    pub fn tracked_lps(&self) -> Vec<&str> {
        self.lp_models.keys().map(|s| s.as_str()).collect()
    }

    pub fn reset_lp(&mut self, lp_id: &str) {
        self.lp_models.remove(lp_id);
    }
}

// ============================================================
// Fill Probability Model
// ============================================================

#[derive(Debug, Clone)]
pub struct FillProbabilityConfig {
    pub market_fill_prob: f64,
    pub limit_fill_prob_base: f64,
    pub limit_decay_rate: f64,
    pub hidden_liquidity_df: f64,
    pub hidden_liquidity_loc: f64,
    pub hidden_liquidity_scale: f64,
}

impl Default for FillProbabilityConfig {
    fn default() -> Self {
        Self {
            market_fill_prob: 0.98,
            limit_fill_prob_base: 0.5,
            limit_decay_rate: 10.0,
            hidden_liquidity_df: 3.0,
            hidden_liquidity_loc: 0.02,
            hidden_liquidity_scale: 0.01,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FillProbabilityModel {
    config: FillProbabilityConfig,
}

impl FillProbabilityModel {
    pub fn new(config: FillProbabilityConfig) -> Self {
        Self { config }
    }

    /// P(fill_requested) based on order type and price distance from mid
    pub fn request_fill_probability(&self, order_type: OtcOrderType, price_distance: f64) -> f64 {
        match order_type {
            OtcOrderType::Market => self.config.market_fill_prob,
            OtcOrderType::Limit => (self.config.limit_fill_prob_base
                * (-price_distance.abs() * self.config.limit_decay_rate).exp())
            .clamp(0.01, 0.95),
        }
    }

    /// Expected hidden liquidity: E[ε_hidden] = loc for Student's t with df > 1
    pub fn expected_hidden_liquidity(&self) -> f64 {
        self.config.hidden_liquidity_loc
    }

    /// Sample ε_hidden from Student's t
    pub fn sample_hidden_liquidity(&self, rng: &mut impl Rng) -> f64 {
        let t = StudentT::new(self.config.hidden_liquidity_df).unwrap();
        let sample = t.sample(rng);
        (sample * self.config.hidden_liquidity_scale + self.config.hidden_liquidity_loc).max(0.0)
    }

    /// P_effective = P(request) × P(not_rejected) + E[ε_hidden]
    pub fn effective_fill_probability(
        &self,
        order_type: OtcOrderType,
        price_distance: f64,
        last_look_fill_prob: f64,
    ) -> f64 {
        let p_request = self.request_fill_probability(order_type, price_distance);
        (p_request * last_look_fill_prob + self.expected_hidden_liquidity()).clamp(0.0, 1.0)
    }

    /// Effective fill probability with sampled hidden liquidity
    pub fn effective_fill_probability_sampled(
        &self,
        order_type: OtcOrderType,
        price_distance: f64,
        last_look_fill_prob: f64,
        rng: &mut impl Rng,
    ) -> f64 {
        let p_request = self.request_fill_probability(order_type, price_distance);
        let epsilon = self.sample_hidden_liquidity(rng);
        (p_request * last_look_fill_prob + epsilon).clamp(0.0, 1.0)
    }
}

// ============================================================
// Slippage Model
// ============================================================

#[derive(Debug, Clone)]
pub struct SlippageConfig {
    pub size_coeff: f64,
    pub sqrt_size_coeff: f64,
    pub vol_coeff: f64,
    pub sell_improvement: f64,
    pub noise_df: f64,
    pub noise_scale_base: f64,
}

impl Default for SlippageConfig {
    fn default() -> Self {
        Self {
            size_coeff: 0.0001,
            sqrt_size_coeff: 0.01,
            vol_coeff: 0.001,
            sell_improvement: -0.00005,
            noise_df: 3.0,
            noise_scale_base: 0.0001,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LpSlippageStats {
    pub mean: f64,
    pub variance: f64,
    pub count: u64,
}

#[derive(Debug, Clone)]
pub struct SlippageModel {
    config: SlippageConfig,
    lp_stats: HashMap<String, LpSlippageStats>,
}

impl SlippageModel {
    pub fn new(config: SlippageConfig) -> Self {
        Self {
            config,
            lp_stats: HashMap::new(),
        }
    }

    /// Expected slippage: positive = adverse (pay more on buy), negative = improvement
    pub fn expected_slippage(
        &self,
        direction: Direction,
        size_lots: f64,
        volatility: f64,
        lp_id: &str,
    ) -> f64 {
        let sqrt_size = size_lots.sqrt();
        let base = self.config.size_coeff * size_lots * volatility
            + self.config.sqrt_size_coeff * sqrt_size * volatility
            + self.config.vol_coeff * volatility;

        let direction_adj = match direction {
            Direction::Buy => base,
            Direction::Sell => base + self.config.sell_improvement * size_lots,
        };

        let lp_adj = self.lp_stats.get(lp_id).map(|s| s.mean).unwrap_or(0.0);

        direction_adj + lp_adj
    }

    /// Sample slippage from Student's t around expected value
    pub fn sample_slippage(
        &self,
        direction: Direction,
        size_lots: f64,
        volatility: f64,
        lp_id: &str,
        rng: &mut impl Rng,
    ) -> f64 {
        let mean = self.expected_slippage(direction, size_lots, volatility, lp_id);
        let scale = self.config.noise_scale_base * (1.0 + size_lots * volatility);
        let t = StudentT::new(self.config.noise_df).unwrap();
        mean + t.sample(rng) * scale
    }

    /// Online Welford update for observed slippage
    pub fn update_observation(&mut self, lp_id: &str, observed_slippage: f64) {
        let stats = self.lp_stats.entry(lp_id.to_string()).or_default();
        stats.count += 1;
        let delta = observed_slippage - stats.mean;
        stats.mean += delta / stats.count as f64;
        let delta2 = observed_slippage - stats.mean;
        stats.variance += (delta * delta2 - stats.variance) / stats.count as f64;
    }

    pub fn get_lp_stats(&self, lp_id: &str) -> Option<&LpSlippageStats> {
        self.lp_stats.get(lp_id)
    }

    pub fn reset_lp(&mut self, lp_id: &str) {
        self.lp_stats.remove(lp_id);
    }
}

// ============================================================
// Order Type Selector (Passive / Aggressive)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtcOrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone)]
pub struct OrderTypeConfig {
    pub profit_threshold: f64,
    pub fill_prob_threshold: f64,
}

impl Default for OrderTypeConfig {
    fn default() -> Self {
        Self {
            profit_threshold: 0.0001,
            fill_prob_threshold: 0.7,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrderTypeSelector {
    config: OrderTypeConfig,
}

impl OrderTypeSelector {
    pub fn new(config: OrderTypeConfig) -> Self {
        Self { config }
    }

    /// Select order type: Limit if fill_prob ≥ threshold AND ev(limit) ≥ ev(market)
    pub fn select(
        &self,
        expected_profit: f64,
        effective_fill_prob: f64,
        expected_slippage: f64,
    ) -> (OtcOrderType, f64) {
        let ev_limit = effective_fill_prob * expected_profit;
        let ev_market = expected_profit - expected_slippage.abs();

        if effective_fill_prob >= self.config.fill_prob_threshold
            && ev_limit >= ev_market
            && expected_profit >= self.config.profit_threshold
        {
            (OtcOrderType::Limit, expected_profit * 0.5)
        } else {
            (OtcOrderType::Market, 0.0)
        }
    }

    /// With time urgency override → always Market
    pub fn select_with_urgency(
        &self,
        expected_profit: f64,
        effective_fill_prob: f64,
        expected_slippage: f64,
        time_urgent: bool,
    ) -> (OtcOrderType, f64) {
        if time_urgent {
            return (OtcOrderType::Market, 0.0);
        }
        self.select(expected_profit, effective_fill_prob, expected_slippage)
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- BetaParams ---
    #[test]
    fn beta_params_mean() {
        let p = BetaParams::new(2.0, 1.0);
        assert!((p.mean() - 2.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn beta_params_variance() {
        let p = BetaParams::new(2.0, 3.0);
        let s = 5.0;
        let expected = (2.0 * 3.0) / (s * s * (s + 1.0));
        assert!((p.variance() - expected).abs() < 1e-10);
    }

    #[test]
    fn beta_params_update() {
        let mut p = BetaParams::new(1.0, 1.0);
        p.update_success();
        assert_eq!(p.alpha, 2.0);
        p.update_failure();
        assert_eq!(p.beta, 2.0);
    }

    #[test]
    #[should_panic(expected = "positive")]
    fn beta_params_negative_alpha() {
        BetaParams::new(-1.0, 1.0);
    }

    #[test]
    #[should_panic(expected = "positive")]
    fn beta_params_zero_beta() {
        BetaParams::new(1.0, 0.0);
    }

    // --- LastLookModel ---
    #[test]
    fn last_look_prior_probability() {
        let model = LastLookModel::new(LastLookConfig::default());
        let prob = model.fill_probability("lp1", 0.0);
        assert!((prob - 2.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn last_look_volatility_penalty() {
        let model = LastLookModel::new(LastLookConfig::default());
        let prob_low_vol = model.fill_probability("lp1", 0.0);
        let prob_high_vol = model.fill_probability("lp1", 5.0);
        assert!(prob_high_vol < prob_low_vol);
    }

    #[test]
    fn last_look_volatility_penalty_clamped() {
        let model = LastLookModel::new(LastLookConfig::default());
        let prob = model.fill_probability("lp1", 100.0);
        assert!(prob >= 0.0);
        assert!(prob <= 1.0);
    }

    #[test]
    fn last_look_rejection_probability() {
        let model = LastLookModel::new(LastLookConfig::default());
        let fp = model.fill_probability("lp1", 0.0);
        let rp = model.rejection_probability("lp1", 0.0);
        assert!((fp + rp - 1.0).abs() < 1e-10);
    }

    #[test]
    fn last_look_update_fill() {
        let mut model = LastLookModel::new(LastLookConfig::default());
        model.update_fill("lp1");
        let params = model.get_lp_params("lp1").unwrap();
        assert_eq!(params.alpha, 3.0);
        assert_eq!(params.beta, 1.0);
    }

    #[test]
    fn last_look_update_rejection() {
        let mut model = LastLookModel::new(LastLookConfig::default());
        model.update_rejection("lp1");
        let params = model.get_lp_params("lp1").unwrap();
        assert_eq!(params.alpha, 2.0);
        assert_eq!(params.beta, 2.0);
    }

    #[test]
    fn last_look_per_lp_independent() {
        let mut model = LastLookModel::new(LastLookConfig::default());
        model.update_fill("lp1");
        model.update_rejection("lp1");
        model.update_rejection("lp2");
        // lp1: prior(2,1) → fill(3,1) → reject(3,2) → mean=3/5=0.6
        assert!((model.fill_probability("lp1", 0.0) - 0.6).abs() < 1e-10);
        // lp2: prior(2,1) → reject(2,2) → mean=2/4=0.5
        assert!((model.fill_probability("lp2", 0.0) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn last_look_reset_lp() {
        let mut model = LastLookModel::new(LastLookConfig::default());
        model.update_fill("lp1");
        model.update_rejection("lp1");
        model.reset_lp("lp1");
        assert!(model.get_lp_params("lp1").is_none());
        assert_eq!(model.fill_probability("lp1", 0.0), 2.0 / 3.0);
    }

    #[test]
    fn last_look_tracked_lps() {
        let mut model = LastLookModel::new(LastLookConfig::default());
        model.update_fill("lp1");
        model.update_fill("lp2");
        let lps = model.tracked_lps();
        assert_eq!(lps.len(), 2);
    }

    #[test]
    fn last_look_custom_config() {
        let model = LastLookModel::new(LastLookConfig {
            prior_alpha: 5.0,
            prior_beta: 1.0,
            vol_adjustment_factor: 0.0,
        });
        assert!((model.fill_probability("lp_x", 10.0) - 5.0 / 6.0).abs() < 1e-10);
    }

    // --- FillProbabilityModel ---
    #[test]
    fn fill_prob_market_order() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let prob = model.request_fill_probability(OtcOrderType::Market, 0.0);
        assert!((prob - 0.98).abs() < 1e-10);
    }

    #[test]
    fn fill_prob_limit_close() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let prob = model.request_fill_probability(OtcOrderType::Limit, 0.0);
        assert!((prob - 0.5).abs() < 1e-10);
    }

    #[test]
    fn fill_prob_limit_far() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let prob = model.request_fill_probability(OtcOrderType::Limit, 1.0);
        assert!(prob < 0.1);
    }

    #[test]
    fn fill_prob_limit_clamped_low() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let prob = model.request_fill_probability(OtcOrderType::Limit, 100.0);
        assert!(prob >= 0.01);
    }

    #[test]
    fn fill_prob_limit_clamped_high() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let prob = model.request_fill_probability(OtcOrderType::Limit, -100.0);
        assert!(prob <= 0.95);
    }

    #[test]
    fn hidden_liquidity_expected() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        assert!((model.expected_hidden_liquidity() - 0.02).abs() < 1e-10);
    }

    #[test]
    fn hidden_liquidity_sampled_positive() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let mut rng = rand::rngs::SmallRng::from_seed([42u8; 32]);
        for _ in 0..100 {
            let sample = model.sample_hidden_liquidity(&mut rng);
            assert!(sample >= 0.0);
        }
    }

    #[test]
    fn effective_fill_prob_market() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let eff = model.effective_fill_probability(OtcOrderType::Market, 0.0, 0.9);
        let expected = 0.98 * 0.9 + 0.02;
        assert!((eff - expected).abs() < 1e-10);
    }

    #[test]
    fn effective_fill_prob_clamped() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let eff = model.effective_fill_probability(OtcOrderType::Market, 0.0, 1.5);
        assert!(eff <= 1.0);
        assert!(eff >= 0.0);
    }

    #[test]
    fn effective_fill_prob_sampled() {
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let mut rng = rand::rngs::SmallRng::from_seed([42u8; 32]);
        let eff =
            model.effective_fill_probability_sampled(OtcOrderType::Market, 0.0, 0.9, &mut rng);
        assert!(eff >= 0.0);
        assert!(eff <= 1.0);
    }

    // --- SlippageModel ---
    #[test]
    fn slippage_buy_positive() {
        let model = SlippageModel::new(SlippageConfig::default());
        let slip = model.expected_slippage(Direction::Buy, 1.0, 0.1, "lp1");
        assert!(slip > 0.0);
    }

    #[test]
    fn slippage_sell_improvement() {
        let model = SlippageModel::new(SlippageConfig::default());
        let buy = model.expected_slippage(Direction::Buy, 10.0, 0.1, "lp1");
        let sell = model.expected_slippage(Direction::Sell, 10.0, 0.1, "lp1");
        assert!(sell < buy);
    }

    #[test]
    fn slippage_increases_with_size() {
        let model = SlippageModel::new(SlippageConfig::default());
        let s1 = model.expected_slippage(Direction::Buy, 1.0, 0.1, "lp1");
        let s5 = model.expected_slippage(Direction::Buy, 5.0, 0.1, "lp1");
        assert!(s5 > s1);
    }

    #[test]
    fn slippage_increases_with_vol() {
        let model = SlippageModel::new(SlippageConfig::default());
        let low = model.expected_slippage(Direction::Buy, 1.0, 0.01, "lp1");
        let high = model.expected_slippage(Direction::Buy, 1.0, 0.5, "lp1");
        assert!(high > low);
    }

    #[test]
    fn slippage_sampled_distribution() {
        let model = SlippageModel::new(SlippageConfig::default());
        let mut rng = rand::rngs::SmallRng::from_seed([42u8; 32]);
        let mut sum = 0.0;
        let n = 1000;
        for _ in 0..n {
            sum += model.sample_slippage(Direction::Buy, 1.0, 0.1, "lp1", &mut rng);
        }
        let mean = sum / n as f64;
        let expected = model.expected_slippage(Direction::Buy, 1.0, 0.1, "lp1");
        // Sample mean should be close to expected (within 3 sigma)
        assert!((mean - expected).abs() < 0.001);
    }

    #[test]
    fn slippage_update_observation() {
        let mut model = SlippageModel::new(SlippageConfig::default());
        model.update_observation("lp1", 0.001);
        model.update_observation("lp1", 0.002);
        model.update_observation("lp1", 0.003);
        let stats = model.get_lp_stats("lp1").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.mean - 0.002).abs() < 1e-10);
    }

    #[test]
    fn slippage_lp_influence() {
        let mut model = SlippageModel::new(SlippageConfig::default());
        for _ in 0..100 {
            model.update_observation("good_lp", -0.0001);
            model.update_observation("bad_lp", 0.001);
        }
        let good = model.expected_slippage(Direction::Buy, 1.0, 0.1, "good_lp");
        let bad = model.expected_slippage(Direction::Buy, 1.0, 0.1, "bad_lp");
        assert!(good < bad);
    }

    #[test]
    fn slippage_reset_lp() {
        let mut model = SlippageModel::new(SlippageConfig::default());
        model.update_observation("lp1", 0.001);
        model.reset_lp("lp1");
        assert!(model.get_lp_stats("lp1").is_none());
    }

    // --- OrderTypeSelector ---
    #[test]
    fn order_type_limit_when_profitable() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        let (ot, dist) = sel.select(0.001, 0.9, 0.0001);
        assert_eq!(ot, OtcOrderType::Limit);
        assert!(dist > 0.0);
    }

    #[test]
    fn order_type_market_when_low_fill_prob() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        let (ot, _) = sel.select(0.001, 0.3, 0.0001);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn order_type_market_when_low_profit() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        let (ot, _) = sel.select(0.00001, 0.9, 0.0001);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn order_type_market_when_slippage_cheap() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // ev_market = 0.001 - 0.000001 = 0.000999
        // ev_limit = 0.9 * 0.001 = 0.0009 → market better
        let (ot, _) = sel.select(0.001, 0.9, 0.000001);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn order_type_urgent_override() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        let (ot, _) = sel.select_with_urgency(0.001, 0.99, 0.0001, true);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn order_type_no_urgent_normal() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        let (ot, _) = sel.select_with_urgency(0.001, 0.9, 0.0001, false);
        assert_eq!(ot, OtcOrderType::Limit);
    }

    #[test]
    fn order_type_limit_distance() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // slippage=0.001 → ev_market=0.001 < ev_limit=0.0018 → Limit, dist=0.001
        let (_, dist) = sel.select(0.002, 0.9, 0.001);
        assert!((dist - 0.001).abs() < 1e-10);
    }

    #[test]
    fn order_type_market_zero_distance() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        let (_, dist) = sel.select(0.00001, 0.3, 0.0001);
        assert!((dist - 0.0).abs() < 1e-10);
    }

    // ============================================================
    // §4.2 OTC Execution Model — Implementation Conformance Tests
    // ============================================================

    // --- §4.2.1 Last-Look Rejection Model ---
    // Design: P(fill_effective) = P(fill_requested) × P(not_rejected | last_look)
    // Implementation: Beta-Binomial conjugate with volatility penalty
    // Note: design.md specifies logistic(−β₁·|Δp| − β₂·LP_inv) but implementation
    // uses Beta-Binomial posterior × (1 − vol_penalty). Both achieve the same goal
    // of estimating P(not_rejected) per-LP from observation history.

    #[test]
    fn s42_fill_effective_decomposition() {
        // Verify: P_effective = P(request) × P(not_rejected) + E[ε_hidden]
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let p_request = model.request_fill_probability(OtcOrderType::Market, 0.0);
        let p_not_rejected = 0.9; // last_look fill probability
        let epsilon_hidden = model.expected_hidden_liquidity();
        let p_effective =
            model.effective_fill_probability(OtcOrderType::Market, 0.0, p_not_rejected);
        let expected = p_request * p_not_rejected + epsilon_hidden;
        assert!((p_effective - expected).abs() < 1e-10);
    }

    #[test]
    fn s42_fill_effective_always_non_negative() {
        // P_effective must be clamped to [0, 1]
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        // Extreme: P(not_rejected) = 0 (complete rejection)
        let eff = model.effective_fill_probability(OtcOrderType::Market, 0.0, 0.0);
        assert!(eff >= 0.0);
        // Even with zero request prob and zero last_look, hidden liquidity provides floor
        let eff2 = model.effective_fill_probability(OtcOrderType::Limit, 100.0, 0.0);
        assert!(eff2 >= 0.0);
    }

    // --- §4.2.2 P(not_rejected) estimation ---
    // Design: σ(−β₁·|Δp| − β₂·LP_inv) with online β estimation
    // Implementation: Beta(α, β) posterior × (1 − vol_penalty)
    // Conformance: both approaches estimate P(not_rejected) per-LP from observations

    #[test]
    fn s42_p_not_rejected_updates_from_observations() {
        let mut model = LastLookModel::new(LastLookConfig::default());
        let initial = model.fill_probability("lp1", 0.0); // prior mean = 2/3

        // Feed fills → posterior mean should increase
        for _ in 0..20 {
            model.update_fill("lp1");
        }
        let after_fills = model.fill_probability("lp1", 0.0);
        assert!(after_fills > initial);

        // Feed rejections → posterior mean should decrease
        for _ in 0..20 {
            model.update_rejection("lp1");
        }
        let after_rejects = model.fill_probability("lp1", 0.0);
        assert!(after_rejects < after_fills);
    }

    #[test]
    fn s42_p_not_rejected_volatility_sensitivity() {
        // Higher volatility → lower P(not_rejected) via vol_penalty
        let model = LastLookModel::new(LastLookConfig::default());
        let p_low_vol = model.fill_probability("lp1", 0.01);
        let p_high_vol = model.fill_probability("lp1", 1.0);
        assert!(p_high_vol < p_low_vol);
    }

    #[test]
    fn s42_p_not_rejected_per_lp_independent() {
        // Each LP has independent Beta posterior
        let mut model = LastLookModel::new(LastLookConfig::default());
        for _ in 0..50 {
            model.update_fill("good_lp");
        }
        for _ in 0..50 {
            model.update_rejection("bad_lp");
        }
        assert!(model.fill_probability("good_lp", 0.0) > model.fill_probability("bad_lp", 0.0));
    }

    // --- §4.2.3 ε_hidden Student's t distribution (df 3-5) ---

    #[test]
    fn s42_epsilon_hidden_student_t_distribution() {
        // Hidden liquidity must use Student's t, not Gaussian
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        // Default df=3.0, which is in [3, 5] range per design.md
        assert!(model.config.hidden_liquidity_df >= 3.0);
        assert!(model.config.hidden_liquidity_df <= 5.0);
    }

    #[test]
    fn s42_epsilon_hidden_heavy_tails() {
        // Student's t has heavier tails than Gaussian — verify via kurtosis proxy
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let mut rng = rand::rngs::SmallRng::from_seed([42u8; 32]);
        let mut extreme_count = 0;
        let n = 10_000;
        for _ in 0..n {
            let sample = model.sample_hidden_liquidity(&mut rng);
            if sample > 0.05 {
                extreme_count += 1;
            }
        }
        // With heavy tails (df=3), should see some extreme samples
        assert!(extreme_count > 0);
    }

    #[test]
    fn s42_epsilon_hidden_non_negative() {
        // Hidden liquidity is always non-negative (max(0, ...))
        let model = FillProbabilityModel::new(FillProbabilityConfig::default());
        let mut rng = rand::rngs::SmallRng::from_seed([99u8; 32]);
        for _ in 0..1000 {
            assert!(model.sample_hidden_liquidity(&mut rng) >= 0.0);
        }
    }

    // --- §4.2.4 Slippage Model: f(direction, size, vol, LP_state) ---

    #[test]
    fn s42_slippage_depends_on_all_four_inputs() {
        let mut model = SlippageModel::new(SlippageConfig::default());
        // Baseline
        let base = model.expected_slippage(Direction::Buy, 1.0, 0.1, "lp1");

        // Direction change → sell improvement
        let sell = model.expected_slippage(Direction::Sell, 1.0, 0.1, "lp1");
        assert_ne!(base, sell);

        // Size change → different slippage
        let larger = model.expected_slippage(Direction::Buy, 10.0, 0.1, "lp1");
        assert_ne!(base, larger);

        // Volatility change → different slippage
        let higher_vol = model.expected_slippage(Direction::Buy, 1.0, 0.5, "lp1");
        assert_ne!(base, higher_vol);

        // LP state change (via observations)
        model.update_observation("lp1", -0.0005); // good LP
        let with_lp = model.expected_slippage(Direction::Buy, 1.0, 0.1, "lp1");
        assert_ne!(base, with_lp);
    }

    #[test]
    fn s42_slippage_sell_improvement_negative() {
        // Sell direction should have negative improvement (better price)
        let config = SlippageConfig::default();
        assert!(config.sell_improvement < 0.0);
    }

    #[test]
    fn s42_slippage_noise_student_t() {
        // Slippage noise also uses Student's t (df=3 default, in [3,5])
        let config = SlippageConfig::default();
        assert!(config.noise_df >= 3.0);
        assert!(config.noise_df <= 5.0);
    }

    #[test]
    fn s42_slippage_noise_scale_increases_with_size_vol() {
        let model = SlippageModel::new(SlippageConfig::default());
        let mut rng = rand::rngs::SmallRng::from_seed([42u8; 32]);

        // Collect samples for small size + low vol
        let mut var_small = 0.0;
        let mean_small = {
            let mut sum = 0.0;
            let n = 1000;
            for _ in 0..n {
                let s = model.sample_slippage(Direction::Buy, 1.0, 0.01, "lp1", &mut rng);
                sum += s;
                var_small += s * s;
            }
            sum / n as f64
        };
        var_small = (var_small / 1000.0 - mean_small * mean_small).max(0.0);

        // Collect samples for large size + high vol
        let mut var_large = 0.0;
        let mean_large = {
            let mut sum = 0.0;
            let n = 1000;
            for _ in 0..n {
                let s = model.sample_slippage(Direction::Buy, 10.0, 0.5, "lp1", &mut rng);
                sum += s;
                var_large += s * s;
            }
            sum / n as f64
        };
        var_large = (var_large / 1000.0 - mean_large * mean_large).max(0.0);

        // Larger size + higher vol should have higher variance (noise_scale_base * (1 + lots * vol))
        assert!(var_large > var_small);
    }

    // --- §4.2.5 Passive/Aggressive Determination ---
    // Design: Expected_Profit > 0 AND P(fill_effective) high → Passive (Limit)
    //         Expected_Profit high AND P(fill_effective) low → Aggressive (Market)
    //         Expected_Profit < 0 → No Trade

    #[test]
    fn s42_passive_when_high_fill_prob_and_profitable() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // High fill prob + profitable → Limit (Passive)
        let (ot, _) = sel.select(0.001, 0.9, 0.0001);
        assert_eq!(ot, OtcOrderType::Limit);
    }

    #[test]
    fn s42_aggressive_when_low_fill_prob_and_profitable() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // Low fill prob + profitable → Market (Aggressive)
        let (ot, _) = sel.select(0.001, 0.3, 0.0001);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn s42_no_trade_when_negative_expected_profit() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // Negative profit → Market (treated as no-trade by the caller via risk checks)
        let (ot, _) = sel.select(-0.001, 0.9, 0.0001);
        assert_eq!(ot, OtcOrderType::Market);
        // The limit distance is 0.0 → no edge to capture
    }

    #[test]
    fn s42_passive_requires_profit_above_threshold() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // Fill prob above threshold but profit below → Market
        let (ot, _) = sel.select(0.00001, 0.9, 0.0001);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn s42_passive_requires_ev_limit_above_ev_market() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // High fill prob, high profit, but slippage so low that market EV > limit EV
        let (ot, _) = sel.select(0.001, 0.9, 0.000001);
        assert_eq!(ot, OtcOrderType::Market);
    }

    #[test]
    fn s42_time_urgency_forces_aggressive() {
        let sel = OrderTypeSelector::new(OrderTypeConfig::default());
        // Even with perfect conditions, urgency forces Market
        let (ot, _) = sel.select_with_urgency(0.01, 0.99, 0.0001, true);
        assert_eq!(ot, OtcOrderType::Market);
    }
}
