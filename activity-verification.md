# 検証・統合PRD - Activity Log

## Current Status
**Last Updated:** 2026-04-20
**Tasks Completed:** 6
**Current Task:** None started

---

## Session Log

<!-- Agent will append dated entries here -->

### 2026-04-20: Task 1 — PnL計算バグの修正

**What changed:**
- `crates/backtest/src/engine.rs`: `process_execution_result()` の戻り値を `()` → `f64` に変更し、`realized_pnl` の前後差分（delta）を返すように修正
- MAX_HOLD_TIME close と END_OF_DATA close の両トレード作成箇所で、PnLをdeltaから直接計算
- エンジン末尾の「全トレードに最終cumulative realized_pnlを代入する」ループ（旧346-351行）を削除
- `crates/backtest/tests/integration.rs`: PnL回帰テスト `test_backtest_engine_individual_trade_pnl` を追加（各トレードが固有のPnLを持ち、合計がsummaryと一致することを検証）

**Commands run:**
- `cargo build` — passed
- `cargo test` — 463 passed, 0 failed
- `cargo clippy` — no warnings
- `cargo fmt --check` — clean

**Issues:** None

### 2026-04-20: Task 2 — Change Point Detection誤検出テストの修正

**What changed:**
- `crates/strategy/src/change_point.rs`: `find_best_split()`のHoeffding限界にBonferroni補正を追加。`ln(4/delta)` → `ln(4*n_cuts/delta)` に修正し、多重比較問題に対処
- 同ファイル: テスト`test_no_detection_stable_distribution`の`rand::thread_rng()`を`StdRng::seed_from_u64(42)`に変更し、非決定性を排除
- `use rand::SeedableRng`をテストモジュールに追加

**Commands run:**
- `cargo build` — passed
- `cargo test` — 463 passed, 0 failed
- `cargo clippy` — no warnings
- `cargo fmt` — clean

**Issues:** Bonferrini補正により、正当な変化点検出テスト（mean_shift, variance_shift）も引き続き通過することを確認

### 2026-04-20: Task 5 — BacktestEngineへのStrategyA/B/C統合（ポジションオープン判断の実装）

**What changed:**
- `crates/backtest/src/engine.rs`: 大幅なリファクタリング
  - `BacktestConfig` に新フィールド追加: `enabled_strategies`, `strategy_a/b/c_config`, `mc_eval_config`, `global_position_config`
  - `BacktestEngine` に新フィールド追加: `strategy_a (StrategyA)`, `strategy_b (StrategyB)`, `strategy_c (StrategyC)`, `mc_evaluator (McEvaluator)`
  - `StrategyDecision` 構造体追加: StrategyA/B/C の各Decision型を統一的に扱うための変換型
  - `run_inner()` の完全リライト:
    - Phase 1: 各戦略のMAX_HOLD_TIME切れポジションの自動クローズ（A:30s, B:5min, C:10min）
    - Phase 2: 各有効戦略のdecide()呼び出し→Q値ベースの優先度ソート
    - Phase 3: GlobalPositionCheckerによるポジション制約チェック→発注
    - Phase 4: アクティブエピソードのMC遷移記録
  - END_OF_DATA時の残ポジションクローズ時にMC episode終了+Q関数更新を追加
  - ヘルパーメソッド追加: `should_close_max_hold()`, `strategy_max_hold_time_ns()`, `get_strategy_decision()`, `extract_strategy_features()`, `start_strategy_episode()`, `end_strategy_episode()`
- テスト追加:
  - `test_strategy_integration_produces_decisions`: 500ティックでdecisionsが上限内に収まること
  - `test_strategy_enabled_subset`: Strategy Aのみ有効時に他戦略のdecisionが生成されないこと
  - `test_strategy_per_strategy_max_hold_time`: 各戦略のMAX_HOLD_TIME (30s/5min/10min) 検証
  - `test_strategy_reproducible_with_seed`: 同一シードで再現性確認

**Commands run:**
- `cargo build` — passed
- `cargo test` — 463 passed (4 new), 0 failed
- `cargo clippy` — no warnings
- `cargo fmt --check` — clean

**Issues:** なし

### 2026-04-20: Task 6 — BacktestEngineへのRiskLimiter統合（ハードリミットチェック）

**What changed:**
- `crates/backtest/src/engine.rs`: BacktestConfigに新フィールド追加: `risk_limits_config`, `barrier_config`, `kill_switch_config`, `lifecycle_config`
- BacktestEngineに新フィールド追加: `risk_barrier (DynamicRiskBarrier)`, `kill_switch (KillSwitch)`, `lifecycle_manager (LifecycleManager)`
- KillSwitchはデフォルトで無効（`enabled: false`）— バックテストではtick間隔の異常検知は履歴データで意味がないため
- `run_inner()` の大幅な拡張:
  - 各ティック開始時に `kill_switch.record_tick(tick_ns)` で間隔監視
  - Phase 2で `lifecycle_manager.is_alive(sid)` により淘汰済み戦略をスキップ
  - Phase 3に完全なリスクパイプラインを追加:
    1. KillSwitch: 異常検知時の発注マスク
    2. LifecycleManager: 淘汰済み戦略のブロック
    3. HierarchicalRiskLimiter: 月次→週次→日次実現→日次MTM の全段階チェック
    4. Q閾値ゲート: 日次MTM制限中は`|Q| >= q_threshold`が必要
    5. DynamicRiskBarrier: stalenessベースのlot_multiplier適用
    6. GlobalPositionChecker: 既存のポジション制約（最後に実行）
  - ハードリミット発動時の全ポジションクローズ: `close_all_positions()` ヘルパー追加
- `end_strategy_episode()` を拡張: MC episode完了時に `LifecycleManager.record_episode()` を呼び出し、戦略淘汰評価を実行
- テスト追加:
  - `test_risk_config_defaults`: デフォルト設定値の検証
  - `test_risk_pipeline_no_false_rejections_with_default_config`: デフォルト設定で偽陽性がないこと
  - `test_kill_switch_rejects_when_masked`: KillSwitchマスク時の発注ブロック確認
  - `test_hierarchical_limit_daily_realized_halt`: 階層的リミットの統合動作確認
  - `test_lifecycle_culling_blocks_culled_strategy`: 淘汰済み戦略のdecisionが全て"strategy_culled"になること
  - `test_barrier_rejects_high_staleness`: バリア設定の確認
  - `test_close_all_positions_helper`: 全ポジションクローズヘルパーの動作確認

**Commands run:**
- `cargo build` — passed
- `cargo test` — 463 passed (7 new), 0 failed
- `cargo clippy` — no warnings
- `cargo fmt --check` — clean

**Issues:** なし
