# MLパイプライン実行ガイド

## 概要

Rustバックテスト → 特徴量ダンプ → Python学習 → ONNXエクスポート → Rust推論、のパイプラインが実装済み。

## ステップ1: バックテスト実行 + 特徴量ダンプ

```bash
cargo run -p fx-cli -- backtest \
  --data USDJPY_Ticks_2024.04.20_2026.04.20.csv \
  --dump-features artifacts/features.csv \
  --output artifacts/backtest
```

- バックテスト結果は `artifacts/backtest/` に出力
- 特徴量は `artifacts/features.csv` に41列（3メタデータ + 38特徴量）で出力

## ステップ2: レジームモデル学習 + ONNXエクスポート

```bash
python -m research.models.train_regime \
  --features artifacts/features.csv \
  --output research/models/onnx/
```

オプション:
| 引数 | デフォルト | 説明 |
|---|---|---|
| `--n-regimes` | 4 | レジーム数 |
| `--validation-fraction` | 0.2 | 検証データ割合 |
| `--learning-rate` | 0.01 | 学習率 |
| `--seed` | 42 | 乱数シード |

出力:
- `research/models/onnx/regime_v1.onnx` — ONNXモデル（入力: `[1,38]`、出力: `[1,4]`）
- `research/models/onnx/regime_v1_meta.json` — メタデータ（標準化パラメータ含む）

## ステップ3: ONNXモデルを使ったバックテスト

**前提**: ONNX Runtime共有ライブラリのパスを環境変数で指定する必要がある。

```bash
export ORT_DYLIB_PATH=".venv/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.1.24.4"
```

設定ファイル（`config/backtest-onnx.toml` は既に作成済み）:

```toml
[regime]
feature_dim = 38
n_regimes = 4
model_path = "research/models/onnx/regime_v1.onnx"
```

実行:

```bash
ORT_DYLIB_PATH=".venv/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.1.24.4" \
cargo run -p fx-cli -- backtest \
  --data USDJPY_Ticks_2024.04.20_2026.04.20.csv \
  --config config/backtest-onnx.toml \
  --output artifacts/backtest-onnx
```

`model_path` を省略するとヒューリスティック推定にフォールバック（ORT_DYLIB_PATHも不要）。

## テスト確認

```bash
# Rust全テスト
cargo test

# Python全テスト
pytest research/tests/

# 個別テスト
pytest research/tests/test_feature_pipeline.py   # 特徴量ローダー
pytest research/tests/test_e2e_pipeline.py       # 学習→ONNX E2E
```

## 合成データでの動作確認（実データなしの場合）

```bash
python -m research.models.generate_regime_model \
  --output research/models/onnx/regime_v1.onnx
```

500個の合成サンプルで学習 → ONNX生成。パイプラインの動作確認用。

## データフロー図

```
tick CSV
  │
  ▼
[Rust Backtest] ──dump-features──► features.csv (41列)
  │                                    │
  │                                    ▼
  │                           [Python train_regime]
  │                                    │
  │                                    ▼
  │                           regime_v1.onnx + meta.json
  │                                    │
  ◄─── model_path ─────────────────────┘
  │
  ▼
[Rust Backtest with ONNX] ──► backtest_result.json + trades.csv
```
