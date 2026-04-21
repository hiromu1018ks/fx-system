# FX System - バックテスト & フォワードテストガイド

FX市場向けの超短期（秒〜分）自動取引システム。MDP（マルコフ決定過程）+ ベイズ線形回帰 + トンプソンサンプリングによる強化学習アプローチを採用。Rustコア実行基盤（8クレート）+ Python研究/MLパイプラインで構成される。

設計ドキュメント: `docs/spec/design.md`

---

## 目次

- [ビルド & テスト](#ビルド--テスト)
- [実行前ランブック](#実行前ランブック)
  - [1. 環境準備](#1-環境準備)
  - [2. バックテスト実行](#2-バックテスト実行)
  - [3. フォワードテスト実行（recorded）](#3-フォワードテスト実行recorded)
  - [4. Validation 実行](#4-validation-実行)
  - [5. 外部プロバイダ / ONNX / 保存先のユーザー作業](#5-外部プロバイダ--onnx--保存先のユーザー作業)
- [アーキテクチャ概要](#アーキテクチャ概要)
- [バックテスト](#バックテスト)
  - [概要と狙い](#概要と狙い)
  - [実行方法](#実行方法)
  - [データ準備](#データ準備)
  - [BacktestConfig パラメータ詳細](#backtestconfig-パラメータ詳細)
  - [シミュレーションループの仕組み](#シミュレーションループの仕組み)
  - [OTC執行モデル（バックテスト内）](#otc執行モデルバックテスト内)
  - [リスク管理パイプライン](#リスク管理パイプライン)
  - [統計・メトリクス](#統計メトリクス)
- [フォワードテスト](#フォワードテスト)
  - [概要と狙い](#概要と狙い-1)
  - [実行方法](#実行方法-1)
  - [ForwardTestConfig パラメータ詳細](#forwardtestconfig-パラメータ詳細)
  - [ペーパーエグゼキューションエンジン](#ペーパーエグゼキューションエンジン)
  - [パフォーマンストラッカー](#パフォーマンストラッカー)
  - [アラートシステム](#アラートシステム)
  - [レポート生成](#レポート生成)
  - [バックテスト比較エンジン](#バックテスト比較エンジン)
- [3つの戦略アルゴリズム](#3つの戦略アルゴリズム)
  - [戦略A: 流動性ショックリバージョン](#戦略a-流動性ショックリバージョン)
  - [戦略B: モメンタム継続](#戦略b-モメンタム継続)
  - [戦略C: レンジ平均回帰](#戦略c-レンジ平均回帰)
- [ベイズ線形回帰 Q関数](#ベイズ線形回帰-q関数)
  - [特徴量パイプライン](#特徴量パイプライン)
  - [トンプソンサンプリング](#トンプソンサンプリング)
  - [モンテカルロ評価](#モンテカルロ評価)
  - [ホールド支配の防止](#ホールド支配の防止)
- [リスク管理の詳細](#リスク管理の詳細)
- [OTC執行モデルの詳細](#otc執行モデルの詳細)
- [過学習防止の統計的検証パイプライン](#過学習防止の統計的検証パイプライン)

---

## ビルド & テスト

### Rust

```bash
cargo build                    # 全ワークスペースクレートをビルド
cargo test                     # 全テスト実行
cargo test -p fx-backtest      # バックテストクレートのテストのみ
cargo test -p fx-forward       # フォワードテストクレートのテストのみ
cargo test --test integration  # 統合テスト実行
cargo clippy                   # リント
cargo fmt --check              # フォーマットチェック
```

### Python (research/)

```bash
pytest research/tests/                              # 全Pythonテスト
pytest research/tests/test_validation_pipeline.py    # 特定テストファイル
ruff check research/                                 # リント
uv pip install -e .                                  # インストール
```

---

## 実行前ランブック

以下のコマンド例は **リポジトリルート** から実行する前提。現在のユーザー向け実行経路は `fx-cli` です。状態としては次のとおりです。

| 項目 | 現状 |
|------|------|
| Backtest | 実行可 |
| Forward test (`recorded`) | 実行可 |
| Validation | Python依存導入後に実行可 |
| Forward test (`external`) | 接続スケルトンのみ。実データ受信は未配線 |

### 1. 環境準備

1. **Rust**: 通常どおり `cargo build` / `cargo test` が通る環境を用意する。  
2. **Python**: `pyproject.toml` は **Python 3.12+** を要求する。  
3. **research 依存導入**: `pandas` を含むため、Validation や `pytest research/tests/` の前に必ず導入する。  

```bash
uv pip install -e '.[dev]'
# もしくは
python -m pip install -e '.[dev]'
```

4. **確認**:

```bash
cargo test
pytest research/tests/ -q
```

`pandas` が未導入だと Python 側テストと Validation は失敗する。

### 2. バックテスト実行

最短経路は CSV を直接渡す方法。

```bash
cargo run -p fx-cli -- \
  backtest \
  --data data/usd_jpy_ticks.csv \
  --output artifacts/backtest
```

よく使うオプション:

- `--config <toml>`: `crates/cli/src/config.rs` が読む BacktestConfig 上書き
- `--strategies A,B` : 有効戦略を限定
- `--start-time <ns|RFC3339>` / `--end-time <ns|RFC3339>`: バックテスト対象期間を CLI から上書き
- `--dump-features <csv>`: regime v1 学習用の 38 次元特徴量を CSV にストリーミング出力
- `--import-q-state <path>`: 事前学習済みの Q 関数 posterior を読み込んで初期化
- `--export-q-state <path>`: バックテスト終了時点の Q 関数 posterior を保存（`.json` / `.bin` で自動判定）
- 出力物: `artifacts/backtest/backtest_result.json`, `artifacts/backtest/trades.csv`

Validation や将来の比較に使うので、`backtest_result.json` は run ごとに保存場所を分けて保管すること。

事前学習 → out-of-sample 検証の最短手順:

```bash
# 期間Aで学習して posterior を保存
cargo run -p fx-cli -- \
  backtest \
  --data data/usd_jpy_ticks.csv \
  --start-time 2024-01-01T00:00:00Z \
  --end-time 2024-03-31T23:59:59Z \
  --export-q-state artifacts/q-state/train-period.json \
  --output artifacts/backtest-train

# 期間Bで posterior を読み込んで検証
cargo run -p fx-cli -- \
  backtest \
  --data data/usd_jpy_ticks.csv \
  --start-time 2024-04-01T00:00:00Z \
  --end-time 2024-04-30T23:59:59Z \
  --import-q-state artifacts/q-state/train-period.json \
  --output artifacts/backtest-oos
```

regime v1 用の特徴量ダンプも同じ backtest 経路で取得できる。

```bash
cargo run -p fx-cli -- \
  backtest \
  --data data/usd_jpy_ticks.csv \
  --output artifacts/backtest \
  --dump-features artifacts/regime/features.csv
```

ダンプ CSV は `timestamp_ns, source_strategy, feature_version` に続いて、`feature_vector_v1_38` の 38 列を固定順で出力する。

### 2.5. regime v1 ONNX の再学習と反映

実データで regime v1 を更新する最短手順は次の 3 ステップ。

1. backtest で特徴量をダンプする
2. Python で ONNX と metadata JSON を再生成する
3. `[regime] model_path` を指定した backtest/forward config で読み込む

```bash
python -m research.models.train_regime \
  --features artifacts/regime/features.csv \
  --output research/models/onnx
```

生成物:

- `research/models/onnx/regime_v1.onnx`
- `research/models/onnx/regime_v1_meta.json`

`regime_v1_meta.json` には `feature_version`, `feature_dim`, `n_regimes`, `feature_columns`, `standardization_means`, `standardization_scales`, `test_features`, `expected_posterior` が保存される。`research.models.generate_regime_model` は synthetic fixture 用の薄いラッパーとして残してあり、実データ学習の主経路は `train_regime.py`。

backtest 側で ONNX を使う設定例:

```toml
[regime]
feature_dim = 38
model_path = "research/models/onnx/regime_v1.onnx"
```

```bash
export ORT_DYLIB_PATH="$(
  python - <<'PY'
import pathlib
import onnxruntime
root = pathlib.Path(onnxruntime.__file__).resolve().parent
matches = list(root.rglob("libonnxruntime.so*"))
if not matches:
    raise SystemExit("libonnxruntime.so not found")
print(matches[0])
PY
)"

cargo run -p fx-cli -- \
  backtest \
  --data data/usd_jpy_ticks.csv \
  --config config/backtest-regime.toml \
  --output artifacts/backtest-regime
```

### 3. フォワードテスト実行（recorded）

現在エンドツーエンドで使えるフォワード経路は `recorded` のみ。`--data-path` には **CSV** または **Tier1 Event Store パス** を渡せる。

最短コマンド:

```bash
cargo run -p fx-cli -- \
  forward-test \
  --source recorded \
  --data-path data/usd_jpy_ticks.csv \
  --output artifacts/forward \
  --seed 42
```

開始/終了時刻フィルタやリスク閾値を固定したい場合は TOML を使う。`start_time` / `end_time` は **ナノ秒整数** または **RFC3339** 文字列を使える。

```toml
enabled_strategies = ["A", "B", "C"]
duration = 1800

[data_source]
Recorded = { event_store_path = "data/usd_jpy_ticks.csv", speed = 0.0, start_time = "2024-01-01T00:00:00Z", end_time = "2024-01-01T01:00:00Z" }

[alert_config]
channels = ["Log"]
risk_limit_threshold = 0.8
execution_drift_threshold = 2.0
sharpe_degradation_threshold = 0.3

[report_config]
output_dir = "artifacts/forward"
format = "Json"
interval = 60

[risk_config]
max_position_lots = 10.0
max_daily_loss_mtm = 500.0
max_daily_loss_realized = 1000.0
max_weekly_loss = 2500.0
max_monthly_loss = 5000.0
daily_mtm_lot_fraction = 0.25
daily_mtm_q_threshold = 0.01
max_drawdown = 1000.0

[regime_config]
n_regimes = 4
unknown_regime_entropy_threshold = 1.8
regime_ar_coeff = 0.9
feature_dim = 38
# model_path = "research/models/regime.onnx"
```

```bash
cargo run -p fx-cli -- \
  forward-test \
  --config config/forward-recorded.toml \
  --output artifacts/forward \
  --seed 42
```

補足:

- `--source` / `--data-path` を付けると `data_source` 設定を CLI 側で上書きする。`start_time` / `end_time` を TOML で使う場合は付けない
- `--output` を付けた場合、CLI は `report_config.output_dir` より `--output` を優先する
- 現在 CLI が確実に出力する forward 成果物は `forward_result.json`
- `speed=0.0` は最速、`1.0` は実時間、`2.0` は2倍速

### 4. Validation 実行

Validation は Rust の `backtest_result.json` を Python ブリッジに渡して実行する。

```bash
cargo run -p fx-cli -- \
  validate \
  --backtest-result artifacts/backtest/backtest_result.json \
  --python-path .venv/bin/python \
  --output artifacts/validation
```

補足:

- 出力物は `artifacts/validation/validation_result.json`
- `--num-features` を付けると JSON 内の `num_features` を上書きできる
- `--python-path` は `uv`/venv で依存を入れた Python を指すこと

### 5. 外部プロバイダ / ONNX / 保存先のユーザー作業

#### 外部API接続（ユーザー作業）

`forward-test --source external` は **設定検証と接続スケルトンのみ** で、現時点では `next_tick()` がデータを返さないため、実運用用の外部 feed としては未完成。使う前に、ユーザー側で以下を確定させる必要がある。

- provider 名（`--provider` / `data_source.ExternalApi.provider`）
- 認証方式
- streaming endpoint / メッセージ形式
- シンボル正式表記（例: `USD/JPY` か provider 固有表記か）
- rate limit / reconnect 制約

資格情報ファイルは現状 **`key=value` 形式で `api_key=...` を含むこと** が前提。

```text
api_key=YOUR_PROVIDER_API_KEY
```

CLI 引数:

```bash
cargo run -p fx-cli -- \
  forward-test \
  --source external \
  --provider OANDA \
  --credentials secrets/provider.env
```

ただし上記だけでは live/streaming 実行にはならない。実データ接続には provider 固有実装が別途必要。

#### ONNX Runtime（ユーザー作業）

ONNX を使うのは `regime_config.model_path` を設定したときだけ。ワークスペースは `ort` の `load-dynamic` 機能を使っているため、実行時に ONNX Runtime の共有ライブラリが必要。

1. Python 依存（`onnxruntime` を含む）を導入する  
2. `libonnxruntime.so` の実体を見つける  
3. `ORT_DYLIB_PATH` を export する  

例:

```bash
export ORT_DYLIB_PATH="$(
  python - <<'PY'
import pathlib
import onnxruntime
root = pathlib.Path(onnxruntime.__file__).resolve().parent
matches = list(root.rglob("libonnxruntime.so*"))
if not matches:
    raise SystemExit("libonnxruntime.so not found")
print(matches[0])
PY
)"
```

`model_path` が未設定、または ONNX モデル/共有ライブラリを読めない場合、現在の runtime はヒューリスティック regime 推定にフォールバックする。

backtest 用設定は `[regime]`、forward 用設定は `[regime_config]` を使う。どちらも `model_path` に同じ `research/models/onnx/regime_v1.onnx` を指定できる。

#### Webhook / レポート保存先（ユーザー作業）

- **レポート保存先**: まずは `--output` または `report_config.output_dir` に、永続化されるディレクトリを明示する。CLI の実出力は `backtest_result.json` / `trades.csv` / `forward_result.json` / `validation_result.json`
- **バックテスト成果物の保管**: Validation 入力に再利用するため、`backtest_result.json` を run ごとに退避しておく
- **Webhook URL**: `alert_config.channels` には `Webhook { url = ... }` を書けるが、現在の `fx-cli forward-test` 実行経路では webhook 送信は未配線。実際の URL はユーザー管理値として別途用意し、現状は `channels = ["Log"]` の運用を推奨

---

## アーキテクチャ概要

### クレート依存関係

```
core → events → strategy, execution, risk, gateway
                 ↘ backtest (全クレートを使用)
                   ↘ forward (全クレートを使用)
```

| クレート | 役割 |
|---------|------|
| `fx-core` | 共通型、エラー型、監視/異常検知 |
| `fx-events` | イベントソーシング: バス、ストア、プロジェクタ、ギャップ検知、protobuf生成 |
| `fx-strategy` | 特徴量抽出、ベイズLR、トンプソンサンプリング、3戦略、MC評価、レジーム分類 |
| `fx-execution` | OTC執行モデル: Last-Look、約定確率、スリッページ、オーダータイプ選択 |
| `fx-risk` | 階層型リスク制限、動的バリア、キルスイッチ、ライフサイクル、グローバルポジション |
| `fx-gateway` | FIXプロトコルハンドラ、マーケットデータゲートウェイ |
| `fx-backtest` | バックテストエンジン + 統計 |
| `fx-forward` | フォワードテスト（ペーパートレード）: ランナー、ペーパーエンジン、設定、トラッカー、アラート、レポート |

### イベントソーシング

全モジュールは `PartitionedEventBus` を通じて通信（直接の状態共有なし）。4つのパーティションストリーム: Market, Strategy, Execution, State。`StateProjector` が全ストリームを消費し、単一の `StateSnapshot` を維持。

---

## バックテスト

### 概要と狙い

バックテストは、過去のマーケットデータ（ティックデータ）に対して3つの戦略を同時実行し、OTC執行モデル（Last-Look拒否、スリッページ、LP切替）を含む完全なシミュレーションを行う。狙いは以下の通り:

- **戦略の実現可能性検証**: 各戦略（A/B/C）が過去データで期待通りのエッジ（+0.05〜+0.2 pip/トレード）を生み出せるか
- **リスク制限の妥当性確認**: 階層型リスク制限が破滅的損失を防げるか
- **OTC執行モデルの影響評価**: Last-Look拒否やスリッページが戦略パフォーマンスに与える影響を定量化
- **パラメータ感度分析**: リスクパラメータ変更時のSharpe、最大ドローダウン、トレード頻度の変化を観察

バックテストクレート（`fx-backtest`）は**ライブラリのみ**であり、バイナリエントリポイントを持たない。他のクレートや統合テストからプログラム的に呼び出される。

### 実行方法

#### 1. CSVデータからの実行

```rust
use fx_backtest::{data, engine, stats};

// CSV読み込み（複数タイムスタンプフォーマット対応）
let ticks = data::load_csv("data/usd_jpy_ticks.csv")?;

// GenericEventに変換
let events = data::ticks_to_events(&ticks);

// バックテスト設定
let config = engine::BacktestConfig {
    symbol: "USD/JPY".to_string(),
    start_time_ns: 1_700_000_000_000_000_000,  // 開始時刻 (ナノ秒)
    end_time_ns: 1_700_100_000_000_000_000,    // 終了時刻 (ナノ秒)
    rng_seed: Some([42u8; 32]),                 // 乱数シード（決定性再現）
    enabled_strategies: HashSet::from([StrategyId::A, StrategyId::B, StrategyId::C]),
    ..Default::default()
};

// エンジン生成・実行
let mut engine = engine::BacktestEngine::new(config)?;
let result = engine.run_from_events(&events)?;

// 結果出力
let summary = &result.summary;
println!("総PnL: {:.2}", summary.total_pnl);
println!("トレード数: {}", summary.total_trades);
println!("勝率: {:.2}%", summary.win_rate * 100.0);
println!("Sharpe: {:.4}", summary.sharpe_ratio);
println!("最大DD: {:.2}", summary.max_drawdown);
```

#### 2. EventStoreからの実行

```rust
let result = engine.run(&event_store)?;
```

#### 3. 合成データによるテスト

```rust
// 100ティックの合成データ生成（決定的）
let events = engine::generate_synthetic_ticks(
    1_700_000_000_000_000_000,  // 開始時刻
    100,                          // ティック数
    100,                          // ティック間隔 (ms)
    150.0,                        // 基準価格
    0.001,                        // ボラティリティ
);
```

#### 4. テスト実行

```bash
# バックテストクレートの全テスト
cargo test -p fx-backtest

# 統合テスト（フルパイプライン）
cargo test --test integration
```

### データ準備

#### 対応CSVフォーマット

CSVヘッダーは自動的に正規化される。以下のカラム名を認識:

| 正規化後 | 認識するカラム名 |
|---------|---------------|
| `timestamp` | `Local_Time`, `time`, `Timestamp`, `timestamp` |
| `bid` | `bid`, `Bid` |
| `ask` | `ask`, `Ask` |
| `bid_volume` | `bidvolume`, `BidVol`, `bid_volume`, `BidVolume` |
| `ask_volume` | `askvolume`, `AskVol`, `ask_volume`, `AskVolume` |

#### 対応タイムスタンプフォーマット

- ナノ秒整数: `1700000000000000000`
- 秒浮動小数点: `1700000000.123`
- ISO 8601: `2023-11-15T00:00:00.123Z`
- スペース区切り: `2023-11-15 00:00:00.123`
- Dukascopy EET: `2023.11.15 02:00:00.123`（UTC+2として解釈）
- GMTオフセット付き: `15.11.2023 00:00:00.123 GMT+0200`

#### バリデーション

- `bid >= ask`（クロス市場）の行はエラーとして fail-fast
- タイムスタンプが単調増加でない行はエラーとして fail-fast
- デフォルト通貨ペア: `USD/JPY`

### BacktestConfig パラメータ詳細

| パラメータ | 型 | デフォルト | 説明 |
|-----------|-----|-----------|------|
| `start_time_ns` | `u64` | `0` | シミュレーション開始時刻（ナノ秒、包含） |
| `end_time_ns` | `u64` | `u64::MAX` | シミュレーション終了時刻（ナノ秒、包含） |
| `replay_speed` | `f64` | `0.0` | リプレイ速度倍率。0=最大速度、1.0=リアルタイム、10.0=10倍速 |
| `symbol` | `String` | `"USD/JPY"` | バックテスト対象の通貨ペア |
| `global_position_limit` | `f64` | `10.0` | グローバルポジション制限（ロット単位） |
| `default_lot_size` | `u64` | `100_000` | デフォルトロットサイズ |
| `rng_seed` | `Option<[u8; 32]>` | `None` | 乱数シード。指定すると同一データで完全に決定的な再現が可能 |
| `enabled_strategies` | `HashSet<StrategyId>` | 全戦略(A,B,C) | 有効にする戦略のセット |

#### FeatureExtractorConfig（特徴量抽出）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `spread_window` | 200 | スプレッドの移動平均ウィンドウ |
| `obi_window` | 200 | オーダーブックインバランス(OBI)のウィンドウ |
| `vol_window` | 100 | ボラティリティ計算の短期ウィンドウ |
| `vol_long_window` | 500 | ボラティリティ計算の長期ウィンドウ |
| `trade_intensity_window_ns` | 60s | トレード強度計算の時間窓 |
| `execution_lag_ns` | 0 | 実行遅延（情報リーク防止用ラグ） |
| `default_decay_rate` | 0.001 | デフォルト時価減衰率 |
| `typical_lot_size` | 100_000 | 典型的なロットサイズ |
| `max_hold_time_ms` | 30_000 | 最大ホールド時間（ミリ秒） |
| `session_hours_utc` | [0, 8, 13, 22] | セッション境界時間（UTC時間） |

#### StrategyAConfig（流動性ショックリバージョン）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `spread_z_threshold` | 2.0 | スプレッドzスコアの発動閾値。これを超えるとリバージョンシグナル |
| `depth_drop_threshold` | -0.3 | OBI急落の閾値。これを下回ると流動性ショックと判定 |
| `vol_spike_threshold` | 2.0 | ボラティリティスパイクの閾値 |
| `regime_kl_threshold` | 1.5 | レジームKLダイバージェンスの閾値 |
| `max_hold_time_ms` | 30_000 | 戦略Aの最大ホールド時間: 30秒（秒スケールの戦略） |
| `decay_rate_a` | 0.001 | 時価減衰率λ_A（秒スケール） |
| `lambda_reg` | 0.1 | L2正則化パラメータ |
| `halflife` | 100 | 適応ノイズ分散の半減期 |
| `initial_sigma2` | 0.01 | 事後分散の初期値 |
| `optimistic_bias` | 0.01 | 楽観的初期化バイアス（ホールド支配防止用） |
| `non_model_uncertainty_k` | 1.0 | 非モデル不確実性σ_non_modelの係数 |
| `latency_penalty_k` | 0.01 | レイテンシペナルティの係数 |

#### RiskLimitsConfig（階層型リスク制限）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `max_daily_loss_mtm` | -500.0 | 日次MTM損失警告（-500達成でロット25%に削減） |
| `max_daily_loss_realized` | -1000.0 | 日次実現損失ハードストップ（全ポジション強制決済+取引停止） |
| `max_weekly_loss` | -2500.0 | 週次ハードストップ（全決済+翌月曜まで停止） |
| `max_monthly_loss` | -5000.0 | 月次ハードストップ（全決済+翌月まで停止+運用者レビュー必須） |
| `daily_mtm_lot_fraction` | 0.25 | MTM警告時のロット削減率（25%） |
| `daily_mtm_q_threshold` | 0.01 | MTM警告時の新規エントリーQ値閾値 |

#### DynamicRiskBarrierConfig（動的リスクバリア）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `staleness_threshold_ms` | 5_000 | 古さ閾値。これを超えるとトレード停止 |
| `warning_threshold_ratio` | 0.4 | 警告段階の比率（閾値の40%で警告） |
| `min_lot_multiplier` | 0.01 | 最小ロット乗数（0.01 = 1%） |
| `default_lot_size` | 100_000 | デフォルトロットサイズ |
| `max_lot_size` | 1_000_000 | 最大ロットサイズ |
| `min_lot_size` | 1_000 | 最小ロットサイズ |

動的リスクバリアは、最新のマーケットデータからの経過時間（古さ）に応じてロットサイズを動的に削減する:

```
lot_multiplier = max(0, 1 - (staleness_ms / staleness_threshold_ms)²)
```

二次関数により、小さな遅延は小さなペナルティ、大きな遅延は急速なゼロ収束となる。

#### KillSwitchConfig（キルスイッチ）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `min_samples` | 100 | 異常検知の最小サンプル数 |
| `z_score_threshold` | 3.0 | ティック間隔のzスコア閾値。これを超えると異常と判定 |
| `max_history` | 2_000 | 保持する履歴の最大数 |
| `mask_duration_ms` | 50 | 異常検出後のマスク期間（ms）。この間はオーダー送信をブロック |
| `enabled` | true | バックテストデフォルトではfalseに上書き |

#### LifecycleConfig（ライフサイクルマネージャー）

| パラメータ | 目的 |
|-----------|------|
| `rolling_window` | ローリングSharpe計算のウィンドウサイズ |
| `min_episodes_for_eval` | 評価を開始する最小エピソード数 |
| `death_sharpe_threshold` | 戦略を「退場」させるSharpe閾値 |
| `consecutive_death_windows` | 連続して閾値を下回るウィンドウ数 |
| `sharpe_annualization_factor` | Sharpeの年率化係数 |
| `strict_unknown_regime` | 不明レジーム時の厳密モード |
| `auto_close_culled_positions` | 退場戦略のポジション自動決済フラグ |

#### GlobalPositionConfig（クロス戦略ポジション管理）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `strategy_max_positions` | {A: 5.0, B: 5.0, C: 5.0} | 戦略別最大ポジション（ロット） |
| `correlation_factor` | 1.5 | 戦略間相関の推定値 |
| `floor_correlation` | 1.0 | 相関ファクターの下限（ストレス時の安全マージン） |
| `lot_unit_size` | 100_000 | ロット単位サイズ |
| `min_lot_size` | 1_000 | 最小ロットサイズ |

グローバルポジション制約の計算式:

```
P_max^global = Σ P_max^i / max(correlation_factor, FLOOR_CORRELATION)
```

戦略間の相関が高い場合（全戦略が同方向）、`correlation_factor` が大きくなり、許容ポジションが削減される。

#### RegimeConfig（レジーム分類）

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `n_regimes` | 4 | レジーム数（calm, normal, turbulent, crisis） |
| `unknown_regime_entropy_threshold` | 1.8 | 不明レジームのエントロピー閾値 |
| `regime_ar_coeff` | 0.9 | レジームの自己回帰係数 |
| `feature_dim` | 38 | 特徴量次元数 |
| `model_path` | `None` | 指定時は ONNX regime 推定を使用。未指定/読込失敗時はヒューリスティックにフォールバック |

### シミュレーションループの仕組み

バックテストは**ティックバイティック（イベント駆動）**で動作。バー集計は行わない。各マーケットイベントを時系列順に以下のフェーズで処理する:

#### フェーズ0: ティック処理

```
マーケットイベント → KillSwitch（間隔異常検知）→ StateProjector（状態更新）→ FeatureExtractor（特徴量更新）
```

`StateProjector` がポジション、PnL、古さ、状態ハッシュを更新。`FeatureExtractor` が移動統計量を更新し、戦略ごとの `FeatureVector` を抽出。

#### フェーズ1: 最大ホールド時間強制執行

各戦略のオープンポジションについて、`max_hold_time_ms` を超過しているかチェック。超過していれば強制決済し、MCエピソードを `TerminalReason::MaxHoldTimeExceeded` で終了。

戦略別の最大ホールド時間:
- **戦略A**: 30秒（秒スケールのリバージョン）
- **戦略B**: 5分（モメンタム継続）
- **戦略C**: 10分（レンジ平均回帰）

#### フェーズ2: レジーム更新と戦略意思決定

1. `RegimeCache` を戦略Aの特徴量から更新（ヒューリスティックスコアリング: spread_zscore, realized_volatility, volatility_ratio）
2. レジームが "unknown" の場合、全戦略の評価を抑制
3. 各有効戦略に対してトンプソンサンプリングでQ値を計算し、意思決定を取得
4. 全戦略の意思決定をQ値降順でソート（設計書 §9.5）

#### フェーズ3: 戦略意思決定の実行

Q値優先度順に各戦略の意思決定を処理:

**事前チェック（リスクパイプライン5段階）**:
1. **KillSwitch**: 直近の間隔異常検出時にリジェクト
2. **LifecycleManager**: 退場済み戦略をリジェクト
3. **HierarchicalRiskLimiter**: 日次MTM警告（ロット25%削減）、日次実現損失ハードストップ、週次ハードストップ、月次ハードストップ
4. **DynamicRiskBarrier**: 古さに応じたロット削減（Normal → Warning → Degraded → Halted）
5. **GlobalPositionChecker**: マルチ戦略ネットポジション制限。低優先度戦略のロットをQ値ランクに応じて削減

**ロットサイズ決定**:
```
effective_lots = original_lots × MTM乗数 × バリア乗数
```

**執行**: `ExecutionGateway.simulate_execution()` に委譲（OTCモデル: LP選択、約定確率、スリッページ）

#### フェーズ4: MC遷移記録

アクティブなMCエピソードがある戦略について、状態遷移（Q-action, 特徴量φ, 状態スナップショット, 分分散プロキシ）を記録し、エピソード終了時にQ関数の重みを更新。

#### データ終了時処理

全ティック処理後、残存オープンポジションを最終mid価格で決済（`close_reason: "END_OF_DATA"`）。

### OTC執行モデル（バックテスト内）

バックテストは取引所のようなオーダーブックモデルではなく、**OTC（店頭）市場モデル**をシミュレートする:

1. **LP選択**: `LP_PRIMARY` または `LP_BACKUP` を約定/拒否統計に基づいて選択
2. **Last-Look拒否**: LPが15-200msの観察窓で価格変動を確認し、不利なら拒否。Beta-Binomial事後分布で `P(not_rejected)` を推定
3. **約定確率**: `P_effective = P(request) × P(not_rejected) + ε_hidden`（Student's t分布による隠れた流動性）
4. **スリッページ**: Student's t分布（df=3、太い裾）からサンプリング。サイズとボラティリティに比例してスケール
5. **LP切替**: 拒否率が閾値を超えるとバックアップLPに切替、リキャリブレーションモード（ロット25%、σ 2倍）に移行

### リスク管理パイプライン

リスク管理は **Q値評価の前にチェック** される。これは設計上の重要な原則で、戦略がどれほど高いQ値を出しても、リスク制限を超えていれば取引は実行されない。

```
KillSwitch → LifecycleManager → HierarchicalRiskLimiter → DynamicRiskBarrier → GlobalPositionChecker → 執行
```

### 統計・メトリクス

#### TradeSummary（取引サマリ）

| メトリクス | 説明 |
|-----------|------|
| `total_pnl` | 全トレードPnLの合計 |
| `total_trades` | 約定数 |
| `winning_trades` / `losing_trades` | 勝トレード数 / 負トレード数 |
| `win_rate` | 勝率 = winning_trades / total_trades |
| `avg_win` / `avg_loss` | 勝トレード平均PnL / 負トレード平均PnL |
| `profit_factor` | 総勝PnL / |総負PnL| |
| `max_drawdown` | ピークからトラフへの最大下落（通貨単位） |
| `max_drawdown_duration_ns` | 最大ドローダウンの継続時間 |
| `sharpe_ratio` | 年率化Sharpe比（年間~864,000トレード想定） |
| `sortino_ratio` | 下行偏差のみを使用したSharce比 |
| `avg_slippage` | 平均絶対スリッページ |
| `avg_fill_probability` | 平均実効約定確率 |
| `avg_latency_ms` | 平均実行レイテンシ |
| `max_consecutive_wins` / `max_consecutive_losses` | 最大連勝 / 最大連敗 |

#### ExecutionStats（LP実行統計）

| メトリクス | 説明 |
|-----------|------|
| `total_requests` / `total_fills` / `total_rejections` | LP別リクエスト/約定/拒否数 |
| `fill_rate_ema` | 約定率の指数移動平均 |
| `is_adversarial` | LPが敵対的と判定されたか |
| `active_lp_id` | 現在のアクティブLP |
| `overall_fill_rate` | 全体約定率 |
| `avg_slippage` | 平均スリッページ |
| `recalibration_triggered` | リキャリブレーションが発動したか |

#### その他の分析

- **`compute_equity_curve()`**: 時系列のエクイティカーブとドローダウン
- **`compute_strategy_breakdown()`**: 戦略別のトレード数、PnL、勝率、平均PnL

---

## フォワードテスト

### 概要と狙い

フォワードテストは、記録されたマーケットデータをリプレイしながら、**実際のオーダーパスに接続することなく**完全な戦略/リスク/執行パイプラインを通るペーパートレード（模擬取引）を行う。狙い:

- **バックテスト結果の検証**: バックテストで得られたパフォーマンスが、同じパイプラインを通して再現するか
- **OTC執行モデルの妥当性確認**: ペーパーエグゼキューション（`simulate_execution`）の結果がバックテストと一致するか
- **リスク管理の動作確認**: キルスイッチ、動的バリア、階層型制限が正しく動作するか
- **パフォーマンス監視のテスト**: トラッカー更新と CLI 結果出力が正しく行われるか
- **構造的安全性の保証**: **フォワードテストは実際のオーダーパスに決して接続されない**。これは構造的に保証されており、バグによっても実際の注文が発注されることはない

フォワードテストクレート（`fx-forward`）はライブラリであり、ユーザー向け実行エントリポイントは `fx-cli forward-test`。

### 実行方法

#### 1. 記録データからのリプレイ

```rust
use fx_forward::{config::ForwardTestConfig, runner::ForwardTestRunner, feed::RecordedDataFeed};

// TOML設定ファイルから読み込み
let config = ForwardTestConfig::load_from_file("config/forward_test.toml")?;

// またはプログラム的に構築
let config = ForwardTestConfig {
    enabled_strategies: HashSet::from(["A".to_string(), "B".to_string()]),
    data_source: DataSourceConfig::Recorded {
        event_store_path: "data/event_store".to_string(),
        speed: 0.0,       // 0 = 最大速度
        start_time: None,  // 全期間
        end_time: None,
    },
    ..Default::default()
};

// VecEventStoreにデータをロード
let store = VecEventStore::new(events);

// フィードとランナーを構築
let feed = RecordedDataFeed::new(store, 0.0, None, None);
let mut runner = ForwardTestRunner::new(feed, config);

// 実行（async context / シード指定で決定的再現）
let result = runner.run(12345).await?;

println!("ティック数: {}", result.total_ticks);
println!("決定数: {}", result.total_decisions);
println!("トレード数: {}", result.total_trades);
println!("最終PnL: {:.2}", result.final_pnl);
println!("戦略: {:?}", result.strategies_used);
```

#### 2. リプレイ速度の制御

```
speed = 0.0  → 最大速度（遅延なし、テスト推奨）
speed = 1.0  → リアルタイム（元のティック間隔を再現）
speed = 2.0  → 2倍速
speed = 0.5  → 半分の速度
```

#### 3. テスト実行

```bash
cargo test -p fx-forward
cargo test --test integration -p fx-forward
```

### ForwardTestConfig パラメータ詳細

| パラメータ | 型 | デフォルト | 説明 |
|-----------|-----|-----------|------|
| `enabled_strategies` | `HashSet<String>` | {"A","B","C"} | 有効にする戦略。"A", "B", "C" のみ有効 |
| `data_source` | `DataSourceConfig` | Recorded, speed 1.0 | データソース設定 |
| `duration` | `Option<Duration>` | `None` | 最大実行時間。None = フィード枯渇まで実行 |
| `alert_config` | `AlertConfig` | （下記参照） | アラート設定 |
| `report_config` | `ReportConfig` | （下記参照） | レポート出力設定 |
| `risk_config` | `ForwardRiskConfig` | （下記参照） | リスク制限設定 |
| `comparison_config` | `Option<ComparisonConfig>` | `None` | バックテスト比較設定 |

#### DataSourceConfig

**Recorded（記録データリプレイ）**:
| パラメータ | 説明 |
|-----------|------|
| `event_store_path` | CLI では CSV または Tier1 Event Store のパスとして扱う |
| `speed` | リプレイ速度倍率（0=最大速度） |
| `start_time` | 開始時刻フィルタ（オプション） |
| `end_time` | 終了時刻フィルタ（オプション） |

**ExternalApi（外部API接続スケルトン）**:
| パラメータ | 説明 |
|-----------|------|
| `provider` | プロバイダ名（例: "OANDA"） |
| `credentials_path` | `api_key=...` を含む認証情報ファイルのパス |
| `symbols` | 購読する通貨ペアのリスト |

現在の実装は接続/購読の枠組みと認証ファイル読込までで、実ティック受信は未配線。

#### AlertConfig

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `channels` | `[Log]` | アラート出力先: `Log`（tracing出力）または `Webhook { url }`（HTTP POST） |
| `risk_limit_threshold` | 0.8 | リスクリミットアラートの閾値。現在損失/最大損失がこの比率を超えると発火（0, 1] |
| `execution_drift_threshold` | 2.0 | 実行ドリフト（期待vs実際の約定差）のzスコア閾値 |
| `sharpe_degradation_threshold` | 0.3 | Sharpe比劣化の閾値。ベースラインからの低下率がこの値を超えると発火 |

#### ReportConfig

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `output_dir` | `"./reports"` | CLI で `--output` 未指定時の出力先候補 |
| `format` | `Both` | ライブラリ側 ReportGenerator 用の形式設定 |
| `interval` | `None` | ライブラリ側の定期レポート間隔（CLI では未使用） |

#### ForwardRiskConfig

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `max_position_lots` | 10.0 | 最大ポジションサイズ（ロット） |
| `max_daily_loss_mtm` | 500.0 | 日次 MTM 警告閾値 |
| `max_daily_loss_realized` | 1000.0 | 日次実現損失ハードストップ |
| `max_weekly_loss` | 2500.0 | 週次実現損失ハードストップ |
| `max_monthly_loss` | 5000.0 | 月次実現損失ハードストップ |
| `daily_mtm_lot_fraction` | 0.25 | MTM 警告時のロット縮小率 |
| `daily_mtm_q_threshold` | 0.01 | MTM 警告時の最小 \|Q\| |
| `max_drawdown` | 1_000.0 | 最大ドローダウン |

#### TOML設定例

```toml
enabled_strategies = ["A", "B"]

[data_source]
Recorded = { event_store_path = "/data/store", speed = 2.0 }

[alert_config]
channels = ["Log"]
risk_limit_threshold = 0.8
execution_drift_threshold = 2.0
sharpe_degradation_threshold = 0.3

[report_config]
output_dir = "./reports"
format = "Json"
interval = 60

[risk_config]
max_position_lots = 5.0
max_daily_loss_mtm = 300.0
max_daily_loss_realized = 600.0
max_weekly_loss = 1500.0
max_monthly_loss = 3000.0
daily_mtm_lot_fraction = 0.25
daily_mtm_q_threshold = 0.01
max_drawdown = 800.0

[regime_config]
n_regimes = 4
unknown_regime_entropy_threshold = 1.8
regime_ar_coeff = 0.9
feature_dim = 38
```

`alert_config` / `comparison_config` / `report_config.format` は設定型として存在するが、現在の `fx-cli forward-test` 実行経路で常時使われるのは主に `report_config.output_dir`（または `--output`）である。

### ペーパーエグゼキューションエンジン

`PaperExecutionEngine` はフォワードテストのコアとなる安全性構造。`ExecutionGateway` をラップし、**`simulate_execution()` メソッドのみ**を使用する。

```rust
pub struct PaperExecutionEngine {
    gateway: ExecutionGateway,
    rng: SmallRng,  // 決定的乱数生成器（Xoshiro256++）
}
```

**実行フロー**:

1. 戦略が `ExecutionRequest` を生成
2. `PaperExecutionEngine.execute()` が `gateway.simulate_execution()` を呼び出し
3. OTC実行モデル（Last-Look、スリッページ、約定確率）をソフトウェア内で完全シミュレーション
4. **ネットワーク呼び出しは一切発生しない**
5. 結果を `PaperOrderResult` として返却

**決定性保証**: 同じシード + 同じリクエスト順序 = 完全に同一の約定結果、スリッページ、拒否判定。これはテストで明示的に検証されている。

### パフォーマンストラッカー

`PerformanceTracker` はローリングウィンドウでパフォーマンスデータを追跡:

| メトリクス | 説明 |
|-----------|------|
| `cumulative_pnl` | 累積PnL（realized + unrealized） |
| `realized_pnl` | 実現PnL |
| `unrealized_pnl` | 未実現PnL |
| `rolling_sharpe` | ローリングSharpe比（√252で年率化、デフォルトウィンドウ100） |
| `max_drawdown` | ピークからの最大下落 |
| `win_rate` | 勝率 |
| `total_trades` | 総トレード数 |
| `winning_trades` | 勝トレード数 |
| `execution_drift_mean` | 実行ドリフトのローリング平均 |
| `execution_drift_std` | 実行ドリフトのローリング標準偏差 |

### アラートシステム

#### アラートタイプ

| タイプ | 説明 | デフォルトSeverity |
|-------|------|-------------------|
| `RiskLimit` | 現在損失が最大損失に接近 | Warning（>= 1.0でCritical） |
| `ExecutionDrift` | 実行ドリフトのzスコアが閾値を超過 | Warning |
| `KillSwitch` | キルスイッチが発動（ティック異常検知） | Critical |
| `SharpeDegradation` | Sharpe比がベースラインから閾値以上劣化 | Warning |
| `StrategyCulled` | 戦略がライフサイクル管理により退場 | Warning |
| `FeedAnomaly` | データフィードの異常（ギャップなど） | Warning |

#### デバウンス機構

各アラートタイプにタイムスタンプベースのクールダウンがあり、同一アラートの連続発火を防止。

#### 出力チャネル

- **Log**: `tracing::error!`（Critical）または `tracing::warn!`（Warning/Info）
- **Webhook**: ファイア＆フォーゲットHTTP POST（tokioタスクで非ブロッキング）

### レポート生成

`ReportGenerator` は以下のファイルを出力:

**`session_report.json`** - セッション全体のJSONレポート:
- `ForwardTestResult`（総ティック数、決定数、トレード数、実行時間、最終PnL、使用戦略）
- `PerformanceSnapshot`（累積PnL、ローリングSharpe、最大DD、勝率、実行ドリフト）
- `ComparisonReport`（バックテスト比較、設定時のみ）

**`performance_summary.csv`** - 主要メトリクスのCSV:
```
total_ticks,1000
total_decisions,50
total_trades,30
duration_secs,10.5
final_pnl,150.0
realized_pnl,120.0
unrealized_pnl,30.0
rolling_sharpe,1.2
max_drawdown,50.0
win_rate,0.6
```

**`trades.csv`** - 全トレードの詳細:
```
trade_id,timestamp_ns,symbol,side,lots,fill_price,slippage,pnl,strategy
ORD-1-...,1700000000,USD/JPY,Buy,100000,150.123,0.001,5.0,A
```

### バックテスト比較エンジン

`ComparisonEngine` はフォワードテスト結果とバックテスト結果を比較し、システムの信頼性を評価する。

#### 比較閾値（デフォルト）

| メトリクス | 許容差 | 比較方法 |
|-----------|--------|---------|
| PnL | 20% | 相対誤差 |
| 勝率 | 10pp | 絶対誤差 |
| Sharpe | 0.3 | 絶対誤差 |
| ドローダウン | 20% | 相対誤差 |
| 約定率 | 10pp | 絶対誤差 |
| スリッページ | 0.5 pip | 絶対誤差 |

`overall_pass` は**全メトリクスが許容範囲内**の場合のみ `true`。

#### PnL差分の分解

PnL差分は以下の4つの要因に分解される:

| 要因 | 説明 |
|------|------|
| `execution_component` | スリッページによる差分 |
| `latency_component` | 推定レイテンシによる差分（PnL差の10%と仮定） |
| `impact_component` | 約定率の差分による影響 |
| `residual` | 説明できない残差 |

---

## 3つの戦略アルゴリズム

全戦略は同じ **ベイズ線形回帰 Q関数 + トンプソンサンプリング** のアーキテクチャを共有するが、異なる特徴量パイプライン `φ(s)` と時間スケールを持つ。

### 戦略A: 流動性ショックリバージョン

- **時間軸**: 秒（最大ホールド30秒）
- **狙い**: 流動性ショック（急激なスプレッド拡大、板深さ急落、ボラティリティスパイク）は一時的なもので、直後にリバージョンすると仮定
- **主要特徴量**: `spread_z`, `depth_drop`, `vol_spike`, `regime_kl`, `reversion_speed`, `queue_position`
- **発動条件**: `spread_z >= 2.0`（スプレッドが平均の2σ以上拡大）または `depth_drop <= -0.3`（板深さが急落）
- **時価減衰**: `λ_A = 0.001`（秒スケールの速い減衰）

### 戦略B: モメンタム継続

- **時間軸**: 分（最大ホールド5分）
- **狙い**: ボラティリティスパイク後の方向性継続をキャッチ
- **主要特徴量**: `rv_spike`, `trend`, `OFI`（Order Flow Imbalance）, `intensity`, `delta_t`, `queue_position`
- **時価減衰**: `λ_B`（分スケールの遅い減衰。λ_A << λ_B）

### 戦略C: レンジ平均回帰

- **時間軸**: 分〜時間（最大ホールド10分）
- **狙い**: セッションの構造的バイアス（アジア/ロンドン/NYセッションの境界での価格パターン）を利用したレンジ内リバージョン
- **主要特徴量**: `session`, `range_break`, `liquidity_resiliency`, `queue_position`
- **時価減衰**: `λ_C`（戦略固有の減衰率）

### 戦略分離の報酬設計

各戦略の報酬は**独立**に計算され、戦略間の結合はない:

```
r_t^i = PnL_t^i - λ_risk × σ²_{i,t} - λ_dd × min(DD_t^i, DD_cap)
```

- `σ²_{i,t} = p_i² × σ²_price`: 戦略固有のポジション分散（ポートフォリオ全体ではない）
- `DD_t^i = max(0, equity_peak_i - equity_{i,t})`: 戦略固有のドローダウン
- `DD_cap`: DD項のキャップ。キャップなしだと回復トレードが阻害されるため

---

## ベイズ線形回帰 Q関数

### 特徴量パイプライン

状態 `s_t = (X_t^market, p_t^position)` から特徴量ベクトル `φ(s)` を構成:

1. **線形項**: spread_z, OBI, delta_OBI, vol, session, position など
2. **非線形変換項**（既知関数、学習対象外）: self_impact, decay, dynamic_cost, P(revert) など
3. **交互作用項**（手動設計）: spread_z × vol, OBI × session, depth_drop × vol_spike など
4. **ポジション状態**: size, direction, holding_time, pnl_unrealized

Q値の計算（重みのみ学習）:

```
Q(s, a) = w_a^T × φ(s)
```

### トンプソンサンプリング

**ゼロコスト探索**: ε-greedyやBoltzmann Softmaxは、0.05〜0.2 pipの薄いエッジではランダムトレードの期待損失（-spread/2 ≈ -0.1 pip）が致命的になるため採用しない。

**手順**:
1. 事後分布から重みをサンプリング: `w̃ ~ N(ŵ, Σ̂)`
2. サンプリング重みでQ値を計算し、事後ペナルティを適用:
   ```
   Q̃_final(s, a) = w̃_a^T × φ(s) - self_impact - dynamic_cost - k × σ_non_model - latency_penalty(a)
   ```
3. 最適アクションを選択: `a* = argmax_{a ∈ A_valid} Q̃_final(s, a)`

**重要な設計原則**: `σ_model`（モデル不確実性）は**点推定には含めない**。トンプソンサンプリングのサンプリング分散としてのみ反映。これにより不確実性の三重カウントを防止。

### モンテカルロ評価

ブートストラップ（`y = r + γ × max Q`）ではなく、**完全エピソードの割引累積報酬**を使用:

```
G_t = Σ γ^k × r_{t+k}
```

**デッドリー・トリアドの回避**（Sutton & Barto, 2018）:
- オフポリシー → **オンポリシー**: 実行したアクションのみを使用
- ブートストラップ → **モンテカルロ**: フルトラジェクトトリターンを使用
- 関数近似: ベイズ正則化が重み発散を防止

3要素のうち2つが構造的に除去されており、発散条件は満たされない。

### ホールド支配の防止

ポリシーが「ホールドのみ」に収束するのを防ぐ3つのメカニズム:

1. **楽観的初期化**: `ŵ_buy`, `ŵ_sell` をホールド値より高く初期化。初期のトンプソンサンプリングが買/売を好む
2. **最小トレード頻度監視**: 最近Nティックでの買/売回数が `MIN_TRADE_FREQUENCY` 未満の場合、事後分散を膨張: `Σ_inflated = α_inflation × Σ`
3. **γ 減衰**: 割引因子が遠い将来の報酬を自然に減価させ、長期ホールドにペナルティ

---

## リスク管理の詳細

### 階層型ハードリミット（月→週→日）

リスク制限はQ値やポリシー判断に**依存せず独立して**発動する:

#### 月次ハードリミット
```
monthly_realized_pnl < -MAX_MONTHLY_LOSS (例: 資本金-5%)
→ 全ポジション決済 → 当月中の再開禁止 → 運用者事後レビュー必須
```

#### 週次ハードリミット
```
weekly_realized_pnl < -MAX_WEEKLY_LOSS (例: 資本金-3%)
→ 全ポジション決済 → 翌月曜まで停止 → 運用者承認が必要
```

#### 日次2段階リミット

**ステージ1 - MTM警告**:
```
mark_to_market_pnl < -MAX_DAILY_LOSS_MTM (例: 資本金-3%)
→ 全戦略のロット制限を25%に削減 → Q値閾値以上のみ新規エントリー許可
```

**ステージ2 - 実現PnLハードストップ**:
```
realized_pnl < -MAX_DAILY_LOSS_REALIZED (例: 資本金-2%)
→ 即座に全ポジションをマーケットオーダーで決済 → 当日の新規エントリー全面禁止 → 翌営業日まで停止
```

**設計根拠**: MTMはフラッシュスパイクで極端な値になり、不要な実現損失を引き起こす可能性がある。実現PnLは確定値。2段階設計によりMTMスパイク時のリスク低減と、実際損失でのみ完全停止を両立。

### キルスイッチ

ティック到着間隔の異常をzスコアで検知:

```
|interval - mean| / std > z_score_threshold (default: 3.0)
→ mask_duration_ms (default: 50ms) 間は全オーダー送信をブロック
```

### ライフサイクルマネージャー

各戦略のローリングSharpe比を監視し、一定期間（連続ウィンドウ数）にわたり「死の閾値」を下回り続けると:

- 新規エントリーをハードブロック
- 既存ポジションを自動決済（オープンポジションが残らないことを保証）

### 動的リスクバリア

マーケットデータの古さ（最新データからの経過時間）に応じてロットサイズを動的に削減。同期待機（ファストマーケットでのレイテンシ追加）の代わりに、常にコマンドを通すが `staleness_ms` を付与:

```
lot_multiplier = max(0, 1 - (staleness_ms / staleness_threshold_ms)²)
```

- 二次関数: 小遅延=小ペナルティ、大遅延=急速なゼロ収束
- `staleness_ms > threshold` → `lot_multiplier = 0` → トレード停止

### グローバルポジションチェッカー

複数戦略が同時にシグナルを出した場合の統合管理:

```
|Σ p_i| ≤ P_max^global
```

相関調整:
```
P_max^global = Σ P_max^i / max(correlation_factor, FLOOR_CORRELATION)
```

Q値最上位の戦略が優先権を持ち、下位戦略のロットが削減される。

---

## OTC執行モデルの詳細

FXは取引所ではなく**OTC（店頭）市場**。Last-Look拒否、内部化、プライス改善/悪化は構造的に不可避。

### Last-Look拒否モデル

Beta-Binomial共役事前分布によるLPごとの `P(not_rejected)` 推定:

```
P(not_rejected | lp, vol) = posterior_mean × (1 - vol_adjustment × vol)
```

- 事前パラメータ: `α=2.0, β=1.0`（事前約定率 = 2/3 ≈ 0.667）
- ボラティリティペナルティ: `vol_adjustment = 0.1`
- 各約定/拒否観測で共役ベイズ更新（α or βをインクリメント）

### 実効約定確率

```
P_effective = P(request) × P(not_rejected) + ε_hidden
```

- `P(request)`: Market = 0.98、Limit = `0.5 × exp(-|distance| × 10.0)`
- `ε_hidden`: Student's t（df=3）からサンプリング。期待値 = 0.02（隠れた流動性のフロア）

### スリッページモデル

**決定的期待値**:
```
E[slippage] = 0.0001 × lots × vol + 0.01 × √lots × vol + 0.001 × vol + LP_平均偏差
```

**確率的サンプリング**:
```
slippage = E[slippage] + StudentT(df=3) × (0.0001 × (1 + lots × vol))
```

Student's t（df=3）は正規分布より太い裾を持ち、現実的な極端スリッページイベントを再現。LPごとのWelfordオンライン平均/分散が分布全体をシフト。

### パッシブ/アグレッシブ決定

| 条件 | 決定 |
|------|------|
| `P_effective >= 0.7` AND `EV_limit >= EV_market` AND `E[profit] >= 0.0001` | **Limit Order** |
| 上記を満たさない | **Market Order** |
| 緊急（`time_urgent = true`） | 常に **Market Order** |

### LP切替プロトコル

1. **監視**: EMA（α=0.1）でLPごとの約定率を追跡
2. **敵対的判定**: `fill_rate_ema < 0.5` または `連続拒否 >= 5回`
3. **切替**: 円形リストで次の非敵対的LPに切替
4. **リキャリブレーション**: ロット25%、σ 2倍で安全モード。30サンプル蓄積後、スリッページ推定誤差 < 0.02% AND 約定率誤差 < 0.1 で完了。最大5分

### 実行レイテンシ

シミュレーションでは一様分布 `U[0.5, 2.5]` ms。

---

## 過学習防止の統計的検証パイプライン

設計書 §5に基づく、全戦略がライブ候補になる前に**全て合格**する必要がある検証:

| 検証 | 条件 | 目的 |
|------|------|------|
| **CPCV** | Combinatorially Purged Cross-Validation | 時系列データリーク防止 |
| **PBO** | Probability of Backtest Overfitting > 0.1 → 却下 | バックテスト過学習の確率を評価 |
| **DSR** | Deflated Sharpe Ratio >= 0.95 | 多重検定を考慮したSharpe検定 |
| **複雑性ペナルティ** | Sharpe / √(特徴量数) | 特徴量過多のペナルティ |
| **Sharpe天井** | 年率Sharpe > 1.5 → 強制却下 | FX準短期戦略での過剰適合検出 |
| **許容Sharpe** | 0.8 - 1.2 | リアルな目標範囲 |
| **ライブ劣化テスト** | サンプル外Sharpe劣化 < 30% | 汎化性能の確認 |
| **Q関数2段階検証** | CPCV/PBOでアウトオブサンプル評価 | Q値過学習の検出 |
| **情報リーク検証** | ラグあり/なしでBacktest比較 | 実行特徴量の未来情報混入確認 |
| **ポリシー堅牢性検証** | 事後分散幅の摂動で報酬安定性確認 | ポリシーの不安定性検出 |
| **報酬関数感度分析** | λ_risk, λ_dd, DD_capの変化でSharpe/DD/頻度観察 | 過適合パラメータの検出 |
