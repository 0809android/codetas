# Mistral Vibe のサブスクを CODETAS で使う

状態: 実装済み。2026-11-06 に Pro プランの実キーで確認した（末尾の「検証」を参照）。

## 接続方法

どちらかで `mistral` プロバイダーに接続する。

1. **Vibe の取り込み（推奨）**: `vibe --setup` でサインインするか API キーを設定すると、CODETAS が
   そのキーを取り込む。ブラウザサインインも最終的には API キーになるので、経路は同じ。
   Vibe の `VIBE_CLI_OIDC_ENABLED` モードには取り込めるキーがないので対象外。
   - 読み取り順は Vibe と同じで `~/.vibe/.env`、次に Keychain（サービス `ai.mistral.vibe`、
     アカウント `MISTRAL_API_KEY`）。`.env` は `MISTRAL_API_KEY` だけを読む。同じ名前の行が複数あれば、
     Vibe のサーバーが使う python-dotenv と同じく最後の行を使う。最後の行が空ならキーなしとして Keychain へ進む。
   - 取り込んだキーは CODETAS の `auth.json` に保存する。Vibe のログインを読むのは次のときだけで、
     キーが保存済みならリクエストごとには読まない。
     - 起動時の取り込みで、`auth.json` にキーがないとき。
     - 接続カードの「Vibe で再取り込み」と「CODETAS に登録」を押したとき。読んだキーはその場で保存する。
     - 「登録確認」を押したとき。キーがあるかを見るだけで、保存はしない。
     - リクエストの時点で `auth.json` にキーがないとき。読めたキーを保存して使う。
     - キーが 401/403 になったとき。読み直したキーが同じならリトライせず失敗にする。

     古いキーが有効なまま Vibe 側で別のキーに替えたときは「Vibe で再取り込み」を押す。
     画面を開いたときの CLI の一覧は Vibe のログインを読まず、`auth.json` にあるものだけで判断する。
   - 401/403 の読み直しで別のキーへ替えたら、それから 1 分は読み直さず、拒否されたリクエストを保存済みの
     キーで 1 回だけリトライする。新しいキーの前に送ったリクエストを救うためで、キーと無関係な 403 でも
     この 1 分間は 1 回余分に送る。
   - `.env` はファイルなので毎回読む。Keychain を自動で読むのは（起動時、リクエスト時、401/403 時）、結果に
     よらず 1 分に 1 回まで。キーがないままリクエストを送っても毎回は読まず、起動時に取り込みが 2 回走っても
     確認ダイアログは 1 回で済む。読み取りの最中に来たリクエストはその結果を最大 35 秒待つ。ボタンはこの制限を
     受けない。
   - Keychain の読み取りは 30 秒で打ち切る。認証ストアのロックを持たずに読み、リクエスト処理のスレッドも
     止めないので、確認ダイアログを待つ間も他のプロバイダーのリクエストは止まらない。起動時の取り込みと
     登録の間は、CODETAS の設定の変更だけが待たされる。「登録確認」もメインスレッドの外で動くので、ダイアログを
     待つ間も CODETAS の画面は止まらない。
   - `api.mistral.ai` 以外を指す `mistral` 接続には取り込まない。起動時も、登録でも、再取り込みでも接続を
     変えず、有効にもしない（自分のキーを持つ接続は登録で有効にする）。URL を後から変えた接続に残った
     ログインは、リクエストの時点で拒否する。
   - 接続が自分のキー（Keychain 参照、値のある環境変数など）を使っているときは、起動時に取り込まない。
   - 接続カードでアカウントを削除すると、Vibe にキーが残っていても次回起動で復活しない
     （`accountPool.dismissedCliImports`）。「CODETAS に登録」で元に戻る。削除の瞬間に処理中だったリクエストが
     同じキーを `auth.json` へ取り込み直すことはある。接続には付かず、アカウント一覧に戻るだけなので、もう一度削除する。
   - ブラウザサインインの経路は Vibe のソースから確認した内容で、実機では API キー設定の経路だけを試した。
2. **キーの直接設定**: <https://chat.mistral.ai/code/extensions> で発行したキーを、接続の編集画面で
   Keychain 参照または環境変数 `MISTRAL_API_KEY` に設定する。

macOS の GUI アプリはシェルの環境変数を継承しないので、通常は 1 か Keychain 参照を使う。

Codex のモデル一覧に許可リストを設定している場合は、接続後に Mistral のモデルを選んでから
Codex を再起動する。

## キーの種類と課金

プランには月間の枠が 2 つある（Pro の実測値: Vibe Code 300 USD、API 30 USD）。

| キー | 発行場所 | `plan_type` | 消費先 |
| --- | --- | --- | --- |
| Vibe CLI キー | Code › Vibe CLI / `vibe --setup` | `chat` | Vibe Code の枠 |
| API キー | <https://console.mistral.ai> | `api` | Vibe Code の枠は使わない。公式の案内は API のレート制限と従量課金で、API の枠との関係は未確認 |

- **Vibe キーは、クライアントが Vibe 本体でなくても Vibe Code の枠から引かれる。** 素の HTTP
  クライアントと CODETAS 経由の呼び出しで `vibe_budget` だけが増え、`api_budget` は 0 のままだった。
  `mistral-vibe-cli-latest` 以外のモデル（`mistral-medium-latest`、`mistral-small-latest`、
  `codestral-latest`）でも同じ。コミュニティには「外部ツールでの利用は API 課金になる」という
  二次情報があるが、この実測とは一致しない。公式文書に明記はないので、挙動が変わる可能性はある。
- 枠を使い切った後は組織の設定による。従量課金 OFF（既定）なら次の請求期間まで停止、ON なら
  前払いクレジットから引かれ、残高ゼロで HTTP 402 になる。

## 使用量の表示

Vibe 本体のフッターと同じ値を読む。推論は行わない。

| 値 | 取得元 | 表示 |
| --- | --- | --- |
| 各枠の使用率とリセット日時 | `GET https://api.mistral.ai/v1/billing/budget` の `vibe_budget` / `api_budget` | 共有ウィンドウ `Vibe Code` / `API` |
| プラン名と従量課金の状態 | 同上、`GET /v1/billing/subscription` | プラン欄 |
| キーの種類 | `GET https://console.mistral.ai/api/vibe/whoami` の `plan_type` | プラン欄 |

- `usage_percentage` は 0〜100。従量課金 ON なら 100 を超える。ゲージは 100 で止め、プラン欄の
  「枠を超えると従量課金」で区別する。
- `initial_budget` が 0 以下、または枠がなければ表示せず、0% とは表示しない。
- `/v1/billing/budget` は組織の使用状況を見る権限が必要で、権限がないメンバーは HTTP 403 になる。
- 組織全体の値なので、CODETAS 外（Vibe 本体や Studio）の利用も含む。反映はほぼ即時。
- `whoami` は公開 API リファレンスに載っていない Vibe 用の経路で、読めなくても動く。
- プラン名とキーの種類は補助の情報なので、Vibe の billing 取得と同じ 3 秒で打ち切る。
- キーを別の宛先へ送らないよう、`base_url` が `api.mistral.ai` の接続でだけ取得する。プロキシや
  専用環境の接続は「未対応」と表示する。

## 接続先の仕様

エンドポイントは `https://api.mistral.ai/v1/chat/completions`。以下は実 API で確認した内容。

| 内容 | 結果 | CODETAS の対応 |
| --- | --- | --- |
| `prompt_cache_key` | **ないと同じ 5k トークンの前置きでもキャッシュ 0。あると 5248/5381 がヒット** | `prompt_cache_key = true`。`base_url` を `api.mistral.ai` 以外へ変えた接続には送らない |
| `reasoning_effort` 付きの応答 | `delta.content` が `thinking` / `text` の配列。思考と本文が 1 つの delta に同居し、文字列の delta も混ざる | 順序を保った文字列の差分に分解（`translate/util.rs`） |
| 思考の履歴 `reasoning_content` | HTTP 422 | 次のターンには送り返さない |
| 長い tool call ID（`call_...`） | 受理 | 書き換えない |
| `stream_options`、`parallel_tool_calls`、`tool_choice`（`required` 含む）、`json_schema` | 受理 | そのまま送る |
| 最後のメッセージが assistant | HTTP 400 | 末尾の developer / system を user として送る。それでも assistant で終わるなら user を足す（下記） |
| freeform の `apply_patch` | 形式を知らず unified diff を返す | 具体例入りの説明に差し替える（下記） |

以下の対応はすべて `api.mistral.ai` 宛ての送信用コピーの中だけで行い、保存する履歴は変えない。

**履歴の末尾が assistant になる形は 2 つある。**

- コンパクション: 指示は履歴の末尾に付くが、Chat への変換で developer メッセージは先頭の system に
  統合される。履歴が assistant で終わると assistant が最後に残るので、末尾の指示を user に変える。
- 不完全な応答の再試行: 応答が `incomplete` で終わると、Codex は記録済みの内容（「Still working…」など）を
  含めて再試行する。続く指示がないので user を足す。進捗文だけで終わるときは、ツール呼び出しが無効で
  何も実行されなかったことと `apply_patch` の形を伝える。ツール結果待ちの呼び出しには足さない。

**`apply_patch` の形式を説明する。** Codex はこのツールを freeform で宣言し、形式を文法で渡す。Chat には
対応するツール種別がないので文法は失われ、Codex の説明文は「JSON で包むな」と書いてあって、変換後の
`input` 引数と矛盾する。Mistral のモデルは形式を知らないので、unified diff を返して無効な呼び出しとして
捨てられていた。文法を説明文に足しても同じだった。具体例を入れた説明に差し替えると、プランの 4 モデルで
12 回すべて正しい形式になった。Codex 0.159.1 は `apply_patch_tool_type` に `freeform` しか受け付けないので、
カタログから関数形式を選ぶことはできない。他の Chat プロバイダーには適用していない。

モデルは `/v1/model-catalog?surface=vibe-code` に合わせる。「モデル取得」もこのカタログを読む。
範囲外の effort は HTTP 400 になる。

| モデル | コンテキスト | effort | 画像 |
| --- | --- | --- | --- |
| `mistral-large-4-0`（推奨、既定） | 1,048,576 | low / medium / high | 可 |
| `mistral-medium-latest`（別名 `mistral-vibe-cli-latest`） | 262,144 | high（API は none も受理） | 可 |
| `zai-glm-5-3` | 1,048,576 | low / high / max | 不可 |
| `zai-glm-5-3-flash` | 1,048,576 | low / high / max | 可 |

`mistral-medium-latest` に `none` を載せないのは、effort が「要求以下で最も近い値」へ丸められるので、
Codex の `medium` が黙って推論なしになるため。

`zai-glm-5-3` は `model_input_modalities` の行にかかわらず、カタログでも送信時も text のみとして扱う。
画像付きの入力は、Vision サイドカーがあればその説明文に置き換わり、なければ明示的なエラーになる。
保存済みの Vision サイドカーがこのモデルを指している間は、`model_input_modalities` の行を足さない。足すと
検証に落ちて、全プロバイダーの設定を読めなくなるため。サイドカーを外すと次の読み込みで足す。

`/v1/models` は 64 件で、埋め込みや OCR、音声も含むので通常は使わない。カタログが 403 か 404 のときだけ
こちらへ戻り、`completion_chat` と `function_calling` が偽の行は読み飛ばす。429 や 5xx は一時的なので
戻らず、取得を失敗にする。カタログに新しいモデルが増えたら、そこに載る effort（`none` を除く）と
表示名を使い、既定の effort は `high`（なければ一覧の最上位）にする。

effort の一覧がないモデル（保存済みの `codestral-latest` など、`/v1/models` へ戻ったときの行、カタログで
effort を持たない行）には、カタログで effort を出さず、`reasoning_effort` も送らない。推測で送ると範囲外に
なって HTTP 400 を受けるため。これは `api.mistral.ai` を指す接続だけの扱いで、プロキシなど別の宛先を指す
`mistral` 接続は他のプロバイダーと同じ既定の effort になる。

この対応より前に保存した `mistral` 接続は、設定の移行（registry revision 27）で同じ内容になる。
`api.mistral.ai` 以外を指す接続と、利用者が選んだ既定モデル、effort の設定は変えない。保存済みの値と
矛盾する既定値は入れない。`discovery.path` は旧既定の `/models` のときだけ変える。

次の 3 つは、保存された値が利用者の選択か旧既定かを区別できないので、`api.mistral.ai` を指す接続に移行時に
一度だけ書き込む。移行後に戻せば、以後は変わらない。

- `prompt_cache_key` を `true` にする。
- `capabilities.reasoning` を `true` にする。
- プランの 4 モデルがなければ `models` に足す。以前に外したモデルも戻る。

実装は `registry/vendors_b.rs`、`translate/util.rs`、`compat/chat_tools.rs`、`account_usage.rs`、
`oauth/cli.rs`、`discovery.rs`。
仕様の出典は Vibe 本体のソース（Apache-2.0、<https://github.com/mistralai/mistral-vibe>）と上記の実測。

## 検証

2026-11-06、Pro プランの Vibe キー。隔離した Validation ビルド経由で Responses API を流して確認した。

- ツール呼び出し → 結果の再送 → 次のターンまで完了。応答は `reasoning` → `function_call` / `message` の順。
- 2 ターン目以降のキャッシュヒット: 5248/5453、5248/5462。
- 途中でのモデル切り替え、他プロバイダー由来の長い call ID、`medium` の GLM への丸めが成功。
- 接続カードで、プラン名、2 つの枠、リセット日時の表示を確認。
- 初回起動で取り込んだ接続が、再起動なしで `/v1/models` に出てルーティングされることを確認。
- `/v1/responses/compact`: 履歴が assistant で終わる形、ツール結果で終わる形とも HTTP 200
  （`mistral-medium-latest`、`mistral-large-4-0`）。修正前は assistant で終わる形が HTTP 400。
- 古い形式の接続が移行で 4 モデルになり、2 回目の呼び出しでキャッシュが 5120/5371 ヒット。
- 画面で「Vibe で再取り込み」が成功。「モデル取得」が 4 件を返す。削除が再起動後も有効で、
  「CODETAS に登録」で元に戻り、推論まで成功。
- 実際の Codex CLI 0.159.1 を CODETAS のカタログと単体ゲートウェイで実行: `mistral-medium-latest` と
  `mistral-large-4-0` が `apply_patch` で新規作成と既存ファイルの編集を行い、シェルで確認して完了。
  キャッシュヒットは 49152/62493、40704/55741。
- 「Still working…」で終わる履歴と、通常の応答で終わる履歴を `/v1/responses` へ直接送り、どちらも HTTP 200。

未確認: Codex アプリからの実利用、従量課金 ON での 100% 超の表示、`api` キーでの表示と
カタログの取得、ブラウザサインインで得たキーの取り込み、モデルが実際に無効な `apply_patch` を返した
ときの再試行の通し（修正後のモデルは無効な形を返さなかったので、履歴を直接送る確認に留まる）。
