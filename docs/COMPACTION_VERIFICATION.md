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

追加した回帰テスト:

- prefix が空なら要約器が tail を読む
- prefix があれば従来どおり prefix を優先する
- 前回の retained を新しい tail へ持ち越さない
- 持ち越し除外がプロトコル・モデル非依存である
- 除外後も tool call と tool result の対応が壊れない
- 前回の envelope が無い初回は従来と同一の split になる
- offline 経路も同じ除外を行う
- 6ラウンド駆動で retained が定常サイズに収束する
- Gemini 形状の履歴が同じ有界 split になる

## 5. 未完了

- **3コミット（`fda278c`、`ac5c43a`、`e2b6cfe`）と `1a1887b` は未ビルド・未配置**です。
  インストール済みアプリは 0.1.1 のままなので、修正を有効にするには再ビルドと
  配置が必要です。
- `google-antigravity` の実 API でのコンパクション検証は未実施です。
  OAuth セッションと Cloud Code Assist プロジェクトが必要です。
- OpenAI native Responses 経路（`openai` と Codex login forwarding）は
  今回の変更の対象外で、従来の挙動のままです。
