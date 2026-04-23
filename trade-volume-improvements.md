# 取引量改善・バックテスト信頼性改善提案

## 概要

Lifecycle の即時再 culling 問題は修正され、バックテストは「ほぼ停止状態」からは脱した。
ただし、現状は still-low volume / 不安定な PnL / 戦略偏在 / exit 不全が残っており、
「取引が少ない理由」と「その結果を信頼してよいか」はまだ十分に解消されていない。

本ドキュメントは、ここまでの調査で見えた改善点を **原因別** に整理し、
**優先順位付き** でまとめたものである。

---

## 現状の要点

- 取引数は 4 trades の異常状態から回復したが、まだ十分とは言い難い
- Strategy A は 0 trades で、実質 B/C のみが稼働している
- B/C のほぼ全件が `MAX_HOLD_TIME` で強制クローズされている
- `triggered` に比べて `filled` が少なく、entry 後の funnel が細い
- execution rejection / global position rejection / already-in-position が主要な詰まり候補
- backtest と forward で execution 入力が一致していない
- 複数 seed・診断粒度・統計評価の面でも、まだ実験設計が弱い

---

## 原因整理

## 1. Backtest execution が戦略エッジと切れている

backtest の `simulate_order()` は `ExecutionRequest.expected_profit` に `0.0` を渡している。
一方、forward は `decision.q_sampled` を渡している。

この差により、backtest 側では execution gateway の order type selection が
シグナル強度を参照できず、執行モデルが戦略の期待エッジと切り離される。
結果として fill / slippage / last-look が不自然になり、取引量・PnL の両方を歪める。

### 改善案

1. backtest でも `expected_profit` を `decision.q_sampled` もしくは
   `expected_edge - dynamic_cost` を価格単位に変換した値で渡す
2. backtest / forward 共通の `ExecutionRequest` 生成ヘルパーを作り、入力差分をなくす
3. `time_urgent` も共通ロジック化し、戦略別の保有期限や signal decay と整合させる

### 期待効果

- execution rejection の減少
- fill / slippage モデルの妥当性向上
- backtest と forward の比較可能性向上

---

## 2. GlobalPositionChecker が平常時から低順位戦略を抑制している

現在の global position ロジックは、global limit に近づいていない場面でも、
順位が 2 位・3 位の戦略に対して常に lot を 0.5 倍 / 0.25 倍する。

これは「限界時の安全制御」ではなく、「平常時からの常時スロットリング」であり、
低順位戦略の約定数を構造的に減らす。

### 改善案

1. lot 縮小は `|current_global_position| / global_limit` が一定閾値
   (例: 0.7〜0.8) を超えたときだけ発動する soft-cap に変更する
2. global limit に余裕があるときは requested lot をそのまま通す
3. rejection だけでなく、`requested_lots -> effective_lots` の縮小率を strategy 別に記録する
4. same-direction accumulation と offsetting の挙動を明示的に検証するテストを追加する

### 期待効果

- `global_position_rejected` の減少
- B/C 以外の戦略も参加しやすくなる
- 「安全のために必要な制限」と「不要な抑制」を分離できる

---

## 3. Execution rejection の原因が見えていない

現状でも `execution_rejected` は大きなボトルネック候補だが、
どの条件で reject が増えているかを説明できる粒度の診断が不足している。

さらに、出力 JSON では `filled_entries` と `execution_stats.total_fills` に不整合が見られ、
観測面そのものの信頼性も十分ではない。

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
2. `triggered -> order_attempted -> risk_passed -> filled -> closed` に加え、
   skip reason の集計を strategy 別に JSON へ出す
3. console 出力と JSON 出力が同一データ源を使うように統一する
4. execution diagnostics の整合性をテストで固定する

### 期待効果

- reject の支配要因を定量化できる
- パラメータ変更が効いたかどうかを再現可能に比較できる

---

## 4. Exit ロジックが実質的に「時間切れ」しか機能していない

平均ホールド時間分析では、217 件中 216 件が `MAX_HOLD_TIME` 到達でクローズされている。
これは B/C が「シグナルに基づいて退出する戦略」ではなく、
「入った後はタイマーで閉じる戦略」になっていることを意味する。

この状態では、保有中に発生した次のシグナルは `already_in_position` で潰れ、
取引密度が下がる。

ただし、5〜10 分 hold だけでは 2 年で数百 trades という水準を単独では説明できないため、
これは **主因ではなく増幅要因** とみなすべきである。

### 改善案

1. `MAX_HOLD_TIME` を safety valve に格下げし、
   通常の exit は signal-driven にする
2. exit 条件候補
   - `q_sampled` / `q_point` の閾値割れ
   - 逆シグナル
   - alpha 消失 (`expected_edge <= cost`)
   - PnL / volatility 条件
3. `already_in_position` の回数を strategy 別・時間帯別に出し、
   hold 中の機会損失を定量化する
4. `MAX_HOLD_TIME` を再校正し、B/C については現在値の半分程度でも比較する

### 期待効果

- 保有中に次の機会を逃す量が減る
- close 理由が `MAX_HOLD_TIME` 偏重から分散し、戦略としての意味が出る

---

## 5. Strategy A が事実上死んでいる

最新の分析では Strategy A の取引が 0 件であり、
現状の volume は B/C だけに依存している。
これは戦略分散の喪失であり、単に取引数が少ないだけでなく、
システム全体が一部戦略に偏っていることを意味する。

### 改善案

1. Strategy A の trigger 条件が実データでどれだけ成立しているかを再分析する
2. `spread_z`, `depth_drop`, `vol_spike` の同時成立頻度を分位点ベースで検証する
3. A の trigger 緩和は手調整ではなく、実データの分布に基づいて決める
4. strategy 別 funnel を常設し、A が
   `evaluated` で止まっているのか
   `triggered` で止まっているのか
   `filled` で止まっているのかを分離する

### 期待効果

- 戦略分散の回復
- 「B/C だけで回している」偏りの緩和

---

## 6. Q 関数未学習状態での Thompson Sampling が PnL 分散を増幅している

取引量の主因ではないが、PnL の大きなぶれは未学習状態での Thompson Sampling に強く依存している。
初期 posterior が薄い状態では、Q サンプルのばらつきが大きく、
seed による結果差が過大になりやすい。

### 改善案

1. Python 学習済みモデルから ONNX / Q-state を export し、
   backtest 起動時に import できるようにする
2. `--import-q-state` のような明示的初期化経路を作る
3. 未学習・事前学習済み・オンライン継続学習の 3 条件を比較する

### 期待効果

- PnL / Sharpe の seed 依存を緩和
- early-stage の取引品質向上

---

## 7. 実験設計がまだ弱い

1 seed の結果だけでは、真の期待値も、変更が効いたかどうかも判断しにくい。

### 改善案

1. 複数 seed で backtest を回し、平均 PnL / Sharpe / fills の信頼区間を出す
2. 変更前後比較は単一 seed ではなく seed sweep で行う
3. `filled`, `fill_rate`, `global_position_rejected`, `execution_rejected`,
   `MAX_HOLD_TIME close` を主要 KPI にする
4. run ごとに seed・config hash・git SHA を成果物へ保存する

### 期待効果

- 「たまたま当たった / 外れた」を排除できる
- 改善効果を統計的に比較できる

---

## 8. 完全な再現性の残課題

seed 指定は改善されたが、なお反復順序や出力整合性については注意が必要である。
決定的な比較実験を成立させるには、入力・順序・出力の全経路で再現性を担保する必要がある。

### 改善案

1. strategy 実行順・tie-break・集計順が常に決定的であることを継続確認する
2. JSON / console / artifact の並び順を固定する
3. 非決定的コンテナが残っている箇所は BTreeMap / IndexMap への置換を検討する

### 期待効果

- seed sweep や差分比較の信頼性向上

---

## 優先順位

## P0: まず直すべきもの

1. backtest の `expected_profit=0.0` をやめ、forward と統一する
2. GlobalPositionChecker の常時 lot 縮小を soft-cap 化する
3. execution / risk / skip reason の診断を strategy 別に増やす

## P1: 次にやるべきもの

1. B/C の exit を signal-driven 化する
2. `already_in_position` と hold 中機会損失を定量化する
3. `MAX_HOLD_TIME` を再校正する
4. Strategy A の trigger 条件を実データ分布で再検証する

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
| multi-seed mean / CI | 改善が統計的に有意か |

---

## 結論

現状の低取引量は、単一の原因ではない。
主因は **entry 後の funnel の細さ** であり、
特に **backtest execution の入力不整合** と
**global position の平常時抑制** が優先課題である。

一方で、B/C の `MAX_HOLD_TIME` 依存と A の無活動は、
戦略としての完成度不足を示している。

したがって、次の実装順は以下を推奨する。

1. execution 入力を backtest / forward で統一
2. global position を soft-cap 化
3. diagnostics を強化して詰まりを可視化
4. B/C exit を signal-driven 化
5. A trigger を再設計
6. Q-state pretraining と multi-seed 評価を導入
