# design.md未実装機能 - Product Requirements Document

## Overview

design.md v15.0と現在の実装の差分を埋め、バックテスト・フォワードテスト・ペーパートレードがdesign.md仕様通りに完全動作する状態にする。既存の4PRD（prd.md全28タスク、prd-forward.md全11タスク、prd-verification.md全30タスク、prd-backtest-fix.md全8タスク）は全てパス済み。本PRDは残りの設計-実装ギャップに対応する。

## Target Audience

- **主利用者**: システム開発者
- **目的**: design.md仕様との完全な一致、バックテスト/フォワードテストの信頼性向上
- **前提**: 実際のトレードは行わない。FIX/WebSocket実装は対象外

## Gap Summary

| # | ギャップ | 影響 | 設計書セクション |
|---|---------|------|-----------------|
| 1 | HDP-HMM Pythonバグ (drift/training) | Regime推論不能 | §3.0.1 |
| 2 | 特徴量交互作用項2個不足 (OBI×vol, spread_z×self_impact) | 特徴量ベクトル不完全 | §3.4 |
| 3 | Dynamic K Calculator未実装 (固定値0.1) | 不確実性ペナルティ非適応 | §4.1 |
| 4 | GapDetector取引停止未配線 (常にfalse) | 深刻ギャップ時も取引継続 | §7.1 |
| 5 | PreFailureMetrics未配線 (構造体のみ) | 観測性なし | §8.2 |
| 6 | ONNX Runtime統合なし (ort crate不在) | Python→Rust推論パイプライン不通 | §3.0.1 |
| 7 | バックテストのヒューリスティックRegime → ONNXモデル切替 | Regime品質が設計未達 | §3.0.1 |
| 8 | フォワードテストのRegime → ONNXモデル切替 | 同上 | §3.0.1 |

## Tech Stack

- **Rust**: ort crate (ONNX Runtime bindings), 既存ワークスペース
- **Python**: onnx, onnxruntime (依存追加), 既存research/
- **テスト**: cargo test, pytest research/tests/

## Architecture

### 変更範囲

```
research/models/hdp_hmm.py          → バグ修正
research/models/onnx_export.py      → 依存解消
crates/strategy/src/features.rs     → 交互作用項2個追加 (34→36次元)
crates/strategy/src/extractor.rs    → 抽出ロジック追加
crates/strategy/src/thompson_sampling.rs → Dynamic K Calculator
crates/events/src/gap_detector.rs   → 取引停止ロジック
crates/strategy/src/regime.rs       → ONNX モデルローダー
crates/backtest/src/engine.rs       → ONNX Regime切替 + PreFailure配線
crates/forward/src/runner.rs        → ONNX Regime切替 + PreFailure配線
```

### 影響を受ける定数

- `FeatureVector::DIM`: 34 → 36
- `STRATEGY_A_EXTRA_DIM`: 5 (変更なし)
- `STRATEGY_A_FEATURE_DIM`: 39 → 41
- 戦略B/Cも同様に+2

---

## Task List

```json
[
  {
    "category": "bugfix",
    "description": "HDP-HMM compute_drift() の形状バグ修正",
    "steps": [
      "research/models/hdp_hmm.py line 130: posteriorが(n_regimes,)だがprev_driftとのブロードキャストが正しくない",
      "drift計算: new_drift[d] = ar_coeff * sum_k(posterior[k] * prev_drift[k,d]) に修正",
      "features引数が未使用 (line 258のcompute_driftも) → design.md §3.0.1のdrift_t = sum_k(pi_k * f_k(drift_{t-1}, X_t)) に従いfeaturesを組み込む",
      "pytest research/tests/test_hdp_hmm.py で全テスト通過確認"
    ],
    "passes": true
  },
  {
    "category": "bugfix",
    "description": "HDP-HMM train_hdp_hmm_online() のブロードキャストバグ修正",
    "steps": [
      "research/models/hdp_hmm.py lines 196-199: residual = posterior[k] - posterior が(n_regimes,) → x[np.newaxis,:] との乗算で(n_regimes, feature_dim)になる",
      "勾配計算をスカラーまたは正しい形状に修正: params.weights[k]の更新は(feature_dim,)であるべき",
      "pytest research/tests/test_hdp_hmm.py::TestTrainHdpHmmOnline で全テスト通過確認"
    ],
    "passes": true
  },
  {
    "category": "dependency",
    "description": "Python依存パッケージ追加 (onnx, onnxruntime, scikit-learn)",
    "steps": [
      "pyproject.toml または requirements.txt に onnx, onnxruntime, scikit-learn を追加",
      "pip install でインストール確認",
      "pytest research/tests/test_environment.py で依存チェック通過確認",
      "onnxエクスポートテストが通ることを確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "特徴量交互作用項2個追加: OBI×vol, spread_z×self_impact (FeatureVector 34→36次元)",
    "steps": [
      "features.rs の FeatureVector に obi_x_vol: f64 と spread_z_x_self_impact: f64 フィールドを追加",
      "FeatureVector::DIM を 36 に更新",
      "flattened() と from_flattened() を更新",
      "extractor.rs の extract() で obi_x_vol = obi * realized_volatility, spread_z_x_self_impact = spread_zscore * self_impact を計算",
      "zero() でデフォルト値を設定",
      "戦略A/B/Cの FEATURE_DIM と EXTRA_DIM 定数を更新 (+2)",
      "既存テストが通ることを確認 (cargo test -p fx-strategy)"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Dynamic K Calculator: volatility/regime依存の動的k係数",
    "steps": [
      "thompson_sampling.rs に compute_dynamic_k(volatility: f64, regime_stability: f64) -> f64 関数を追加",
      "design.md §4.1: k = Dynamic_K_Calculator(volatility, regime_stability)",
      "実装: k = base_k * (1.0 + volatility_scale * volatility) * regime_multiplier(regime_stability)",
      "高ボラティリティ・低regime安定性 → k増大 → より保守的",
      "ThompsonSamplingConfig の non_model_uncertainty_k を base_k に変更し、decide()内で動的計算",
      "テスト: 低vol → 低k、高vol → 高k、低安定性 → 高k"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "GapDetector取引停止ロジックの配線",
    "steps": [
      "gap_detector.rs の is_trading_halted() を Severeギャップ検出時にtrueを返すよう実装",
      "内部状態に halted: bool を追加し、Severe検出時にtrue、一定期間(設定可能)後に自動解除または外部リセット",
      "gap_detector.rs に reset_halt() メソッドを追加",
      "バックテストエンジン/フォワードテストランナーのメインループで is_trading_halted() をチェック",
      "halted時: 全戦略hold強制 + TradeSkipEvent(reason: GAP_DETECTED) 発行",
      "テスト: Severeギャップ → is_trading_halted()=true、Minor → false、リセット → false"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "PreFailureMetrics のライブ配線 (バックテスト・フォワードテスト)",
    "steps": [
      "バックテストエンジンのメインループ内で PreFailureMetrics を各コンポーネントから収集",
      "rolling_variance_latency → kill_switch stats",
      "execution_drift_trend → execution gateway stats",
      "self_impact_ratio, liquidity_evolvement → feature extractor",
      "policy_entropy, regime_posterior_entropy → regime cache",
      "daily/weekly/monthly_pnl_vs_limit → risk limiter state",
      "bayesian_posterior_drift → BLR posterior tracking",
      "ObservabilityManager::tick() を各ループで呼び出し",
      "フォワードテストランナーにも同様の配線",
      "テスト: バックテスト実行後、ObservabilityManagerのtotal_ticks > 0"
    ],
    "passes": true
  },
  {
    "category": "dependency",
    "description": "ort crate (ONNX Runtime) をRustワークスペースに追加",
    "steps": [
      "ワークスペース Cargo.toml の [workspace.dependencies] に ort = { version = \"2\", features = [\"load-dynamic\"] } を追加",
      "crates/strategy/Cargo.toml に ort = { workspace = true } を追加",
      "cargo build でコンパイル確認",
      "ORT_DYLIB_PATH 環境変数の設定方法をREADMEに追記"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "ONNXモデルローダー: regime.rsにRegimeModelLoader実装",
    "steps": [
      "regime.rs に OnnxRegimeModel 構造体を追加: ONNXセッション + メタデータ保持",
      "load_from_path(path: &str) -> Result<Self>: ONNXファイルからモデルをロード",
      "predict(features: &[f64]) -> RegimeState: 特徴量→事後確率・エントロピー・KL",
      "RegimeCache に onnx_model: Option<OnnxRegimeModel> を追加",
      "RegimeConfig に model_path: Option<String> を追加",
      "model_pathがSomeの場合はONNXモデルを使用、Noneの場合は従来のヒューリスティック",
      "テスト: ダミーONNXモデルでのロード・推論確認"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "HDP-HMMモデル訓練→ONNXエクスポート→Rust推論のE2Eパイプライン",
    "steps": [
      "Python: 合成データでHDP-HMMパラメータ訓練 → ONNXエクスポートのE2Eテスト作成",
      "Rust: エクスポートされたONNXモデルをロードしてregime推論",
      "bridge CLI: rust-backtest結果 → Python validation → ONNX regime model → Rust loading の統合テスト",
      "モデルファイルの配置規約: research/models/onnx/regime_v1.onnx",
      "テスト: pytest + cargo test でパイプライン全体が通ることを確認"
    ],
    "passes": false
  },
  {
    "category": "integration",
    "description": "バックテストエンジンのヒューリスティックRegime → ONNXモデル切替",
    "steps": [
      "BacktestConfig に regime_model_path: Option<String> を追加",
      "engine.rs の初期化で model_pathが指定された場合、OnnxRegimeModelを構築",
      "run_inner() のregime更新ロジックでONNXモデルが有効な場合はそちらを使用",
      "フォールバック: ONNXモデルなしの場合は従来のヒューリスティック",
      "テスト: ONNXモデルなしで既存テスト全通過 (後方互換)",
      "テスト: ONNXモデルありでregime推論が行われる"
    ],
    "passes": false
  },
  {
    "category": "integration",
    "description": "フォワードテストのRegime → ONNXモデル切替",
    "steps": [
      "ForwardTestConfig の risk_config に regime_model_path を伝播",
      "ForwardTestRunner の初期化でRegimeCacheにONNXモデルを設定",
      "テスト: ONNXモデルなしで既存テスト全通過",
      "テスト: ONNXモデルありでフォワードテストが正常完了"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §5 統計的検証パイプラインのE2E実行確認",
    "steps": [
      "バックテスト結果をJSON出力 → Python bridge → 統計検証パイプライン実行",
      "CPCV, PBO, DSR, Sharpe天井, 複雑度ペナルティ, 情報リーク検証が全て実行可能",
      "pytest research/tests/test_e2e_validation.py が全通過",
      "CLI: cargo run --bin fx-cli -- validate --backtest-result <JSON> で実行確認",
      "テスト結果のpass/failは戦略品質に依存するが、パイプライン自体がエラーなく完走することを確認"
    ],
    "passes": false
  },
  {
    "category": "integration",
    "description": "フルパイプライン統合テスト: CSV → Backtest → Validation → Report",
    "steps": [
      "合成CSVデータまたは実データでフルバックテスト実行",
      "特徴量36次元 + 戦略固有5次元 = 41次元が正しく計算される",
      "Dynamic K がvolatility/regimeに応じて変動する",
      "GapDetectorがSevere gapで取引を停止する",
      "PreFailureMetricsが観測される (total_ticks > 0)",
      "RegimeがONNXモデル (利用可能場合) またはヒューリスティックで動作",
      "結果をJSON出力 → Python検証パイプラインで処理",
      "フォワードテストも同様のフルパイプラインで動作",
      "全テスト通過確認 (cargo test, pytest)"
    ],
    "passes": false
  }
]
```

---

## Agent Instructions

1. Read `activity-design-gap.md` first to understand current state
2. Find next task with `"passes": false`
3. Complete all steps for that task
4. Verify with tests (`cargo test`, `cargo clippy`, `pytest research/tests/`)
5. Update task to `"passes": true`
6. Log completion in `activity-design-gap.md`
7. Commit with `feat(gap): ...` or `fix(gap): ...` prefix
8. Repeat until all tasks pass

**Important:** Only modify the `passes` field. Do not remove or rewrite tasks.

---

## Completion Criteria

All 14 tasks marked with `"passes": true`
