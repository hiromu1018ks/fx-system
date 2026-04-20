# design.md未実装機能 - Activity Log

## Current Status
**Last Updated:** 2026-04-20
**Tasks Completed:** 10
**Current Task:** Task 11 — バックテスト ONNX Regime切替

---

## Session Log

<!-- Agent will append dated entries here -->

### 2026-04-20: Task 1 — HDP-HMM compute_drift() の形状バグ修正

**What changed:**
- `research/models/hdp_hmm.py`: `compute_drift()` を修正
  - `prev_drift` の形状を `(feature_dim,)` → `(n_regimes, feature_dim)` に変更（per-regime drift vectors）
  - `features` 引数を実際に使用: `drift_k = ar * prev_drift_k + (1 - ar) * X_t`
  - 戻り値を `(n_regimes, feature_dim)` per-regime drift vectors に変更
  - `aggregate_drift(posterior, per_regime_drift)` ヘルパー関数を追加: `drift = sum_k(posterior_k * drift_k)` で集約
- `research/tests/test_hdp_hmm.py`: `TestComputeDrift` テストを更新（4件→8件）
  - `test_zero_prev_drift_with_features`: features統合の検証（0.1 * ones）
  - `test_decay`: per-regime AR(1)減衰の検証
  - `test_per_regime_independence`: 各regimeのdrift独立性検証
  - `test_aggregate_drift`: 集約関数の正確性検証
  - `test_weighted_aggregation`: posterior加重集約の検証
  - `test_shape`: 出力形状 (n_regimes, feature_dim) 検証
  - `test_features_integration`: features項の係数検証
  - `test_ar_coeff_one_ignores_features`: ar=1.0でfeatures無視の検証

**Commands run:**
- `pytest research/tests/test_hdp_hmm.py -v` — compute_drift 8/8 passed, train_hdp_hmm_online 2件失敗（事前存在バグ、Task 2で対応）, ONNX export 3件失敗（依存未インストール、Task 3で対応）
- `cargo test -p fx-strategy --lib -- regime` — 42 passed, 0 failed

**Issues:** Rust側 `regime.rs` の `compute_drift()` にも同様の形状バグが存在するが、本タスクはPython側のみ対応。Rust側はTask 7以降のONNX統合時に修正予定

### 2026-04-20: Task 2 — HDP-HMM train_hdp_hmm_online() のブロードキャストバグ修正

**What changed:**
- `research/models/hdp_hmm.py`: `train_hdp_hmm_online()` の勾配計算を修正
  - 旧: `residual = posterior[k] - posterior` (n_regimes,) × `x[np.newaxis, :]` (1, feature_dim) → (n_regimes, feature_dim) で `params.weights[k]` (feature_dim,) に代入しようとして形状不一致
  - 新: winner-take-all competitive learning勾配: `gradient = (1.0 if k == winner else 0.0) - posterior[k]` (スカラー) × `x` (feature_dim,) = (feature_dim,)
  - `winner = argmax(posterior)` で最もlikelyなregimeを選択し、winnerには正の勾配（x方向へ移動）、その他には負の勾配（xから離れる方向へ移動）
  - 対称性破壊: ゼロ初期化から同一入力でも argmax が index 0 を選び、regime 0 が支配的に学習

**Commands run:**
- `pytest research/tests/test_hdp_hmm.py::TestTrainHdpHmmOnline -v` — 3 passed, 0 failed
- `pytest research/tests/test_hdp_hmm.py -v -k 'not Export'` — 31 passed, 3 deselected (ONNX依存未インストール)

**Issues:** なし。ONNX export テスト3件は onnx/onnxruntime 未インストールのため失敗（Task 3で対応）

### 2026-04-20: Task 3 — Python依存パッケージ追加 (onnx, onnxruntime, scikit-learn)

**What changed:**
- `pip install onnx onnxruntime scikit-learn` で依存パッケージをインストール
  - onnx 1.21.0, onnxruntime 1.24.4, scikit-learn 1.8.0
- `research/tests/test_hdp_hmm.py`: テスト内に不足していた `import onnx` を追加
- `research/models/hdp_hmm.py`: ONNX exportのMatMul次元バグを修正
  - 旧: weights `[n_regimes, feature_dim]` をMatMulに直接使用 → 形状不一致
  - 新: weightsを転置 `[feature_dim, n_regimes]` にして `features [1, F] @ W^T [F, K] = [1, K]` に修正

**Commands run:**
- `pip install onnx onnxruntime scikit-learn` — インストール成功
- `pytest research/tests/test_hdp_hmm.py -v` — 34 passed, 0 failed (全テスト初の完全通過)

**Issues:** なし

### 2026-04-20: Task 4 — 特徴量交互作用項2個追加: OBI×vol, spread_z×self_impact (34→36次元)

**What changed:**
- `crates/strategy/src/features.rs`: FeatureVectorに2フィールド追加
  - `obi_x_vol: f64` — OBI × realized_volatility
  - `spread_z_x_self_impact: f64` — spread_zscore × self_impact
  - `DIM`: 34 → 36
  - `flattened()`, `from_flattened()`, `zero()` を更新（indices 34-35）
- `crates/strategy/src/extractor.rs`: extract()に計算ロジック追加
  - `obi_x_vol = obi * realized_vol`
  - `spread_z_x_self_impact = spread_zscore * self_impact`
- `STRATEGY_A/B/C_FEATURE_DIM`: 39 → 41（自動更新、コード変更なし）
- ハードコードされた次元アサーションを修正:
  - `crates/backtest/src/engine.rs`: 39 → 41
  - `crates/strategy/src/strategy_a.rs`: 39 → 41
  - `crates/strategy/src/strategy_b.rs`: 39 → 41
  - `crates/strategy/src/strategy_c.rs`: 39 → 41
  - `crates/strategy/src/mc_eval.rs`: 34 → 36 + 新フィールドアクセス
- `crates/backtest/tests/integration.rs`: 3箇所のFeatureVector構築に新フィールド追加

**Commands run:**
- `cargo build` — passed
- `cargo test --workspace --no-fail-fast` — 1451 passed, 3 failed (全て事前存在のCSVバリデーション失敗)
- `cargo clippy` — no errors
- `cargo fmt --check` — clean

**Issues:** なし。戦略FEATURE_DIMはFeatureVector::DIM + EXTRA_DIMで自動計算されるため、変更不要

### 2026-04-20: Task 5 — Dynamic K Calculator: volatility/regime依存の動的k係数

**What changed:**
- `crates/strategy/src/thompson_sampling.rs`: `compute_dynamic_k()` 関数を追加
  - `k = base_k * (1.0 + 10.0 * volatility) * regime_multiplier(stability)`
  - volatility_factor: 高ボラティリティ → k増大（より保守的）
  - regime_multiplier: stability < 0.5 → `1 + 2*(1-stability)` の増幅（低安定性 = 高k）
  - regime_multiplier: stability >= 0.5 → 1.0（安定レジームでは増幅なし）
- `decide()` メソッド内で動的kを適用:
  - `regime_stability = (1.0 - volatility_ratio.min(1.0)).max(0.0)` を特徴量から算出
  - `non_model_penalty = compute_dynamic_k(base_k, realized_vol, stability) * sigma_noise`
- テスト5件追加:
  - `test_dynamic_k_low_volatility_low_k`: 低vol + 高安定性 → k ≈ base_k
  - `test_dynamic_k_high_volatility_high_k`: 高vol → k増大
  - `test_dynamic_k_low_stability_high_k`: 低安定性 → k大幅増大
  - `test_dynamic_k_zero_volatility_equals_base`: vol=0, stability=1.0 → k=base_k
  - `test_dynamic_k_always_positive`: 全パラメータ組み合わせでk > 0

**Commands run:**
- `cargo test -p fx-strategy --lib -- test_dynamic_k` — 5 passed, 0 failed
- `cargo test --workspace --no-fail-fast` — 1456 passed (+5 new), 3 failed (事前存在CSVバリデーション)
- `cargo clippy` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 6 — GapDetector取引停止ロジックの配線

**What changed:**
- `crates/events/src/gap_detector.rs`: GapDetectorに取引停止機能を実装
  - `halted: bool` フィールドを追加し、`new()` / `with_config()` で `false` 初期化
  - `process_market_event()` で Severe ギャップ検出時に `halted = true` を設定
  - `is_trading_halted()` を `self.halted` を返すよう修正（旧: 常に `false`）
  - `reset_halt()` メソッドを追加
  - `process_market_event_sync()` 同期版を追加（バックテストエンジン用、イベント発行なし）
  - 既存テスト2件に `assert!(detector.is_trading_halted())` を追加
  - 新テスト3件追加:
    - `test_minor_gap_does_not_halt_trading`: Minor → halted=false
    - `test_reset_halt_clears_halted_state`: Severe → reset → false
    - `test_halt_persists_across_normal_ticks`: 通常ティック後もhalt持続
- `crates/backtest/src/engine.rs`: バックテストエンジンにGapDetector配線
  - `run_inner()` で `GapDetector` を作成、各ティックを `process_market_event_sync()` で処理
  - Phase 2 戦略意思決定ループの先頭で `gap_halted` チェック、halted時は全戦略 hold + skip_reason="gap_detected"
- `crates/forward/src/runner.rs`: フォワードテストランナーにGapDetector配線
  - `run()` で `GapDetector` を作成、各ティックを `process_market_event()` で処理
  - kill switch チェック直後に `is_trading_halted()` チェック、halted時は continue
- `crates/strategy/src/thompson_sampling.rs`: fmt修正 (事前存在のフォーマット問題)

**Commands run:**
- `cargo test -p fx-events --lib -- gap_detector` — 29 passed, 0 failed
- `cargo build -p fx-backtest` — passed
- `cargo build -p fx-forward` — passed
- `cargo test --workspace --no-fail-fast` — 失敗は全て事前存在のCSVバリデーション (3件)
- `cargo clippy` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 7 — PreFailureMetrics のライブ配線

**What changed:**
- `crates/backtest/src/engine.rs`: バックテストエンジンにObservabilityManager配線
  - `run_inner()` で `ObservabilityManager::new(AnomalyConfig::default())` を作成
  - 各ティックで `collect_pre_failure_metrics()` からPreFailureMetricsを構築
  - `observability_manager.tick(metrics, tick_ns)` を各ループで呼び出し
  - `BacktestResult` に `observability_ticks: u64` フィールドを追加
  - `collect_pre_failure_metrics()` メソッド追加:
    - `rolling_variance_latency` → `kill_switch.stats().std_interval_ns`
    - `regime_posterior_entropy` → `regime_cache.state().entropy()`
    - `daily/weekly/monthly_pnl_vs_limit` → `limit_state.pnl / config.limit.abs()`
    - その他フィールドは0.0（将来配線用）
  - テスト: `test_backtest_from_events` に `observability_ticks > 0` アサーション追加
- `crates/forward/src/runner.rs`: フォワードテストランナーにObservabilityManager配線
  - `run()` で `ObservabilityManager` を作成
  - snapshot取得直後にPreFailureMetricsを構築・tick()呼び出し
- `crates/cli/src/output.rs`: `BacktestResult` 構築箇所に `observability_ticks: 0` 追加

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-backtest --lib -- test_backtest_from_events` — passed
- `cargo test --workspace --no-fail-fast` — 失敗は全て事前存在のCSVバリデーション (3件)
- `cargo clippy` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 8 — ort crate (ONNX Runtime) をRustワークスペースに追加

**What changed:**
- `Cargo.toml` (workspace): ort 依存のバージョンを `"2"` → `"2.0.0-rc.12"` に修正 (v2はRC版のみ公開)
- `crates/strategy/Cargo.toml`: `ort = { workspace = true }` を追加
- `load-dynamic` featureにより実行時にONNX Runtime共有ライブラリを動的ロード

**Commands run:**
- `cargo build -p fx-strategy` — passed (ort 2.0.0-rc.12 + ort-sys コンパイル成功)
- `cargo build` — passed (全ワークスペース)
- `cargo test --workspace --no-fail-fast` — 失敗は事前存在のCSVバリデーション (3件)

**Issues:** ONNX Runtimeのネイティブライブラリ (libonnxruntime) が必要。`load-dynamic` featureによりビルド時には不要だが、実行時に `ORT_DYLIB_PATH` 環境変数でパスを指定する必要あり

### 2026-04-20: Task 9 — ONNXモデルローダー: regime.rsにRegimeModelLoader実装

**What changed:**
- `crates/strategy/src/regime.rs`: `OnnxRegimeModel` 構造体を実装
  - `session: Mutex<ort::session::Session>` — ONNX Runtimeセッション (Mutexで内部可变性確保、`Session::run()` が `&mut self` を要求するため)
  - `n_regimes: usize`, `feature_dim: usize` — モデルメタデータ
  - 手動 `Debug` impl (ort::SessionはDebugを実装していないため)
  - `load_from_path(path: &str)`: ONNXファイルからセッション構築、入出力名をログ出力
  - `predict(&self, features: &[f64])`: f64→f32変換、`(shape, &[f32])` タプル形式で `TensorRef::from_array_view` 呼び出し、`Session::run()` で推論、`downcast_ref::<DynTensorValueType>()` + `try_extract_tensor::<f32>()` で出力抽出
  - ndarrayバージョン不一致回避: workspace (0.16) vs ort (0.17) → タプル形式でraw sliceを渡す
- `RegimeConfig` に `model_path: Option<String>` フィールド追加、`feature_dim` デフォルトを34→36に変更
- `RegimeCache` に `onnx_model: Option<Arc<OnnxRegimeModel>>` フィールド追加
  - `new()` で `model_path` が指定された場合、ONNXモデルをロード。失敗時はwarnログ出力してヒューリスティックにフォールバック
  - `has_onnx_model()`: ONNXモデルがロードされているか確認
  - `predict_onnx(&self, features: &[f64])`: ONNX推論を実行、失敗時はNone返却
- 全 `RegimeConfig` 構築箇所 (5箇所) に `model_path: None` を追加
- 既存テストの `feature_dim` 34→36 への更新 (3箇所)
- 新テスト4件追加:
  - `test_regime_cache_no_onnx_model_by_default`: デフォルトではONNXモデル未ロード
  - `test_regime_cache_invalid_model_path_falls_back`: 無効パス時のフォールバック (ORT_DYLIB_PATH未設定時はスキップ)
  - `test_onnx_regime_model_debug`: Debug impl動作確認
  - `test_regime_config_model_path_field`: model_pathフィールドの設定/取得

**Commands run:**
- `cargo build -p fx-strategy` — passed (ort 2.0.0-rc.12 API修正後)
- `cargo build` — passed (全ワークスペース)
- `cargo test -p fx-strategy --lib -- regime::tests` — 42 passed, 0 failed
- `cargo test --workspace --no-fail-fast` — 失敗は事前存在のCSVバリデーション (3件)
- `cargo clippy` — no errors
- `cargo fmt --check` — clean

**Issues:** ONNX推論テストは `ORT_DYLIB_PATH` 環境変数が設定されている場合のみ実行。実際のダミーモデルでのロード/推論テストはTask 10 (E2Eパイプライン) で対応。

### 2026-04-20: Task 10 — E2Eパイプライン: Python訓練→ONNXエクスポート→Rust推論

**What changed:**
- `research/models/generate_regime_model.py`: ONNX regime model生成スクリプト
  - HDP-HMMを合成データ(36次元, 4 regimes, 500 samples)で訓練
  - ONNXエクスポート → `research/models/onnx/regime_v1.onnx` に保存
  - テスト用メタデータ(test_features, expected_posterior)を `regime_v1_meta.json` に保存
  - `--output` オプションで出力パス指定可能
- `research/models/onnx/regime_v1.onnx`: 生成されたONNXモデル (MatMul→Add→Softmax, 入力[1,36]→出力[1,4])
- `research/models/onnx/regime_v1_meta.json`: 推論検証用メタデータ
- `research/tests/test_e2e_pipeline.py`: Python E2Eテスト5件
  - `test_train_produces_different_weights`: 訓練で重みが変化することを確認
  - `test_export_matches_python_inference`: ONNX推論結果がPython計算と一致 (atol=1e-5)
  - `test_export_with_correct_input_output_shapes`: 入力[1,36]→出力[1,4]の形状確認
  - `test_posterior_sums_to_one_for_multiple_inputs`: 10個のランダム入力で事後確率が和1
  - `test_generate_model_script_creates_valid_files`: スクリプト出力の完全検証
- `crates/strategy/tests/test_onnx_regime.rs`: Rust統合テスト2件
  - `test_onnx_regime_model_load_and_infer`: ONNXモデルロード→推論→Python期待値との照合 (atol=1e-4)
  - `test_onnx_regime_cache_integration`: RegimeCache経由でのONNX推論
  - ORTライブラリ自動検出: mise/pyenv/python3経由で`ORT_DYLIB_PATH`を自動設定
  - モデルファイル/ライブラリ不在時はSKIP
- `crates/strategy/Cargo.toml`: `serde_json` を dev-dependencies に追加

**Commands run:**
- `python -m research.models.generate_regime_model` — モデル生成成功
- `pytest research/tests/test_e2e_pipeline.py -v` — 5 passed, 0 failed
- `cargo test -p fx-strategy --test test_onnx_regime -- --nocapture` — 2 passed (推論値一致確認済み)
- `cargo test --workspace --no-fail-fast` — 新規テスト全通過、失敗は事前存在のCSVバリデーション (3件)
- `cargo clippy` — no errors
- `cargo fmt --check` — clean

**Issues:** なし。Python→ONNX→Rustの全パイプラインが完全動作。Rust推論結果がPython期待値とfloat32精度で一致。
