# バックテストエンジン修正 PRD

## Overview

バックテストエンジンの5つの問題を修正し、2年一括バックテストを正確かつ安定的に実行可能にする。

## 対象問題

| # | 問題 | 影響度 | 対応方針 |
|---|------|--------|----------|
| 1 | WSL2メモリ不足で落ちる | 実行ブロック | スライディングウィンドウによるストリーミング読み込み |
| 2 | 週末価格ジャンプでPnL不正 | 結果不正 | 週末前強制決済 |
| 3 | 月ごと学習リセット | 学習無効 | 事後分布の月境界引き継ぎ |
| 4 | Cholesky分解失敗 | 初期学習不安定 | 対角近似→完全Cholesky切替 |
| 5 | EET DST未考慮 | 夏期時刻ずれ | chrono-tz による正確な変換 |

## Tech Stack

- **言語**: Rust (既存ワークスペース)
- **追加依存**: `chrono-tz` (問題5のみ)
- **テスト**: `cargo test`, `cargo clippy`, `cargo fmt --check`

## Architecture

### 問題1: スライディングウィンドウ読み込み

現在の `load_csv()` → `Vec<ValidatedTick>` (全メモリ載せ) を、行単位ストリーミング + 直近N行メモリ保持に変更。

- `StreamingCsvReader`: 行単位でCSVを読み込み、スライディングウィンドウで直近N行を保持
- `BacktestEngine` は `run_from_events(&[GenericEvent])` の代わりにイテレータベースの `run_from_stream()` を使用
- ウィンドウサイズは特徴量計算に必要な履歴行数 + α

### 問題2: 週末前強制決済

- `parse_timestamp()` 後、tickの曜日と時刻から市場セッション境界を判定
- 金曜 21:59 UTC (EET土曜 00:00) 以降のtickで、全オープンポジションを強制決済
- 決済理由: `CloseReason::WeekendHalt` (新規enum変種)
- 月曜 00:00 EET (日曜 22:00 UTC) 以降は通常エントリー再開

### 問題3: 事後分布引き継ぎ

- `MonthlyHalt` 発火時、`BayesianLinearRegression` の `reset()` を呼ばない
- 月境界をまたぐ際、事後分布 (`a_inv`, `b`, `w_hat`, `n_observations`) をそのまま保持
- `HierarchicalRiskLimiter` の月次リセットのみ実行 (損失カウンターはリセット)

### 問題4: 対角近似→完全Cholesky切替

- `BayesianLinearRegression::sample_weights()` で `n_observations < dim` の間は対角近似を使用
- 対角近似: `A_inv` の対角成分のみ使い `diag(sqrt(sigma2 * diag(A_inv))) * z` でサンプリング
- `n_observations >= dim` で完全な Cholesky 分解に切替

### 問題5: chrono-tz による DST 対応

- ワークスペース `Cargo.toml` に `chrono-tz` 追加
- `parse_timestamp()` の EET ブランチで `chrono_tz::Europe::Helsinki` を使用し自動 DST 判定
- テスト: 夏時間・冬時間の境界付近のタイムスタンプで UTC 変換を検証

## Constraints

- 既存のテストは全て通ること
- `debug_assert!` は使用しない
- 既存APIの破壊的変更は最小限 (ストリーミング対応は新規メソッド追加)
- 週末判定は EET 基準 (DST問題5と連動)

---

## Task List

```json
[
  {
    "category": "dependency",
    "description": "chrono-tz をワークスペース依存に追加し、backtestクレートで有効化",
    "steps": [
      "ワークスペース Cargo.toml の [workspace.dependencies] に chrono-tz = \"0.10\" を追加",
      "crates/backtest/Cargo.toml に chrono-tz = { workspace = true } を追加",
      "cargo build でコンパイル確認"
    ],
    "passes": true
  },
  {
    "category": "fix",
    "description": "EET DST対応: parse_timestamp() で chrono-tz を使った正確なEET変換",
    "steps": [
      "data.rs の parse_timestamp() EET ブランチ (lines 220-227) を chrono-tz 使用に書き換え",
      "NaiveDateTime を Europe/Helsinki タイムゾーンで解釈し UTC へ変換",
      "冬時間 (UTC+2) と夏時間 (UTC+3) の両方のテストを追加",
      "既存の test_parse_timestamp_eet_format テストが通ることを確認",
      "3月最終日曜・10月最終日曜の境界テストを追加"
    ],
    "passes": true
  },
  {
    "category": "fix",
    "description": "Cholesky対角近似: n_observations < dim の間は対角近似でサンプリング",
    "steps": [
      "bayesian_lr.rs の sample_weights() に分岐追加: n_observations < dim なら対角近似",
      "対角近似実装: diag(sqrt(sigma2 * diag(A_inv))) * z を計算",
      "n_observations >= dim で完全Choleskyに切替 (既存ロジック)",
      "テスト: 初期状態 (n_observations=0) でsample_weightsがパニックしない",
      "テスト: 十分な観測後はCholeskyパスが使われる"
    ],
    "passes": true
  },
  {
    "category": "fix",
    "description": "週末前強制決済: 金曜クローズ時に全ポジションを強制決済",
    "steps": [
      "engine.rs の run_inner() に週末判定ロジックを追加",
      "直前tickが金曜 EET 23:59:59 以前、現在tickが日曜 EET 00:00 以降 → 週末ジャンプ検出",
      "週末ジャンプ検出時、全オープンポジションを直前tick価格で強制決済",
      "CloseReason に WeekendHalt を追加 (risk クレートの enum)",
      "決済tradeに close_reason: Some(\"WEEKEND_HALT\") を記録",
      "テスト: 金曜→月曜tick遷移でポジションが決済される"
    ],
    "passes": true
  },
  {
    "category": "fix",
    "description": "MonthlyHalt時の事後分布引き継ぎ: reset()を呼ばない",
    "steps": [
      "engine.rs の MonthlyHalt ハンドリングで ThompsonSamplingPolicy の reset をスキップ",
      "HierarchicalRiskLimiter の月次リセットは維持",
      "テスト: 月境界をまたぐバックテストで事後分布がリセットされない",
      "テスト: 月次損失制限は月境界でリセットされる"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "StreamingCsvReader: 行単位CSV読み込み + スライディングウィンドウ",
    "steps": [
      "data.rs に StreamingCsvReader 構造体を追加",
      "new(path, window_size) → BufReader ベースで初期化",
      "next_tick() -> Option<ValidatedTick>: 1行読み込み、バリデーション、ウィンドウ更新",
      "window_ticks() -> &[ValidatedTick]: 現在のウィンドウ内tick参照",
      "load_csv() は残す (後方互換・テスト用)",
      "テスト: 小さいCSVでStreamingCsvReaderが正しく動作",
      "テスト: ウィンドウサイズを超えると古いtickが破棄される"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "BacktestEngine ストリーミング対応: run_from_stream() 追加",
    "steps": [
      "engine.rs に run_from_stream<R: Iterator<Item = ValidatedTick>> を追加",
      "内部でStreamingCsvReaderのイテレータを消費しつつイベント処理",
      "既存の run_from_events() は変更しない",
      "テスト: run_from_stream() の結果が run_from_events() と一致する (同じデータで)"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "週末強制決済とストリーミングの統合テスト",
    "steps": [
      "金曜→月曜を含むCSVデータで統合テスト作成",
      "週末ジャンプでポジションが決済されることを検証",
      "月境界を含むデータで事後分布が引き継がれることを検証",
      "2年分のデータシミュレーションでメモリ使用量が一定に保たれるテスト"
    ],
    "passes": false
  }
]
```

---

## Agent Instructions

1. Read `activity-backtest-fix.md` first to understand current state
2. Find next task with `"passes": false`
3. Complete all steps for that task
4. Run: `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check`
5. Update task to `"passes": true`
6. Log completion in `activity-backtest-fix.md`
7. Commit with `fix(backtest): ...` prefix
8. Repeat until all tasks pass

**Important:** Only modify the `passes` field. Do not remove or rewrite tasks.

---

## Completion Criteria
All 8 tasks marked with `"passes": true`
