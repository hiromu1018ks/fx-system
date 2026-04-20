# バックテストエンジン修正 - Activity Log

## Current Status
**Last Updated:** 2026-04-20
**Tasks Completed:** 3 / 8
**Current Task:** Task 4 — 週末前強制決済

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
