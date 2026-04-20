# バックテストエンジン修正 - Activity Log

## Current Status
**Last Updated:** 2026-04-20
**Tasks Completed:** 7 / 8
**Current Task:** Task 8 — 統合テスト

---

## Session Log

<!-- Agent will append dated entries here -->

### 2026-04-20: Task 1 — chrono-tz をワークスペース依存に追加

**What changed:**
- `Cargo.toml` (workspace root): `[workspace.dependencies]` に `chrono-tz = "0.10"` を追加
- `crates/backtest/Cargo.toml`: `chrono-tz = { workspace = true }` を追加

**Commands run:**
- `cargo build` — passed (chrono-tz v0.10.4 + phf v0.12.1 ダウンロード・コンパイル)
- `cargo test` — 96 passed, 2 failed (pre-existing: bid_ge_ask/non_monotonic validation tests は warn+continue の仕様変更とテスト不整合)

**Issues:** pre-existing test failures in data::tests (validation tests expect err but code skips with warn) — chrono-tzとは無関係

### 2026-04-20: Task 2 — EET DST対応: parse_timestamp() で chrono-tz を使った正確なEET変換

**What changed:**
- `crates/backtest/src/data.rs`: `parse_timestamp()` のEETブランチ (lines 220-227) を書き換え
  - 固定UTC+2オフセット → `chrono_tz::Europe::Helsinki` による自動DST判定
  - `and_local_timezone(helsinki).single()` で通常変換
  - 曖昧時間（秋のフォールバック）は `earliest()` でEEST（UTC+3）を優先
  - 非存在時間（春のスプリングフォワード）はUTC+2フォールバック
- 既存テスト `test_parse_timestamp_eet_format` → `test_parse_timestamp_eet_format_winter` に変更（1月のUTC+2検証）
- テスト追加（4件）:
  - `test_parse_timestamp_eet_format_winter`: 2024-01-15 冬時間 (UTC+2) 検証
  - `test_parse_timestamp_eet_format_summer_dst`: 2024-04-22 夏時間 (UTC+3) 検証
  - `test_parse_timestamp_eet_dst_transition_spring`: 2024-03-31 春DST遷移前後検証
  - `test_parse_timestamp_eet_dst_transition_autumn`: 2024-10-27 秋DST遷移前後検証
  - `test_parse_timestamp_eet_dst_ambiguous_time_earliest`: 曖昧時間のearliest選択検証

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-backtest --lib -- data::tests::test_parse_timestamp` — 11 passed, 0 failed
- `cargo clippy -p fx-backtest` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 5 — MonthlyHalt時の事後分布引き継ぎ: reset()を呼ばない

**What changed:**
- `crates/events/src/projector.rs`: `LimitStateData` に `Copy` trait を追加
- `crates/backtest/src/engine.rs`:
  - MonthlyHalt発火時に月次損失カウンターをリセットするコードを追加
  - `projector.update_limit_state()` で `monthly_pnl = 0.0`, `monthly_halted = false` にリセット
  - BLR posterior (BayesianLinearRegression) はリセットしない — 学習が月境界をまたいで継続
  - テスト追加: `test_monthly_halt_preserves_posterior_and_resets_counter`
    - エンジン実行後も楽観的初期化によるBuy > Holdバイアスが保持されることを検証
    - limit_stateの月次リセット（monthly_pnl=0, halted=false）が正しく動作することを検証

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-backtest --lib -- tests::test_monthly_halt_preserves` — 1 passed, 0 failed
- `cargo clippy -p fx-backtest -p fx-events` — no errors
- `cargo fmt --check` — clean

**Issues:** limit_stateはバックテストエンジン内でイベントから更新されていないため、MonthlyHaltは実際のバックテストでは発火しない（limit_stateが常にdefault=0のまま）。これは既存アーキテクチャの制約であり、本タスクの範囲外

### 2026-04-20: Task 6 — StreamingCsvReader: 行単位CSV読み込み + スライディングウィンドウ

**What changed:**
- `crates/backtest/src/data.rs`: `StreamingCsvReader` 構造体を追加
  - `new(path, window_size)`: BufReader + csv::Reader でCSVファイルを開く
  - `next_tick() -> Option<ValidatedTick>`: 1行ずつ読み込み、バリデーション（crossed market, non-monotonic timestamp skip）、スライディングウィンドウ更新
  - `window_ticks() -> Vec<&ValidatedTick>`: 現在のウィンドウ内tick参照
  - 内部で `VecDeque<ValidatedTick>` を使用、window_sizeを超えると古いtickをFIFO破棄
- テスト追加（2件）:
  - `test_streaming_csv_reader_basic`: 3行CSVで正しく3tick読み込まれることを検証
  - `test_streaming_csv_reader_window_eviction`: 10行CSV + window_size=5 で最後5tickのみ保持されることを検証

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-backtest --lib -- data::tests::test_streaming` — 2 passed, 0 failed
- `cargo clippy -p fx-backtest` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 4 — 週末前強制決済: 金曜クローズ時に全ポジションを強制決済

**What changed:**
- `crates/risk/src/limits.rs`: `CloseReason` enumに `WeekendHalt` 変種を追加
- `crates/backtest/src/engine.rs`:
  - `chrono::{Datelike, DateTime}` + `chrono_tz::Tz` import追加
  - `is_weekend_gap(prev_tick_ns, curr_tick_ns)` ヘルパー追加: Europe/Helsinki timezoneでDST対応の週末判定
    - prev weekday <= Friday (4) && curr weekday == Monday (0) && gap >= 12h
  - `run_inner()` メインループに週末ギャップ検出を追加: 検出時に `close_all_positions("WEEKEND_HALT")` を呼び出し
  - `CloseReason` match に `WeekendHalt` arm 追加
- テスト追加（5件）:
  - `test_is_weekend_gap_friday_to_monday`: 金→月遷移で週末ギャップ検出
  - `test_is_weekend_gap_no_gap_consecutive_days`: 連続金曜tickで非検出
  - `test_is_weekend_gap_no_gap_within_week`: 水曜→木曜で非検出
  - `test_is_weekend_gap_zero_prev`: prev_tick_ns=0で非検出
  - `test_weekend_gap_closes_positions`: 金→月E2Eテスト

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-backtest --lib -- tests::test_is_weekend_gap tests::test_weekend_gap_closes` — 5 passed, 0 failed
- `cargo clippy -p fx-backtest -p fx-risk` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 3 — Cholesky対角近似: n_observations < dim の間は対角近似でサンプリング

**What changed:**
- `crates/strategy/src/bayesian_lr.rs`: `sample_weights()` を拡張
  - `n_observations < dim` の場合: 対角近似 `ŵ + diag(sqrt(σ² · diag(A_inv))) · z` を使用
  - `n_observations >= dim` の場合: 既存の完全Cholesky分解パス
  - `sample_standard_normal_scalar()` ヘルパー関数追加
  - テスト2件追加:
    - `test_sample_weights_diagonal_approximation_no_panic`: n_observations=0でパニックせず、点推定と異なる値が生成されることを検証
    - `test_sample_weights_diagonal_then_cholesky_transition`: 観測数増加による対角近似→Cholesky切替を検証

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-strategy --lib -- bayesian_lr::tests::test_sample_weights` — 3 passed, 0 failed
- `cargo clippy -p fx-strategy` — no errors
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 7 — BacktestEngine ストリーミング対応: run_from_stream() 追加

**What changed:**
- `crates/backtest/src/engine.rs`:
  - `run_from_stream<I: Iterator<Item = ValidatedTick>>()` メソッド追加
  - 内部で `tick_to_event()` を使ってtickをGenericEventに変換し、既存のイベント処理パイプラインに流す
  - `run_from_events()` のロジックを再利用しつつ、ストリーミングIteratorを直接消費
  - テスト追加: `test_run_from_stream_matches_run_from_events`
    - 同じCSVデータで `run_from_events()` と `run_from_stream()` の結果が一致することを検証

**Commands run:**
- `cargo build` — passed
- `cargo test -p fx-backtest --lib -- tests::test_run_from_stream` — 1 passed, 0 failed
- `cargo clippy -p fx-backtest` — no errors
- `cargo fmt --check` — clean

**Issues:** なし
