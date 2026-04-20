# 検証・統合PRD - Product Requirements Document

## Overview

design.mdに基づく実装の完全性を検証し、致命的欠落5項目を修正し、エンドツーエンドの動作確認を行う。各コンポーネントは個別に実装・テスト済みだが、コンポーネント間の統合が不完全であり、実際のトレーディングパイプラインとして機能させるための修正・統合・検証が目的。

## Target Audience

- **主利用者**: システム開発者・運用者
- **現段階**: 個別コンポーネント実装完了 → 統合・検証フェーズ
- **最終目標**: バックテスト・フォワードテストがdesign.md通りに動作する完全パイプライン

## Critical Gaps to Address

### 致命的欠落（優先順位順）

| # | 欠落 | 現状 |
|---|------|------|
| 1 | ストラテジー統合 | BacktestEngine::run_inner()にFeatureExtractor/StrategyA/B/C/RiskLimiterが組み込まれていない。ポジションの「オープン判断」が存在しない |
| 2 | 履歴データローダー | CSV/Parquet等のファイル読み込み機能がゼロ。generate_synthetic_ticks()（合成データ）のみ |
| 3 | バイナリエントリポイント | main.rsやCLIツールが存在せず、cargo runで実行不可 |
| 4 | Rust ↔ Python連携 | pyo3/subprocess/JSON橋渡しが一切ない。統計検証パイプラインが孤立している |
| 5 | PnL計算の修正 | 各トレードのPnLが最後のrealized_pnlと同値になるバグあり |

### 追加問題

| # | 問題 | 現状 |
|---|------|------|
| 6 | Change Point誤検出 | `test_no_detection_stable_distribution`が失敗。安定分布でt=132で誤検出 |

## Tech Stack

- **Core Language**: Rust (既存クレートの拡張)
- **Data Loading**: csv + serde crate (CSV), optionally parquet
- **CLI**: clap crate
- **Python Bridge**: subprocess + JSON (シンプルな連携)
- **Build System**: Cargo (既存ワークスペース)

## Architecture

既存のクレート構成を維持し、以下を追加・修正する：

```
fx-backtest: engine.rs修正（戦略統合）+ data_loader.rs追加
fx-core: types拡張（DataTick定義等）
新規バイナリクレート: crates/cli/ (fx-cli)
Python連携: scripts/bridge.py + Rust側JSON I/O
```

## Data Model

### DataTick（汎用ティックデータ）
```rust
struct DataTick {
    timestamp_ns: i64,
    bid: f64,
    ask: f64,
    bid_volume: Option<f64>,
    ask_volume: Option<f64>,
    symbol: String,
}
```

### TradeRecord修正
各トレードが固有のPnLを持つよう、position open時とclose時の価格を正しく記録。

## Task List

```json
[
  {
    "category": "bugfix",
    "description": "PnL計算バグの修正: 各トレードが固有のPnLを持つようにする",
    "steps": [
      "BacktestEngine::run_inner()のPnL計算を調査: 全TradeRecord.pnlが最後のrealized_pnlと同値になる原因を特定",
      "StateProjectorのposition realized_pnl計算を確認: ポジションクローズ時に正しい個別PnLが計算されるか検証",
      "TradeRecord生成時に、open価格・close価格・lot数からPnL = (close_price - open_price) * direction * lot_size を正しく計算",
      "stats.rsのtotal_pnl/realized_pnl集計が修正後の個別PnLを正しく合算することを確認",
      "既存テストが全て通ることを確認し、PnL計算の回帰テストを追加"
    ],
    "passes": true
  },
  {
    "category": "bugfix",
    "description": "Change Point Detection誤検出テストの修正",
    "steps": [
      "test_no_detection_stable_distributionの失敗原因を特定: t=132での誤検出メカニズムを分析",
      "ADWINアルゴリズムの閾値パラメータが安定分布に対して適切か確認",
      "FeatureAdwinのε（epsilon）やδ（delta）パラメータの調整、または最小検出サイズの設定",
      "安定分布データでの誤検出率が統計的に期待範囲内（false positive rate < 0.05）になることを確認",
      "テストが安定して通ることを確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "履歴データローダーの実装（CSV対応）",
    "steps": [
      "crates/backtest/Cargo.tomlにcsv + serde依存を追加",
      "crates/backtest/src/data_loader.rsを作成",
      "DataTick構造体定義: timestamp_ns, bid, ask, bid_volume, ask_volume, symbol",
      "CsvLoader実装: CSVファイルパスを受け取りVec<DataTick>またはIterator<DataTick>を返す",
      "CSVフォーマット仕様: timestamp,bid,ask,bid_volume,ask_volume,symbol（ヘッダー必須）",
      "タイムスタンプの柔軟なパース対応: ISO 8601, Unix timestamp (ns), YYYY-MM-DD HH:MM:SS.mmm",
      "バリデーション: bid < ask、timestamp単調増加、欠損値の処理",
      "BacktestEngine::run_inner()がVec<DataTick>を受け取れるインターフェース追加",
      "ユニットテスト: 合成CSVの読み込み、バリデーション、エラーハンドリング"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "BacktestEngineへのFeatureExtractor統合",
    "steps": [
      "fx-backtestのCargo.tomlにfx-strategy依存を確認・追加（既にある場合は確認のみ）",
      "BacktestEngine::run_inner()内でMarketEventからFeatureExtractorを用いて特徴量抽出を行う処理を追加",
      "FeatureExtractorの初期化: 適切なウィンドウサイズと特徴量設定で初期化",
      "各ティック到着時にfeature vectorを更新するパイプラインを構築",
      "特徴量ベクトルがStrategyとRiskの評価に渡せるよう、中間データ構造を定義",
      "ユニットテスト: 合成データで特徴量が正しく抽出されることを確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "BacktestEngineへのStrategyA/B/C統合（ポジションオープン判断の実装）",
    "steps": [
      "BacktestEngineに戦略選択設定（enabled_strategies: HashSet<StrategyId>）を追加",
      "各ティック処理ループ内で、有効な各戦略のevaluate()を呼び出し、行動（buy/sell/hold）を取得",
      "戦略の行動決定フロー: FeatureExtractor → StrategyX.evaluate(features, position_state) → Action",
      "複数戦略が同時にシグナルを出した場合の優先度処理（Q値ベース、design.md §9.5）",
      "Thompson Samplingによる行動選択の統合: ベイズ事後分布からのサンプリング",
      "戦略別のエピソード管理（on-policy Monte Carlo評価用）",
      "ユニットテスト: 各戦略のシグナル生成、複数戦略同時シグナルの処理"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "BacktestEngineへのRiskLimiter統合（ハードリミットチェック）",
    "steps": [
      "BacktestEngineにHierarchicalRiskLimiterを統合",
      "戦略の行動決定後、発注前にdesign.md §9.4の全段階チェックを実行",
      "チェック順序: 月次 → 週次 → 日次MTM → 日次実現 → グローバルポジション制約",
      "ハードリミット発動時の処理: ポジション全クローズ + 取引停止 + TradeSkipEvent発行",
      "DynamicRiskBarrierの統合: stalenessベースのlot_multiplier適用",
      "Kill Switchの統合: 異常検知時の即座な発注マスク",
      "ユニットテスト: 各リミット段階の発動、リミット発動後の取引停止確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "BacktestEngineへのOTC Execution Gateway統合",
    "steps": [
      "BacktestEngineの既存simulate_execution呼び出しを、fx-executionのExecutionGateway経由に変更",
      "戦略決定 → リスクチェック通過後 → ExecutionGateway.submit_order() のフローを構築",
      "OTC約定モデルの適用: Last-Look拒否、fill確率、slippage計算",
      "約定結果（FillEvent/RejectEvent）のEventBus(Execution Stream)への発行",
      "LP別fill率・reject率の追跡",
      "ユニットテスト: 約定シミュレーション、Last-Look拒否、slippage計算"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "報酬計算・Monte Carlo評価の統合",
    "steps": [
      "BacktestEngineに戦略分離型報酬関数を統合: r_t^i = PnL_i - λ_risk·σ²_i - λ_dd·DD_i",
      "McEvaluatorの統合: エピソード定義（ポジションオープン → クローズ/MAX_HOLD_TIME/ハードリミット）",
      "各エピソード終了時に割引累積報酬 G_t = Σ γ^k·r_{t+k} を計算",
      "BayesianLinearRegressionのオンライン更新: エピソード完了ごとに事後分布を更新",
      "LifecycleManagerの統合: Rolling Sharpe監視、戦略淘汰判定",
      "ユニットテスト: 報酬計算、エピソード完了判定、事後分布更新"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Regime管理の統合（HDP-HMM lightweight online指標）",
    "steps": [
      "BacktestEngineにRegimeCacheを統合",
      "各ティック処理時にregime_posteriorを更新（軽量オンライン指標）",
      "未知Regime検出時の処理: regime_entropy > threshold → 全戦略hold + TradeSkipEvent",
      "drift_t = Σ_k (regime_posterior_k * f_k(drift_{t-1}, X_lagged)) の計算",
      "drift_tを特徴量パイプラインに反映",
      "ユニットテスト: regime推定、未知Regime検出、drift計算"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "フルパイプラインバックテストのエンドツーエンドテスト",
    "steps": [
      "統合BacktestEngineを使用したエンドツーエンドテスト作成",
      "テストフロー: CSV読み込み → FeatureExtractor → StrategyA/B/C → RiskLimiter → ExecutionGateway → PnL計算 → Stats",
      "戦略Aのみ、Bのみ、A+B+C等の組み合わせテスト",
      "ハードリミット発動シナリオテスト: 大幅な不利相場で日次リミットが発動すること",
      "情報リーク検証: 実行系特徴量にlagが適用されていることを確認",
      "再現性テスト: 同一シード・同一データで同一結果が得られること",
      "PerformanceSnapshot/PnL/trade countの妥当性検証"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "CLIエントリポイント（crates/cli/）の実装",
    "steps": [
      "crates/cli/ ディレクトリとCargo.toml作成（binary crate、fx-cliという名前）",
      "Cargo.toml workspaceの members に cli を追加",
      "clap依存を追加しCLI引数定義: backtest, forward-test サブコマンド",
      "backtest サブコマンド: --data <CSV_PATH> --config <TOML_PATH> --output <DIR> --strategies <A,B,C>",
      "forward-test サブコマンド: --config <TOML_PATH> --duration <SECONDS> --strategies <A,B,C>",
      "結果のJSON出力: BacktestResult/ForwardTestResultをJSONファイルとして出力",
      "cargo buildでコンパイル確認、cargo run --bin fx-cli -- --helpで動作確認",
      "ユニットテスト: CLI引数パース、設定ファイル読み込み"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "CLI backtest サブコマンドの統合",
    "steps": [
      "CLI backtest サブコマンドからBacktestEngineを呼び出す処理を実装",
      "CSVファイル読み込み → DataTick変換 → BacktestEngine::run() の呼び出し",
      "TOML設定ファイルからのBacktestConfig構築",
      "実行結果のJSONファイル出力（serde_json）",
      "実行結果のCSV出力（トレード履歴・指標時系列）",
      "プログレス表示: 処理ティック数 / 総ティック数",
      "エラーハンドリング: ファイル不在、不正フォーマット、設定エラー",
      "統合テスト: 実際の合成CSVでCLI経由でバックテストを実行"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "CLI forward-test サブコマンドの統合",
    "steps": [
      "CLI forward-test サブコマンドからForwardTestRunnerを呼び出す処理を実装",
      "TOML設定ファイルからのForwardTestConfig構築",
      "録音データフィードの設定: --source recorded --path <EVENT_STORE_PATH> --speed <SPEED>",
      "外部APIフィードの設定: --source external --provider <PROVIDER> --credentials <PATH>",
      "実行結果のJSON/CSVレポート出力",
      "Ctrl+Cによるグレースフルシャットダウン処理",
      "統合テスト: 合成データでCLI経由でフォワードテストを実行"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "Rust ↔ Python連携ブリッジ（JSON I/O）の実装",
    "steps": [
      "Rust側: バックテスト結果をJSONファイルとして出力する機能を強化（serde_jsonでBacktestResult/trade historyを構造化出力）",
      "Python側: scripts/bridge.py または research/bridge/ を作成",
      "Python側: JSONでRustの出力を読み込み、CPCV/PBO/DSRパイプラインに流す処理",
      "Python側: 検証結果（pass/fail/per-metric details）をJSONで出力",
      "Rust側: Python検証結果のJSONを読み込み、表示・サマリーする機能",
      "CLI サブコマンド validate 追加: cargo run --bin fx-cli -- validate --backtest-result <JSON> --python-path <PATH>",
      "ユニットテスト: JSON出力の読み書き、Python連携のモックテスト"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "統計的検証パイプラインのE2Eテスト（Python側）",
    "steps": [
      "research/tests/test_e2e_validation.pyを作成",
      "合成バックテスト結果JSONを生成するテストフィクスチャ",
      "CPCV検証テスト: 時系列リーク防止が正しく動作すること",
      "PBO検証テスト: 過学習検出が正しく動作すること（PBO > 0.1で破棄）",
      "DSR検証テスト: Deflated Sharpe Ratio >= 0.95の判定",
      "Sharpe天井テスト: 年率Sharpe > 1.5で強制破棄",
      "情報リーク検証テスト: lag有無での精度比較",
      "報酬関数感度分析テスト: λパラメータ摂動での安定性確認",
      "pytest で全テストが通ることを確認"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §3.0 MDP定式化の実装整合性検証",
    "steps": [
      "状態空間 s_t = (X_t^market, p_t^position) の実装確認: FeatureExtractor + PositionState",
      "行動空間 a_t ∈ {buy_k, sell_k, hold} の実装確認: QAction/Action enum",
      "制約 |p_{t+1}| ≤ P_max と |p_{t+1} - p_t| ≤ V_max の実装確認",
      "戦略分離型報酬関数 r_t^i の実装確認: 各戦略の報酬が他戦略に依存しないこと",
      "Q関数 Q(s,a) = w_a^T·φ(s) の実装確認: BayesianLinearRegression",
      "各項目についてユニットテストを追加または既存テストを確認"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §3.0.1 Q関数アーキテクチャの実装整合性検証",
    "steps": [
      "統一特徴量パイプライン φ(s) の実装確認: 一次項・非線形変換項・交互作用項・ポジション状態",
      "適応ノイズ分散 σ²_noise,t = EMA_variance(residuals, halflife=500) の実装確認",
      "On-policy + Monte Carlo評価の実装確認: 実行行動のみで更新、bootstrappingなし",
      "Deadly Triad回避の確認: On-policy + MC + ベイズ正則化",
      "Thompson Samplingがσ_modelの唯一の反映経路であることの確認: 点推定にσ_modelが含まれていないこと",
      "発散監視 ||w_t||/||w_{t-1}|| > 2.0 の実装確認",
      "事後ペナルティ Q_adjusted(s,a) = w_a^T·φ(s) - self_impact - dynamic_cost - k·σ_non_model の実装確認（σ_modelを含まない）"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §3.0.2 エピソード定義の実装整合性検証",
    "steps": [
      "エピソード開始条件の確認: ポジションがゼロから非ゼロになった時点",
      "エピソード終端条件4つの確認: (1)完全クローズ (2)MAX_HOLD_TIME (3)日次ハードリミット (4)未知Regime",
      "フラット期間の扱い確認: エピソードに含めない、学習対象外",
      "部分約定の扱い確認: 全量クローズ時のみエピソード終了",
      "MAX_HOLD_TIME到達時の強制クローズとPnL組み込み確認",
      "各条件についてテストを追加"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §3.0.3 Hold退化防止機構の実装検証",
    "steps": [
      "楽観的初期化の確認: ŵ_buy, ŵ_sell がholdより高い値で初期化されていること",
      "最小取引頻度監視の確認: 直近Nティック中のbuy/sell回数 < MIN_TRADE_FREQUENCY の判定",
      "事後分散膨張の確認: α_inflation適用でThompson Samplingのサンプルが多様化すること",
      "取引頻度回復時のα_inflation漸減の確認",
      "γ-減衰によるhold誘導抑制の確認",
      "各機構についてテストを追加"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §4.1 決定関数の実装整合性検証",
    "steps": [
      "階層的ハードリミットチェックがQ値判定より優先されることの確認",
      "チェック順序の確認: 月次 → 週次 → 日次MTM → 日次実現",
      "Thompson SamplingのQ̃_finalが行動選択の唯一の基準であることの確認",
      "グローバルポジション制約フィルタリングの確認: A_validの構築",
      "行動間整合性チェック: buy/sell同時顕著正のフォールバック",
      "validate_orderの検証がQ̃_finalで行われることの確認（Q_pointではない）",
      "検証用テストを追加"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §4.2 OTC Execution モデルの実装整合性検証",
    "steps": [
      "Last-Look拒否モデルの確認: P(fill_effective) = P(fill_requested) × P(not_rejected | last_look)",
      "P(not_rejected)のロジスティック関数確認: β1, β2パラメータのオンライン推定",
      "ε_hiddenのStudent's t分布（自由度3-5）確認",
      "価格改善/悪化モデルの確認: slippage ~ f(direction, size, vol, LP_state)",
      "Passive/Aggressive判定の確認: Expected_Profitとfill_effectiveに基づく",
      "LP行動適応リスク監視の確認: fill率低下 → adversarial signal → 自動LP切り替え",
      "検証用テストを追加"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §9 リスク管理の実装整合性検証",
    "steps": [
      "§9.1 Kill Switch: ティック到着間隔の平均±3σ逸脱検出の確認",
      "§9.2 Online Change Point Detection: ADWINアルゴリズムの動作確認",
      "§9.3 Lifecycle Manager: Rolling Sharpe監視、死の閾値下回り時のハードブロック確認",
      "§9.4 日次二段階: MTM警戒水準 + 実現損益ハードストップの順序と動作確認",
      "§9.4.1 週次ハードリミット: 翌週月曜までhalt、オペレーター承認なしに再開不可",
      "§9.4.2 月次ハードリミット: 月内再開禁止、事後レビュー必須",
      "§9.5 グローバルポジション制約: |Σp_i| ≤ P_max^global、相関調整、FLOOR_CORRELATION",
      "§9.6 LP切り替え再校正: 安全モード25%制限、パラメータ再推定、校正完了判定",
      "検証用テストを追加"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §8 Observability・Pre-Failure Signatureの実装検証",
    "steps": [
      "§8.1 イベント構造の全フィールドがDecisionEventPayload/ExecutionEventPayloadに実装されていることの確認",
      "§8.2 Pre-Failure Signature全17指標の計算・出力確認",
      "ObservabilityManagerが全指標を統合的に管理していることの確認",
      "構造化ログ出力（tracing）の確認",
      "AnomalyDetectorの動作確認: 異常パターン検知とアラート",
      "検証用テストを追加"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §6-7 Event Sourcing・分散システム対策の実装検証",
    "steps": [
      "§6.1 4ストリーム分割の確認: Market/Strategy/Execution/State",
      "§6.2 sequence_id単調増加と冪等性（event_id + sequence_id重複スキップ）の確認",
      "§6.3 Schema RegistryとUpcasterの動作確認",
      "§7.1 Gap Detection: 軽微ギャップ（Warning + 特徴量ホールド）と深刻ギャップ（取引停止 + Replay）",
      "§7.2 Dynamic Risk Barrier: staleness_ms付与、lot_multiplier二次関数ペナルティ",
      "§7.3 Tiered Event Store: Tier1永続/Tier2圧縮/Tier3 TTL + コールドストレージアーカイブ",
      "検証用テストを追加"
    ],
    "passes": false
  },
  {
    "category": "integration",
    "description": "フルパイプライン統合テスト: design.md準拠の完全なトレーディングループ",
    "steps": [
      "CSV読み込み → FeatureExtractor → StrategyA/B/C → RiskBarrier → ExecutionGateway → PnL → Stats の完全フロー",
      "戦略A（Liquidity Shock Reversion）のトリガー条件テスト: spread_z>3 ∧ depth_drop>θ₁ ∧ vol_spike>θ₂",
      "戦略B（Volatility Decay Momentum）のトリガーテスト",
      "戦略C（Session Structural Bias）のトリグーテスト",
      "Monte Carlo評価: エピソード完了 → 割引累積報酬計算 → BayesianLR事後更新",
      "Regime管理: 正常Regime → 未知Regime検出 → 取引停止 → Regime安定化 → 再開",
      "全戦略同時稼働 + グローバルポジション制約テスト",
      "結果のJSON出力 + Python統計検証パイプライン実行テスト"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md Critical Domain Rulesの実装監査",
    "steps": [
      "No debug_assert!確認: grep で debug_assert の使用がないことを検証",
      "情報リーク確認: 実行系特徴量とposition_pnl_unrealizedに強制ラグが適用されていること",
      "OTC市場モデル確認: 取引所モデルの板順序約定を前提としていないこと",
      "ハードリミット優先確認: リスクチェックがQ値判定より前に実行されること",
      "Thompson Sampling σ_model確認: σ_modelが点推定に含まれていないこと（Thompson Sampling内のみ）",
      "戦略分離報酬確認: 各戦略の報酬が他戦略に依存していないこと",
      "ペーパー実行安全性確認: ForwardTestが実際の発注パスに接続しないこと",
      "release build安全性確認: 全チェックがassert!またはResult<_, RiskError>であること"
    ],
    "passes": false
  },
  {
    "category": "integration",
    "description": "フォワードテストのフルパイプライン統合テスト",
    "steps": [
      "RecordedDataFeed → ForwardTestRunner → PaperExecution → PerformanceTracker → ReportGeneratorの全流れ",
      "戦略A/B/Cの個別・組み合わせテスト",
      "リスクイベントでのアラート送信確認（Log + Webhookモック）",
      "ComparisonEngine: フォワード結果 vs バックテスト結果の差分分析",
      "期間管理テスト: 指定時間でのグレースフルシャットダウン",
      "再現性テスト: 同一シード・同一データで同一結果",
      "結果レポートのJSON/CSV出力確認"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "design.md §12 運用指針に基づく破綻シナリオのストレステスト",
    "steps": [
      "連続負け自己相関シナリオ: 連続マイナスでQ(s_t,a)が急低下しhold選択に自動遷移",
      "非線形スケーリング崩壊シナリオ: Impact ∝ |position|^α でQ値指数悪化 → ロット削減",
      "時間減衰パラメータズレシナリオ: ボラティリティ急変でQ値マイナス → エントリー不可",
      "LP Adversarial Adaptationシナリオ: fill率統計的有意低下 → 自動LP切り替え",
      "Hold退化シナリオ: 初期hold支配 → 楽観的初期化 + 分散膨張による探索回復",
      "各シナリオでシステムがdesign.md通りの挙動を示すことを確認"
    ],
    "passes": false
  }
]
```

---

## Agent Instructions

1. Read `activity-verification.md` first to understand current state
2. Find next task with `"passes": false`
3. Complete all steps for that task
4. Verify with tests (`cargo test`, `pytest research/tests/`)
5. Update task to `"passes": true`
6. Log completion in `activity-verification.md`
7. Commit with `feat(verification): ...` prefix
8. Repeat until all tasks pass

**Important:** Only modify the `passes` field. Do not remove or rewrite tasks.

---

## Completion Criteria

All tasks marked with `"passes": true`
