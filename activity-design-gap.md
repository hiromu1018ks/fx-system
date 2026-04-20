# design.md未実装機能 - Activity Log

## Current Status
**Last Updated:** 2026-04-20
**Tasks Completed:** 4
**Current Task:** Task 5 — Dynamic K Calculator実装

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
