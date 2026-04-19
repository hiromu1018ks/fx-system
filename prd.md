# FX AI準短期自動売買システム - Product Requirements Document

## Overview

機関HFT領域を回避した準短期戦略（数秒〜数分ホライズン）において、MDP + ベイズ線形回帰 + Thompson Samplingによる自己進化・自己淘汰型クオンツシステム。Event Sourcingアーキテクチャで分散システムの破綻を防止し、統計的堅牢性により過学習を撲滅する。+0.05〜0.2 pipの極小エッジを高頻度で累積し、OTC市場の約定現実をモデルに組み込んだ実戦完結型システム。

## Target Audience

- **主利用者**: 個人トレーダー（機関レベルの設計品質を前提）
- **現段階**: 研究・検証フェーズ（バックテスト・フォワードテスト・実行検証）
- **最終目標**: 実戦運用への移行（コロケーション環境）

## Core Features

1. **MDP最適化ポリシーエンジン**: ベイズ線形回帰によるQ関数推定 + Thompson Sampling行動選択
2. **3戦略統合**: Liquidity Shock Reversion(A) / Volatility Decay Momentum(B) / Session Structural Bias(C)
3. **Event Sourcingアーキテクチャ**: 4ストリーム分割、スキーマ進化管理、冪等性保証
4. **OTC市場対応約定モデル**: Last-Look拒否・Internalization・価格改善/悪化モデル
5. **階層的リスク管理**: 日次二段階 + 週次 + 月次の損失リミット、グローバルポジション制約
6. **統計的検証パイプライン**: CPCV, PBO, DSR, Sharpe天井による過学習排除
7. **HDP-HMM動的Regime管理**: バックグラウンド推論 + 軽量オンライン指標

## Tech Stack

- **Core Language**: Rust (実行基盤、所有権システムによるメモリ安全性・データ競合排除)
- **Research/ML**: Python (特徴量生成・モデル開発・バックテスト分析)
- **Model Deployment**: ONNX (学習済みモデルのRust側デプロイ)
- **Schema Definition**: Protobuf (イベントスキーマ、Schema Evolution管理)
- **Build System**: Cargo (Rust), uv/pip (Python)
- **Data Protocol**: WebSocket / FIX 4.4/5.0 (REST API禁止)
- **Storage**: NVMe SSD (Tier1/2) + コールドストレージ (Tier3)
- **Authentication**: API Key / Secret (ブローカー認証)
- **Hosting**: ローカル + クラウド (開発) → コロケーション LD4/NY4 (本番)

## Architecture

### 全体構成
Partitioned Event Busによるモジュラー・モノリス構成。全モジュールは直接状態を共有せずEvent Bus経由のみ通信。

### 4ストリーム分割
1. **Market Stream**: 市場データ入力
2. **Strategy Stream**: 判断プロセス
3. **Execution Stream**: 注文ライフサイクル
4. **State Stream**: 集約スナップショット

### コンポーネント構成
1. Market Gateway (Low Latency) → Market Stream
2. Partitioned Event Bus & Gap Detection
3. Strategy & MDP Policy Engine (Stateless)
4. Dynamic Risk Barrier (非同期・低遅延)
5. Execution Gateway (Causal-Leak-Free)
6. State & Risk Manager (Stateful Projector)
7. Tiered Event Store & Observability

## Data Model

### イベント構造
- `EventHeader`: event_id, parent_event_id, stream_id, sequence_id, timestamp_ns, schema_version, tier
- `DecisionEventPayload`: 特徴量ベクトル、Q値、Thompson Sampling統計、ポジション状態
- `ExecutionEventPayload`: 約定価格、slippage、fill確率、LP情報
- `StateSnapshotPayload`: ポジション、損益、リミット状態、グローバル制約

### MDP定式化
- **状態空間**: s_t = (X_t^market, p_t^position)
- **行動空間**: a_t ∈ {buy_k, sell_k, hold} (制約: P_max, V_max)
- **報酬関数**: 戦略分離型 r_t^i = PnL_i - λ_risk·σ²_i - λ_dd·DD_i
- **Q関数**: Q(s,a) = w_a^T·φ(s) (ベイズ線形回帰)

## UI/UX Requirements

- CLIベースの運用・監視インターフェース
- ログベースの観測性（Pre-Failure Signature監視）
- オペレーター向けアラート通知システム
- リアルタイムダッシュボード（将来拡張）

## Security Considerations

- API Key/Secretの安全な管理（.env除外設定済み）
- ネットワーク通信の暗号化（FIX/TLS）
- 不変条件の実行時検証（release build安全、debug_assert!禁止）
- ステートマシンによる不正遷移のコンパイル時防止
- Tiered Event Storeによるデータ保護

## Third-Party Integrations

- FXブローカー/LP API (FIX / WebSocket)
- コールドストレージ (S3/Glacier等)
- ONNX Runtime (Rust側モデル推論)
- 観測・監視ツール（将来拡張）

## Constraints & Assumptions

- **開発段階**: バックテスト・フォワードテスト・実行検証が現優先
- **インフラ**: 開発はローカル+クラウド、本番はコロケーション
- **言語**: Rust core + Python research のハイブリッド構成
- **エッジ**: +0.05〜0.2 pip の極小エッジ前提。コスト推定は利益計算と同等に重要
- **OTC市場**: 取引所モデル不可。Last-Look・Internalizationは構造的不可避
- **レイテンシ**: ms以下の土俵には立たない（HFT領域の回避）
- **統計**: 年率Sharpe > 1.5は強制破棄、目標0.8〜1.2

## Success Criteria

- [ ] Event Sourcing基盤が稼働し、イベントのリプレイが可能
- [ ] バックテストフレームワークでCPCV/PBO/DSRを通過する戦略が存在
- [ ] OTC約定モデルがバックテストと実戦で一貫性を持つ
- [ ] 階層的損失リミットがQ値に関わらず確実に発動する
- [ ] Thompson Samplingの事後分散が不確実性を適切に探索に変換している
- [ ] 情報リーク排除が実装され、強制ラグが検証済み
- [ ] フォワードテストでSharpe 0.8以上を達成

---

## Task List

```json
[
  {
    "category": "setup",
    "description": "Rustプロジェクト初期化とディレクトリ構造の構築",
    "steps": [
      "cargo init でRustプロジェクトを初期化",
      "ワークスペース構成でクレートを分割: core, events, strategy, execution, risk, gateway, backtest",
      "Cargo.tomlの依存関係設定（tokio, prost/tonic, serde, tracing, chrono, ndarray等）",
      ".gitignoreにRust/Python向け設定を追加",
      "cargo build でコンパイル確認"
    ],
    "passes": true
  },
  {
    "category": "setup",
    "description": "Protobufスキーマ定義（イベント構造）",
    "steps": [
      "proto/ ディレクトリ作成",
      "event_header.proto: EventHeader, EventTier enumを定義",
      "market_event.proto: MarketEventPayload（価格、板情報）を定義",
      "decision_event.proto: DecisionEventPayload（§8.1の全フィールド）を定義",
      "execution_event.proto: ExecutionEventPayload（§8.1の全フィールド）を定義",
      "state_snapshot.proto: StateSnapshotPayload（§8.1の全フィールド）を定義",
      "policy_command.proto: PolicyCommandメッセージを定義",
      "trade_skip_event.proto: TradeSkipEventメッセージを定義",
      "build.rsでprost-buildによるRustコード生成を設定"
    ],
    "passes": true
  },
  {
    "category": "setup",
    "description": "Python研究環境のセットアップ",
    "steps": [
      "pyproject.tomlまたはrequirements.txtの作成（numpy, pandas, scipy, matplotlib, jupyter等）",
      "research/ ディレクトリ構造作成（features/, models/, backtest/, analysis/）",
      "ONNXエクスポート用のユーティリティスクリプト作成",
      "Python環境の動作確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Event Busコア実装（パーティション分割ストリーム）",
    "steps": [
      "Event traitとEventHeaderのRust構造体を生成されたProtobufコードから定義",
      "StreamId enum定義（Market, Strategy, Execution, State）",
      "PartitionedEventBus実装: tokio::broadcastベースのマルチストリームイベントバス",
      "EventPublisher: ストリームへのイベント発行とシーケンスID採番",
      "EventSubscriber: ストリームの購読とフィルタリング",
      "冪等性処理: event_id + sequence_idによる重複スキップ",
      "ユニットテスト: 発行・購読・重複排除の確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Tiered Event Store実装",
    "steps": [
      "EventTier enum定義（Tier1=Critical, Tier2=Derived, Tier3=Raw）",
      "EventStore trait定義: store, load, replay メソッド",
      "Tier1Store: NVMe SSDへの永続化（SledまたはSQLiteバックエンド）",
      "Tier2Store: Delta Encoding + 圧縮アーカイブ",
      "Tier3Store: メモリ/高速SSD + TTL管理 + コールドストレージへの自動アーカイブ",
      "Schema Registry: Protobufベースの不変スキーマ管理",
      "Upcaster: 過去イベントの最新スキーマ自動変換",
      "ユニットテスト: ストア・ロード・リプレイの確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "State Projector実装（イベント → 状態スナップショット）",
    "steps": [
      "StateSnapshot構造体定義（ポジション、損益、リミット状態、state_hash）",
      "StateProjector実装: イベントストリームからStateSnapshotへの射影",
      "state_version管理とstaleness_ms計算",
      "ハッシュ検証による状態整合性確認",
      "StateSnapshotEventの定期発行",
      "ユニットテスト: イベント系列からの状態復元"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Gap Detection Engine実装",
    "steps": [
      "ティック到着間隔の統計的監視（平均 + σ）",
      "軽微ギャップ検出: 1-2ティック欠損 → Warning + 特徴量ホールド",
      "深刻ギャップ検出: 3ティック以上 → 取引停止 + Event Replay",
      "GapEventの発行とログ記録",
      "ユニットテスト: 各ギャップレベルの検出と対応"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "特徴量抽出パイプライン φ(s) の実装",
    "steps": [
      "FeatureExtractor trait定義",
      "マイクロ構造特徴量: spread, spread_zscore, OBI, ΔOBI, depth_change_rate, queue_position",
      "ボラティリティ特徴量: realized_volatility, volatility_ratio, volatility_decay_rate",
      "時間系特徴量: session(one-hot), time_since_open, time_since_last_spike, holding_time",
      "ポジション状態特徴量: position_size, direction, hold_time, entry_price, pnl_unrealized（瞬時mid-price使用）",
      "オーダーフロー/実行系特徴量: trade_intensity, signed_volume, recent_fill_rate(lagged), recent_slippage(lagged)",
      "非線形変換項: self_impact, time_decay, dynamic_cost, P(revert), P(continue), P(trend)",
      "交互作用項: spread_z×vol, OBI×session, depth_drop×vol_spike, position_size×vol等",
      "情報リーク排除: 実行系データとposition_pnl_unrealizedに強制ラグ適用",
      "ユニットテスト: 各特徴量の計算とラグ確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Q関数（ベイズ線形回帰）の実装",
    "steps": [
      "BayesianLinearRegression構造体: 事後分布 N(ŵ, Σ̂) の管理",
      "オンライン事後更新（Bayesian update）",
      "適応ノイズ分散: σ²_noise,t = EMA_variance(residuals, halflife=500)",
      "Q値計算: Q(s,a) = w_a^T·φ(s)",
      "事後分散計算: σ_model(s,a) = √(φ(s)^T·Σ̂·φ(s))",
      "発散監視: ||w_t|| / ||w_{t-1}|| > 2.0 → リセット",
      "楽観的初期化: ŵ_buy, ŵ_sell をholdより高く設定",
      "ユニットテスト: 学習・推定・発散検出"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Thompson Sampling ポリシーの実装",
    "steps": [
      "ThompsonSamplingPolicy構造体",
      "事後分布からの重みサンプリング: w̃ ~ N(ŵ, Σ̂)",
      "Q̃_final計算: w̃_a^T·φ(s) - self_impact - dynamic_cost - k·σ_non_model - latency_penalty",
      "グローバルポジション制約フィルタリング: |Σp_i + δ(a)| ≤ P_max^global",
      "行動間整合性チェック: buy/sell同時顕著正のフォールバック",
      "Q_point（点推定）の計算（監視・記録用）",
      "Hold退化防止: 最小取引頻度監視 + 事後分散膨張",
      "ユニットテスト: サンプリング・制約・整合性"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "戦略A: Liquidity Shock Reversion の実装",
    "steps": [
      "StrategyA用の特徴量パイプライン φ_A(s) 実装",
      "トリガー条件: spread_z > 3 ∧ depth_drop > θ₁ ∧ vol_spike > θ₂ ∧ regime_kl < threshold",
      "戦略A固有非線形項: Self_Impact, P(revert), decay_A (数秒スケール)",
      "戦略A固有交互作用項: depth_drop×vol_spike, spread_z×OBI",
      "エピソード管理: MAX_HOLD_TIME（数秒〜数十秒）",
      "ユニットテスト: トリガー・特徴量計算・エピソード終了"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "戦略B: Volatility Decay Momentum の実装",
    "steps": [
      "StrategyB用の特徴量パイプライン φ_B(s) 実装",
      "戦略B固有非線形項: P(continue), decay_B (数分スケール, λ_B << λ_A)",
      "戦略B固有交互作用項: rv_spike×trend, OFI×intensity",
      "エピソード管理: MAX_HOLD_TIME（数分〜数十分）",
      "ユニットテスト: 特徴量計算・エピソード管理"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "戦略C: Session Structural Bias の実装",
    "steps": [
      "StrategyC用の特徴量パイプライン φ_C(s) 実装",
      "戦略C固有非線形項: P(trend) = Adaptive_Estimate(last_N_days, decay_weighting), decay_C",
      "戦略C固有交互作用項: session×OBI, range_break×liquidity_resiliency",
      "エピソード管理: MAX_HOLD_TIME（戦略C固有）",
      "ユニットテスト: 特徴量計算・エピソード管理"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "On-policy Monte Carlo評価の実装",
    "steps": [
      "エピソード定義: 開始=ポジション非ゼロ、終端=クローズ/MAX_HOLD_TIME/ハードリミット/未知Regime",
      "割引累積報酬 G_t = Σ γ^k·r_{t+k} の計算",
      "戦略分離型報酬: r_t^i = PnL_i - λ_risk·σ²_i - λ_dd·DD_i（DD_cap付き）",
      "部分約定の扱い: 全量クローズ時のみエピソード終了",
      "MAX_HOLD_TIME到達時の強制クローズとPnL組み込み",
      "Deadly Triad回避の確認（On-policy + MC + ベイズ正則化）",
      "ユニットテスト: エピソード完了と報酬計算"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "HDP-HMM Regime管理の実装",
    "steps": [
      "HDP-HMMバックグラウンド推論エンジン（Python側で実装、ONNXエクスポート）",
      "軽量オンラインRegime指標（Rust側）: キャッシュからの事後確率取得",
      "Regime entropy計算と未知Regime検出（閾値超で取引停止）",
      "Drift推定: drift_t = Σ_k(π_k · f_k(drift_{t-1}, X_t))",
      "RegimeCache: 最新事後確率の高速アクセス",
      "ユニットテスト: regime推定・エントロピー閾値・drift計算"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "Dynamic Risk Barrierの実装",
    "steps": [
      "Staleness検知: staleness_ms計算とlot_multiplier算出",
      "二次関数ペナルティ: lot_multiplier = max(0, 1 - (staleness/threshold)²)",
      "動的ロット数制限（staleness閾値超で取引停止）",
      "コマンド通過型設計（同期待機なし、staleness_ms付与のみ）",
      "ユニットテスト: staleness計算・ペナルティカーブ"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "階層的損失リミットの実装（日次二段階+週次+月次）",
    "steps": [
      "日次第一段階（MTM警戒）: MTM PnL < -MAX_DAILY_LOSS_MTM → ロット25%制限 + Q閾値制限",
      "日次第二段階（実現損益ハードストップ）: realized PnL < -MAX_DAILY_LOSS_REALIZED → 全クローズ + halt",
      "週次ハードリミット: weekly PnL < -MAX_WEEKLY_LOSS → 全クローズ + 翌週までhalt",
      "月次ハードリミット: monthly PnL < -MAX_MONTHLY_LOSS → 全クローズ + オペレーター承認必須",
      "決定関数内での全段階チェック（Q値判定より優先）",
      "validate_orderへの統合（Rust Result<_, RiskError>で実装）",
      "ユニットテスト: 各段階の発動とクローズ確認"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "グローバルポジション制約の実装",
    "steps": [
      "グローバル制約: |Σ p_i| ≤ P_max^global",
      "相関調整: P_max^global = Σ P_max^i / max(correlation_factor, FLOOR_CORRELATION)",
      "戦略間優先度: Q値最高の戦略を優先、下位戦略はロット削減",
      "FLOOR_CORRELATION: ストレスイベント時の下限値（推奨1.5-2.0）",
      "validate_orderへの統合",
      "ユニットテスト: 制約チェック・相関調整・優先度"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "OTC市場対応Execution Gatewayの実装",
    "steps": [
      "Last-Look拒否モデル: P(not_rejected | last_look) の推定",
      "Effective Fill確率: P(fill_requested) × P(not_rejected) + ε_hidden（Student's t分布）",
      "価格改善/悪化モデル: slippage ~ f(direction, size, vol, LP_state)",
      "Passive/Aggressive判定: Expected_Profit と fill_effective に基づくLimit/Market選択",
      "LP行動適応リスク監視: fill率低下 → adversarial signal → 自動LP切り替え",
      "OrderCommand発行とFillEvent/RejectEvent受信",
      "ユニットテスト: fill確率計算・slippageモデル・LP切り替え"
    ],
    "passes": true
  },
  {
    "category": "feature",
    "description": "LP切り替え再校正プロトコルの実装",
    "steps": [
      "安全モード移行: LP切り替え直後のロット25%制限",
      "パラメータ再校正: β₁, β₂, slippage分布, fill率の再推定",
      "σ_execution暫定値の2倍設定",
      "校正完了判定: 推定誤差閾値以下で安全モード解除",
      "Thompson Sampling協調: 校正期間中の自然な保守的行動",
      "ユニットテスト: 再校正フロー・完了判定"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "Market Gateway実装（WebSocket/FIX Handler）",
    "steps": [
      "WebSocket受信ハンドラ（ティック・板データ）",
      "FIX 4.4/5.0発注プロトコルハンドラ",
      "MarketEvent (Tier3) の生成とMarket Streamへの発行",
      "レイテンシ測定とlatency_msの記録",
      "接続管理・再接続・ハートビート",
      "ユニットテスト: データ受信・イベント生成・レイテンシ"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "Online Change Point Detectionの実装",
    "steps": [
      "ADWINまたは類似アルゴリズムによる特徴量分布変化検知",
      "change_point検出時の事後分布部分的リセット",
      "オフライン再学習トリガーの発行",
      "ユニットテスト: 変化点検出とリセット"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "Lifecycle Manager（自動淘汰）の実装",
    "steps": [
      "Rolling Sharpe計算と監視",
      "Regime別PnL監視",
      "「死の閾値」下回り時の新規エントリーハードブロック",
      "既存ポジションの自動クローズ機構",
      "ユニットテスト: 淘汰判定とクローズ"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "ハードウェア級Kill Switchの実装",
    "steps": [
      "ティック到着間隔の統計監視（平均 ± 3σ）",
      "逸脱検出時の10-50ms以内の発注マスク",
      "非同期シグナルハンドリング（tokio signal）",
      "ユニットテスト: キルスイッチ発動"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "Observability（Pre-Failure Signature）の実装",
    "steps": [
      "§8.2の全指標の計算・出力: rolling_variance_latency, feature_distribution_kl_divergence等",
      "構造化ログ出力（tracingクレート）",
      "Anomaly Detector: 異常パターンの検知とアラート",
      "ユニットテスト: 各指標の計算とアラート"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "バックテストフレームワークの実装",
    "steps": [
      "履歴MarketEventのEvent Storeへのロード",
      "時間圧縮リプレイ（リアルタイムより高速）",
      "シミュレーション実行結果の記録",
      "パフォーマンス統計の計算（Sharpe, DD, Win rate等）",
      "ユニットテスト: リプレイと統計計算"
    ],
    "passes": false
  },
  {
    "category": "feature",
    "description": "統計的検証パイプラインの実装（Python）",
    "steps": [
      "CPCV (Combinatorial Purged Cross-Validation): 時系列リーク防止",
      "PBO (Probability of Backtest Overfitting): >0.1は破棄",
      "DSR (Deflated Sharpe Ratio): >=0.95を必須",
      "Sharpe天井: 年率>1.5は強制破棄",
      "複雑度ペナルティ: Sharpe / sqrt(num_features)",
      "Live Degradation Test: OOS Sharpe低下30%以内",
      "情報リーク検証: ラグ有無での精度比較",
      "報酬関数感度分析: λ_risk, λ_dd, DD_cap の摂動"
    ],
    "passes": false
  },
  {
    "category": "integration",
    "description": "統合テスト: エンドツーエンド取引フロー",
    "steps": [
      "Market Gateway → Feature Extraction → Strategy → Risk Barrier → Execution の全流れ",
      "イベントリプレイによる決定の再現性確認",
      "階層的リミットの発動テスト",
      "OTC約定モデルとの統合テスト",
      "複数戦略同時稼働時のグローバルポジション制約テスト"
    ],
    "passes": false
  }
]
```

---

## Agent Instructions

1. Read `activity.md` first to understand current state
2. Find next task with `"passes": false`
3. Complete all steps for that task
4. Verify with tests (`cargo test`)
5. Update task to `"passes": true`
6. Log completion in `activity.md`
7. Repeat until all tasks pass

**Important:** Only modify the `passes` field. Do not remove or rewrite tasks.

---

## Completion Criteria
All tasks marked with `"passes": true`
