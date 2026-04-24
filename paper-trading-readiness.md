# ペーパートレード開始前の実装状況と、これからやること

## この資料の目的

来週から「本格的にペーパートレードを始めたい」という前提で、次の3つを整理します。

1. **今、何が実装済みか**
2. **何がまだ足りないか**
3. **来週までに最低限やるべきことは何か**

ここでいうペーパートレードは、**実際の市場データを使うが、実注文は出さずにシミュレーションで約定させる運用**を指します。

---

## まず結論

結論を先に書きます。

- **録画済みデータを使った forward replay は、かなり実行できる状態です**
- **ただし、外部 API につないで live の市場データで回す paper trade は、まだ未完成です**
- したがって、**「来週から live データで本格稼働」はまだ早い**です
- 来週できる現実的な案は、**録画データでの長時間 forward run** か、**live feed 実装を先に終わらせたうえで限定運用**です

要するに、

- **paper execution（実注文を出さないこと）** は実装済み
- **live market data を本当に受け取ること** は未完成

です。

---

## 現在の実装状況

## 1. 実注文を出さない仕組みはある

### 状況

forward は `PaperExecutionEngine` を使っており、**実注文経路にはつながらない構造**になっています。

### これは何を意味するか

- 間違って本番注文を出す危険はかなり低い
- 「市場データだけ live、約定は simulation」という paper trade の形は作りやすい

### 評価

**これは良い状態です。**  
paper trading の安全性という意味では、重要な土台はあります。

---

## 2. recorded データの forward replay は動く

### 状況

CLI から `forward-test` を実行でき、`recorded` ソースで CSV / Event Store を読んで forward runner を回せます。

### できること

- duration 指定
- strategy 指定
- seed 指定
- output ディレクトリ指定
- Ctrl+C 時の部分結果保存

### 評価

**これは「本番前の擬似運転」にかなり使えます。**  
来週までにやるべき長時間運転テストは、まずこの経路で回すべきです。

---

## 3. forward runner 自体はかなり育っている

### 状況

forward runner には、以下が入っています。

- 戦略評価
- risk limit
- lifecycle
- global position 制御
- close / force-close
- MC episode 終了
- funnel 診断
- reproducibility テスト
- PIT 系の回帰テスト

### 評価

**「売買フローそのもの」がまったく無い状態ではありません。**  
backtest と forward の不整合も以前よりかなり減っています。

---

## 4. live 外部 API feed は未完成

### 状況

`ExternalApiFeed` はありますが、**骨組みだけ**です。  
接続時に credentials を読み、`connect()` / `subscribe()` は通りますが、`next_tick()` は現在 **常に `None` を返す実装**です。

### これは何を意味するか

- 外部 API を指定しても、実際には live tick が流れてきません
- つまり、**live 市場データを使った paper trade は今のままでは走りません**

### 評価

**ここが最大の未実装ポイントです。**  
来週から live paper trade をやりたいなら、最優先でここを実装する必要があります。

---

## 5. alert / report / comparison は「部品はある」が、実行経路に十分つながっていない

### 状況

`alert.rs`、`report.rs`、`comparison.rs` というモジュールはあります。  
しかし、現状の CLI / runner の実行経路を見ると、**これらが本番運用向けに十分活用されている状態ではありません**。

実際に今すぐ確実に出るのは、主に:

- forward result の出力
- 一部の summary
- トラッカー由来の集計

です。

### これは何を意味するか

- アラートの存在だけで安心してはいけない
- レポートの仕組みがあっても、運用時に自動で欲しい形で出るとは限らない

### 評価

**運用面ではまだ弱い**です。  
来週から回すなら、「何か起きたときにすぐ分かる状態」にしておく必要があります。

---

## 6. forward config が全部は runtime に反映されていない

### 状況

`ForwardTestConfig` には色々な設定がありますが、runner 側では一部しか使われていません。  
特に実行時には、いくつかの重要コンポーネントが `default()` で作られています。

例:

- `ExecutionGatewayConfig::default()`
- `DynamicRiskBarrierConfig::default()`
- `LifecycleConfig::default()`
- `GlobalPositionConfig::default()`
- `KillSwitchConfig::default()`

### これは何を意味するか

- 設定ファイルを変えても、実際の挙動に反映されない項目がある
- 「設定を詰めたから大丈夫」と思っても、runtime がその設定を使っていない可能性がある

### 評価

**運用開始前に必ず直すべき項目です。**  
ここが未解決だと、設定と実挙動がズレます。

---

## 7. forward には Q-state import 経路がまだない

### 状況

backtest では Q-state の export / import ができます。  
しかし、forward CLI には **Q-state import の入口がありません**。

### これは何を意味するか

- 来週から paper trade を始めても、基本的には未学習に近い状態から始まります
- seed 依存や初期の不安定さが、そのまま paper でも出やすいです

### 評価

**必須ではないが、かなり重要**です。  
本格的に paper trade をやるなら、できるだけ早く入れたほうが良いです。

---

## 来週までに最低限やるべきこと

ここは優先順位順に書きます。

## P0: 来週前に必須

### 1. live market data feed を実装する

#### 何をやるか

- `ExternalApiFeed` の `next_tick()` を本物にする
- provider（例: OANDA）との接続、購読、tick 受信、切断、再接続を実装する

#### なぜ必要か

**これがないと live paper trade そのものが始まりません。**

#### 完了条件

- 外部 API 接続で tick が継続的に流れる
- 一時切断後に再接続できる
- 数時間流しても止まらない

---

### 2. forward config を runtime に正しく配線する

#### 何をやるか

- execution config
- barrier config
- lifecycle config
- global position config
- kill switch config

を `ForwardTestConfig` から runner に渡す

#### なぜ必要か

**設定ファイルと実動作が一致しないまま運用するのは危険**だからです。

#### 完了条件

- 設定変更が実際の runtime 挙動に反映される
- 少なくとも主要 config に対して回帰テストがある

---

### 3. 運用時の監視出力を最低限そろえる

#### 何をやるか

- 日次または一定間隔で summary を出す
- trades / pnl / drawdown / halt / strategy funnel を見やすく出す
- 異常時にログや webhook へ通知する

#### なぜ必要か

**回っているだけでは足りず、異常をすぐ見つけられないと意味がない**からです。

#### 完了条件

- どこを見るべきかが明確
- 停止・risk limit・execution drift・極端な drawdown を人間がすぐ把握できる

---

### 4. recorded データで長時間 soak test をやる

#### 何をやるか

- recorded feed で数時間〜数日相当を連続実行
- day rollover / week rollover / shutdown / reconnect 相当を確認

#### なぜ必要か

live に行く前に、**長時間運転で落ちないか** を見ないと危険だからです。

#### 完了条件

- 長時間で panic しない
- 結果ファイルが出る
- partial shutdown でも結果が壊れない

---

## P1: できれば来週前、遅くとも直後にやる

### 5. forward に Q-state import を追加する

#### 何をやるか

- forward CLI に Q-state import オプションを追加
- backtest / offline training で作った Q-state を読み込めるようにする

#### なぜ必要か

未学習のまま始めると、paper trade の結果が「モデルの弱さ」より「初期学習不足」に強く引っ張られるからです。

#### 完了条件

- 事前学習あり/なしで forward を比較できる

---

### 6. 実際の運用 runbook を作る

#### 何をやるか

- 起動手順
- 停止手順
- ログ確認手順
- 障害時の切り分け手順
- 日次確認項目

を 1 枚にまとめる

#### なぜ必要か

来週から回すなら、**コードより先に運用で迷わないこと** が大事だからです。

#### 完了条件

- 他人でも起動・停止・確認ができる

---

## P2: 本格運用前に必要

### 7. alert / report / comparison を本当に使う形で配線する

#### 何をやるか

- `AlertSystem`
- `ReportGenerator`
- `ComparisonEngine`

を CLI / runner に組み込む

#### なぜ必要か

今は「部品はある」が、「運用で効く形」にはなっていないからです。

#### 完了条件

- forward 実行後に自動でレポートが出る
- 必要な異常が通知される
- backtest との差が簡単に見える

---

### 8. live paper の評価基準を決める

#### 何をやるか

- 何日連続で動けば合格か
- どの指標を見れば停止すべきか
- どの程度の drawdown なら許容か

を決める

#### なぜ必要か

基準がないと、「動いているから OK」になってしまうからです。

#### 完了条件

- Go / No-Go の条件が明文化されている

---

## 来週の現実的な進め方

今の状態を踏まえると、現実的には次の2案です。

## 案A: 来週は recorded forward を本格運用する

### やること

- recorded feed で forward を長時間回す
- report / log / funnel を確認する
- live feed 実装はその後に進める

### メリット

- すぐ始められる
- 実注文リスクはもちろんゼロ
- 長時間運転の不具合を先に潰せる

### デメリット

- live 市場との接続まわりは検証できない

### 向いているケース

- 「まずは forward の安定性確認をしたい」
- 「来週から絶対に live 接続したいわけではない」

---

## 案B: 来週から live paper をやる

### 前提

これは **P0 を終わらせることが前提**です。

### 最低条件

1. ExternalApiFeed 実装完了
2. config 配線完了
3. alert / logging 最低限完了
4. recorded soak test 済み

### 評価

**今すぐこのままは不可**です。  
ただし、集中して実装すれば「限定的な live paper 開始」は視野に入ります。

---

## 私のおすすめ

今のコードを見たうえでのおすすめは、次です。

### おすすめ方針

1. **来週前半は recorded forward の長時間運転**
2. **並行して ExternalApiFeed を実装**
3. **live paper 開始は、feed 実装と config 配線が終わってから**

### 理由

いま一番危ないのは、

- 「paper execution は安全だから大丈夫」と思って
- **live データ接続と監視が未完成のまま始めること**

です。

paper trade で一番最初に必要なのは、勝つことより先に

- ちゃんと tick が来る
- 止まらない
- 状態が壊れない
- 異常が見える

ことです。

今はそこがまだ十分ではありません。

---

## 最後に

現状を一言でまとめると、

- **paper execution はある**
- **forward replay もある**
- **でも live paper trading の入口はまだ未完成**

です。

したがって、来週から本格的にやるなら、

**「まず recorded forward を安定運用できる状態にし、そのうえで live feed を実装してから live paper に進む」**

のが最も安全です。
