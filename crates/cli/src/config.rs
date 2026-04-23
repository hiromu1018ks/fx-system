use anyhow::{Context, Result};
use fx_core::random::expand_u64_seed;
use std::path::Path;

use fx_backtest::engine::BacktestConfig;

/// Load a BacktestConfig from a TOML file.
///
/// Supports top-level fields and nested tables:
/// - Top-level: symbol, global_position_limit, default_lot_size, replay_speed,
///   start_time_ns, end_time_ns, rng_seed
/// - `[strategy_a]`, `[strategy_b]`, `[strategy_c]`: per-strategy trigger thresholds
/// - `[mc_eval.reward]`: lambda_risk, lambda_dd, dd_cap, gamma
/// - `[risk_limits]`: daily/weekly/monthly loss limits
/// - `[barrier]`: staleness-based lot reduction
/// - `[kill_switch]`: tick-interval anomaly detection
/// - `[lifecycle]`: strategy culling thresholds
/// - `[regime]`: regime parameters, including optional ONNX `model_path`
/// - `[feature_extractor]`: feature pipeline windows
/// - `[global_position]`: cross-strategy position constraints
///
/// Missing fields use defaults.
pub fn load_backtest_config(path: &Path) -> Result<BacktestConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let raw: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config TOML: {}", path.display()))?;

    let mut config = BacktestConfig::default();
    let table = match raw.as_table() {
        Some(t) => t,
        None => return Ok(config),
    };

    // Top-level fields
    if let Some(v) = table.get("symbol").and_then(|v| v.as_str()) {
        config.symbol = v.to_string();
    }
    if let Some(v) = table
        .get("global_position_limit")
        .and_then(|v| v.as_float())
    {
        config.global_position_limit = v;
    }
    if let Some(v) = table.get("default_lot_size").and_then(|v| v.as_integer()) {
        config.default_lot_size = v as u64;
    }
    if let Some(v) = table.get("replay_speed").and_then(|v| v.as_float()) {
        config.replay_speed = v;
    }
    if let Some(v) = table.get("start_time_ns").and_then(|v| v.as_integer()) {
        config.start_time_ns = v as u64;
    }
    if let Some(v) = table.get("end_time_ns").and_then(|v| v.as_integer()) {
        config.end_time_ns = v as u64;
    }
    if let Some(v) = table.get("rng_seed").and_then(|v| v.as_integer()) {
        config.rng_seed = Some(expand_u64_seed(v as u64));
    }

    // Strategy A config
    if let Some(sa) = table.get("strategy_a").and_then(|v| v.as_table()) {
        apply_strategy_a(&mut config.strategy_a_config, sa);
    }
    // Strategy B config
    if let Some(sb) = table.get("strategy_b").and_then(|v| v.as_table()) {
        apply_strategy_b(&mut config.strategy_b_config, sb);
    }
    // Strategy C config
    if let Some(sc) = table.get("strategy_c").and_then(|v| v.as_table()) {
        apply_strategy_c(&mut config.strategy_c_config, sc);
    }

    // MC eval / reward
    if let Some(mc) = table.get("mc_eval").and_then(|v| v.as_table()) {
        if let Some(rew) = mc.get("reward").and_then(|v| v.as_table()) {
            f64_field(rew, "lambda_risk", |v| {
                config.mc_eval_config.reward.lambda_risk = v
            });
            f64_field(rew, "lambda_dd", |v| {
                config.mc_eval_config.reward.lambda_dd = v
            });
            f64_field(rew, "dd_cap", |v| config.mc_eval_config.reward.dd_cap = v);
            f64_field(rew, "gamma", |v| config.mc_eval_config.reward.gamma = v);
        }
    }

    // Risk limits
    if let Some(rl) = table.get("risk_limits").and_then(|v| v.as_table()) {
        f64_field(rl, "max_daily_loss_mtm", |v| {
            config.risk_limits_config.max_daily_loss_mtm = v
        });
        f64_field(rl, "max_daily_loss_realized", |v| {
            config.risk_limits_config.max_daily_loss_realized = v
        });
        f64_field(rl, "max_weekly_loss", |v| {
            config.risk_limits_config.max_weekly_loss = v
        });
        f64_field(rl, "max_monthly_loss", |v| {
            config.risk_limits_config.max_monthly_loss = v
        });
        f64_field(rl, "daily_mtm_lot_fraction", |v| {
            config.risk_limits_config.daily_mtm_lot_fraction = v
        });
        f64_field(rl, "daily_mtm_q_threshold", |v| {
            config.risk_limits_config.daily_mtm_q_threshold = v
        });
    }

    // Barrier
    if let Some(b) = table.get("barrier").and_then(|v| v.as_table()) {
        u64_field(b, "staleness_threshold_ms", |v| {
            config.barrier_config.staleness_threshold_ms = v
        });
        f64_field(b, "warning_threshold_ratio", |v| {
            config.barrier_config.warning_threshold_ratio = v
        });
        f64_field(b, "min_lot_multiplier", |v| {
            config.barrier_config.min_lot_multiplier = v
        });
        u64_field(b, "default_lot_size", |v| {
            config.barrier_config.default_lot_size = v
        });
        u64_field(b, "max_lot_size", |v| {
            config.barrier_config.max_lot_size = v
        });
        u64_field(b, "min_lot_size", |v| {
            config.barrier_config.min_lot_size = v
        });
    }

    // Kill switch
    if let Some(ks) = table.get("kill_switch").and_then(|v| v.as_table()) {
        usize_field(ks, "min_samples", |v| {
            config.kill_switch_config.min_samples = v
        });
        f64_field(ks, "z_score_threshold", |v| {
            config.kill_switch_config.z_score_threshold = v
        });
        usize_field(ks, "max_history", |v| {
            config.kill_switch_config.max_history = v
        });
        u64_field(ks, "mask_duration_ms", |v| {
            config.kill_switch_config.mask_duration_ms = v
        });
        bool_field(ks, "enabled", |v| config.kill_switch_config.enabled = v);
    }

    // Lifecycle
    if let Some(lc) = table.get("lifecycle").and_then(|v| v.as_table()) {
        usize_field(lc, "rolling_window", |v| {
            config.lifecycle_config.rolling_window = v
        });
        usize_field(lc, "min_episodes_for_eval", |v| {
            config.lifecycle_config.min_episodes_for_eval = v
        });
        f64_field(lc, "death_sharpe_threshold", |v| {
            config.lifecycle_config.death_sharpe_threshold = v
        });
        u32_field(lc, "consecutive_death_windows", |v| {
            config.lifecycle_config.consecutive_death_windows = v
        });
        f64_field(lc, "sharpe_annualization_factor", |v| {
            config.lifecycle_config.sharpe_annualization_factor = v
        });
        bool_field(lc, "strict_unknown_regime", |v| {
            config.lifecycle_config.strict_unknown_regime = v
        });
        f64_field(lc, "unknown_regime_sharpe_multiplier", |v| {
            config.lifecycle_config.unknown_regime_sharpe_multiplier = v
        });
        bool_field(lc, "auto_close_culled_positions", |v| {
            config.lifecycle_config.auto_close_culled_positions = v
        });
    }

    // Regime
    if let Some(rg) = table.get("regime").and_then(|v| v.as_table()) {
        usize_field(rg, "n_regimes", |v| config.regime_config.n_regimes = v);
        f64_field(rg, "unknown_regime_entropy_threshold", |v| {
            config.regime_config.unknown_regime_entropy_threshold = v
        });
        f64_field(rg, "regime_ar_coeff", |v| {
            config.regime_config.regime_ar_coeff = v
        });
        usize_field(rg, "feature_dim", |v| config.regime_config.feature_dim = v);
        if let Some(v) = rg.get("model_path").and_then(|v| v.as_str()) {
            config.regime_config.model_path = Some(v.to_string());
        }
    }

    // Feature extractor
    if let Some(fe) = table.get("feature_extractor").and_then(|v| v.as_table()) {
        usize_field(fe, "spread_window", |v| {
            config.feature_extractor_config.spread_window = v
        });
        usize_field(fe, "obi_window", |v| {
            config.feature_extractor_config.obi_window = v
        });
        usize_field(fe, "vol_window", |v| {
            config.feature_extractor_config.vol_window = v
        });
        usize_field(fe, "vol_long_window", |v| {
            config.feature_extractor_config.vol_long_window = v
        });
        u64_field(fe, "trade_intensity_window_ns", |v| {
            config.feature_extractor_config.trade_intensity_window_ns = v
        });
        u64_field(fe, "execution_lag_ns", |v| {
            config.feature_extractor_config.execution_lag_ns = v
        });
        f64_field(fe, "default_decay_rate", |v| {
            config.feature_extractor_config.default_decay_rate = v
        });
        f64_field(fe, "typical_lot_size", |v| {
            config.feature_extractor_config.typical_lot_size = v
        });
        f64_field(fe, "max_hold_time_ms", |v| {
            config.feature_extractor_config.max_hold_time_ms = v
        });
    }

    // Global position
    if let Some(gp) = table.get("global_position").and_then(|v| v.as_table()) {
        f64_field(gp, "correlation_factor", |v| {
            config.global_position_config.correlation_factor = v
        });
        f64_field(gp, "floor_correlation", |v| {
            config.global_position_config.floor_correlation = v
        });
        f64_field(gp, "lot_unit_size", |v| {
            config.global_position_config.lot_unit_size = v
        });
        f64_field(gp, "min_lot_size", |v| {
            config.global_position_config.min_lot_size = v
        });
        if let Some(smp) = gp.get("strategy_max_positions").and_then(|v| v.as_table()) {
            use fx_core::types::StrategyId;
            use std::collections::HashMap;
            let mut map = HashMap::new();
            if let Some(v) = smp.get("A").and_then(|v| v.as_float()) {
                map.insert(StrategyId::A, v);
            }
            if let Some(v) = smp.get("B").and_then(|v| v.as_float()) {
                map.insert(StrategyId::B, v);
            }
            if let Some(v) = smp.get("C").and_then(|v| v.as_float()) {
                map.insert(StrategyId::C, v);
            }
            if !map.is_empty() {
                config.global_position_config.strategy_max_positions = map;
            }
        }
    }

    Ok(config)
}

// ---------------------------------------------------------------------------
// Helpers for extracting typed fields from TOML tables
// ---------------------------------------------------------------------------

fn f64_field(table: &toml::map::Map<String, toml::Value>, key: &str, apply: impl FnOnce(f64)) {
    if let Some(v) = table.get(key).and_then(|v| v.as_float()) {
        apply(v);
    }
}

fn u64_field(table: &toml::map::Map<String, toml::Value>, key: &str, apply: impl FnOnce(u64)) {
    if let Some(v) = table.get(key).and_then(|v| v.as_integer()) {
        apply(v as u64);
    }
}

fn u32_field(table: &toml::map::Map<String, toml::Value>, key: &str, apply: impl FnOnce(u32)) {
    if let Some(v) = table.get(key).and_then(|v| v.as_integer()) {
        apply(v as u32);
    }
}

fn usize_field(table: &toml::map::Map<String, toml::Value>, key: &str, apply: impl FnOnce(usize)) {
    if let Some(v) = table.get(key).and_then(|v| v.as_integer()) {
        apply(v as usize);
    }
}

fn bool_field(table: &toml::map::Map<String, toml::Value>, key: &str, apply: impl FnOnce(bool)) {
    if let Some(v) = table.get(key).and_then(|v| v.as_bool()) {
        apply(v);
    }
}

fn apply_strategy_a(
    cfg: &mut fx_strategy::strategy_a::StrategyAConfig,
    table: &toml::map::Map<String, toml::Value>,
) {
    f64_field(table, "spread_z_threshold", |v| cfg.spread_z_threshold = v);
    f64_field(table, "depth_drop_threshold", |v| {
        cfg.depth_drop_threshold = v
    });
    f64_field(table, "vol_spike_threshold", |v| {
        cfg.vol_spike_threshold = v
    });
    f64_field(table, "regime_kl_threshold", |v| {
        cfg.regime_kl_threshold = v
    });
    u64_field(table, "max_hold_time_ms", |v| cfg.max_hold_time_ms = v);
    f64_field(table, "decay_rate_a", |v| cfg.decay_rate_a = v);
    f64_field(table, "lambda_reg", |v| cfg.lambda_reg = v);
    usize_field(table, "halflife", |v| cfg.halflife = v);
    f64_field(table, "initial_sigma2", |v| cfg.initial_sigma2 = v);
    f64_field(table, "optimistic_bias", |v| cfg.optimistic_bias = v);
    f64_field(table, "non_model_uncertainty_k", |v| {
        cfg.non_model_uncertainty_k = v
    });
    f64_field(table, "latency_penalty_k", |v| cfg.latency_penalty_k = v);
    f64_field(table, "min_trade_frequency", |v| {
        cfg.min_trade_frequency = v
    });
    usize_field(table, "trade_frequency_window", |v| {
        cfg.trade_frequency_window = v
    });
    f64_field(table, "hold_degeneration_inflation", |v| {
        cfg.hold_degeneration_inflation = v
    });
    f64_field(table, "inflation_decay_rate", |v| {
        cfg.inflation_decay_rate = v
    });
    u64_field(table, "max_lot_size", |v| cfg.max_lot_size = v);
    u64_field(table, "min_lot_size", |v| cfg.min_lot_size = v);
    f64_field(table, "consistency_threshold", |v| {
        cfg.consistency_threshold = v
    });
    u64_field(table, "default_lot_size", |v| cfg.default_lot_size = v);
}

fn apply_strategy_b(
    cfg: &mut fx_strategy::strategy_b::StrategyBConfig,
    table: &toml::map::Map<String, toml::Value>,
) {
    f64_field(table, "vol_spike_threshold", |v| {
        cfg.vol_spike_threshold = v
    });
    f64_field(table, "vol_decaying_threshold", |v| {
        cfg.vol_decaying_threshold = v
    });
    f64_field(table, "obi_alignment_threshold", |v| {
        cfg.obi_alignment_threshold = v
    });
    f64_field(table, "regime_kl_threshold", |v| {
        cfg.regime_kl_threshold = v
    });
    u64_field(table, "max_hold_time_ms", |v| cfg.max_hold_time_ms = v);
    f64_field(table, "decay_rate_b", |v| cfg.decay_rate_b = v);
    f64_field(table, "lambda_reg", |v| cfg.lambda_reg = v);
    usize_field(table, "halflife", |v| cfg.halflife = v);
    f64_field(table, "initial_sigma2", |v| cfg.initial_sigma2 = v);
    f64_field(table, "optimistic_bias", |v| cfg.optimistic_bias = v);
    f64_field(table, "non_model_uncertainty_k", |v| {
        cfg.non_model_uncertainty_k = v
    });
    f64_field(table, "latency_penalty_k", |v| cfg.latency_penalty_k = v);
    f64_field(table, "min_trade_frequency", |v| {
        cfg.min_trade_frequency = v
    });
    usize_field(table, "trade_frequency_window", |v| {
        cfg.trade_frequency_window = v
    });
    f64_field(table, "hold_degeneration_inflation", |v| {
        cfg.hold_degeneration_inflation = v
    });
    f64_field(table, "inflation_decay_rate", |v| {
        cfg.inflation_decay_rate = v
    });
    u64_field(table, "max_lot_size", |v| cfg.max_lot_size = v);
    u64_field(table, "min_lot_size", |v| cfg.min_lot_size = v);
    f64_field(table, "consistency_threshold", |v| {
        cfg.consistency_threshold = v
    });
    u64_field(table, "default_lot_size", |v| cfg.default_lot_size = v);
}

fn apply_strategy_c(
    cfg: &mut fx_strategy::strategy_c::StrategyCConfig,
    table: &toml::map::Map<String, toml::Value>,
) {
    f64_field(table, "session_active_threshold", |v| {
        cfg.session_active_threshold = v
    });
    f64_field(table, "obi_significance_threshold", |v| {
        cfg.obi_significance_threshold = v
    });
    f64_field(table, "max_session_open_ms", |v| {
        cfg.max_session_open_ms = v
    });
    f64_field(table, "regime_kl_threshold", |v| {
        cfg.regime_kl_threshold = v
    });
    u64_field(table, "max_hold_time_ms", |v| cfg.max_hold_time_ms = v);
    f64_field(table, "decay_rate_c", |v| cfg.decay_rate_c = v);
    f64_field(table, "lambda_reg", |v| cfg.lambda_reg = v);
    usize_field(table, "halflife", |v| cfg.halflife = v);
    f64_field(table, "initial_sigma2", |v| cfg.initial_sigma2 = v);
    f64_field(table, "optimistic_bias", |v| cfg.optimistic_bias = v);
    f64_field(table, "non_model_uncertainty_k", |v| {
        cfg.non_model_uncertainty_k = v
    });
    f64_field(table, "latency_penalty_k", |v| cfg.latency_penalty_k = v);
    f64_field(table, "min_trade_frequency", |v| {
        cfg.min_trade_frequency = v
    });
    usize_field(table, "trade_frequency_window", |v| {
        cfg.trade_frequency_window = v
    });
    f64_field(table, "hold_degeneration_inflation", |v| {
        cfg.hold_degeneration_inflation = v
    });
    f64_field(table, "inflation_decay_rate", |v| {
        cfg.inflation_decay_rate = v
    });
    u64_field(table, "max_lot_size", |v| cfg.max_lot_size = v);
    u64_field(table, "min_lot_size", |v| cfg.min_lot_size = v);
    f64_field(table, "consistency_threshold", |v| {
        cfg.consistency_threshold = v
    });
    u64_field(table, "default_lot_size", |v| cfg.default_lot_size = v);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_load_backtest_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
symbol = "EUR/USD"
global_position_limit = 5.0
default_lot_size = 50000
replay_speed = 10.0
"#
        )
        .unwrap();

        let config = load_backtest_config(&path).unwrap();
        assert_eq!(config.symbol, "EUR/USD");
        assert!((config.global_position_limit - 5.0).abs() < 1e-10);
        assert_eq!(config.default_lot_size, 50000);
        assert!((config.replay_speed - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_load_backtest_config_full_nested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full_config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
symbol = "GBP/JPY"
global_position_limit = 8.0
default_lot_size = 50000
rng_seed = 42

[strategy_a]
spread_z_threshold = 5.0
max_hold_time_ms = 60000

[strategy_b]
vol_spike_threshold = 3.0
max_hold_time_ms = 600000

[strategy_c]
session_active_threshold = 0.7
max_hold_time_ms = 900000

[mc_eval.reward]
lambda_risk = 0.2
lambda_dd = 0.8
dd_cap = 200.0
gamma = 0.95

[risk_limits]
max_daily_loss_mtm = -300.0
max_daily_loss_realized = -800.0

[barrier]
staleness_threshold_ms = 3000

[kill_switch]
enabled = false
z_score_threshold = 2.5

[lifecycle]
death_sharpe_threshold = -1.0
rolling_window = 100

[regime]
unknown_regime_entropy_threshold = 2.0
feature_dim = 38
model_path = "research/models/onnx/regime_v1.onnx"

[feature_extractor]
spread_window = 100
vol_window = 50

[global_position]
correlation_factor = 1.2
strategy_max_positions = {{ A = 3.0, B = 4.0, C = 5.0 }}
"#
        )
        .unwrap();

        let config = load_backtest_config(&path).unwrap();
        assert_eq!(config.symbol, "GBP/JPY");
        assert_eq!(config.default_lot_size, 50000);
        assert!(config.rng_seed.is_some());

        // Strategy A overrides
        assert!((config.strategy_a_config.spread_z_threshold - 5.0).abs() < 1e-10);
        assert_eq!(config.strategy_a_config.max_hold_time_ms, 60000);

        // Strategy B overrides
        assert!((config.strategy_b_config.vol_spike_threshold - 3.0).abs() < 1e-10);

        // Strategy C overrides
        assert!((config.strategy_c_config.session_active_threshold - 0.7).abs() < 1e-10);

        // MC eval
        assert!((config.mc_eval_config.reward.lambda_risk - 0.2).abs() < 1e-10);
        assert!((config.mc_eval_config.reward.gamma - 0.95).abs() < 1e-10);

        // Risk limits
        assert!((config.risk_limits_config.max_daily_loss_mtm - (-300.0)).abs() < 1e-10);

        // Barrier
        assert_eq!(config.barrier_config.staleness_threshold_ms, 3000);

        // Kill switch
        assert!(!config.kill_switch_config.enabled);
        assert!((config.kill_switch_config.z_score_threshold - 2.5).abs() < 1e-10);

        // Lifecycle
        assert!((config.lifecycle_config.death_sharpe_threshold - (-1.0)).abs() < 1e-10);
        assert_eq!(config.lifecycle_config.rolling_window, 100);

        // Regime
        assert!((config.regime_config.unknown_regime_entropy_threshold - 2.0).abs() < 1e-10);
        assert_eq!(config.regime_config.feature_dim, 38);
        assert_eq!(
            config.regime_config.model_path.as_deref(),
            Some("research/models/onnx/regime_v1.onnx")
        );

        // Feature extractor
        assert_eq!(config.feature_extractor_config.spread_window, 100);

        // Global position
        assert!((config.global_position_config.correlation_factor - 1.2).abs() < 1e-10);
        use fx_core::types::StrategyId;
        assert_eq!(
            config.global_position_config.strategy_max_positions[&StrategyId::A],
            3.0
        );
    }

    #[test]
    fn test_load_backtest_config_defaults_on_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();

        let config = load_backtest_config(&path).unwrap();
        assert_eq!(config.symbol, "USD/JPY");
        assert_eq!(config.default_lot_size, 100_000);
    }

    #[test]
    fn test_load_backtest_config_file_not_found() {
        let result = load_backtest_config(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_backtest_config_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml [[[[").unwrap();
        let result = load_backtest_config(&path);
        assert!(result.is_err());
    }
}
