# コンパクション検証記録

2026-09-26 に CODETAS のローカル合成コンパクションで見つかった不具合と、
その修正・検証の記録です。対象は `crates/codetas-gateway` の
`compaction` 経路で、**OpenAI の native Responses 経路は含みません**。

実機で再現した症状は次の2つでした。

1. 長いセッションで Codex がほぼ毎ターン「コンテキストを自動的に圧縮しました」
   と表示し、最後は `Error running remote compact task: Connection failed`
   で fail した。
2. 圧縮の結果が会話を捉えず、要約が要約依頼そのものを説明していた。

## 1. 原因

独立した3つの不具合が重なっていました。3つ目は検証手順が誘発したもので、
製品の不具合ではありません。

### 1.1 context window が未設定で圧縮閾値が低すぎた

`claude-fable-5-1` と `claude-opus-5-5` は `model_context_windows` を持たずに
追加されていました。名前が `family-major.minor` 形式でないため
`resolve_model_context_window` の継承も効かず、既定の 128,000 が使われ、
カタログの `auto_compact_token_limit` は 115,200 になっていました。

実測した入力は 139,199〜152,819 トークンで、**最初から閾値を超えていました**。
Anthropic は両モデルとも `max_input_tokens: 1000000` を返します。

| 項目 | 修正前 | 修正後 |
|---|---|---|
| `context_window` | 128000 | 1000000 |
| `auto_compact_token_limit` | 115200 | 900000 |

### 1.2 prefix が空になり、元の会話が要約に渡っていなかった

Codex Desktop は `app-context`、skills、プラグイン一覧、developer ターンと
いった固定ブロックを毎回注入します。CODETAS の `sanitize_history_item` は
これらを正しく除去しますが、**実際の会話が `tailTokenLimit`（20,000）に
収まる場合、残るのは短い実タスク1件だけ**になります。

`split_prefix_and_tail` はそれを tail に入れるため prefix が空になり、
要約器には何も渡りませんでした。モデルは仕方なくこう答えていました。

```text
No user task, requirements, or confirmed facts appear in the visible context.
The only instruction present is the request to write this checkpoint summary.
```

固定コンテキストの実測内訳（実セッション、合計約 61,883 字 ≒ 15,470 トークン）:

| ブロック | サイズ | role | 生成元 |
|---|---|---|---|
| `app-context` | 20,202 字 | developer | Codex Desktop |
| `world_state.permissions`（承認済みコマンド接頭辞） | 20,665 字 | — | Codex Desktop |
| `world_state.host_skills`（SKILL.md 一覧） | 10,263 字 | — | Codex Desktop |
| `multi_agent` プロンプト | 2,264 字 | developer | Codex Desktop |
| `recommended_plugins` | 2,231 字 | user | Codex Desktop |
| `turn_context` | 2,258 字 | — | Codex Desktop |
| `base_instructions` | 1,511 字 | — | CODETAS カタログ |
| `multi_agent_mode` | 271 字 | developer | Codex Desktop |

CODETAS の寄与は `base_instructions` の 377 トークン（全体の 0.3%）だけで、
大半は Codex Desktop が生成しています。なお `base_instructions` には
`with a context window of 128000 tokens` という誤った自己認識が焼き込まれて
いました（1.1 の副作用）。

### 1.3 retained が圧縮ごとに累積していた

Codex は毎回のコンパクションで履歴を前回の envelope に**置き換えます**。
`normalize_compaction_history` はその retained を履歴へ戻すため、
`split_prefix_and_tail` が再び tail に入れ、新しいターンが加算されます。

実セッションで計測した累積（`compaction` 1件あたり +5 件）:

| ordinal | 20 | 36 | 52 | 66 | 82 | 98 |
|---|---|---|---|---|---|---|
| retained 件数 | 4 | 9 | 14 | 19 | 24 | 29 |

envelope も 43 KB から 85 KB へ膨張しました。上限は
`MAX_RETAINED_ITEMS = 256` で、到達するとコンパクションは劣化ではなく
**失敗**します。

### 1.4 観測された 400 について

検証中に `provider request failed with HTTP 400` を観測しましたが、
同じ本文を Anthropic API へ直接送ると **429（レート制限）** でした。
修正の欠陥ではなく、短時間に多数の大型リクエストを送った検証手順の副作用です。

同様に `Error running remote compact task: Connection failed` は、
検証スクリプトがアプリを再起動した時刻（12:36:30、スレッド終了は 12:36:41）と
一致します。環境の問題ではありません。

## 2. 修正

| コミット | 内容 |
|---|---|
| `1a1887b` | 両モデルに 1,000,000 の context window を明示し、既存設定を修復する移行を追加（`REGISTRY_REVISION` 16） |
| `fda278c` | prefix が空なら tail を要約対象にする `summarizer_source_items` を追加 |
| `ac5c43a` | 前回の retained を新しい tail から除外し prefix へ回す `split_prefix_and_tail_excluding` / `recover_split_for_offline_excluding` を追加 |
| `e2b6cfe` | プロトコル横断と offline 経路の回帰テスト |

除外は**2つの split 入口の両方**に必要でした。live の要約経路だけでなく、
offline checkpoint 経路（`recover_split_for_offline`）も独自に split を組むため、
片方だけでは DeepSeek のような offline へ落ちる経路で累積が残ります。

## 3. 代替案の検証

prefix が空のときの対処として3案を実 API とモックで実測し、A案を採用しました。

| 観点 | A案（tail を要約） | B案（offline へ委譲） |
|---|---|---|
| 要約品質 | 文脈と重点変更を捉える | 機械的な列挙 |
| 事実の正確性 | 忠実 | 「cooling down」と虚偽の説明を出す |
| 所要時間 | 約 16.6 秒 | 0.003 秒（上流を呼ばない） |
| 採用 | **採用** | 不採用 |

B案が生成した要約の実例:

```text
## User corrections and open disagreements
- none
## Durable observations
- Local compaction continued without an upstream summarizer because
  the selected provider target is cooling down.
- inspected /tmp
- inspected /
```

実際にはクールダウンしておらず、ユーザーの重点変更（「特にUIUXをメインで」）も
捉えていませんでした。A案は同じ入力から次を生成しました。

```text
## User corrections and open disagreements
- 重点の変更がありました。エージェントは当初「表示・SEO・セキュリティ・性能」で
  進めていましたが、ユーザーは「特にUIUXをメインで」と指示しています
```

## 4. 検証結果

### 4.1 累積の解消（実測）

Codex と同じ手順（前回の envelope + 新規ターンで履歴を置換）で駆動しました。

| ラウンド | 修正前 | 修正後 |
|---|---|---|
| 1 | 4 | 4 |
| 2 | 9 | 3 |
| 3 | 14 | 7 |
| 4 | 19 | — |
| 5 | 24 | — |
| 6 | 29 | — |

修正後は増減を繰り返して定常状態に入り、単調増加しません。

### 4.2 他モデルへの影響（重点検証）

修正は `Local` モードの全プロバイダに適用されます。`Responses` ネイティブ
判定に該当する `openai` だけが対象外です。

10 プロバイダを同一の recording mock に通し、3ラウンドの retained 件数を
計測しました。

| プロバイダ | protocol | 結果 |
|---|---|---|
| Anthropic | anthropicMessages | OK `[4, 3, 7]` |
| xAI | chatCompletions | OK `[4, 3, 7]` |
| Kimi | chatCompletions | OK `[4, 3, 7]` |
| DeepSeek | Responses | OK `[4, 3, 7]` |
| Meta | chatCompletions | OK `[4, 3, 7]` |
| OpenCode Go | chatCompletions | OK `[4, 3, 7]` |
| Alibaba Token Plan | chatCompletions | OK `[4, 3, 7]` |
| GitHub Models | chatCompletions | OK `[4, 3, 7]` |
| Command Code | chatCompletions | OK `[4, 3, 7]` |
| Gemini | geminiGenerateContent | 認証経路が必要なため mock 不可。ユニットテストで同一 split を検証 |

`google-antigravity` は Cloud Code Assist の OAuth 経路が必要で mock では
通せませんが、compaction は `compact.rs` の同一呼び出しを通り、
プロトコル分岐は応答変換だけです。Gemini 形状の履歴が同じ有界 split に
なることをユニットテストで確認しています。

**通常リクエスト（非 compaction）への影響はありません。** 変更した関数と
フィールドは compaction 経路からのみ参照されます。

```text
split_prefix_and_tail_excluding -> compact.rs:213, compaction.rs:2137
summarizer_source_items         -> compact.rs:219
previous_retained               -> normalize と上記のみ
```

### 4.3 自動テスト

`cargo test -p codetas-gateway --lib` は **623 passed / 15 failed**。
失敗15件はこの作業の前から存在する既存の失敗で、compaction のテスト58件は
すべて通過します。

追加した回帰テスト（`ac5c43a` の revert 後に残るもの）:

- prefix が空なら要約器が tail を読む
- prefix があれば従来どおり prefix を優先する
- 明示した context window（8,192 / 64,000 / 128,000 / 128,001 / 900,000）を
  migration が上書きしない
- context window が未設定なら補完する


## 6. 独立レビューで判明した除外案の欠陥

`ac5c43a`（前回 retained を tail から除外する案）は独立レビューで差し戻され、
`522ace5` で revert しました。実コードで再現した欠陥は次の4件です。

### P1: 未完了 tool call と新しい result の分断

```text
previous_retained = [user("調査して"), function_call(call_id="c1")]
今回の追加入力      = [function_call_output(call_id="c1", output="結果")]

結果:
  prefix: ["message", "function_call"]      ← call が prefix へ
  tail:   ["function_call_output"]          ← result が孤立
  validate_retained_items(&tail) = Err("orphan tool output")
```

`validate_retained_items` は末尾の未完了 call を合法と認めます（`compaction.rs:1270`）。
pair 検査は除外**前**に走るため（同 `1511`）これを検出できませんでした。

### P2: 最後の質問の保護が取り消される

```text
prev  = [user("選択肢を提示して"), assistant("1: A案、2: B案。どちらにしますか？")]
今回  = [..., user("2")]
結果: tail に残るのは "2" のみ
```

`pin_last_question_in_tail` は `split_prefix_and_tail_raw` から呼ばれるため
（`compaction.rs:1524`）、その後に走る除外が質問を prefix へ移します。offline 専用
ではありませんでした。`.ai/HISTORY_LOSS_AND_LOOPS.md` の再発防止条件に反します。

### P2b: 同文の新規ターンまで除外される

```text
prev  = [user("続けて"), assistant("対応します")]
今回  = [..., user("続けて")]   ← 新しい入力
結果: tail が空になる
```

除外が「前回 envelope から展開した位置」ではなく値の集合への所属判定だったためです。

### その他

- migration が明示 64,000 を 1,000,000 に上書き（`dc01cad` で修正済み）
- offline 経路で、checkpoint が記録しない一般的な tool result が保存先を失う
- 除外後に `retained_turns` / `selection.truncated` が更新されない

## 7. 採用する設計（C′）

レビューの結論は、除外ではなく
**「保存責任を保ったまま、依存関係付きの履歴を有界に選択する」** でした。

> 前回 retained に保存したことと、checkpoint に要約済みであることは別である。

前回 prefix が存在したなら、retained はそもそも前回の要約対象ではありません。
「前回も残した」ことを削除理由にしてはいけません。

### 不変条件

1. 各 raw イベントは、保持・要約対象・archive のいずれかで扱われる
2. 「前回 retained にあった」は削除理由にならない
3. tool result は対応 call なしに replay しない
4. 未完了 call、最新ユーザー文、対象質問は必須保持
5. 保持項目の順序は元履歴の部分列として維持する
6. 件数・トークン・シリアライズ容量を最終形で検証する
7. `retained_turns`、`estimated_tokens`、`truncated` は最終選択から再計算する
8. 保存成功前に旧世代を捨てない

### 論点ごとの方針

| 論点 | 方針 |
|---|---|
| 未完了 call と今回の result | split **前**に履歴全体で対応付ける。未完了 call は原文保持。並列 call（`C(a),C(b),R(a),R(b)`）も扱う |
| offline の未記録 result | 予算内なら retained に原文保存 → 超過なら永続 archive と取得可能な参照 → checkpoint に「要約未実施」を記録。冒頭の文字数クリップを「保存済み」と扱わない |
| 最後の質問 | 選択開始時点で必須保持集合に入れ、原文と順序を維持する。後段の除外・件数調整は退避できない |
| 同文の新規ターン | 内容ではなく履歴イベントの出自で区別する |
| 小さい context window | 由来不明の既存値は変更しない。未設定のみ補完（`dc01cad`） |

### 二重 split の解消

要約に渡す計画と最終 envelope に使う計画を**同一**にします。現状は
`server/compact.rs:213` と `compaction.rs:2135` が別々に split を呼びます。

## 8. 実装した C' の各段階

| コミット | 内容 |
|---|---|
| `522ace5` | `ac5c43a` と `e2b6cfe` を revert し、既知の安全な挙動へ戻す |
| `dc01cad` | migration を欠損のみの補完に変更（段階2） |
| `502676b` | tail を件数でも制限し、質問と回答の順序を復元（段階1・3・4・7） |
| `8a26450` | offline checkpoint に tool result の値を転記（段階5） |
| `55747df` | `retained_turns` を最終 tail から再計算（段階6） |

### 発見した、レビューにも挙がっていなかった欠陥

`split_prefix_and_tail` は tail をトークン予算だけで制限していましたが、
`validate_retained_items` は件数（`MAX_RETAINED_ITEMS = 256`）も検査します。
小さなターンを重ねたセッションではトークン予算内のまま件数上限を超え、
コンパクションが失敗していました。実測:

```text
300ターン（13,071トークン、予算20,000以内）
  tail items = 600
  validate: ERR "compaction retained item count exceeds the limit"
```

件数でも切るようにし、境界は interaction group 単位にして
tool call と result を分離しません。

### offline の情報消失（実測）

```text
lookup の結果 "deployment_id=dep-731 region=ap-northeast-1" の後ろに
小さなターンを40回追加した場合:

  prefix items = 75, tail items = 8
  tool result が tail に残ったか: false
  offline envelope に dep-731: false   ← 情報が消えた
```

offline checkpoint は要約器を呼べず、抽出対象はユーザー文・assistant 文・
ファイル操作だけでした。tool result の値を Durable observations に
転記するようにし、既存 checkpoint へのマージでも引き継ぎます。

## 9. 実機で確認した有界性

Codex と同じ手順（固定コンテキストを再注入し、前回の envelope と
新しいターンで履歴を置換）で 58 ラウンド駆動しました。

| モデル | protocol | 挙動 |
|---|---|---|
| Anthropic | anthropicMessages | 256件で安定 |
| xAI | chatCompletions | 256件で安定 |
| Kimi | chatCompletions | 256件で安定 |
| DeepSeek | Responses | 256件で安定 |

```text
r 50: retained=250 tok=17052
r 56: retained=256 tok=17421
r 58: retained=256 tok=17424   ← 22ラウンド経過しても増えない
```

最終 envelope の内訳（r59）:

```text
generation: 59
checkpoint: 310 字
retained:   256 items (message 154, function_call 51, function_call_output 51)
tool pair:  calls 51 == outputs 51（対応が保たれている）
最新のユーザー発言: 保持されている
```

修正前は上限に達すると `orphan tool output` や件数超過で失敗しました。
現在は上限で頭打ちになり、コンパクションが継続します。

## 10. 残作業

- 未ビルド・未配置（インストール済みは 0.1.1）
- 未 push
- `google-antigravity` の実 API 検証は未実施
- 永続 raw archive は未実装。現状は保持上限（256件 / 20,000トークン）で
  有界にする設計で、上限を超えた古い tool result は offline checkpoint の
  Durable observations に転記される
- 複数 envelope を入力が含む場合の契約は未定義（最新の1つを正本として扱う）
