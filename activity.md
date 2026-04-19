# FX AI準短期自動売買システム - Activity Log

## Current Status
**Last Updated:** 2026-04-19
**Tasks Completed:** 14
**Current Task:** Task 15: HDP-HMM Regime管理の実装

---

## Session Log

### 2026-04-19 — PRD作成セッション
- **完了**: 設計書(docs/spec/design.md)に基づくPRD作成
- **完了**: PROMPT.mdをRust/Python向けに更新
- **完了**: .claude/settings.jsonにcargo/rustc/uv/pytest等のパーミッション追加
- **完了**: activity.mdの初期化

<!-- Agent will append dated entries here -->

### 2026-04-19 — Task 1: Rustプロジェクト初期化とディレクトリ構造の構築
- **完了**: Cargo workspace構成で7クレート作成 (core, events, strategy, execution, risk, gateway, backtest)
- **完了**: 各クレートにCargo.toml、lib.rs、スタブモジュール配置
- **完了**: .gitignoreにRust/Python向け設定追加
- **完了**: cargo build, cargo test, cargo clippy, cargo fmt全て通過
- **依存関係**: tokio, serde, prost, tonic, tracing, chrono, ndarray, nalgebra, sled, ort, rand, thiserror等

### 2026-04-19 — Task 2: Protobufスキーマ定義（イベント構造）
- **完了**: 7つのprotoファイル作成 (event_header, market_event, decision_event, execution_event, state_snapshot, policy_command, trade_skip_event)
- **完了**: build.rsでprost-build + protoc-bin-vendoredによるRustコード生成を設定
- **完了**: lib.rsで生成されたコードを`pub mod proto`として公開
- **修正**: OrderType enumの`MARKET`/`LIMIT`を`ORDER_MARKET`/`ORDER_LIMIT`にリネーム（StreamId::MARKETとの名前衝突解消）
- **追加依存**: protoc-bin-vendored = "3" (protocバンドル版、システムprotoc不要)
- **検証**: cargo build, cargo test, cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 3: Python研究環境のセットアップ
- **完了**: pyproject.toml作成（numpy, pandas, scipy, matplotlib, jupyterlab, scikit-learn, onnx, onnxruntime, skl2onnx等）
- **完了**: mise.tomlにpython 3.12, uv latestを追加
- **完了**: research/ ディレクトリ構造作成（features/, models/, backtest/, analysis/, tests/）
- **完了**: ONNXエクスポートユーティリティ（research/models/onnx_export.py）作成：Bayesian LR Q関数のONNXエクスポート、バリデーション機能
- **完了**: .venv作成、pip bootstrap経由で依存パッケージインストール
- **完了**: pytest 3テスト全て通過（依存import確認、パッケージ構造確認、onnx_export確認）
- **検証**: cargo build, cargo test, cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 4: Event Busコア実装（パーティション分割ストリーム）
- **完了**: Event trait + GenericEvent実装（crates/events/src/event.rs）
- **完了**: PartitionedEventBus: tokio::broadcastベースの4ストリーム（Market, Strategy, Execution, State）パーティション分割イベントバス
- **完了**: EventPublisher: ストリームへのイベント発行、RwLockベースのアトミック・シーケンスID採番
- **完了**: EventSubscriber: 複数ストリーム購読、StreamIdフィルタリング（recv/recv_from）
- **完了**: 冪等性処理: HashSet<UUID>によるevent_id重複スキップ
- **ユニットテスト**: 8テスト全て通過（発行・購読・シーケンス増分・マルチストリーム分離・冪等性・recv_from・サブスクライバなし発行・ストリーム別シーケンスカウンタ）
- **検証**: cargo build, cargo test (8 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 5: Tiered Event Store実装
- **完了**: EventStore trait定義 (store, load, replay, remove メソッド)
- **完了**: Tier1Store: Sled永続化バックエンド (NVMe SSD向け)、イベント+ストリームインデックスツリー
- **完了**: Tier2Store: Delta Encoding (XOR) + flate2/Deflate圧縮、定期ベースイベント (デフォルト10イベント間隔) からのデルタチェーン再構築
- **完了**: Tier3Store: インメモリ + TTL管理 + コールドストレージ (JSON) への自動アーカイブ
- **完了**: SchemaRegistry: Protobuf不変スキーマ管理、バージョン登録・取得・最新版追跡
- **完了**: Upcaster: スキーマバージョン間変換チェーン対応、upcast_to_latest で一括最新化
- **追加依存**: sled (workspace), flate2 = "1", tempfile = "3" (workspace/events)
- **ユニットテスト**: 26新規テスト全て通過 (tier1: 6, tier2: 7, tier3: 6, schema: 7)
- **検証**: cargo build, cargo test (34 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 6: State Projector実装（イベント → 状態スナップショット）
- **完了**: Position構造体（戦略別ポジション追跡: size, entry_price, unrealized/realized PnL, entry_timestamp_ns）
- **完了**: LimitStateData構造体（日次/週次/月次PnL + リミットフラグ管理）
- **完了**: StateSnapshot構造体（集約状態: positions, global_position, PnL, limit_state, staleness_ms, state_hash, lot_multiplier）
- **完了**: StateProjector実装: Market/Strategy/Executionストリームからのイベント射影
  - MarketEvent → last_market_data_ns更新, staleness_msリセット, unrealized PnL再計算
  - DecisionEvent → last_active_strategy追跡, staleness再計算
  - ExecutionEvent → ポジション更新（新規/追加/部分決済/全決済/ショート対応）
- **完了**: state_version管理（状態変更ごとにインクリメント + ハッシュ再計算）
- **完了**: staleness_ms計算（イベントタイムスタンプベース、リプレイ安全）
- **完了**: lot_multiplier導出（二次関数ペナルティ: max(0, 1 - (staleness/5000ms)²)）
- **完了**: ハッシュ検証（DefaultHasherによる決定的ハッシュ、全状態フィールド涵盖）
- **完了**: StateSnapshotEvent発行（proto::StateSnapshotPayloadとしてState Streamへpublish）
- **完了**: 外部インターフェース: update_limit_state, set_lot_multiplier, process_execution_for_strategy
- **ユニットテスト**: 26新規テスト全て通過（初期状態, タイムスタンプ更新, unrealized PnL, ポジション開設/決済/部分決済/追加/ショート, 拒否無視, version増分, hash整合性/変更/決定性, staleness計算, lot_multiplier, 戦略追跡, snapshot発行, イベント系列復元, limit_state更新, proto roundtrip, lot_multiplier clamp, holding_time_ms, 複数戦略独立, ゼロfill無視）
- **検証**: cargo build, cargo test (60 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 7: Gap Detection Engine実装
- **完了**: proto/gap_event.proto 作成 (GapSeverity enum: MINOR/SEVERE, GapEventPayload message)
- **完了**: GapDetector実装 (crates/events/src/gap_detector.rs)
  - ティック到着間隔の統計的監視: Welfordオンラインアルゴリズム + EMA追跡
  - 軽微ギャップ検出: 1-2ティック欠損 → GapLevel::Minor (Warning + 特徴量ホールド)
  - 深刻ギャップ検出: 3ティック以上 → GapLevel::Severe (取引停止 + Event Replay)
  - z-scoreベース検出: min_samples以上のデータがある場合、z >= 3.0 でMinor検出
  - GapEventのStrategy Streamへの自動発行 (Tier1Critical)
  - 構造化ログ出力 (tracing: warn for Minor, error for Severe)
- **完了**: EventPublisher に Clone derive 追加 (bus.rs)
- **完了**: build.rs に gap_event.proto 追加
- **ユニットテスト**: 20新規テスト全て通過（正常ティック, 1/2/3/5ティック欠損, min_samples前シーケンスベース検出, z-score検出, 連続シーケンス正常, 小分散正常, 統計更新後, GapInfoフィールド確認, 初回ティック, 逆方向タイムスタンプ無視, Strategy Stream発行Minor/Severe, 正常ティック非発行, 複数ギャップ連続, 回復, 平均・標準偏差収束）
- **検証**: cargo build, cargo test (80 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 8: 特徴量抽出パイプライン φ(s) の実装
- **完了**: FeatureVector構造体（34次元）の定義とflattened()/from_flattened()ラウンドトリップ対応
- **完了**: FeatureExtractor実装 (crates/strategy/src/extractor.rs)
  - マイクロ構造特徴量: spread, spread_zscore (RollingWindow z-score), OBI, ΔOBI, depth_change_rate, queue_position
  - ボラティリティ特徴量: realized_volatility (log-return std), volatility_ratio (short/long), volatility_decay_rate
  - 時間系特徴量: session (one-hot: Tokyo/London/NY/Sydney), time_since_open, time_since_last_spike, holding_time
  - ポジション状態特徴量: position_size, direction, entry_price, pnl_unrealized (StateSnapshot由来)
  - オーダーフロー/実行系特徴量: trade_intensity, signed_volume, recent_fill_rate (EMA), recent_slippage (EMA)
  - 非線形変換項: self_impact (Kyle's lambda簡易版), time_decay (exp(-λt)), dynamic_cost (spread+OBI+vol premium), P(revert), P(continue), P(trend)
  - 交互作用項: spread_z×vol, OBI×session, depth_drop×vol_spike, position_size×vol
- **完了**: 情報リーク防止の実装
  - 実行系データ (fill_rate, slippage) にfirst_execution_nsベースの強制ラグ適用
  - pnl_unrealizedはStateProjectorが計算した値（前回mid-price基準、本質的に1ティックラグ付き）
  - LaggedExecutionStats: EMA更新 + ウィンドウベース集計 + first_execution_ns追跡
- **完了**: 内部ユーティリティ: RollingWindow (online mean/var/z-score), VolatilityState (log-return vol), Session列挙型
- **追加依存**: prost, uuid (fx-strategy)
- **ユニットテスト**: 66新規テスト全て通過（マイクロ構造: 6, ボラティリティ: 4, 時間: 6, ポジション: 4, 実行系: 6, 非線形: 5, 交互作用: 4, 情報リーク: 3, エッジケース: 4, RollingWindow: 5, LaggedExec: 2, VolState: 2, FeatureVector: 4, セッション: 3, 統合: 2, デコードエラー: 3, gap_hold: 1）
- **検証**: cargo build, cargo test (146 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 9: Q関数（ベイズ線形回帰）の実装
- **完了**: BayesianLinearRegression構造体 (crates/strategy/src/bayesian_lr.rs)
  - 事後分布 N(ŵ, Σ̂) の管理: Σ̂ = σ²_noise,t · (Φ^T Φ + λ_reg I)^{-1}
  - オンライン事後更新: Sherman-Morrison公式による効率的A_inv更新
  - 適応ノイズ分散: EMA_variance(residuals, halflife=500)、フロア値1e-10で下限保護
  - Q値計算: Q(s,a) = ŵ^T φ(s) — σ_modelは点推定に含めない（Thompson Samplingのみで反映）
  - 事後分散: σ_model(s,a) = √(σ²_noise · φ(s)^T A_inv φ(s))
  - 発散監視: ||w_t|| / ||w_{t-1}|| > 2.0 → 検出（初回5観測はスキップ）
  - 楽観的初期化: apply_optimistic_bias で b を設定し ŵ = bias·ones を実現
  - 共分散膨張: inflate_covariance によるhold退化防止機構対応
  - リセット: reset（ノイズ推定維持）/ reset_full（全初期化）
- **完了**: QFunctionラッパー (crates/strategy/src/bayesian_lr.rs)
  - QAction列挙型 (Buy, Sell, Hold) による3行動の独立した事後分布管理
  - 楽観的初期化: Buy/Sellにのみバイアス適用、Holdはゼロ
  - On-policy更新: update(action, phi, target) で特定行動のみ更新
  - Thompson Sampling: sample_weights / sample_q_value による事後サンプリング
  - 監視用API: q_values / posterior_stds による全行動のQ値・事後std取得
  - リセット時の楽観的初期化自動復元
- **ユニットテスト**: 32新規テスト全て通過
  - BayesianLinearRegression: 作成, ゼロ初期予測, 単一更新, 既知重みへの収束, 事後std減少, 事後std非負, 適応ノイズ分散, 発散検出, 発散誤検出なし, リセット, 完全リセット, 共分散膨張, 膨張係数検証, 下限値パニック, サンプル重み分布, サンプル予測vs点推定, 楽観的初期化, バイアス希釈, Sherman-Morrison等価性, 残差確認
  - QFunction: 作成と楽観的バイアス検証, 単一行動更新, 全Q値取得, サンプルQ値分散, 事後std取得, 行動リセット, 全体リセット, 完全リセット, 共分散膨張, 点推定等価性, FeatureVector DIM対応, ノイズ分散フロア
- **検証**: cargo build, cargo test (178 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 10: Thompson Sampling ポリシーの実装
- **完了**: ThompsonSamplingPolicy構造体 (crates/strategy/src/thompson_sampling.rs)
  - ThompsonSamplingConfig: non_model_uncertainty_k, latency_penalty_k, min_trade_frequency, trade_frequency_window, hold_degeneration_inflation, max_lot_size, min_lot_size, consistency_threshold, default_lot_size
  - ThompsonDecision: action, q_point, q_sampled, posterior_std, all_sampled_q, all_point_q, hold_degeneration_detected, consistency_fallback
  - TradeFrequencyTracker: スライディングウィンドウベースの取引頻度監視
- **完了**: decide()パイプライン実装
  - 事後分布からの重みサンプリング: QFunction::sample_q_value 経由で w̃ ~ N(ŵ, Σ̂)
  - Q̃_final計算: w̃_a^T·φ(s) - self_impact - dynamic_cost - k·σ_non_model - latency_penalty（Buy/Sellのみ、Holdはペナルティなし）
  - グローバルポジション制約フィルタリング: global_position ± 1.0 ≤ global_position_limit でBuy/Sell制限
  - 行動間整合性チェック: Buy/Sell両方が正かつ相対差 < consistency_threshold → Holdフォールバック
  - Q_point（点推定）: QFunction::q_values 経由で監視用純粋 ŵ^T·φ を取得
  - Hold退化防止: TradeFrequencyTrackerで取引頻度監視 → 閾値下回り時にQFunction::inflate_covariance で事後分散膨張
  - ロットサイズ計算: default_lot_size × lot_multiplier、min_lot_size未満でHold
- **完了**: lib.rsにpub mod thompson_sampling追加
- **ユニットテスト**: 32新規テスト全て通過
  - 作成, 決定, カウンタ増分, 楽観的バイアス探索, グローバル制約(Buy/Sell両方向ブロック), 両方向ブロック→Hold, Buyブロック時選択, argmax選択, 整合性チェック(両正接近/遠隔/片方負/両負), ロット乗数/最大クランプ/低乗数→Hold/ゼロ→Hold, Hold退化検出/十分取引時非検出/早期非チェック/共分散膨張, TradeFrequencyTracker, サンプルQ値変動, Point Q整合性, トラッカーリセット, Config/QFunctionアクセス, レイテンシペナルティ, 整合性フォールバック決定, Sellブロック時選択
- **検証**: cargo build, cargo test (210 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 11: 戦略A: Liquidity Shock Reversion の実装
- **完了**: `crates/strategy/src/strategy_a.rs` 作成
- **完了**: StrategyA構造体 — 独自のQFunction(39次元) + エピソード管理 + Thompson Sampling意思決定パイプライン
- **完了**: StrategyAConfig — トリガー閾値(spread_z=3, depth_drop=-0.2, vol_spike=3, regime_kl=1.0), MAX_HOLD_TIME=30s, decay_rate_a=0.001
- **完了**: トリガー条件実装: spread_z > θ ∧ depth_drop < θ ∧ vol_spike > θ ∧ regime_kl < θ
- **完了**: 戦略A固有特徴量(5次元追加):
  - spread_z×OBI (交互作用項)
  - self_impact_A: base self_impact × (1 + min(|depth_change|, 2.0)) (深度変化増幅)
  - p_revert_A: spread信号(0.4) + depth信号(0.35) + vol信号(0.25) の重み付き [0,1]
  - time_decay_A: exp(-0.001 × holding_time_ms) (数秒スケール遅い減衰)
  - depth_drop×realized_vol (連続交互作用項)
- **完了**: エピソード管理: Idle/Active状態、MAX_HOLD_TIME超過で強制クローズ、外部ポジションクローズの同期
- **完了**: decide()パイプライン: エピソードタイムアウト→ポジション同期→トリガーチェック→特徴量抽出→Thompson Sampling→ペナルティ→整合性→グローバル制約→ロット sizing→Hold退化監視
- **完了**: EpisodeState列挙型(Idle/Active)、StrategyADecision構造体
- **完了**: lib.rsにpub mod strategy_a追加
- **ユニットテスト**: 74新規テスト全て通過
  - トリガー(7): 全条件/各閾値不足/カスタム閾値
  - 特徴量抽出(13): 次元/ベース保存/spread_z×OBI/self_impact_A(3)/p_revert_A(3)/time_decay_A(3)/depth_drop×vol/全有限
  - エピソード管理(8): 初期/開始/終了/境界内/境界/超過/残り時間(3)/ゼロ
  - 決定パイプライン(11): idle skip/triggered/explore/timeout long/short/no position/外部同期/active bypass/entry starts/global buy/sell blocks/low lot
  - Q関数(4): 次元/楽観バイアス/update/extended features
  - 設定(2): デフォルト/定数
  - ロット(4): full/half/max clamp/zero
  - 整合性(4): both close/far/one negative/both negative
  - アクション選択(6): argmax buy/sell/hold/buy blocked/both blocked
  - Hold退化(3): detected/not detected/early
  - リセット(2): tracker/Q function
  - エピソードライフサイクル(2): full/remaining time
  - p_revert_a詳細(3): signal weights/depth scaled/no signal
- **検証**: cargo build, cargo test (284 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 12: 戦略B: Volatility Decay Momentum の実装
- **完了**: `crates/strategy/src/strategy_b.rs` 作成
- **完了**: StrategyB構造体 — 独自のQFunction(39次元) + エピソード管理 + Thompson Sampling意思決定パイプライン
- **完了**: StrategyBConfig — トリガー閾値(vol_spike=2, vol_decaying=0, obi_alignment=0.1, regime_kl=1.0), MAX_HOLD_TIME=300s(5分), decay_rate_b=0.0001
- **完了**: トリガー条件実装: vol_ratio > θ ∧ vol_decaying (decay_rate < 0) ∧ |OBI| > θ ∧ regime_kl < θ
- **完了**: 戦略B固有特徴量(5次元追加):
  - rv_spike×trend (realized_volatility × OBI)
  - OFI×intensity (delta_obi × trade_intensity)
  - p_continue_B: vol_decay_signal(0.4) + obi_signal(0.35) + intensity_signal(0.25) の重み付き [0,1]
  - time_decay_B: exp(-0.0001 × holding_time_ms) (数分スケール、Aの10倍遅い減衰)
  - vol_ratio×signed_volume (ボラティリティ・モメンタム交互作用)
- **完了**: エピソード管理: Idle/Active状態、MAX_HOLD_TIME超過で強制クローズ、外部ポジションクローズの同期
- **完了**: decide()パイプライン: エピソードタイムアウト→ポジション同期→トリガーチェック→特徴量抽出→Thompson Sampling→ペナルティ→整合性→グローバル制約→ロット sizing→Hold退化監視
- **完了**: EpisodeStateB列挙型(Idle/Active)、StrategyBDecision構造体
- **完了**: lib.rsにpub mod strategy_b追加
- **ユニットテスト**: 94新規テスト全て通過
  - トリガー(8): 全条件/各閾値不足/カスタム閾値/負OBI/境界
  - 特徴量抽出(12): 次元/ベース保存/rv_spike×trend/OFI×intensity/p_continue_b(4)/time_decay_b(4)/vol_ratio×signed_volume/全有限
  - エピソード管理(10): 初期/開始/終了/境界内/境界/超過/残り時間(4)/ゼロ
  - 決定パイプライン(13): idle skip/triggered/explore/timeout long/short/no position/外部同期/active bypass/entry starts/global buy/sell blocks/low lot
  - Q関数(4): 次元/楽観バイアス/update/extended features
  - 設定(2): デフォルト/定数
  - ロット(4): full/half/max clamp/zero
  - 整合性(4): both close/far/one negative/both negative
  - アクション選択(5): argmax buy/sell/hold/buy blocked/both blocked
  - Hold退化(3): detected/not detected/early
  - リセット(2): tracker/Q function
  - エピソードライフサイクル(2): full/remaining time
  - p_continue_b詳細(3): signal weights/intensity scaled/no decay no signal
- **検証**: cargo build, cargo test (278 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 13: 戦略C: Session Structural Bias の実装
- **完了**: `crates/strategy/src/strategy_c.rs` 作成
- **完了**: StrategyC構造体 — 独自のQFunction(39次元) + エピソード管理 + Thompson Sampling意思決定パイプライン
- **完了**: StrategyCConfig — トリガー閾値(session_active=0.5, obi_significance=0.05, max_session_open=3.6M ms, regime_kl=1.0), MAX_HOLD_TIME=600s(10分), decay_rate_c=0.00005
- **完了**: トリガー条件実装: session_active ∧ |OBI| > θ ∧ time_since_open < θ ∧ regime_kl < θ
- **完了**: 戦略C固有特徴量(5次元追加):
  - session×OBI (支配的セッションで加重されたOBI)
  - depth_change×queue_position (range_break×liquidity_resiliency)
  - p_trend_c: OBI信号(0.4) + session信号(0.35) + time_weight(0.25) の重み付き [0,1]（セッション開始直後が最強）
  - time_decay_C: exp(-0.00005 × holding_time_ms) (10分スケール、最も遅い減衰)
  - OBI×time_since_open (セッション成熟度で変調されたオーダーフロー強度、60分で頭打ち)
- **完了**: エピソード管理: Idle/Active状態、MAX_HOLD_TIME超過で強制クローズ、外部ポジションクローズの同期
- **完了**: decide()パイプライン: エピソードタイムアウト→ポジション同期→トリガーチェック→特徴量抽出→Thompson Sampling→ペナルティ→整合性→グローバル制約→ロット sizing→Hold退化監視
- **完了**: EpisodeStateC列挙型(Idle/Active)、StrategyCDecision構造体
- **完了**: lib.rsにpub mod strategy_c追加
- **ユニットテスト**: 97新規テスト全て通過
  - トリガー(10): 全条件/各閾値不足/no conditions/カスタム閾値/Tokyo/NY/負OBI/time境界
  - 特徴量抽出(14): 次元/ベース保存/session×OBI/range_break×liquidity/p_trend_c(3)/time_decay_c(3)/OBI×time(2)/全有限
  - エピソード管理(10): 初期/開始/終了/境界内/境界/超過/残り時間(4)/ゼロ
  - 決定パイプライン(12): idle skip/triggered/explore/timeout long/short/no position/外部同期/active bypass/entry starts/global buy/sell blocks/low lot
  - Q関数(4): 次元/楽観バイアス/update/extended features
  - 設定(2): デフォルト/定数
  - ロット(4): full/half/max clamp/zero
  - 整合性(4): both close/far/one negative/both negative
  - アクション選択(5): argmax buy/sell/hold/buy blocked/both blocked
  - Hold退化(3): detected/not detected/early
  - リセット(2): tracker/Q function
  - エピソードライフサイクル(2): full/remaining time
  - p_trend_c詳細(3): signal weights/session scaled/late session decay
- **検証**: cargo build, cargo test (435 passed), cargo clippy, cargo fmt --check 全て通過

### 2026-04-19 — Task 14: On-policy Monte Carlo評価の実装
- **完了**: `crates/strategy/src/mc_eval.rs` 作成
- **完了**: TerminalReason列挙型 (PositionClosed, MaxHoldTimeExceeded, DailyHardLimit, UnknownRegime)
- **完了**: EpisodeTransition構造体 (timestamp_ns, action, phi, reward)
- **完了**: EpisodeBuffer — エピソード内遷移記録、即時報酬計算
  - 報酬関数: r_t = ΔPnL_t - λ_risk·σ²_i,t - λ_dd·min(DD_t, DD_cap)
  - ΔPnL: 前回equityからの差分 (realized + unrealized)
  - σ²_i,t = p²·σ²_price: ポジション分散
  - DD_t = max(0, equity_peak - equity_t): ピークからのドローダウン
  - DD_cap: DD項の飽和上限（Q値のPnL成分による回復取引を阻害しない）
- **完了**: RewardConfig (lambda_risk=0.1, lambda_dd=0.5, dd_cap=100.0, gamma=0.99)
- **完了**: McEvaluator — エピソードライフサイクル管理 + MCリターン計算 + Q関数更新
  - start_episode: ポジション非ゼロ化でエピソード開始
  - record_transition: 各決定ステップで遷移記録 + 報酬計算
  - end_episode: 終端条件でエピソード終了、割引累積報酬 G_t 計算
  - end_episode_and_update: エピソード終了 + 即座にQ関数更新
  - update_from_result: EpisodeResultからQ関数更新 (静的メソッド)
  - compute_returns: G_t = Σ γ^k·r_{t+k} を逆順O(n)で計算
- **完了**: EpisodeResult (strategy_id, terminal_reason, num_transitions, total_reward, return_g0, duration_ns, returns, transitions)
- **完了**: Deadly Triad回避の確認 — On-policy(実行行動のみ記録) + Monte Carlo(全エピソードリターン、ブートストラップなし) + Bayesian正則化(QFunctionに委譲)
- **完了**: 部分約定の扱い — 全量クローズ時のみエピソード終了、部分クローズはエピソード内イベント
- **完了**: MAX_HOLD_TIME到達時 — TerminalReason::MaxHoldTimeExceededで強制クローズ、実現PnLを報酬に組み込み
- **完了**: lib.rsにpub mod mc_eval追加
- **ユニットテスト**: 40新規テスト全て通過
  - EpisodeBuffer(8): new/basic reward/components/dd cap/equity peak tracking/dd from peak/negative equity/initial nonzero
  - compute_returns(6): empty/single/two steps/decay/gamma_1/negative rewards
  - McEvaluator lifecycle(5): start/double_start_panics/end_without_start_panics/full lifecycle/multi strategy
  - MC returns(2): end_episode_returns_computed/duration
  - Q-function update(2): end_episode_and_update/update_from_result
  - Episode result(2): avg_reward/avg_reward_zero
  - Terminal reasons(1): all reasons
  - Partial fill(1): continues episode
  - Empty episode(1): zero transitions
  - Completed tracking(1): multi-strategy counting
  - On-policy(1): only records taken actions
  - MC vs bootstrap(1): full returns verification
  - Reward edge cases(3): zero position/no risk, negative equity, initial nonzero
  - Gamma effect(1): return concentration
  - Q convergence(2): positive/negative episodes
  - Full integration(1): risk + DD + update
  - MAX_HOLD_TIME(1): forced close
  - Config access(2): active episode/config values
- **検証**: cargo build, cargo test (475 passed), cargo clippy, cargo fmt --check 全て通過
