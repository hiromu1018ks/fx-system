# フォワードテストシステム - Product Requirements Document

## Overview

FX AI準短期自動売買システムのフォワードテスト（実市場データ＋ペーパー実行）システム。バックテストで検証済みの戦略を、録音済みデータのリアルタイム再生を経て実際の市場データに接続し、ペーパー実行でパフォーマンス・リスク制御・実行品質を検証する。既存のEvent Sourcingアーキテクチャ、OTC約定モデル、階層的リスク管理をそのまま活用し、新規クレート `crates/forward/` に実装する。

## Target Audience

- **主利用者**: システム開発者・運用者
- **目的**: バックテスト結果の実市場検証、実行品質の定量的評価、リスク制御の動作確認
- **最終目標**: 実戦運用への移行判断材料の提供

## Core Features

1. **段階的データフィード**: 録音データのリアルタイム再生 → 外部FX API接続の段階的切替
2. **ペーパー実行エンジン**: 既存OTC約定モデルを使ったシミュレート実行（実際の発注なし）
3. **リアルタイムパフォーマンス追跡**: Rolling Sharpe、PnL、ドローダウンの継続監視
4. **バックテスト比較**: フォワードテスト結果とバックテスト結果の自動比較・差分分析
5. **リスク監視・アラート**: 設定可能な通知（ログ + Webhook）によるリアルタイムリスク監視
6. **戦略個別選択**: 各戦略（A/B/C）を個別に有効化/無効化可能

## Tech Stack

- **Core Language**: Rust (新規クレート `fx-forward` を追加)
- **Data Feed**: 録音データ再生 + 外部API (OANDA等) のtrait-based抽象化
- **Execution**: 既存 `fx-execution` のOTCモデルをペーパー実行に適用
- **Alerts**: 設定可能な通知チャネル（ログ tracing + Webhook HTTP POST）
- **Reports**: JSON + CSV形式の出力
- **Configuration**: TOMLベースの設定ファイル

## Architecture

### 全体構成

既存のEvent Sourcingアーキテクチャの上に、Forward Test Layerを構築する。既存クレート（events, strategy, execution, risk, backtest）をそのまま活用し、`crates/forward/` にオーケストレーション・データフィード・レポート機能を集約する。

### コンポーネント構成

1. **Data Feed Adapter**: `MarketFeed` traitによるデータソース抽象化
   - `RecordedDataFeed`: Event Storeからの録音データ再生（リアルタイムまたは加速）
   - `ExternalApiFeed`: 外部FX API接続のtrait実装（OANDA等）
2. **Paper Execution Engine**: 発注を行わずOTCモデルで約定シミュレーション
3. **Forward Test Runner**: メインオーケストレーション（戦略選択、期間管理）
4. **Performance Tracker**: リアルタイムパフォーマンス指標の計算・保持
5. **Risk Alert System**: 閾値ベースのアラート生成（ログ + Webhook）
6. **Comparison Engine**: バックテスト結果との差分分析
7. **Report Generator**: JSON/CSV形式のレポート出力

### データフロー

```
DataFeedAdapter → MarketGateway → EventBus(Market Stream)
                                        ↓
FeatureExtractor → Strategy Engine → Risk Barrier → Paper Execution
                                        ↓                    ↓
                              EventBus(Strategy)    EventBus(Execution)
                                        ↓                    ↓
                              PerformanceTracker    ComparisonEngine
                                        ↓                    ↓
                              RiskAlertSystem     ReportGenerator
```

## Data Model

### Forward Test Configuration

```rust
struct ForwardTestConfig {
    enabled_strategies: HashSet<String>,  // "A", "B", "C" の部分集合
    data_source: DataSourceConfig,
    duration: Option<Duration>,           // None = 無期限
    alert_config: AlertConfig,
    report_config: ReportConfig,
    risk_config: ForwardRiskConfig,
    comparison_config: Option<ComparisonConfig>,
}

enum DataSourceConfig {
    Recorded { event_store_path: String, speed: f64 },  // speed: 1.0=realtime, 0=max
    ExternalApi { provider: String, credentials_path: String, symbols: Vec<String> },
}
```

### Performance Metrics

- 累積PnL（realized + unrealized）
- Rolling Sharpe Ratio（年率化）
- 最大ドローダウン
- 勝率・損率
- 平均約定スリッページ
- Fill確率（expected vs actual）
- Execution Drift（予測 vs 実績の乖離統計）
- EV補正頻度
- 戦略別パフォーマンス内訳

### Alert Types

- リスク制限接近/発動
- Execution Drift異常
- Kill Switch発動
- Rolling Sharpe低下
- 戦略淘汰
- データフィード異常（gap、切断）

## UI/UX Requirements

- CLIベースの実行・制御インターフェース
- 構造化ログ出力（tracingクレート、JSON形式）
- セッション終了時のJSON/CSVレポート自動生成
- リスクイベントのリアルタイムログ出力

## Security Considerations

- API認証情報の安全な管理（設定ファイル経由、.env除外設定）
- Webhook URLの設定ファイル管理
- ペーパー実行であることの明示的保証（実際の発注を行わない構造的保証）

## Third-Party Integrations

- 外部FX API (OANDA等): trait-based抽象化で将来的な追加対応
- Webhook通知: Slack/Discord等へのHTTP POST
- 既存Event Store: 録音データの読み込み

## Constraints & Assumptions

- **既存クレートの変更最小化**: forward crateから既存crateを利用し、既存crateの変更は最小限
- **ペーパー実行の保証**: 実際の発注パス（FIX/WebSocket order）には一切接続しない
- **既存リスク管理の再利用**: Dynamic Risk Barrier、階層的損失リミット、Kill Switch等をそのまま適用
- **戦略個別選択**: ForwardTestConfig.enabled_strategiesで制御
- **段階的データフィード**: RecordedDataFeedで検証後、ExternalApiFeedに切替可能

## Success Criteria

フォワードテストシステムの成功は以下の複合条件で定義する：

1. **バックテストとの差分が説明可能であること**
   - PnL・約定率・スリッページの差がExecution/Latency/Impactで説明できる
2. **Execution Driftが許容範囲内であること**
   - expected vs actual fillの乖離が統計的に安定
3. **リスク制御が全て正しく動作すること**
   - kill switch・ポジション制限・staleness制御
4. **EV補正ロジックが過剰/過小でないこと**
   - ev_adjustment_frequencyが異常値でない
5. **システムが再現可能であること**
   - 同一入力に対し同一結果が再現できる（決定的シード対応）

---

## Task List

```json
[
  {
    "category": "setup",
    "description": "Forward Test crate初期化（crates/forward/）",
    "steps": [
      "crates/forward/ ディレクトリとCargo.toml作成",
      "Cargo.toml workspaceの members に forward を追加",
      "lib.rs にモジュールスタブ配置（feed, paper, runner, tracker, alert, comparison, report, config）",
      "fx-forward の依存関係設定（fx-events, fx-strategy, fx-execution, fx-risk, fx-backtest, fx-core, fx-gateway）",
      "cargo build でコンパイル確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Data Feed trait + Recorded Data Replay adapterの実装",
    "steps": [
      "MarketFeed trait定義: connect, subscribe, next_tick, disconnect, is_connected",
      "TickReceiver trait定義: 非同期ティック受信インターフェース",
      "RecordedDataFeed実装: Event StoreからのMarketEvent読み込み → TickData変換 → リアルタイムペースまたは高速再生",
      "再生速度制御: speed=1.0でリアルタイム、speed=0で最大速度、speed=2.0で2倍速",
      "時間範囲フィルタリング: start_time/end_timeによる録音データの部分再生",
      "ユニットテスト: trait実装確認、再生速度制御、時間範囲フィルタ、イベント終端処理"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Paper Execution Engineの実装",
    "steps": [
      "PaperExecutionEngine構造体: 実際の発注を行わないペーパー実行エンジン",
      "既存OTCモデル（Last-Look, slippage, fill probability）を使った約定シミュレーション",
      "PaperOrderResult: 約定価格、slippage、fill確率、拒否理由を記録",
      "ペーパー実行の構造的保証: 発注パス（FIX/WebSocket order）に一切接続しない設計",
      "ExecutionEvent（proto形式）の生成とEventBus(Execution Stream)への発行",
      "ユニットテスト: ペーパー約定/拒否/slipppage計算、実際の発注が行われないことの確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Forward Test Configurationの実装",
    "steps": [
      "ForwardTestConfig構造体: enabled_strategies, duration, data_source, alert_config, report_config",
      "DataSourceConfig enum: Recorded（path, speed）, ExternalApi（provider, credentials, symbols）",
      "AlertConfig構造体: alert_channels（Log, Webhook URL）, 各alert type の閾値",
      "ReportConfig構造体: output_dir, format（JSON/CSV）, interval",
      "ForwardRiskConfig: 既存リスク設定のforward test向けラッパー",
      "ComparisonConfig: バックテスト結果パス、比較対象指標",
      "TOML設定ファイルからの読み込み（serde + toml）",
      "ユニットテスト: 設定の構築/TOML読み込み/デフォルト値/バリデーション"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Forward Test Runner（コアオーケストレーション）の実装",
    "steps": [
      "ForwardTestRunner構造体: データフィード→戦略→リスク→ペーパー実行のオーケストレーション",
      "戦略選択ロジック: enabled_strategiesに基づく戦略A/B/Cの有効化/無効化",
      "メインループ: ティック受信 → 特徴量抽出 → 戦略決定 → リスクバリア → ペーパー実行",
      "実行期間管理: duration到達時のグレースフルシャットダウン、残ポジションクローズ",
      "既存コンポーネント統合: StateProjector, GapDetector, ChangePointDetector, LifecycleManager",
      "ForwardTestResult: 実行サマリー（期間、トレード数、PnL、統計）",
      "ユニットテスト: ランナー作成、戦略フィルタリング、期間管理、グレースフルシャットダウン"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Performance Tracker（リアルタイムパフォーマンス指標）の実装",
    "steps": [
      "PerformanceTracker構造体: リアルタイムパフォーマンス指標の計算・保持",
      "累積PnL追跡: realized + unrealized PnLの継続更新",
      "Rolling Sharpe計算: スライディングウィンドウベースの年率化Sharpe",
      "最大ドローダウン追跡: equity peak からの最大下落",
      "戦略別パフォーマンス内訳: 各戦略の個別PnL/Sharpe/DD",
      "Execution Drift追跡: expected vs actual fillの乖離統計",
      "EV補正頻度追跡: 動的コスト調整の頻度監視",
      "PerformanceSnapshot: 任意時点のパフォーマンススナップショット",
      "ユニットテスト: PnL計算、Rolling Sharpe、DD、戦略別内訳、drift追跡"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Risk Alert System（設定可能通知）の実装",
    "steps": [
      "AlertChannel trait定義: send_alert(alert: Alert) の非同期インターフェース",
      "LogAlertChannel実装: tracingクレートによるログ出力（warn/errorレベル）",
      "WebhookAlertChannel実装: HTTP POSTによる外部通知（Slack/Discord等）",
      "Alert構造体: alert_type, severity, message, timestamp, metrics_snapshot",
      "AlertType enum: RiskLimit, ExecutionDrift, KillSwitch, SharpeDegradation, StrategyCulled, FeedAnomaly",
      "AlertEvaluator: 閾値ベースのアラート評価（PerformanceTracker/既存リスク状態から判定）",
      "設定可能チャネル: AlertConfigでチャネル選択（ログのみ/ログ+Webhook/カスタム）",
      "デバウンス: 同一アラートの連続発火防止",
      "ユニットテスト: 各チャネル送信、閾値評価、デバウンス、設定確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Backtest-Forward Comparison Engineの実装",
    "steps": [
      "ComparisonEngine構造体: フォワードテスト結果とバックテスト結果の比較",
      "ComparisonConfig: 比較対象指標の設定",
      "指標比較: PnL差分、勝率差分、Sharpe差分、ドローダウン差分",
      "約定品質比較: fill rate、平均slippage、execution drift のバックテスト vs フォワード",
      "差分説明可能性分析: PnL差分をExecution/Latency/Impact成分に分解",
      "ComparisonReport: 比較結果の構造化レポート（overall_pass, per_metric_details）",
      "ユニットテスト: 指標比較、差分分解、レポート生成、一貫性判定"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Report Generator（JSON/CSV出力）の実装",
    "steps": [
      "ReportGenerator構造体: JSON/CSV形式のレポート出力",
      "セッションサマリーレポート: 全期間の統計サマリー",
      "戦略パフォーマンスレポート: 各戦略の個別パフォーマンス",
      "リスクイベントレポート: 発動したリスク制限・アラートの履歴",
      "比較レポート: バックテスト vs フォワードテストの差分分析",
      "Execution品質レポート: fill rate、slippage、driftの分析",
      "JSON出力: serde_jsonによる構造化出力",
      "CSV出力: トレード履歴・指標の時系列出力",
      "ユニットテスト: 各フォーマットでの出力確認、内容検証"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "External API Data Feed Adapter（trait実装）の実装",
    "steps": [
      "ExternalApiFeed trait実装のインターフェース定義",
      "ApiFeedConfig: provider名、認証情報パス、symbols、再接続設定",
      "OANDA adapterのtrait実装スケルトン: REST streaming API接続のインターフェース",
      "認証管理: API Key/Tokenの安全な読み込み",
      "エラーハンドリング: 接続失敗・タイムアウト・認証エラーの処理",
      " RecordedDataFeed との切替可能な設計（同じMarketFeed traitを実装）",
      "ユニットテスト: trait適合性、設定構築、エラーハンドリング（モック使用）"
    ],
    "passes": true
  },
  {
    "category": "integration",
    "description": "エンドツーエンド フォワードテスト統合（録音データ使用）",
    "steps": [
      "録音データ（合成またはバックテスト生成）を使用したフルパイプライン統合テスト",
      "RecordedDataFeed → ForwardTestRunner → Paper Execution → Performance Tracker → Report Generator の全流れ",
      "戦略個別選択の動作確認（Aのみ/B+C等の組み合わせ）",
      "期間管理テスト: 指定時間でのグレースフルシャットダウン",
      "リスク制御の統合テスト: 階層的リミット・Kill Switch・ポジション制約の発動確認",
      "再現性テスト: 同一シードで同一結果が得られることの確認",
      "アラート統合テスト: リスクイベントでのログ/Webhook送信確認"
    ],
    "passes": false
  },
  {
    "category": "validation",
    "description": "フォワードテスト成功基準の検証テスト",
    "steps": [
      "バックテスト差分の説明可能性テスト: PnL・約定率・スリッページの差をExecution/Latency/Impact成分で分解し、残差が閾値内であること",
      "Execution Drift安定性テスト: expected vs actual fillの乖離が統計的に有意でないこと（平均±2σ内）",
      "リスク制御完全動作テスト: kill switch・ポジション制限・staleness制御が全て発動すること",
      "EV補正頻度正常性テスト: ev_adjustment_frequencyが統計的異常値でないこと",
      "再現性テスト: 同一入力・同一シードで100%同一結果が再現されること",
      "統合バリデーション: 全5基準をパスするエンドツーエンドシナリオ"
    ],
    "passes": false
  }
]
```

---

## Agent Instructions

1. Read `activity-forward.md` first to understand current state
2. Find next task with `"passes": false`
3. Complete all steps for that task
4. Verify with tests (`cargo test`)
5. Update task to `"passes": true`
6. Log completion in `activity-forward.md`
7. Repeat until all tasks pass

**Important:** Only modify the `passes` field. Do not remove or rewrite tasks.

---

## Completion Criteria
All tasks marked with `"passes": true`
