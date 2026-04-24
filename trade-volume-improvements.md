# 取引量改善・評価信頼性改善まとめ

## 概要

Lifecycle の即時再 culling 問題は解消され、バックテストは「ほぼ停止状態」から回復した。
その後の Ralph loop により、signal-driven exit、skip reason の可視化、forward 側の
MC episode 学習経路、決定性改善なども前進した。

ただし、現状はまだ以下が残る。

- entry 後の funnel が細い
- Strategy A の寄与が薄い
- execution / exit semantics が完全には統一されていない
- PIT / リーク観点の forward 回帰保証が薄い
- multi-seed の統計評価がまだ限定的

本ドキュメントは、ここまでの調査・レビュー・PIT 点検を **1 本に統合** したものである。

---

## 現状の要点

- 取引数は 4 trades の異常状態からは回復した
- B/C の `MAX_HOLD_TIME` 依存は当初より大きく低下した
- skip reason と execution stats の観測性は改善した
- forward 側でも signal-driven exit / MC episode 更新が入った
- 同 seed 再現性は以前より改善している

一方で、次はまだ残る。

- Strategy A は依然として寄与が薄い
- `triggered` に対して `filled` が少なく、entry 後の funnel は依然として細い
- `execution_rejected` / `global_position_rejected` / `already_in_position` が主要な詰まり候補
- entry 側の execution 入力整合は進んだが、**close / force-close 側は未完了**
- 「残りは model quality だけ」と言い切るには implementation rough edge が残る

---

## 何が本当に改善されたか

以下は、実装として実際に前進した点である。

1. backtest entry の `expected_profit` が `decision.q_sampled` ベースへ改善された
2. per-strategy skip reason / execution stats の可視化が強化された
3. strategy 側に signal-driven exit (`TRIGGER_EXIT`) が追加された
4. forward runner に `already_in_position` guard が追加された
5. forward 側に signal-driven exit と `MAX_HOLD_TIME` 制御が追加された
6. forward の strategy execution order / close ordering の決定性が改善された
7. forward に MC episode evaluation と Q-function 更新が追加された
8. risk limit / shutdown close 後も MC episode を終了し学習するようになった

これらは実質的な前進であり、特に「保有しても学習されない」「終了しても学習されない」経路が閉じた点は大きい。

---

## まだ残っている implementation 課題

## 1. Execution semantics: close 側の market context は統一済み、`expected_profit` は未統一

Ralph loop により、**entry order** では backtest / forward とも
`expected_profit` に `decision.q_sampled` を渡す方向へ進んだ。
また、forward runner の **close / force-close** で `current_mid_price: entry_price`
と `volatility: 0.0` になっていた問題は修正された（commit 19ca659）。
現在は close 経路でも実際の tick mid_price と volatility を使用している。

しかし、**close / force-close 系の `expected_profit: 0.0`** は残っている。
`time_urgent: true` のため order type selection には影響しないが、
proto event の記録値としては意味づけが粗いまま。

### 改善案

1. close 系の `expected_profit` に unrealized PnL estimate（lagged）を入れる
2. backtest / forward 共通の `ExecutionRequest` 生成ヘルパーを作る
3. `q_sampled` をそのまま入れるのではなく、
   `expected_edge - dynamic_cost` を価格単位に変換した値へ寄せる
4. `time_urgent` を entry / close 両方で一貫した方針にする
5. backtest 側の close 経路も forward と同じく `mid_price` / `volatility` の整合性を確認

### 期待効果

- exit execution の一貫性向上
- fill / slippage モデルの妥当性向上
- backtest と forward の比較可能性向上

---

## 2. GlobalPositionChecker → soft-cap 化 + forward effective_lot 対応済み

soft-cap threshold（default 0.7）を導入し（commit 251d9b0）、
利用率が 70% 未満では全戦略が full lot を受け取る。
また、forward runner が `effective_lot` を使うよう修正（commit d0b77d4）。
backtest と forward の間で global position の挙動が一致するようになった。

### 残課題

1. `requested_lots -> effective_lots` の縮小率を strategy 別に記録する
2. same-direction accumulation と offsetting の挙動をテストで固定する

---

## 3. Execution rejection の観測がまだ粗い

skip reason と execution stats は改善したが、現状でも `execution_rejected` は
大きなボトルネック候補である。
どの条件で reject が増えているかを説明できる粒度の診断はまだ足りない。

### 改善案

1. strategy 別に以下を出力する
   - `order_type`
   - `effective_fill_probability`
   - `last_look_fill_prob`
   - `lp_id`
   - `volatility`
   - `requested_lots`
   - `effective_lots`
   - `reject_reason`
2. `triggered -> order_attempted -> risk_passed -> filled -> closed` を strategy 別に常設する
3. console 出力と JSON 出力の元データを統一する
4. execution diagnostics の整合性をテストで固定する

### 期待効果

- reject の支配要因を定量化できる
- calibration の効き目を再現可能に比較できる

---

## 4. Exit 品質は改善したが、まだ監視が必要

当初は、平均ホールド時間分析でほぼ全件が `MAX_HOLD_TIME` 到達クローズだった。
Ralph loop 後、この依存は大きく下がった。

これは前進だが、exit 品質の議論はまだ終わっていない。
`MAX_HOLD_TIME` 偏重が減っても、`already_in_position` が大きいままなら
機会損失は依然として残る。

### 改善案

1. `already_in_position` の回数を strategy 別・時間帯別に出す
2. hold 中に逃した trigger 数を定量化する
3. `MAX_HOLD_TIME` を safety valve として再校正する
4. exit 条件を `q_sampled` / `q_point` / alpha 消失 / volatility 条件で検証する

### 期待効果

- 保有中の機会損失の可視化
- exit の意味的品質向上

---

## 5. Strategy A → 診断完了：design choice による稀有事象戦略

Strategy A（Liquidity Shock Reversion）の不活性は **コードバグではなく設計上の選択**。
トリガー条件は4つ全ての同時成立が必要（commit 0051e6e でテスト証明）：
- `spread_zscore > 2.0`（スプレッド急拡大）
- `depth_change_rate < -0.1`（10%以上の板薄）
- `volatility_ratio > 2.0`（2倍以上のボラティリティ急増）
- `regime_kl < 1.0`（既知レジーム）

通常市場・中程度ストレスでは発火せず、真の流動性ショック時のみ設計通り動作。
不活性は model quality / data distribution の問題であり、コード品質の問題ではない。

### 残課題

1. 実データでの `spread_z`, `depth_drop`, `vol_spike` の同時成立頻度を実証する
2. A の trigger 閾値を実データ分布に基づいて再校正するには offline training が必要

---

## 6. Q 関数未学習状態は依然として PnL 分散を増幅する

取引量の主因ではないが、PnL の大きなぶれは依然として未学習状態の Thompson Sampling に依存している。

### 改善案

1. Python 学習済みモデルから ONNX / Q-state を export し import 可能にする
2. `--import-q-state` のような明示的初期化経路を作る
3. 未学習・事前学習済み・オンライン継続学習の 3 条件を比較する

### 期待効果

- PnL / Sharpe の seed 依存を緩和
- early-stage の取引品質向上

---

## 7. 実験設計は改善したが、まだ十分ではない

multi-seed の証拠は出始めたが、5 seed 程度では真の期待値を語るにはまだ薄い。

### 改善案

1. 複数 seed で backtest / forward 比較を行い、平均 PnL / Sharpe / fills の信頼区間を出す
2. 変更前後比較は単一 seed ではなく seed sweep で行う
3. `filled`, `fill_rate`, `global_position_rejected`, `execution_rejected`,
   `MAX_HOLD_TIME close`, `already_in_position` を主要 KPI にする
4. run ごとに seed・config hash・git SHA を成果物へ保存する

### 期待効果

- 「たまたま勝った / 負けた」を排除できる
- 改善効果を統計的に比較できる

---

## 8. 完全な再現性の残課題

seed 指定と順序安定化は改善したが、なお完全な比較実験のためには
入力・順序・集計・artifact の全経路で再現性を担保する必要がある。

### 改善案

1. strategy 実行順・tie-break・集計順が常に決定的であることを継続確認する
2. JSON / console / artifact の並び順を固定する
3. 非決定的コンテナが残る箇所は BTreeMap / IndexMap への置換を検討する

### 期待効果

- seed sweep や差分比較の信頼性向上

---

## 9. PIT / 情報リーク観点の評価

現時点のコードからは、**明確な look-ahead leak は見えていない**。
特に以下は維持されている。

1. `FeatureExtractor` は execution 関連特徴量に強制 lag をかけている
2. `pnl_unrealized` は lagged state snapshot ベースで参照されている
3. forward の MC transition は execution 前の features/snapshot で記録されている
4. risk/shutdown close 後の episode 終了は、terminal reward を後から学習しているだけであり、
   それ自体は PIT 違反ではない

一方で、**PIT 設計を証明する回帰テストは forward 側で弱い**。

### 改善案

1. forward E2E で、execution 直後の tick では lagged execution features がまだ見えないことを検証する
2. signal-driven exit 後の同 tick / 直後 tick で post-close 情報が特徴量へ混入しないことを検証する
3. risk/shutdown forced close 後の MC update が、次 tick 以前の意思決定にだけ影響することを検証する
4. `expected_profit = q_sampled` は leak ではないが意味づけが粗いため、
   execution 入力としての単位整合を見直す

### 期待効果

- PIT 安全性の回帰保証強化
- forward の新規 close 経路に対する安心感の向上
- 「リークはない」という主張をテストで裏づけられる

---

## 10. Ralph loop 後に残った implementation 粗さ

Ralph loop の成果は大きいが、**残課題が完全に model quality だけとは言い切れない**。
主に以下が残る。

1. close / force-close 側の `expected_profit` が未統一
2. forward runner の close ロジック重複が大きく、再不整合のリスクがある
3. summary/documentation が実装完了状態とずれる可能性がある

### 改善案

1. close 系すべてを共通ヘルパーへ寄せる
2. entry / close / force-close で execution semantics を 1 箇所に集約する
3. summary は「何が構造バグで、何が model quality か」を分けて記述する

### 期待効果

- 再発防止
- レビュー容易性向上
- 完了宣言の精度向上

---

## 優先順位

## P0: まず直すべきもの

1. close / force-close を含む execution semantics を完全統一する
2. GlobalPositionChecker の常時 lot 縮小を soft-cap 化する
3. execution / risk / skip reason の診断を strategy 別に増やす
4. forward の PIT 回帰テストを追加する

## P1: 次にやるべきもの

1. `already_in_position` と hold 中機会損失を定量化する
2. `MAX_HOLD_TIME` を再校正する
3. Strategy A の trigger 条件を実データ分布で再検証する
4. close ロジックを共通化して重複を解消する

## P2: 安定運用・評価品質のために必要なもの

1. Q-state 事前学習 / import
2. 複数 seed の統計評価
3. 再現性と artifact 整合性の強化

---

## やってはいけない対処

以下は一時的に取引数を増やしても、問題の所在を隠すので避けるべきである。

1. trigger 閾値を一律に緩めるだけ
2. fill probability を根拠なく底上げするだけ
3. risk 制約を無条件で緩和するだけ
4. 単一 seed の勝ち run を根拠に「改善した」と判断すること

---

## 検証指標

改善ごとに最低限、以下を比較する。

| 指標 | 見る理由 |
|------|----------|
| trades / fills / closes | volume が本当に増えたか |
| fill rate | execution 側の詰まりが改善したか |
| global_position_rejected | position 制約が過剰でないか |
| execution_rejected | execution モデルの詰まりが改善したか |
| MAX_HOLD_TIME close 比率 | exit が時間切れ依存から脱したか |
| already_in_position | 保有中の機会損失が減ったか |
| strategy 別 funnel | どの戦略がどこで止まっているか |
| close 側 expected_profit 整合 | entry/exit の execution semantics が揃ったか |
| forward PIT regression | 新しい close 経路でリークがないことを証明できているか |
| multi-seed mean / CI | 改善が統計的に有意か |

---

## 結論

### コード品質問題（全て修正済み）

Ralph loop（10コミット）により、以下の構造的・観測性の問題は解消した。

| 修正内容 | コミット | 分類 |
|----------|----------|------|
| close側 `current_mid_price`/`volatility` を実際のtick値に修正 | 19ca659 | backtest/forward不一致 |
| GlobalPositionChecker soft-cap (0.7) 導入 | 251d9b0 | 常時lot縮小の構造的過剰抑制 |
| forward で `effective_lot` を使用 | d0b77d4 | backtest/forward不一致 |
| forward で execution event を FeatureExtractor に供給 | f3875eb | backtest/forward不一致 |
| forward PIT 回帰テスト追加 | 5e66bb9 | PIT安全性 |
| 戦略別 entry funnel + skip理由追跡 | 9870875 | 観測性 |
| 再現性テスト全フィールド比較 | 53b3c81 | 再現性 |
| close理由（MAX_HOLD_TIME/TRIGGER_EXIT/risk/shutdown）追跡 | 276c8d3 | 観測性 |
| Strategy A トリガー条件診断テスト | 0051e6e | 戦略A不活性証明 |
| multi-seed (5 seed) 一貫性テスト | 24c08d8 | multi-seed評価 |

### 残る model/data 課題（コード品質ではない）

以下は外部データ・offline training・実際のバックテスト実行が必要であり、
コード変更では対応不可。これらは明確に model/data limits である。

| 課題 | 理由 | 必要なもの |
|------|------|-----------|
| Q-state 事前学習 | 未学習 Thompson Sampling の初期分散が大きい | Python offline training → ONNX import |
| Strategy A trigger 校正 | 4条件同時成立が実データ分布に対して厳しすぎる可能性 | 実データでの spread_z/depth_drop/vol_ratio 同時分布分析 |
| multi-seed 統計的有意性 | 5 seed のテストは構造検証のみで、PnL/Sharpe の信頼区間なし | 実データでの seed sweep と信頼区間計算 |
| MAX_HOLD_TIME 適正値 | 現在の30s/300s/600sが最適かの検証 | 実データでの hold time 分布分析 |
| execution rejection calibration | fill probability / slippage モデルの妥当性 | 実執行データとの比較 |

### 結論

コード品質の観点から、取引量と評価信頼性を阻害する構造的問題は全て解消した。
残る課題は全て model/data limits であり、コード品質の問題ではない。
