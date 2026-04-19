# FX AI準短期自動売買システム：完全統合版 マスターデザインドキュメント

**ドキュメントバージョン**: 15.0 (Unified Uncertainty + Episode-Defined MC + Strategy-Decoupled Reward + Full Risk Limits)
**システム定義**: 機関HFT領域を構造的に回避した準短期戦略において、数学的統計堅牢性により過学習を撲滅し、実務的Event Sourcingアーキテクチャにより分散システム特有の破綻を完全に防止した上で、**MDP + ベイズ線形回帰 + Thompson Sampling**（統一不確実性処理: Thompson Samplingがσ_modelの唯一の反映経路）、**エピソード定義付きOn-policy Monte Carlo**（最大ホールド時間・hold退化防止機構付き）、**戦略分離型報酬関数**（ポートフォリオ結合排除）、**グローバルポジション制約付き状態空間**、**OTC市場（Last-Look・Internalization）対応約定モデル**、情報リーク完全排除の実行ロジック（実行系+PnL特徴量の強制ラグ）、**HDP-HMM動的Regime管理**（バックグラウンド推論 + 軽量オンライン指標）、**日次二段階+週次+月次の完全階層的損失リミット**（決定関数内で全段階チェック）、時系列依存する動的流動性を内部コスト化し、極小エッジを高頻度で最適化実行する、自己進化・自己淘汰型クオンツ・システム。

---

## 1. システム哲学と究極の前提

本システムは、「AIが市場を1ステップ先読みして勝つ」という神話を捨て去り、以下の物理法則・構造的限界をシステムアーキテクチャの根底に組み込みます。

1.  **速度における構造的敗北の回避**: ms以下の土俵から撤退。HFTが支配できない「数秒〜数分の持続的歪み」のみを狙う。
2.  **統計における偽陽性の排除**: CPCV, PBO, DSRで論理的棄却。年率 `Sharpe > 1.5` は強制破棄し、現実的な `Sharpe 0.8〜1.2` を目標とする。
3.  **状態の非信頼化**: **不変のイベントログのみが真実**。意思決定プロセスも完全にイベント化し、リプレイによる再現を必須とする。
4.  **分散システムの地雷の無力化**: Event Loss、Eventual Consistency、Schema Evolution、Storage Explosionを動的リスク制限で防ぐ。
5.  **誤差・遅延・非定常性の究極のコスト化**: 利益を時間積分 $\sum \text{reward}(s_t)$ として定義。多層不確実性（モデル・実行・遅延・Fat-tail）、非線形インパクト、時系列依存ドリフトを全て内部コスト化。
6.  **情報リーク（因果逆転）の完全排除**: 未来の実行結果を特徴量として使うと因果が逆転し、バックテストで高精度でも実戦で崩壊するため、**実行系データには必ず強制ラグをかける**。
7.  **利益構造の現実**: 1トレードあたり **+0.05 〜 0.2 pip** の極小エッジを、勝率52〜55%・低コスト・高頻度で累積させる。「勝てるかどうか」ではなく「誤差に食われないか」を管理するシステム。
8.  **意思決定の数理的閉包**: EVの閾値判定ではなく、MDPに基づく最適化ポリシー $\pi(a|s)$ により意思決定を数理的に閉じる。閾値最適化への依存を排除し、regime変化に対してロバストな決定を実現する。
9.  **報酬のリスク調整**: 報酬関数を $r_t = \text{PnL}_t - \lambda_{risk} \cdot \sigma^2_{portfolio,t} - \lambda_{dd} \cdot \text{DD}_t$ と定義し、リスクとドローダウンを報酬に直接統合。局所的に勝って全体で負ける構造を排除する。
10. **ポジション連続性の制約**: ポジションを状態空間に組み込み、$|p_{t+1}| \leq P_{max}$、$|p_{t+1} - p_t| \leq V_{max}$（変化速度制限）の制約を課す。EVが正でも無限に積まない「破産しないモデル」。
11. **OTC市場の現実**: FXは取引所ではなくOTC市場。Last-look拒否・Internalization・価格改善/悪化は構造的に不可避。板データの順序通りに約定する前提は崩壊する。約定モデルはOTC特性を反映する必要がある。
12. **ハードリミットの絶対優位**: Q値やポリシーの判断を待たずに発動する日次最大損失リミットが存在する。いかにQ値が正でも、日次損失が閾値に達した時点で全ポジションを強制クローズし取引停止する。
13. **release build安全性**: `debug_assert!` はrelease buildで除去される（Rust仕様）。本番防御はすべて `assert!` または `Result<_, RiskError>` による明示的エラーハンドリングで実装する。
14. **エッジの物理的限界**: +0.05〜0.2 pipのエッジはスプレッド・手数料・slippageのコスト合計と同程度以下。Dynamic_Costの推定誤差がエッジを超えれば正味のエッジは消滅する。コスト推定は利益計算と同等に重要。

---

## 2. コアーキテクチャ（全体像）

すべてのモジュールは直接状態を共有せず、**Partitioned Event Bus**を通じてのみ通信します。

```mermaid
flowchart TD
    subgraph "1. Market Gateway (Low Latency)"
        NET[Colocation LD4/NY4 <1ms] --> FIX[FIX / WS Handler]
        FIX -->|MarketEvent (Tier3)| M_STREAM[(Market Stream)]
    end

    subgraph "2. Partitioned Event Bus & Gap Detection"
        M_STREAM --> GAP[Gap Detection Engine]
        S_STREAM[(Strategy Stream)] --> GAP
        E_STREAM[(Execution Stream)] --> GAP
        ST_STREAM[(State Stream)]
        GAP -.->|深刻なgap: 取引停止 & Replay| FIX
        GAP -.->|軽微なgap: Warningログ| RISK
    end

    subgraph "3. Strategy & MDP Policy Engine (Stateless)"
        M_STREAM --> FEAT[Feature Extractor<br>OBI, ΔOBI, Vol, Signed Vol, Intensity, Time, Queue Pos, Position State]
        FEAT --> NONLIN[Dynamic Liquidity Model<br>Impact_t = f(pos_t, liq_t) / liq_t+1 = g(Impact_t)]
        NONLIN -->|Adjusted X| DRIFT[HDP-HMM Regime Switching Drift<br>dynamic K regimes · drift_t = Σ π_k · f_k(drift_t-1, X_t)]
        DRIFT -->|X_adjusted + drift_t| Q_CALC[Q-Function Calculator<br>Q(s,a) = Σ γ^k · r_{t+k} · exp(-λ·Δt)]
        Q_CALC --> SIGMA[Execution σ Calculator<br>σ_non_model = sqrt(σ_exec² + σ_latency²)]
        SIGMA -->|Q(s,a), σ_non_model| POLICY[Policy Optimizer<br>Thompson Sampling (σ_model via posterior) · a* = argmax Q̃_final(s,a)]
        POLICY -->|PolicyCommand| BARRIER
    end

    subgraph "4. Dynamic Risk Barrier (非同期・低遅延)"
        BARRIER{Staleness検知 & Global Position Constraints & Daily Hard Limit & Directional Latency Penalty}
        BARRIER -->|a* = argmax Q(s,a) within constraints| E_STREAM
        BARRIER -->|No valid action| SKIP[Trade Skip Event]
    end

    subgraph "5. Execution Gateway (Causal-Leak-Free)"
        E_STREAM --> EXEC[Execution Alpha Engine<br>P(fill | queue_pos, flow, lagged_data)]
        EXEC -->|OrderCommand| OMS[Order Sender]
        OMS --> LP[(LP / Broker)]
        LP -->|OrderSentEvent| E_STREAM
        LP -->|FillEvent / RejectEvent| E_STREAM
    end

    subgraph "6. State & Risk Manager (Stateful Projector)"
        M_STREAM & S_STREAM & E_STREAM --> SM[State Projector]
        SM -->|StateSnapshotEvent (Tier1)| ST_STREAM
        SM --> RISK[Risk & Lifecycle Manager]
        RISK -->|KillCommand| S_STREAM
        SM -->|state_version & staleness_ms| BARRIER
    end

    subgraph "7. Tiered Event Store & Observability"
        M_STREAM & S_STREAM & E_STREAM --> ES[(Tiered Event Store)]
        ES --> REG[(Immutable Schema Registry)]
        ES --> DET[Anomaly Detector]
    end
```

### 2.1 Command / Event の厳格な分離
*   **Command（意図）**: 処失や破棄の可能性がある要求。
*   **Event（事実）**: 取り消し・変更が絶対に不可能な確定した事実。

---

## 3. 【中核】利益生成アルゴリズムの完全定義（MDP最適化モデル・OTC市場対応）

全戦略をEV閾値判定からMDP（マルコフ決定過程）に基づく最適化ポリシーへ移行し、意思決定を数理的に閉じた形で定義します。Q関数推定はベイズ線形回帰（On-policy + Monte Carlo評価）により規定し、OTC市場（Last-Look・Internalization）の約定現実をモデルに組み込みます。

### 3.0 MDP定式化

*   **状態空間**: $s_t = (X_t^{market}, p_t^{position})$
    *   $X_t^{market}$: 市場特徴量ベクトル（OBI, Vol, Session等、各戦略固有）
    *   $p_t^{position}$: ポジション状態（サイズ, 方向, ホールド時間, エントリー価格）
*   **行動空間**: $a_t \in \{\text{buy}_k, \text{sell}_k, \text{hold}\}$（$k$: ロットサイズ）
    *   制約: $|p_{t+1}| \leq P_{max}$
    *   制約: $|p_{t+1} - p_t| \leq V_{max}$（変化速度制限）
*   **報酬関数**（戦略分離型・自己凍結防止）:
    $r_t^i = \text{PnL}_t^i - \lambda_{risk} \cdot \sigma^2_{i,t} - \lambda_{dd} \cdot \min(\text{DD}_t^i, \text{DD}_{cap})$
    *   $r_t^i$: 戦略$i$の報酬。他戦略の行動に依存しない。
    *   $\text{PnL}_t^i$: 戦略$i$の実現＋未実現損益
    *   $\sigma^2_{i,t}$: 戦略$i$のポジション分散（$\sigma^2_{i,t} = p_i^2 \cdot \sigma^2_{price}$）。ポートフォリオ全体の分散ではなく、各戦略の独立リスク指標。
    *   $\text{DD}_t^i = \max(0, \text{equity\_peak}_i - \text{equity}_{i,t})$: 戦略$i$の単独ドローダウン
    *   $\text{DD}_{cap}$: DD項の上限（$\lambda_{dd} \cdot \text{DD}_{cap}$ が報酬を完全に支配しないよう設定）。DDがcapを超えた場合、報酬のDD項は飽和し、Q値のPnL成分による回復取引を阻害しない。代わりに§9.4のハードリミットが発動。
    *   **設計意図**: 各戦略のQ関数が他戦略の行動に依存する報酬で学習されると、独立MDPとしての数理的整合性が崩壊する。戦略固有のリスク指標により各MDPを独立に閉じ、ポートフォリオ全体のリスクは§9.5のグローバルポジション制約と§9.4の階層的損失リミットで管理する。
*   **Q関数**:
    $Q(s_t, a_t) = E\left[\sum_{k=0}^{H} \gamma^k \cdot r_{t+k} \mid s_t, a_t\right]$
*   **ポリシー**（Thompson Sampling — ゼロコスト探索）:
    1.  事後分布から重みをサンプル: $\tilde{w} \sim \mathcal{N}(\hat{w}, \hat{\Sigma})$
    2.  サンプル重みでQ値計算: $\tilde{Q}(s,a) = \tilde{w}^T \phi(s)$
    3.  最適行動を選択: $a^* = \arg\max_{a \in \mathcal{A}_{valid}} \tilde{Q}(s,a)$
    *   ** Thompson Samplingの利点**: 不確実な時はサンプルが多様化→自然に探索、確実な時は一貫→活用。ε-greedyのような固定確率のランダム探索が不要で、エッジの侵食ゼロ。
    *   **ε-greedy不採用の理由**: ε=0.10の場合、10%の取引が完全ランダムで期待利益≈-spread/2≈-0.1pip。エッジ0.1pipでは全体期待値が0.08pipに低下（20%消失）。ε=0.15なら30%消失。Thompson Samplingはこの問題を回避。
    *   **Boltzmann Softmax不採用の理由**: 0.05〜0.2 pipの極小エッジにおいて、確率的サンプリングの分散がエッジを埋没させる。

### 3.0.1 Q関数アーキテクチャ（統一版）

§3.1-3.3の非線形計算は「別のQ関数」ではなく、**統一特徴量パイプライン φ(s)** の設計仕様です。学習対象は重み $w$ のみ。**システム内にQ関数の定義は以下の一つのみ**であり、§3.1-3.3は各戦略のφ(s)の設計仕様を示します。

*   **特徴量パイプライン φ(s)**:
    *   一次項: spread_z, OBI, ΔOBI, vol, session, position 等（§3.4完全定義）
    *   非線形変換項（既知関数、学習なし）: self_impact = f(pos, liq), decay = exp(-λ·t), dynamic_cost, P(revert) 等
    *   交互作用項（手動設計）: spread_z × vol, OBI × session, depth_drop × vol_spike 等（§3.4完全定義）
    *   ポジション状態: size, direction, holding_time, pnl_unrealized
*   **Q値計算（学習対象は重みのみ、これが唯一のQ関数定義）**:
    $Q(s, a) = w_a^T \phi(s)$
*   **学習アルゴリズム**: ベイズ線形回帰（On-policy + Monte Carlo評価）
    *   事後分布: $w \sim \mathcal{N}(\hat{w}, \hat{\Sigma})$ where $\hat{\Sigma} = \hat{\sigma}^2_{noise,t} \cdot (\Phi^T \Phi + \lambda_{reg} I)^{-1}$
    *   **適応ノイズ分散**: $\hat{\sigma}^2_{noise,t} = \text{EMA}_{variance}(\text{residuals}, \text{halflife}=500)$。固定値ではなく、直近の残差からノイズ分散を推定。ボラティリティクラスタリング等の非定常性に自動追従し、事後分散 $\hat{\Sigma}$ が環境変化を反映。
    *   オンライン更新: 新データ到着ごとに事後を更新（Bayesian update）
    *   **On-policy**: 実際に取った行動の結果のみで更新。Off-policyデータは使用しない。
    *   **Monte Carlo評価**: エピソード完了後の割引累積報酬 $G_t = \sum_{k=0}^{T} \gamma^k r_{t+k}$ をターゲットに使用。Bootstrapping（$y = r + \gamma \max Q$）は行わない。エピソード定義は§3.0.2を参照。
    *   **Deadly Triad回避の根拠**（Sutton & Barto, 2018 §11.3）:
        *   Off-policy → On-policy: 実行行動のみ使用 ✓
        *   Bootstrapping → Monte Carlo: 完全体のリターンを使用 ✓
        *   関数近似: ベイズ正則化で重み発散を抑制 ✓
        *   三つのTriad要素のうち二つを構造的に除去。発散条件を満たさない。
    *   **サンプル効率のトレードオフ**: On-policy + Monte CarloはOff-policy + TDよりサンプル効率が低いが、発散リスクを完全に排除する。0.05-0.2 pipの極小エッジにおいて、発散による壊滅的損失はサンプル効率の低下より致命的である。
*   **不確実性量化 — 一元化設計**: **Thompson Samplingがσ_modelの唯一の反映経路**。Q値の点推定にσ_modelを含めない。事後分散が大きい（不確実性が高い）状態ではサンプルが多様化し自然に探索、事後分散が小さい（確実性が高い）状態ではサンプルが収束し活用。この機構単体で探索-活用トレードオフを処理するため、点推定側で重複ペナルティを課さない。
    *   事後分散: $\sigma_{model}(s, a) = \sqrt{\phi(s)^T \hat{\Sigma} \phi(s)}$（Thompson Sampling内でのみ使用）
    *   **実行環境不確実性（点推定側で処理）**: $\sigma_{non\_model} = \sqrt{\sigma^2_{execution} + \sigma^2_{latency}}$（σ_modelを含まない）
    *   キャリブレーション監視: 事後分散と実際の残差を比較、乖離時はσ²_noiseの適応係数を調整
    *   **ベイズ事後分散の特性と限界**: ベイズ事後分散はデータ到着順序に依存しないが、**標準的なベイズ線形回帰はw不変・σ²不変を前提とする**。非定常環境では事後分散が過小推定される（データが溜まるほど事後が狭くなるが、真のwは変動）。§3.0.1の適応ノイズ分散（$\hat{\sigma}^2_{noise,t}$）がこの問題を緩和するが、完全な解決ではない。regime変化時は事後分散の急激な縮小を防ぐため、Changepoint Detection（§9.2）と連動して事後を部分的にリセットする。
*   **発散監視**: $\|w_t\| / \|w_{t-1}\| > 2.0$ → 初期値にリセット + 取引停止 + リキャリブレーション
*   **制約**: σ_modelがDynamic_Costの推定誤差を含むエッジ幅を超える場合、Thompson Samplingのサンプル分散が拡大し、自然にhold選択確率が上昇する。明示的なNo Trade判定は行わず、サンプリング結果に委ねる。
*   **事後ペナルティ（学習対象外、既知関数 — σ_modelを含まない）**:
    $Q_{adjusted}(s,a) = w_a^T \phi(s) - \text{self\_impact}_t - \text{dynamic\_cost} - k \cdot \sigma_{non\_model}$
    *   σ_modelはThompson Samplingのサンプリング分散を通じてのみ反映。点推定（Q_adjusted）にはσ_modelを含めない。
    *   **Thompson Sampling（行動選択に使用する値、これが唯一の行動選択基準）**:
        1.  事後分布から重みをサンプル: $\tilde{w} \sim \mathcal{N}(\hat{w}, \hat{\Sigma})$
        2.  サンプル重みでQ値計算、事後ペナルティ適用: $\tilde{Q}_{final}(s,a) = \tilde{w}_a^T \phi(s) - \text{self\_impact} - \text{dynamic\_cost} - k \cdot \sigma_{non\_model} - \text{latency\_penalty}(a)$
        3.  最適行動を選択: $a^* = \arg\max_{a \in \mathcal{A}_{valid}} \tilde{Q}_{final}(s,a)$
        4.  記録: PolicyCommandに $a^*$, $\tilde{Q}_{final}(s,a^*)$, $Q_{point}(s,a^*) = \hat{w}^T \phi(s) - \text{penalties}$ の両方を記録

### 3.0.2 エピソード定義（On-policy Monte Carlo評価の前提）

Monte Carlo評価はエピソードの完了（割引累積報酬 $G_t$ の確定）を必要とするため、エピソードの終端条件を明示する。

*   **エピソード開始**: ポジションがゼロから非ゼロになった時点（buy_k or sell_k の実行時）
*   **エピソード終端条件（いずれかを満たした時点）**:
    1.  ポジションの完全クローズ（自発的またはリスク制限による強制クローズ）
    2.  ホールド時間 > MAX_HOLD_TIME（戦略ごとに設定: 戦略A: 数秒〜数十秒、戦略B/C: 数分〜数十分）。これらは物理的タイムスケールから導出し、ブラックボックス最適化しない。
    3.  日次ハードリミット発動（§9.4）
    4.  未知Regime検出による取引停止
*   **フラット（ポジションなし）期間**: エピソードに含めない。学習対象外。hold行動はポジション保持中のholdのみを対象とし、フラット期間のholdは行動空間 $\mathcal{A}$ から除外。
*   **部分約定の扱い**: ポジション全量クローズ時のみエピソード終了。部分クローズはエピソード内のイベントとして扱い、残余ポジションに対して継続的にQ値評価を行う。
*   **MAX_HOLD_TIME到達時の処理**: 強制的にポジションをクローズし、エピソードを終了。実現したPnLを報酬の最終項に組み込む。この強制クローズ自体もOn-policyデータとして学習に使用される。

### 3.0.3 Hold支配退化の防止機構

On-policy学習では「実際に取った行動のみで更新」するため、初期段階でholdが支配的になるとbuy/sellのデータが蓄積せず、正のフィードバックでhold-onlyに収束するリスクがある。

*   **楽観的初期化（Optimistic Initialization）**: 学習開始時、$\hat{w}_{buy}$, $\hat{w}_{sell}$ をholdより高い値で初期化。初期のThompson Samplingがbuy/sellを選択しやすくする。初期化値はバックテストでの平均Q値から導出。
*   **最小取引頻度の監視**:
    *   直近Nティック中のbuy/sell実行回数 < MIN_TRADE_FREQUENCY の場合、事後分散 $\hat{\Sigma}$ に人工的な膨張係数を適用: $\hat{\Sigma}_{inflated} = \alpha_{inflation} \cdot \hat{\Sigma}$（$\alpha_{inflation} > 1$）
    *   これによりThompson Samplingのサンプルが多様化し、buy/sellの探索が強制される
    *   取引頻度が回復したら$\alpha_{inflation}$を1に漸減
*   **γ-減衰によるhold誘導の抑制**: 割引率 $\gamma$ により、遠い未来の報酬は減衰する。holdを続けるほどQ値が低下する構造を利用し、長期holdの期待値を自然に低下させる。ただし、これだけでは不十分なため上記2つの機構を併用。

### 3.1 戦略A：Liquidity Shock Reversion — 特徴量パイプライン φ_A(s)
戦略Aは、流動性ショック時のリバージョンを狙う数秒ホライズンの戦略です。以下の特徴量が $\phi_A(s)$ に含まれます。
*   **状態定義**: $s_t = (\text{spread\_z}, \text{depth\_drop}, \text{vol\_spike}, \text{regime\_kl}, \text{reversion\_speed}, \text{queue\_position}, p_t)$
*   **トリガー**: $\text{spread\_z} > 3 \land \text{depth\_drop} > \theta_1 \land \text{vol\_spike} > \theta_2 \land \text{regime\_kl} < \text{threshold}$
*   **エントリー**: $\text{direction} = \text{sign}(\text{price\_move})$ の逆方向へ。
*   **戦略A固有の非線形特徴量（既知関数、学習なし、φ_A(s) の構成要素）**:
    *   $\text{Self\_Impact}_t = f(p_t, \text{liquidity}_t)$: ポジションサイズと流動性による自己インパクト
    *   $\text{liquidity}_{t+1} = g(\text{Self\_Impact}_t)$: 動的流動性変化
    *   $P(\text{revert}) = \sigma(\dots)$: リバージョン確率（ロジスティック関数、パラメータ固定）
    *   $\text{decay}_A = \exp(-\lambda_A \cdot \text{holding\_time})$: 時間減衰（$\lambda_A$: 戦略A固有、数秒スケール）
*   **戦略A固有の交互作用項**: $\text{depth\_drop} \times \text{vol\_spike}$, $\text{spread\_z} \times \text{OBI}$
*   **Q値計算**: §3.0.1の統一定義に従い $Q_{adjusted}(s, a) = w_a^T \phi_A(s) - \text{self\_impact} - \text{dynamic\_cost} - k \cdot \sigma_{non\_model}$ で計算。行動選択はThompson Samplingの $\tilde{Q}_{final}$ で行う。

### 3.2 戦略B：Volatility Decay Momentum — 特徴量パイプライン φ_B(s)
戦略Bは、ボラティリティ減衰に乗るモメンタムを狙う数分ホライズンの戦略です。
*   **状態定義**: $s_t = (\text{rv\_spike}, \text{trend}, \text{OFI}, \text{intensity}, \Delta t, \text{queue\_position}, p_t)$
*   **戦略B固有の非線形特徴量（既知関数、学習なし、φ_B(s) の構成要素）**:
    *   $P(\text{continue}_t) = \sigma(\dots)$: トレンド継続確率
    *   $\text{decay}_B = \exp(-\lambda_B \cdot \Delta t)$: 時間減衰（$\lambda_B$: 戦略B固有、数分スケール。$\lambda_B \ll \lambda_A$）
*   **戦略B固有の交互作用項**: $\text{rv\_spike} \times \text{trend}$, $\text{OFI} \times \text{intensity}$
*   **Q値計算**: §3.0.1の統一定義に従い $Q_{adjusted}(s, a) = w_a^T \phi_B(s) - \text{self\_impact} - \text{dynamic\_cost} - k \cdot \sigma_{non\_model}$ で計算。

### 3.3 戦略C：Session Structural Bias — 特徴量パイプライン φ_C(s)
戦略Cは、セッション構造バイアスを利用する戦略です。
*   **状態定義**: $s_t = (\text{session}, \text{range\_break}, \text{liquidity\_resiliency}, \text{queue\_position}, p_t)$
*   **戦略C固有の非線形特徴量（既知関数、学習なし、φ_C(s) の構成要素）**:
    *   $P(\text{trend}) = \text{Adaptive\_Estimate}(\text{last\_N\_days}, \text{decay\_weighting})$: セッション別トレンド確率
    *   $\text{decay}_C = \exp(-\lambda_C \cdot \Delta t)$: 時間減衰（$\lambda_C$: 戦略C固有）
*   **戦略C固有の交互作用項**: $\text{session} \times \text{OBI}$, $\text{range\_break} \times \text{liquidity\_resiliency}$
*   **Q値計算**: §3.0.1の統一定義に従い $Q_{adjusted}(s, a) = w_a^T \phi_C(s) - \text{self\_impact} - \text{dynamic\_cost} - k \cdot \sigma_{non\_model}$ で計算。

### 3.4 特徴量設計（Feature Vector $\phi(s)$）の完全定義
1.  **マイクロ構造**: `spread`, `spread_zscore`, `OBI`, `ΔOBI`, `depth_change_rate`, `queue_position`
2.  **ボラティリティ**: `realized_volatility`, `volatility_ratio`, `volatility_decay_rate`
3.  **時間系**: `session (one-hot)`, `time_since_open`, `time_since_last_spike`, `holding_time`
4.  **ポジション状態**: `position_size`, `position_direction`, `position_hold_time`, `position_entry_price`, `position_pnl_unrealized` ※`position_pnl_unrealized`のmid-price計算には、特徴量抽出時点と同一の瞬時mid-priceを使用。バー終値（close）を使用すると、バー内の将来価格情報が混入するため情報リークとなる（§1-6の実行系データと同様に強制ラグの対象）。
5.  **オーダーフロー/エグゼキューション系**: `trade_intensity`, `signed_volume`, `recent_fill_rate (lagged)`, `recent_slippage (lagged)`, `recent_reject_rate (lagged)`, `execution_drift_trend (lagged)` ※実行系データは**必ず一定のラグ（例：`older than Δt`）をかけて取得**。
6.  **非線形変換項（既知関数、学習対象外）**: `self_impact = f(pos, liq)`, `time_decay = exp(-λ·t)`, `dynamic_cost = spread_half + commission + slippage`, `P(revert)`, `P(continue)`, `P(trend)` ※パラメータは固定または物理的導出。
7.  **交互作用項（手動設計、線形モデルで自動学習不可能）**:
    *   `spread_z × vol`: スプレッド拡大時のボラティリティ非線形効果
    *   `OBI × session`: セッション別オーダーブック非対称性
    *   `depth_drop × vol_spike`: 流動性ショックの強度指標
    *   `position_size × vol`: ポジションリスクの非線形増大
    *   `OBI × vol`: オーダーフローインバランスのボラティリティ依存
    *   `spread_z × self_impact`: スプレッド拡大時の自己インパクト増幅

---

## 4. MDPポリシーとExecutionの従属・統合関係（情報リーク完全排除）

### 4.1 MDP最適化決定関数（統一Q̃・完全階層リミット・OTC約定・ポジション制約付き）
Projection lag、実行の非対称性、情報リークを完全に排除し、OTC市場の現実を組み込んだ決定関数です。行動選択・検証・記録はすべて $\tilde{Q}_{final}$ で統一し、σ_modelの処理はThompson Samplingに一元化します。

```text
// ======== 前提計算（全戦略共通） ========

// 1. 非線形自己Impact算出 (時系列依存)
self_impact_t = f(position_t, liquidity_t)
liquidity_t_plus1 = g(self_impact_t)

// 2. 特徴量取得とHDP-HMM Regime推定 (動的Regime数)
X_lagged = FeatureExtractor(MarketEvent, lag=Δt) // 情報リーク排除のため強制ラグ
regime_posterior = RegimeCache.latest_posterior()
regime_entropy = H(regime_posterior)

// 未知Regime検出: エントロピーが閾値超 → 全戦略hold強制
if regime_entropy > UNKNOWN_REGIME_THRESHOLD:
    issue TradeSkipEvent(reason: "unknown_regime")
    halt until regime stabilizes

drift_t = Σ_k (regime_posterior_k * f_k(drift_{t-1}, X_lagged))

// 3. Dynamic_Cost明示計算 (スプレッド・手数料・slippage)
dynamic_cost = spread_half(s_t) + commission_per_lot
             + rolling_mean_slippage(lagged)
             + funding_cost(holding_time)

// 4. 実行環境不確実性（σ_modelを含まない、§3.0.1）
σ_non_model = sqrt(σ_execution² + σ_latency²)

// 5. 方向付きレイテンシペナルティ算出
latency_mean_cost = mean(latency_ms * volatility)
latency_tail_risk = TailRiskFunction(latency_distribution, strategy_direction)
latency_penalty = f(mean_cost, tail_risk, strategy_direction)

// 6. 事後ペナルティ係数
k = Dynamic_K_Calculator(volatility, regime_stability)

// ======== 階層的ハードリミットチェック（Q値判定より優先） ========

// 6a. 月次ハードリミット（§9.4.2: 最優先）
if monthly_realized_pnl < -MAX_MONTHLY_LOSS:
    kill_all_positions()
    halt_trading_until_review()  // オペレーター承認まで再開不可
    return

// 6b. 週次ハードリミット（§9.4.1）
if weekly_realized_pnl < -MAX_WEEKLY_LOSS:
    kill_all_positions()
    halt_trading_until_next_week()  // 翌週月曜までhalt
    return

// 6c. 日次二段階ハードリミット（§9.4）
// 第一段階: MTM警戒水準
if mark_to_market_pnl < -MAX_DAILY_LOSS_MTM:
    reduce_lot_limit_to(25%)
    restrict_entry_to(Q_threshold)
    issue WarningEvent(reason: "mtm_alert")
    // 取引は継続（制限付き）

// 第二段階: 実現損益ハードストップ
if realized_pnl < -MAX_DAILY_LOSS_REALIZED:
    kill_all_positions()
    halt_trading_until_next_day()
    return

// ======== Thompson Samplingによる行動選択（唯一のQ̃計算） ========

// グローバルポジション制約を満たす行動のみを候補とする
A_valid = {}
for each action a in {buy_k, sell_k, hold}:
    next_position_global = Σ_i position_strategy_i + delta(a)
    if |next_position_global| > P_max_global: continue
    if |delta(a)| > V_max: continue
    A_valid.add(a)

// Thompson Sampling: σ_modelの唯一の反映経路（§3.0.1）
w̃ ~ N(ŵ, Σ̂)  // ベイズ事後分布からサンプリング（σ_modelはここに含まれる）

for a in A_valid:
    // Q̃_final: 行動選択に使用する唯一の値。全ペナルティを含む。
    Q̃_final(s, a) = w̃_a^T φ(s_t + drift_t) - self_impact_t
                   - dynamic_cost - k * σ_non_model - latency_penalty(a)

// 最適行動を選択
a* = argmax_{a ∈ A_valid} Q̃_final(s, a)

// 行動間整合性チェック（buy/sellの反対称性）
// Q̃_final(s, buy) と Q̃_final(s, sell) がともに顕著に正の場合、
// サンプリングの歪みの可能性 → より大きい方のみを残す
if Q̃_final(s, buy_k) > 0 and Q̃_final(s, sell_k) > 0:
    if |Q̃_final(s, buy_k) - Q̃_final(s, sell_k)| < CONSISTENCY_THRESHOLD:
        // buy/sellの差が微小 → 不確実性が高い → holdにフォールバック
        a* = hold

// ======== 検証と発行 ========

// 点推定（監視・記録用。行動選択には使用しない）
Q_point(s, a*) = ŵ_a*^T φ(s_t + drift_t) - self_impact_t
               - dynamic_cost - k * σ_non_model - latency_penalty(a*)

// 検証はQ̃_finalで（行動選択に使った値と同一）
match validate_order(a*, Q̃_final(s, a*), state):
    Ok(()) => issue PolicyCommand(payload: {
        a*,
        Q_tilde_final: Q̃_final(s, a*),  // 行動選択に使用した値
        Q_point: Q_point(s, a*),          // 点推定（監視用）
        thompson_sample_std: sqrt(diag(Σ̂)) // Thompson Samplingの分散
    })
    Err(e) => issue TradeSkipEvent(reason: e)
```

### 4.2 Execution（OTC市場対応モデル）
FXは取引所ではなくOTC市場。ECN/STPモデルにおいても、Last-look拒否・Internalization・価格改善/悪化は構造的に不可避です。取引所の板モデル（queue position）をそのまま適用することは不可能です。

*   **Last-Look拒否モデル**:
    LPは約定前に15〜200msの観察窓を持ち、価格が不利に動いた場合に拒否できます。
    $P(\text{fill\_effective}) = P(\text{fill\_requested}) \times P(\text{not\_rejected} \mid \text{last\_look})$
    $P(\text{not\_rejected}) = \sigma(-\beta_1 \cdot |\Delta p_{\text{during\_lastlook}}| - \beta_2 \cdot \text{LP\_inventory\_proxy})$
    *   $\beta_1, \beta_2$: 過去のfill/reject履歴（lagged）からオンライン推定
    *   last-look窓の長さはLPごとに異なる（設定値として管理）
*   **Effective Fill確率（OTC統合モデル）**:
    $P(\text{fill\_effective}) = P(\text{fill\_requested} \mid \text{depth, flow}) \times P(\text{not\_rejected} \mid \text{last\_look, LP\_state}) + \epsilon_{hidden}$
    *   $\epsilon_{hidden}$: 観測不可能な流動性（internalization等）による不確実性
    *   **$\epsilon_{hidden}$の分布**: Student's t分布（自由度3-5）。ガウスは不適切（iceberg ordersの離散性・テールの厚さを捕捉できない）
    *   $\sigma_{hidden}$: fill予測偏差からオンライン推定
*   **価格改善/悪化モデル**:
    $\text{fill\_price} = \text{requested\_price} + \text{slippage}$
    $\text{slippage} \sim f(\text{direction, size, volatility, LP\_state})$
*   **LP行動適応リスク（Adversarial Adaptation）**:
    LPはこちらの注文パターンを学習する可能性がある。対策:
    *   注文パターンの定期変更（時間窓のランダム化）
    *   複数LPへの分散発注
    *   LP別fill率・reject率・slippageの監視
    *   異常検知: fill率の統計的有意な低下 → adversarial signal → 自動LP切り替え
*   **条件付き期待値（非対称性の解消）**:
    $\text{Expected\_Profit} = P(\text{fill\_effective}) \cdot E[\text{profit} \mid \text{fill}] + (1 - P(\text{fill\_effective})) \cdot \text{Opportunity\_Cost}$
*   **Passive/Aggressive判定**:
    *   Expected_Profit が正で fill_effective が高い: **Passive（Limit Order）**。
    *   Expected_Profit が高いが fill_effective が低い: **Aggressive（Market Order）**。
    *   Expected_Profit が負: **No Trade**。

---

## 5. 過学習を潰す統計的検証パイプライン

オンライン推論はベイズ線形回帰（行列積 $w_a^T \phi(s)$）と既知関数（self_impact, decay, dynamic_cost等）の評価のみ。ニューラルネットワーク推論は行わない。オフライン検証は以下を**全て**パスした戦略のみ実戦候補とします。

1.  **CPCV**: 時系列リーク防止。
2.  **PBO**: `> 0.1` は破棄。
3.  **DSR**: `>= 0.95` を必須。
4.  **複雑度ペナルティ**: `Sharpe / sqrt(num_features)`。
5.  **Sharpe天井**: 年率 `> 1.5` は強制破棄。採用基準は `0.8 〜 1.2`。
6.  **Live Degradation Test**: Out-of-sample時のSharpe低下が30%以内。
7.  **Self-Impactバックテスト**: 非線形なヒストリックを合成し、スケーリング時のQ値崩壊を検証。
8.  **Q関数バックテスト（二段階検証）**: 第一段階: 過去データから特徴量パイプライン $\phi(s)$ を構築し、重み $w$ を推定。第二段階: 推定済み重みをCPCV/PBOのout-of-sample区間で評価し、過学習を検出。$w$ はQ値→ポリシー→取引頻度→Sharpeの全段に影響するため、単純なin-sample最適化は過学習の温床。
9.  **情報リーク検証**: 実行系特徴量に `lag` を設定した状態と、設定しない通常状態でバックテストし、精度の過大評価（擬似リーク）がないかを確認。`position_pnl_unrealized`のmid-price計算方法（瞬時mid vs バーclose）についても同様にリーク有無を検証。
10. **ポリシー堅牢性検証**: Thompson Samplingの事後分布の幅（$\hat{\Sigma}$ のスケール）を摂動させた際の累積報酬の安定性を確認。事後分散の変化に対する報酬の感性地帯が広すぎる場合はポリシーが不安定。
11. **報酬関数感度分析**: $\lambda_{risk}$、$\lambda_{dd}$、$\text{DD}_{cap}$ を摂動させた際のSharpe・最大DD・取引頻度の変化を確認。感性地帯が狭すぎる場合は過学習の兆候。
12. **ハイパーパラメータ空間管理**: リスクパラメータ（$\lambda_{risk}$, $\lambda_{dd}$, $\text{DD}_{cap}$）は戦略間共通。時間スケール依存パラメータ（$\lambda$（時間減衰）, $\gamma$（割引率））は戦略固有（戦略A: 数秒、戦略B/C: 数分の物理的タイムスケールから導出）。構造パラメータ（$k$, $P_{max}$, $V_{max}$, $c$（信頼係数））は原則共通。パラメータは物理的意味（スプレッド幅、LP遅延等）から導出し、ブラックボックス最適化は禁止。
13. **ポジション制約妥当性検証**: $P_{max}$、$V_{max}$ を変動させた際の累積報酬のエッジ感度を確認。制約が厳しすぎると機会損失、緩すぎると破産リスク。

---

## 6. 実務的Event Sourcingアーキテクチャ

### 6.1 ストリームの完全分割
1.  **Market Stream** (市場入力)
2.  **Strategy Stream** (判断プロセス)
3.  **Execution Stream** (注文ライフサイクル)
4.  **State Stream** (集約スナップショット)

### 6.2 イベント順序の絶対保証と冪等性
*   **Sequence ID**: 各ストリーム内の単調増加64bit整数。
*   **Idempotency**: `event_id` と `sequence_id` による重複処理スキップ。

### 6.3 スキーマ進化の管理
*   **Immutable Schema Registry**: Protobufによる中央管理。後方互換性のない変更は禁止。
*   **Upcaster**: 過去イベントを最新スキーマに自動変換。

### 6.4 Event Sourcingの限界の受容（最終確認）
CQRS/Event Sourcingの構造上、**読み取り（Projection）は書き込みより常に遅延する**という物理法則は消せません。本システムはこれを「過去の状態で判断せざる」という前提で設計しています。これを補正するのがHDP-HMM Drift推定やStaleness管理ですが、完全にゼロにすることは不可能です。したがって、$Q_{\text{final}}(s, a) > \text{latency\_penalty}(a)$ という条件式が、この物理法則によるマイナスを論理的に吸収します。

---

## 7. 分散システムの実装防爆メカニズム

### 7.1 Event Loss対策：Gap Detection & Replay Engine（緩和版）
*   **軽微なギャップ**: 連続する1-2ティックの欠損、かつ欠損期間が $\text{tick\_interval\_mean} + 2\sigma$ 以内。Warningログのみ出力し取引継続。欠損期間中の特徴量は最後の確定値でホールドし、Δ系特徴量（ΔOBI等）はゼロで代替。Fat-tail Drift分布が状態遷移の不確実性を吸収するが、特徴量計算誤差を吸収するものではないため、連続3ティック以上の欠損は「深刻」に分類。
*   **深刻なギャップ**: 取引停止し、Event StoreからReplay。

### 7.2 Eventual Consistency対策：Dynamic Risk Barrier
「リスクが高い時に同期して待つと、最も価格変化が速い局面でレイテンシが増大する」という致命的な副作用を防ぎます。
*   待機による同期は**完全に廃止**。
*   代わりに、BarrierはCommandを**常に通す**が、`staleness_ms` を付与。
*   Risk Managerは、`staleness_ms` に応じて動的に最大許容ロット数を引き下げる:
    $\text{lot\_multiplier} = \max\left(0, 1 - \left(\frac{\text{staleness\_ms}}{\text{staleness\_threshold\_ms}}\right)^2\right)$
    *   二次関数: 軽微な遅延ではペナルティ小、深刻な遅延で急激にゼロに収束
    *   `staleness_threshold_ms`: 戦略ホライズン依存（数秒戦略なら100ms）
    *   `staleness_ms > threshold` の場合: lot_multiplier = 0 → 取引停止

### 7.3 ストレージ爆発対策：Tiered Event Strategy
*   **Tier 1 (Critical)**: `OrderSent`, `Fill`, `StateSnapshot` -> NVMe SSDに永続保存。
*   **Tier 2 (Derived)**: `DecisionEvent`, `PolicyCommand` -> Delta Encoding + 圧縮でアーカイブ。
*   **Tier 3 (Raw)**: `MarketEvent` -> メモリ/高速SSDのみ保持し、TTL期限後に**コールドストレージ（S3/Glacier等）に自動アーカイブ**してから削除。§9.2のオフライン再学習に必要な生データを廃棄しない。

---

## 8. Observability（観測性）と破綻検知の完全設計

### 8.1 イベント構造の最終定義（全パラメータ網羅）
```protobuf
message EventHeader {
    string event_id = 1;
    string parent_event_id = 2;
    string stream_id = 3;
    int64 sequence_id = 4;
    int64 timestamp_ns = 5;
    string schema_version = 6;
    EventTier tier = 7;
}

message DecisionEventPayload {
    string strategy_id = 1;
    repeated float feature_vector_lagged = 2; // 情報リーク排除のため強制lagged
    float model_output_prob = 3;
    // --- MDP Q-function (Thompson Sampling) ---
    repeated float q_tilde_final_values = 4;  // 各行動のQ̃_final(s, a) — 行動選択に使用した値
    int32 selected_action = 5;                // a* = argmax Q̃_final(s,a)
    float thompson_posterior_std = 6;          // 事後分布の標準偏差（Thompson Sampling）
    float regime_entropy = 7;                 // HDP-HMM事後確率エントロピー
    // --- Q点推定（監視用） ---
    float q_point_selected = 34;              // 選択行動のQ_point（点推定、監視・記録用）
    float q_tilde_selected = 35;              // 選択行動のQ̃_final（行動選択値の記録）
    // --- 報酬分解（戦略分離型） ---
    float reward_pnl = 8;                     // 戦略固有PnL成分
    float reward_risk_penalty = 9;            // λ_risk * σ²_i（戦略固有分散）
    float reward_drawdown_penalty = 10;       // λ_dd * DD_i（戦略固有DD）
    float reward_total = 11;                  // r_t^i = PnL_i - risk_i - DD_i
    // --- 不確実性（σ_modelはThompson Sampling内のみ、点推定側はσ_non_model） ---
    float sigma_model = 12;                   // 事後分散（Thompson Sampling用、行動選択の不確実性源）
    float sigma_execution = 13;               // 実行環境の分散 (laggedデータから算出)
    float sigma_latency = 14;                 // レイテンシの分散
    float sigma_non_model = 15;               // sqrt(σ_execution² + σ_latency²) — 点推定側の不確実性
    float dynamic_k = 16;
    // --- ポジション状態 ---
    float position_before = 17;            // 行動前ポジション
    float position_after = 18;             // 行動後ポジション（制約チェック済み）
    float position_max_limit = 19;         // P_max
    float velocity_limit = 20;             // V_max
    // --- Impact & Drift ---
    float self_impact_nonlinear = 21;      // 時系列依存Impact
    float liquidity_evolvement = 22;       // 流動性変化
    repeated float regime_posterior = 23;  // Regime Switching事後確率
    // --- Latency ---
    float latency_mean_cost = 24;          // 平均レイテンシコスト
    float latency_tail_risk = 25;          // Fat-tailリスク
    float dynamic_cost = 26;               // 状態依存コスト
    // --- Execution統合 ---
    float fill_probability = 27;           // キューポジション依存Fill確率
    float hidden_liquidity_noise = 28;     // 隠薄流動性ノイズ ε_hidden
    float opportunity_cost = 29;           // 未約定時の機会損失
    float expected_profit = 30;            // 非対称期待利益
    float time_decay_lambda = 31;          // 時間減衰パラメータ
    float e_win_state = 32;
    float e_loss_state = 33;
}

message ExecutionEventPayload {
    float expected_fill_price = 1;
    float actual_fill_price = 2;
    float slippage = 3;
    float estimated_fill_prob = 4;
    string reject_reason = 5;
    float execution_drift_trend = 6;
    float hidden_liquidity_sigma = 7;      // 隠薄流動性ノイズの推定σ (Student's t)
    float fill_prediction_error = 8;       // 予測fill確率と実際の偏差（σ_hidden推定用）
    float last_look_rejection_prob = 9;    // P(not_rejected | last_look) 推定値
    float lp_id = 10;                      // LP識別子
    float lp_fill_rate_rolling = 11;       // LP別直近fill率
}

message StateSnapshotPayload {
    bytes state_hash = 1;
    double position = 2;
    double cash = 3;
    int64 applied_sequence_id = 4;
    double position_global = 5;            // 全戦略合計ポジション
    double p_max_global = 6;               // グローバルポジション制約
    double daily_pnl = 7;                  // 日次累積損益
    double max_daily_loss = 8;             // 日次ハードリミット閾値
    double daily_mtm_pnl = 9;              // 日次MTM損益（第一段階監視用）
    double max_daily_loss_mtm = 10;        // MTM警戒水準閾値
    double weekly_pnl = 11;               // 週次累積損益
    double max_weekly_loss = 12;           // 週次ハードリミット閾値
    double monthly_pnl = 13;             // 月次累積損益
    double max_monthly_loss = 14;          // 月次ハードリミット閾値
    bool lp_recalibration_active = 15;    // LP再校正プロトコル稼働中フラグ
    int32 lp_recalibration_samples = 16;  // LP再校正蓄積サンプル数
}
```

### 8.2 Pre-Failure Signature（予兆ログ）
*   `rolling_variance_latency`
*   `feature_distribution_kl_divergence`
*   `q_value_adjustment_frequency` (高すぎるのはモデル崩壊の兆候)
*   `execution_drift_trend`
*   `latency_risk_trend`
*   `self_impact_ratio` (Q値に対するインパクト割合が急増したらスケーリング限界の兆候)
*   `liquidity_evolvement` (流動性が急激に変化した場合はshock戦略から自動離脱するトリガー)
*   `policy_entropy` (ポリシーの確率分布のエントロピー。高すぎるとランダム决策、低すぎると過信)
*   `regime_posterior_entropy` (Regime Switchingの事後確率エントロピー。高いとregime遷移中＝高リスク)
*   `hidden_liquidity_sigma` (隠薄流動性ノイズの推定分散。急増時は板情報の信頼性低下)
*   `position_constraint_saturation_rate` (ポジション制約飽和頻度。高すぎる場合はP_maxの再検討が必要)
*   `last_look_rejection_rate` (LP別last-look拒否率。構造的上昇はadversarial signal)
*   `dynamic_cost_estimate_error` (Dynamic_Cost推定値と実績の偏差。エッジ消滅の直接的指標)
*   `lp_adversarial_score` (LP別fill率低下+reject率上昇+slippage増大の統合スコア)
*   `daily_pnl_vs_limit` (日次損失のハードリミットに対する比率。80%到達でearly warning)
*   `weekly_pnl_vs_limit` (週次損失のリミットに対する比率)
*   `monthly_pnl_vs_limit` (月次損失のリミットに対する比率)
*   `lp_recalibration_progress` (LP切り替え後の再校正進捗。N件中M件蓄積)
*   `bayesian_posterior_drift` (事後分布の経時変化。$\|w_t - w_{t-100}\|$ が急増時はモデル崩壊の兆候)

---

## 9. リスク管理と異常検知（生存戦略）

### 9.1 ハードウェア級 Kill Switch
ティック到着間隔の `平均 ± 3σ` 逸脱時、**10〜50ms以内に発注マスク**。

### 9.2 Online Change Point Detection
特徴量分布の変化をADWIN等で検知し、オフライン再学習トリガー。

### 9.3 Lifecycle Manager (自動淘汰)
Rolling SharpeやRegime別PnLが「死の閾値」を下回った場合、新規エントリーをハードブロック。既存ポジションのクローズ機構も同時に動作し、オープンポジションが残存しないよう保証する。

### 9.4 日次最大損失リミット（二段階ハードストップ）
Rolling SharpeやRegime別PnLは遅行指標であり、1日で資金の大半を失う急変相場には間に合わない。Q値やポリシーの判断を待たずに発動する。未実現損益のフラッシュスパイクによる誤発動を防ぐため、評価損益（MTM）と実現損益の二段階設計とする。
*   **第一段階（MTM警戒水準）**: `mark_to_market_pnl < -MAX_DAILY_LOSS_MTM`（例: equity -3%）の場合:
    1.  全戦略の許容ロット数を25%に制限
    2.  新規エントリーはQ値が閾値以上のもののみ許可
    3.  オペレーターに警告通知
*   **第二段階（実現損益ハードストップ）**: `realized_pnl < -MAX_DAILY_LOSS_REALIZED`（例: equity -2%）の場合:
    1.  全ポジションを即時クローズ（Market Order）
    2.  当日中の新規エントリーを全面的に禁止
    3.  翌営業日までシステムをhalt
    4.  オペレーターへの即時アラート
*   **設計意図**: MTMはフラッシュスパイクで瞬間的に極端な値を取るため、これによる強制クローズは不要な実損確定の原因となる。実現損益は確定値であり、本物の損失を正確に反映する。二段階により、MTM急変時はリスク縮小しつつ、実損が閾値に達した場合のみ完全停止する。
*   **MAX_DAILY_LOSS_MTM**: equityの2〜3%を推奨。バックテストでの最大DDの75%以下。
*   **MAX_DAILY_LOSS_REALIZED**: equityの1〜2%を推奨。バックテストでの最大DDの50%以下。
*   **このリミットの絶対優位**: Q値が正であっても、実現損失がリミットに達した場合は強制停止。

### 9.4.1 週次損失リミット
*   **週次ハードリミット**: `weekly_realized_pnl < -MAX_WEEKLY_LOSS`（例: equity -3%）の場合:
    1.  全ポジションクローズ
    2.  翌週月曜までシステムhalt
    3.  オペレーターの承認なしに再開不可

### 9.4.2 月次損失リミット
*   **月次ハードリミット**: `monthly_realized_pnl < -MAX_MONTHLY_LOSS`（例: equity -5%）の場合:
    1.  全ポジションクローズ
    2.  月内の再開を全面的に禁止
    3.  オペレーターによる強制的な事後レビュー必須（パラメータ再校正、モデル再評価、LP健全性確認）
    4.  レビュー完了後、翌月1日またはオペレーター承認日のいずれか遅い方から再開

### 9.5 戦略間ポジション統合管理
戦略A/B/Cが同時にシグナルを出した場合のポジション集中リスクを管理します。
*   **グローバルポジション制約**:
    $\left|\sum_{i \in \text{strategies}} p_i\right| \leq P_{max}^{global}$
*   **相関調整**:
    $P_{max}^{global} = \frac{\sum_i P_{max}^i}{\max(\text{correlation\_factor}, \text{FLOOR\_CORRELATION})}$
    *   correlation_factor: 直近N日の戦略間ポジション相関から推定
    *   全戦略が同方向: factor ≈ 3 → 実質的に単一戦略として扱う
    *   無相関: factor ≈ 1 → 各戦略の独立制約をそのまま適用
    *   **FLOOR_CORRELATION**: ストレスイベント時は過去N日の平穏時データが実態を反映しないため、下限値を設定（推奨: 1.5-2.0）。これにより、平穏時に推定された低いcorrelation_factorによる過大なポジション許容量を防止。
*   **戦略間優先度**: Q値が最も高い戦略を優先し、下位戦略はロット削減。

### 9.6 カウンターパーティリスク（OTC市場特有）とLP切り替えプロトコル
FXはOTC市場であり、ブローカー/LPの破綻・出金拒否・約定拒否の系統的増加に対する設計が必須。
*   **LP健全性監視**: LP別のfill率・reject率・slippage・出金遅延を監視
*   **分散配置**: 単一LPへの依存度を下限以下に維持（例: 単一LPの取引シェア ≤ 50%）
*   **緊急切り替え**: LPのfill率が統計的に有意に低下した場合、自動的にバックアップLPに切り替え
*   **ブローカー破綻シナリオ**: 取引停止→全ポジションの代替LPでの強制クローズ→出金リクエスト
*   **LP切り替え時の再校正プロトコル**:
    1.  LP切り替え直後、全execution parameter（last-look窓長、slippage分布、fill率）が不正確になる
    2.  **安全モード移行**: 切り替え直後は、ロット数上限を通常の25%に制限
    3.  **パラメータ再校正**: 新LPでの取引データがN件（例: 200件）蓄積されるまで、以下を実施:
        *   $\beta_1, \beta_2$（last-look拒否モデル）の再推定
        *   slippage分布の再推定
        *   fill率の再推定
        *   $\sigma_{execution}$の暫定値を通常の2倍に設定
    4.  **校正完了判定**: 各パラメータの推定誤差が閾値以下になった時点で安全モード解除
    5.  **Thompson Samplingとの協調**: 校正期間中は$\sigma_{model}$が増大するため、サンプリングの分散が大きくなり、自然に保守的な行動を選択する

---

## 10. 実装防爆メカニズム（防御壁）

### 10.1 厳格なステートマシン
State Managerはイベントを順次適用するステートマシン。不正遷移はコンパイルエラー。

### 10.2 不変条件の検証（release build安全）
`debug_assert!` はRustの `--release` ビルドでコンパイラにより完全除去されるため、**本番環境では一切使用しない**。すべての不変条件チェックは明示的なエラーハンドリングで実装する。

```rust
fn validate_order(order: &Order, q_tilde_final: f64, state: &State) -> Result<(), RiskError> {
    // Thompson SamplingのQ̃_finalで検証（行動選択に使用した値と同一）
    if q_tilde_final <= order.directional_penalty {
        return Err(RiskError::NegativeEdge {
            q_tilde: q_tilde_final, penalty: order.directional_penalty
        });
    }
    if order.expected_profit <= 0.0 {
        return Err(RiskError::NegativeExpectedProfit);
    }
    if order.position_size > state.p_max {
        return Err(RiskError::PositionLimitBreached {
            size: order.position_size, limit: state.p_max
        });
    }
    if order.velocity > state.v_max {
        return Err(RiskError::VelocityLimitBreached);
    }
    // グローバルポジション制約（§9.5）— Q計算後〜発注間の別戦略ポジション変更を再検証
    let next_global = state.position_global + order.delta;
    if next_global.abs() > state.p_max_global {
        return Err(RiskError::GlobalPositionBreached {
            current: state.position_global,
            delta: order.delta,
            limit: state.p_max_global,
        });
    }
    // 週次ハードリミット（§9.4.1）
    if state.weekly_realized_pnl < -state.max_weekly_loss {
        return Err(RiskError::WeeklyLimitBreached {
            pnl: state.weekly_realized_pnl, limit: -state.max_weekly_loss
        });
    }
    // 月次ハードリミット（§9.4.2）
    if state.monthly_realized_pnl < -state.max_monthly_loss {
        return Err(RiskError::MonthlyLimitBreached {
            pnl: state.monthly_realized_pnl, limit: -state.max_monthly_loss
        });
    }
    if state.hash != state.expected_hash {
        return Err(RiskError::StateCorruption);
    }
    Ok(())
}
// 注意: debug_assert! はrelease buildで除去されるため使用禁止
// 全てのリスクチェックは Result<T, RiskError> で処理し、
// Err の場合は OrderCommand を発行せず TradeSkipEvent を出力する
// 検証はQ̃_final（Thompson Samplingの行動選択値）で行う。
// Q_point（点推定）は監視・記録用であり、検証には使用しない。
```

### 10.3 準決定論的リプレイ
Tier 1 & 2イベントを読み込ませれば、**意思決定ロジック**は過去と同じ結果を出すことを保証。ただし、Market Orderの約定価格はその瞬間の板に依存するため、Tier 3 MarketEventがTTL削除済みの場合は完全な再現は不可能。リプレイは「なぜその決定をしたか」の検証に用い、「同じPnLが再現できるか」の検証には用いない。

---

## 11. インフラストラクチャとストレージ要件

| コンポーネント | 技術仕様 | 理由 |
| :--- | :--- | :--- |
| **実行言語** | Rust (強く推奨) | 所有権システムによるメモリリ安全性、データ競合の排除。 |
| **スキーマ定義** | Protobuf | Schema Evolution管理、零コピーパース。 |
| **データ受信** | WebSocket / FIX | REST APIはオーバーヘッドが致命的のため禁止。 |
| **発注プロトコル** | FIX 4.4/5.0 | 低遅延実行の標準。 |
| **サーバー配置** | LD4 / NY4 コロケーション | 物理距離最小化。 |
| **ネットワーク** | 1Gbps以上, <1ms | 情報優位の劣化防止。 |
| **ストレージ** | NVMe SSD (Tier1,2) + 大容量(Tier3) | Criticalは永続化。Rawは帯域・容量確保。 |

---

## 12. 運用指針と設計の限界（究極の現実）

### 想定内の破綻ケース
*   **連続負けの自己相関**: 連続してマイナスになった場合、マルコフ構造により $Q(s_t, a)$ は急激に低下し、ポリシーが自動的にholdを選択する。これが「機械的かつ客観的に負け続ける」仕組みとして正しく機能する。
*   **非線形スケーリング時の崩壊**: ロットを増やすと $\text{Impact} \propto |\text{position}|^\alpha$ ($\alpha > 1$) により、Q値が指数関数的に悪化し強制ロット削減に繋がる。グローバルポジション制約 $P_{max}^{global}$ が資金の爆発を防ぐ。
*   **時間減衰パラメータのズレ**: マクロ経済指標発表直後など、ボラティリティが急激に変化すると、過去の最適な $\lambda$ ではQ値がマイナスになりエントリーできなくなる。Online Change Point Detectionがこれを検知し、事後分布の不確実性が増大することでThompson Samplingのサンプルが多様化し、自然に探索重視に切り替わる。
*   **HDP-HMM Regime検出遅延**: 動的Regime管理（HDP-HMM）であっても、regime遷移の検出には遅延が生じる。未知Regimeのエントロピーが閾値に達する前に誤ったRegime分類で取引するリスクは不可避。`regime_posterior_entropy` の監視で早期検知する。
*   **報酬関数のミスキャリブレーション**: $\lambda_{risk}$、$\lambda_{dd}$、$\text{DD}_{cap}$ の設定不適合により、リスク回避しすぎて機会を逸する、またはリスクを見積もらずに過剰ポジションを取る。定期的なOut-of-sampleでの感度分析が必須。
*   **ポジション制約の飽和**: $|p_t| = P_{max}^{global}$ に到達すると、Q値が正でも追加エントリー不可。一方通行の相場では機会損失。これは「破産しない」ための正しい代償。
*   **LPのAdversarial Adaptation**: LPがこちらの注文パターンを学習し、last-look拒否率を上昇させる、見せ板でトリガーを誘発する等の対抗行動を取る。LP別fill率監視と分散発注で緩和するが、完全防止は不可能。
*   **DD自己凍結からの回復**: DD項が $\text{DD}_{cap}$ に到達した状態は、PnL成分による回復取引が可能だが、同時に§9.4のハードリミットも監視されている。DD回復にはオペレーターの判断が必要な場合がある。
*   **Last-look拒否率の構造的変化**: LPの拒否率が急激に変化した場合、fill予測モデルのラグによりExpected_Profitが過大評価される。fill_prediction_errorの監視で検知し、モデル再推定までPassive戦略を停止する。
*   **Hold退化リスク**: 初期段階でholdが支配的になると、buy/sellのデータが蓄積せず、On-policy学習が停滞する。§3.0.3の楽観的初期化と最小取引頻度監視で緩和するが、完全防止は保証できない。取引頻度が設計想定の50%を下回った場合はオペレーターによる介入（パラメータ調整・手動探索期間の設定等）が必要。
*   **固定パラメータ特徴量の静的劣化**: P(revert), P(continue), P(trend)等の固定パラメータロジスティック関数が、環境変化に伴い系統的に偏る可能性。線形重み $w_a$ が平均的な偏りは補正できるが、状態依存的な偏り（特定regimeでのみ不正確等）は線形範囲を超える。§8.2のfeature_distribution_kl_divergence監視で検知し、閾値超過時は該当特徴量の重みを縮小または除外。
*   **Thompson Samplingの行動間非対称性**: $w_{buy}$と$w_{sell}$を独立にサンプルするため、buy/sellが同時に高いQ値を持つ非合理的状況が発生し得る。§4.1の整合性チェックで緩和するが、構造的限界として受容する。

### 構造的限界（受容すべき前提）
*   **マルコフ仮定の限界**: FXの機関フローは数時間〜数日のシリアル相関を持ち、中央銀行の介入は数週間に影響する。有限次元の状態ベクトルで完全に表現することは不可能。長期記憶特徴量（exponential moving average等）で緩和するが、根本的な限界は受容する。
*   **エッジの物理的限界**: +0.05〜0.2 pipのエッジは、Dynamic_Cost（スプレッド半分+手数料+slippage推定）の推定誤差と同程度。コスト推定が不正確ならエッジは消滅する。利益計算と同等の精度でコスト推定を行う必要がある。
*   **OTC市場のブラックボックス性**: Last-look窓の正確な長さ、LPの在庫状態、internalizationの実態は観測不能。これらをモデル化しても残留不確実性は不可避。$\sigma_{execution}$ と $\sigma_{hidden}$ で吸収する設計が正しい。
*   **ベイズ事後分散の非定常限界**: 適応ノイズ分散（$\hat{\sigma}^2_{noise,t}$）を導入しても、事後平均 $\hat{w}$ は過去全データの加重平均として更新されるため、急激なregime変化への追従には遅延が生じる。Changepoint Detection（§9.2）による事後の部分的リセットと組み合わせる必要がある。

### 最終的な勝敗の分岐点
システムが完成した後、勝敗を分けるのは以下の5点のみです。
1.  **Q関数推定の現実性**: ベイズ線形回帰（On-policy + Monte Carlo）が、真の報酬構造をどれだけ近似できているか。Thompson Samplingのサンプリング分散が、不確実性を適切に探索に変換できているか。事後分散 $\sigma_{model}$ がエッジを上回る場合の自動抑制が正しく機能するかが命。
2.  **OTC市場の約定現実**: Last-look拒否・Internalizationを正しくモデル化できているか。バックテストのfill仮定と実戦のfill現実の乖離が、設計上最大のリスク。
3.  **ハードリミットの冷徹な発動**: 日次・週次・月次の階層的損失リミット（§9.4-9.4.2）が、Q値が正でも迷わず発動できるか。オペレーターが「自動淘汰が効く」と信じて放置しないか。
4.  **情報リーク排除の厳格な実装**: 実行系特徴量とposition_pnl_unrealizedに**必ず強制ラグをかける**ことを徹底させるか。このルールを破ると、バックテストで高精度なのに実戦で壊れるという最悪のパターンに陥る。
5.  **学習の起動と維持**: On-policy学習がhold退化に陥らず、buy/sellのデータが継続的に蓄積されるか。§3.0.3の防止機構が、実環境で十分に機能するかの検証が初期運用の最重要課題。

**【最終結論】**
本ドキュメントは、v14時点の致命的構造問題（σ_model三重カウント、Q̃/Q_final乖離、エピソード未定義、報酬の戦略間結合、週次/月次リミット未実装、事後分散の非定常性）を完全に解消した改訂版です。**統一不確実性処理**（Thompson Samplingがσ_modelの唯一の反映経路、事後ペナルティはσ_non_modelのみ）、**エピソード定義付きOn-policy Monte Carlo**（最大ホールド時間・hold退化防止機構付き）、**戦略分離型報酬関数**（独立MDPの整合性回復）、**統一Q̃_finalによる行動選択・検証・記録**（乖離の完全排除）、**完全階層的損失リミット**（日次二段階+週次+月次を決定関数・validate_orderに実装）、**グローバルポジション制約**（FLOOR_CORRELATION付き）、**LP切り替え再校正プロトコル**を統合し、数理的にも実装的にも閉じた実戦完結型です。勝負は「数式の美しさ」ではなく、**「release buildで全ての検証が発火するか」**、**「OTC市場の約定現実をモデルに取り込めているか」**、**「ハードリミットが迷わず発動するか」**、**「Thompson Samplingがσ_modelを正しく処理しているか」**という、極めて地味だが致命的なエンジニアリングの精度に委ねられています。
