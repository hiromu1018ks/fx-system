# design.md未実装機能 - Activity Log

## Current Status
**Last Updated:** 2026-04-20
**Tasks Completed:** 2
**Current Task:** Task 3 — Python依存パッケージ追加

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
