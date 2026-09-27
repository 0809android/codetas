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

`cargo test -p codetas-gateway --lib` は **623 passed / 15 failed**
（7.5 の置き換え後は **641 passed / 15 failed**、compaction は **75件**）。
失敗15件はこの作業の前から存在する既存の失敗で、compaction のテストは
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

## 7.5 単一パスへの置き換え（2026-09-26）

段階8の C′ 実装は、greedy fill・pin・eviction・件数調整・重複 ID 解決・
orphan 除去という層を重ねる形でした。独立レビューを8回重ねた結果、各層が
後の層の前提を壊すことが分かり、収束しないと判断しました。実際に、ある層の
修正が新しい欠陥を3回生んでいます（最新の指摘は「保持した live call に古い
result が結び付く」「順序違反を削除で隠した結果、完了済み call が未完了として
残る」）。

そこで選択処理を **`select_retained_history` の1パス**に置き換えました。

1. 元履歴を一度だけ走査し、各 index について call と result の対応
   （`partner`）、最後の task user、最後の実質 assistant、未完了 call を求める
2. 必須項目（未完了 call・最新ユーザー文・対象質問）を先に確保する
3. 元履歴の後ろから、対応ペアを1単位として追加する。予算は件数とトークンの
   両方で見る
4. 選択結果を元の順序のまま prefix / tail に振り分ける

削除した層: `pin_last_question_in_tail`、`reorder_tail_to_source_order`、
`evict_oldest_tail_group`、`required_item_count`、`post_pin_tail_items`、
`enforce_tail_budget`、`resolve_duplicate_call_ids`、`bound_recovered_items`、
`recover_oversized_latest_group`、`shrink_*` 系、合成 tool-file 観察の挿入。
短縮・並べ替え・再 pin・事後の重複解決を行わないため、値の同一性を保つ必要が
なくなり、上記2件は構造的に発生しません。

`split_prefix_and_tail` と `recover_split_for_offline` は同じ
`select_retained_history` を呼びます。live 経路と offline 経路で選択が
食い違わなくなります。

### この置き換えで変わった点

| 項目 | 変更前 | 変更後 |
|---|---|---|
| 上限到達時の tail | 251件（予約のマージン） | 256件（上限を使い切る） |
| 必須項目だけで256件を超える場合 | tail を返して validation で失敗 | `mandatory compaction retained item count exceeds the limit` で明示的に失敗 |
| トークン超過の必須メッセージ | `last user message exceeds the retained tail token limit` で失敗 | 原文のまま保持する |
| offline の tool result 転記 | 最後の6件・各400字（先頭のみ） | 件数・文字数の上限なしで保存 |

トークンは選択の目標であり、ハード上限ではないという位置づけに合わせています。

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

| プロバイダ | protocol | 挙動 |
|---|---|---|
| Anthropic | anthropicMessages | 256件で安定 |
| xAI | chatCompletions | 256件で安定 |
| Kimi | chatCompletions | 256件で安定 |
| DeepSeek | Responses | 256件で安定 |
| Meta | chatCompletions | 256件で安定 |
| OpenCode Go | chatCompletions | 256件で安定 |
| Alibaba | chatCompletions | 256件で安定 |
| GitHub Models | chatCompletions | 256件で安定 |
| Command Code | chatCompletions | 256件で安定 |

`google-antigravity` は OAuth セッションが必要でモック不可のため含めていません。
split 呼び出しは共通なのでユニットテストで検証しています。

```text
r 50: retained=250 tok=17052
r 56: retained=256 tok=17422
r 58: retained=256 tok=17424   ← 2ラウンド経過しても増えない
```

修正前は上限に達すると `orphan tool output` や件数超過で失敗しました。
上記の入力では現在も上限で頭打ちになり、コンパクションが継続します。

## 10. 残作業

- 配布用ビルド・配置は未実施（テスト用ビルドのみ実施）
- 未 push
- `google-antigravity` の実 API 検証は未実施
- checkpoint 本体の容量管理と archive は下記11節で実装。
- 必須メッセージだけで256件を超える入力は、必須項目を保持する方針のため
  `mandatory compaction retained item count exceeds the limit` で失敗します
- prefix と tail が同じ `call_id` を持てます。offline の転記は分割前の
  `history.items` 全体を対象にし、tail 側の live call は原文で残るため、
  対応が保たれていれば両方に同じ id が現れます。`validate` は tail 単体を
  検査するので通ります
- 合成 tool-file 観察の読み戻しは、writer の prefix で識別します。観察メッセージ
  は先頭の非空行が `[compacted tool files]` で、続く全行が
  `*** Add/Update/Delete File: <path>`、`inspected <path>`、あるいは区切りか
  拡張子を持つ1トークンの場合だけ観察として扱います。操作 prefix は値に残すので
  Add と Delete は別項目になります。観察メッセージは全行がエントリであることが
  条件で、`[compacted tool files]\ndocs/a.md\nWhich option?` のような実質問を
  含むメッセージは通常の assistant 文として扱います。旧形式
  `{"codetas_compacted_files": [...]}` は patch 系ツールの引数からのみ読みます。
  制約:
  - 1エントリは240字で、超過分は先頭のみ残して 96 bit の
    FNV-1a digest を付けます（同じ prefix を持つ別パスが同一化しないように）
  - 観察数の8件制限は撤廃。容量管理は checkpoint 全体の archive で行います
  - 旧形式は `arguments` と `input` の両方を解析し、空値・無関係な値は
    他方の読み戻しを妨げません。`exec` / `shell` / `bash` も対象です
  - allowlist 外のツール名による旧形式の読み戻しは対象外です
- 複数 envelope を入力が含む場合の契約は未定義（最新の1つを正本として扱う）

## 11. 再レビュー4件の修正

- tool result は JSON 文字列として引用し、`<` を Unicode escape で表現。
  ソースやログ内の制御文字列・見出しを checkpoint 構造として扱いません。
  空白・改行も復元可能です。モデル生成の不正な制御文字列を拒否する検証は維持。
- checkpoint が128 KiBを超える場合、またはシリアライズ後の出力が2 MiBを
  超える場合は、checkpoint と正規化済みの原履歴をローカル JSON に保存します。
  保存後に各セクションの部分プレビューと取得先の絶対パスを残します。
  保存先はユーザーホームの `.codetas/compaction-archives/`。
  Unixでは新規ディレクトリ700・ファイル600、排他的作成と fsync 後に参照を公開。
  保存失敗は圧縮失敗として返し、欠落した成功出力を返しません。
- NativeTrigger と Standalone の両方で、v2 envelope およびフレーミング込み
  Standalone JSON のサイズを共通検証します（base64化前の上限は2 MiB）。
  v1もJSONエスケープ後のサイズを検査します。
- 後続のモデル要約が省略しても archive 参照を機械的に引き継ぎます。
  再退避時は前の参照を新しい archive 内に保存し、参照を辿って読めます。
  Standalone の正規の handoff も再入力時に checkpoint として回収します。
- 観察は1メッセージ内・全体とも8件で切り捨てません。
  legacyは両フィールドを解析し、一般的なコマンド推測より先に読み戻します。

運用上の制約:

- archive はモデルに自動展開しません。参照されたローカル JSON をファイル読取
  ツールで取得する設計で、別ホストへ履歴を移す場合は archive も移す必要があります。
- 参照切れ防止のため自動削除・ディスク容量上限は未実装です。
  ディスク不足等で保存できなければ明示的に失敗します。
- 必須 raw tail 自体が2 MiBに収まらない場合は、最後の質問や未完了callを
  削除せず失敗します。archive への退避でこの制約を隠しません。

追加回帰テスト: 制御文字と空白の往復、20パスのtail外保存、旧形式の空値・
両フィールド、巨大結果の原履歴一致、4世代の退避参照、保存失敗、JSONサイズ超過、
必須tail超過、Standalone再圧縮時の参照維持。

検証結果: `cargo test -p codetas-gateway --lib compaction -- --test-threads=1`
は144 passed / 1 failed。compaction単体97件と合成compaction経路はすべて通過。
失敗は `ordinary_request_still_activates_repeated_tool_guard` の
`tool_choice` 検査で、変更前の HEAD を別ディレクトリへ展開して再ビルドした
単独テストでも同じ失敗を再現しました。今回の修正対象外です。
`git diff --check` は通過。実API・全crateテスト・本番配置は今回未実施です。

## 12. 引用内の見出しと退避プレビューの境界修正

- セクションの読取・追記・Remaining work置換・訂正の挿入は、共通の
  `checkpoint_section_range` を使用します。検証器と同じく独立した見出し行だけを
  境界とし、引用内の `## Remaining work` 等は区切りません。
  バイト位置で範囲を返すため、日本語・CRLF・見出し前後の空白にも対応します。
- 退避後は実際のファイルパスを含む参照と必須raw tailを先に確保し、
  両出力形式のシリアライズ後のサイズで検証します。プレビューは最大1024文字から
  段階的に短縮し、必要なら完全に省略します。参照やraw tailは削りません。
  参照と必須raw tailだけでも収まらない場合は明示的に失敗します。
- 回帰テスト3件を追加。全5見出しを含む結果の3回再圧縮・JSONからの原文復元、
  セクション操作が他の本文を変えないこと、残容量0/400/1600バイトでの退避を検証。
  境界テストはJSONエスケープが必要な本文と日本語・引用符を含む実際の退避先を使い、
  最後の質問・未完了call・ユーザー文を保持したまま両出力が成功することを確認。

検証: `cargo test --offline -p codetas-gateway --lib compaction::tests -- --test-threads=1`
は **100 passed / 0 failed**。`git diff --check` は通過。
この追修正では実API・全crateテスト・配置・pushは未実施です。

## 13. 複数行ユーザー文の引用

- `quote_control_data` は `<` に加えてLF・CRを含む本文もJSON文字列として引用。
  過去ユーザー文を訂正欄へ転記するとき、本文内の見出しがcheckpoint構造に
  混入しません。重複判定でも引用後の表現を確認し、再登場時の重複転記を防止。
- 回帰テスト2件を追加。全5見出し、LF/CRLF、NativeTrigger/Standaloneの
  組み合わせで3回再圧縮し、JSONからの原文復元・重複なし・両出力の成功を確認。
  単独CR、制御文字列、引用の二重化防止も検証。

検証: `cargo test --offline -p codetas-gateway --lib compaction::tests -- --test-threads=1`
は **102 passed / 0 failed**。`git diff --check` は通過。
実API・全crateテスト・配置・pushは今回未実施です。

## 14. 原文と引用表現を区別する重複判定

- 新たに退避するユーザー文は、単一行も含めて常に
  `User text (JSON): <JSON文字列>` として保存します。原文がJSONやこのマーカーに
  見えても全体を引用するため、実改行・文字としての `\\n`・マーカー文字列を
  混同しません。原文の前後の空白も保持します。
- 重複判定は訂正欄の項目の完全一致のみ。checkpoint全体の部分文字列一致は
  使用せず、他セクションや長い別文に同じ文字列があっても保存を省略しません。
- 旧形式の引用は原文へ推測変換せず、そのまま引き継ぎます。旧形式と新形式が
  併記される場合がありますが、異なる原文を誤って統合することを避けます。
- 回帰テスト3件を追加。NativeTrigger/Standaloneで実改行・引用表現・
  マーカー・前後空白の異なる原文を3回再圧縮し、別々に復元できることを確認。
  セクションをまたぐ誤重複、部分一致、同文再登場、旧形式の引き継ぎも検証。

検証: `cargo test --offline -p codetas-gateway --lib compaction::tests -- --test-threads=1`
は **105 passed / 0 failed**。`git diff --check` は通過。
実API・全crateテスト・配置・pushは今回未実施です。

## 15. 同文の新しい指示と保存済み項目の区別

- 14節のユーザー文の内容による重複排除を変更。前checkpointの管理対象項目は
  保存済みの時系列として引き継ぎ、今回prefixへ移るユーザー文は同文でも
  新しい出来事として全件を順番どおり追加します。A→B→AをA→Bへ縮めません。
- 要約器が管理対象項目を省略・重複・並べ替えしても、前checkpointと今回の
  raw prefixから管理対象リストを再構成します。既存の一般的な訂正説明は保持し、
  管理対象リストの再転記だけで件数が増えないようにします。
- 既存テストの「同文の新しい入力を常に1件へ集約する」という期待を修正。
  新規回帰テストでは同一圧縮内・世代をまたぐA→B→A、NativeTrigger/Standalone、
  4世代の引き継ぎ、要約器によるリストの欠落・重複・順序変更を確認します。

旧形式の自由文には出来事を識別する情報がないため、過去の圧縮ですでに
失われた反復や順序を復元するものではありません。

検証: `cargo test --offline -p codetas-gateway --lib compaction::tests -- --test-threads=1`
は **107 passed / 0 failed**。`git diff --check` は通過。
実API・全crateテスト・配置・pushは今回未実施です。

## 16. 残作業の部分置換と観察の最終確認順

- 再レビューで3つの再現テストを追加し、修正前にすべて失敗することを確認。
  定型のRemaining work文と実作業が同居する場合の実作業消失、定型文を引用した
  作業の誤置換、世代をまたいだAdd→Delete→Addの観察順の逆転を再現しました。
- 定型文は完全一致した箇条書き行だけを除去して新しい残作業を追加します。
  同じセクションの実作業・退避参照・引用内の文字列は変更しません。
- ファイル観察は、単一履歴内の `push_observation` と同じ最終確認順で統合。
  再確認された同一項目を末尾へ移し、以前に記録済みという理由だけで最新の
  観察を無視しません。NativeTrigger/Standalone双方の3世代で検証しました。

検証: `cargo test --offline -p codetas-gateway --lib compaction -- --test-threads=1`
は **157 passed / 1 failed**。compaction単体110件はすべて通過。
失敗は11節で修正前の再現を確認済みの
`ordinary_request_still_activates_repeated_tool_guard` で、今回の変更対象外です。
`git diff --check` は通過。実API・全crateテスト・配置・pushは今回未実施です。

## 17. 未完了呼び出しの再利用と並列結果の順序

- 回帰テスト2件を追加して修正前の失敗を確認。未完了のファイル操作が
  retainedから再読込されるだけで新しい観察として更新される問題と、並列ツールの
  観察順が結果の到着順と一致しない問題を修正しました。
- ファイル観察は呼び出しの発行時ではなく、対応する結果を受け取った位置で
  記録します。call_idで未完了の呼び出しと結果を対応付け、結果処理後は対応を
  外すため、同じIDの後続の呼び出しに古い引数を使いません。
- 未完了の呼び出し自体は従来どおり原文のままretainedに保持します。
  この観察は結果を受領した呼び出しのパス記録であり、操作成功の保証ではありません。
  結果本文も引き続き保存します。
- NativeTrigger/Standalone双方で、新しい結果がない3回の圧縮と、その後の
  結果到着を検証。並列呼び出しの逆順完了も確認しました。

検証: `cargo test --offline -p codetas-gateway --lib compaction::tests -- --test-threads=1`
は **112 passed / 0 failed**。`git diff --check` は通過。
実API・全crateテスト・配置・pushは今回未実施です。既知のツールガード失敗は対象外です。

## 18. 完了済み結果の再利用と新しい同値結果の区別

- 再レビューで、再利用したcall_idの未完了呼び出しがあるために新しい完了ペアが
  prefixへ移り、古い完了ペアだけretainedへ残るケースを再現しました。
  次の圧縮で古い観察を再確認した扱いになり、観察順が逆転していました。
- offline転記済みのretained列について、件数とSHA-256をcheckpointの
  `CODETAS internal replay metadata (JSON)` 行へ記録します。次回の先頭列が
  完全一致した場合だけ、既存観察・結果の再転記を省略します。
  呼び出しと結果の対応付けは省略しないため、前回未完了だった呼び出しへ
  新しく届いた結果は通常どおり処理します。
- 新しい同値結果は内容で重複排除せず、ready→error→ready等の出現順を保持。
  ユーザー文・最後の質問・未完了callのraw保持は変更していません。
- 検証情報の欠損・重複・不正値・件数超過・ハッシュ不一致では省略しません。
  旧形式は従来どおり全文を転記するため、過去の転記済み範囲を推測しません。
  通常のモデル要約では検証情報を除去します（retainedを転記済みと断定できないため）。
- 検証情報は退避後も残し、退避先参照とともに2 MiBの容量計算へ含めます。
  検証情報だけでgeneric checkpointを「作業実績あり」と判定しないようにしました。

回帰テスト6件と容量境界テストの追加ケースで、両出力経路の再利用、新規同値結果、
検証情報の不正・不一致、通常要約への切替、退避後の参照、作業実績判定を検証。
`cargo test --offline -p codetas-gateway --lib compaction -- --test-threads=1` は
**165 passed / 1 failed**。compaction単体118件はすべて通過しました。
失敗は修正前から確認済みの `ordinary_request_still_activates_repeated_tool_guard`。
`git diff --check` は通過。実API・全crateテスト・配置・pushは今回未実施です。

## 19. 空のツール結果と結果のみの履歴の復旧

- 再レビューで回帰テスト2件を追加し、修正前の失敗を確認。
  明示的な空文字・空白のみの結果が転記されず、retainedから外れると失われる問題と、
  完了したツール結果がある履歴を「復旧可能な情報なし」とする問題を修正しました。
- 空の結果もJSON文字列としてそのまま保存します。結果フィールドが欠落している
  場合とは区別し、空ファイル等の値を勝手に「情報なし」へ変換しません。
- 復旧判定にツール結果を含めます。未完了callだけの履歴や空入力を成功扱いには
  しません。call/result対応などの履歴検証は従来どおり実施します。
- 両出力経路で空文字・空白・タブ・CRLFの3回再圧縮と原文復元を検証し、
  ツール結果だけの入力をサーバー入口から復旧する統合テストも追加しました。

検証: `cargo test --offline -p codetas-gateway --lib compaction -- --test-threads=1`
は **168 passed / 1 failed**。compaction単体120件はすべて通過。
失敗は修正前から確認済みの `ordinary_request_still_activates_repeated_tool_guard`。
`git diff --check` は通過。実API・全crateテスト・配置・pushは今回未実施です。

## 20. prefixへ移る関数引数の検証

- レビューで、`function_call.arguments` の検証がretainedだけに適用され、
  小さい保持予算で呼び出しがprefixへ移ると、不正なJSONを検出しない経路を確認。
- 選択前の全履歴走査で、既存の引数検証をすべてのfunction_callへ適用します。
  保持予算によって入力の正否が変わらないようにし、正常なJSON文字列・
  オブジェクト・配列の従来の扱いは維持します。
- 単体テストとサーバー入口の統合テストを追加。不正JSON・null・数値を
  prefix/tailのどちらでも拒否し、正常な引数は受理することを検証します。
  両出力経路で、孤立した結果・未完了call_idの重複も引き続き拒否します。

検証: `cargo test --offline -p codetas-gateway --lib compaction -- --test-threads=1`
は **170 passed / 1 failed**。compaction単体121件はすべて通過。
失敗は修正前から確認済みの `ordinary_request_still_activates_repeated_tool_guard`。
修正前の再現用ビルドは長時間化のため途中停止し、修正後の検証へ切り替えました。
補助的な実ソースハーネスでも引数形式と保持予算の12ケースが通過しました
（ハーネスのトークン推定は検証用代替。上記Cargoテストは実際の推定器を使用）。
`git diff --check` は通過。実API・全crateテスト・配置・pushは今回未実施です。
