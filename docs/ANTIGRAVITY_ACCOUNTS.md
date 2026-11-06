# Google Antigravity: 複数アカウント追加の調査メモ

調査日: 2026-11-05。状態: **実装・実機検証済み**（アプリ内ログイン + CLI取り込みの併用）。

## 症状（修正前）

プロバイダー設定で Google Antigravity の「アカウント追加」ができない。

## 原因（修正前）

- Antigravity は `cli-import` 方式だった。`agy` CLI のログインを取り込むだけで、CODETAS 自身はログインしなかった
  (`crates/codetas-gateway/src/oauth/mod.rs` の `OAUTH_PROVIDER_REGISTRY`、`native_login: false`)。
- `agy` の認証はキーチェーンの 1 か所 (`gemini` / `antigravity`、なければ
  `~/.gemini/antigravity-cli/antigravity-oauth-token`) にしか無い。同時に保持できるアカウントは 1 つ。
- 「アカウント追加」(`apps/desktop/src-tauri/src/provider_gateway/service_cmds.rs`) は
  `provider_supports_native_oauth` が偽のプロバイダーを「アプリ内OAuthに未対応です」で拒否する。
  `login_provider_oauth_new_account` も kimi / anthropic / xai だけを扱う。
- トークン更新 (`oauth/session.rs` の `refresh_session`) は、保存済みの refresh token ではなく
  `agy models` を実行して `agy` の現在のログインを読み直す。`google-antigravity#<slug>` のような
  アカウント別キーに素直に対応させると、更新のたびに `agy` 側のアカウントのトークンが別枠へ書き込まれる。

## 選択肢

| 案 | 内容 | 長所 | 短所 |
| --- | --- | --- | --- |
| A. 取り込み方式 | `agy` に今ログイン中のアカウントを別枠として取り込む。更新は、枠のアカウントが `agy` の現在のログインと一致するときだけ行い、不一致なら再ログインを促す | 他社 CLI の認証情報を使わない。すぐ実装できる | 更新時に `agy` のアカウント切り替えが必要になることがある |
| B. アプリ内ログイン | 認可コード + PKCE のブラウザログインを CODETAS に実装し、refresh token でアカウントごとに更新する | kimi / anthropic / xai と同じ使い勝手。アカウント切り替え不要 | OAuth クライアント ID とシークレットが必要 (下記)。他社 CLI に埋め込まれた認証情報の流用になり、規約・失効のリスクがある |

## 調査結果 (案 B の前提確認)

`agy` のバイナリ内の文字列を調べた。

- スコープ: `cloud-platform`、`userinfo.email`、`userinfo.profile`、`experimentsandconfigs`。
- リダイレクト: `http://localhost:<ポート>` の形 (ローカルの受け口)。
- Google OAuth クライアント ID が 3 つ、クライアントシークレットが 3 つ見つかった。
  バイナリ内の並びからは、どの ID とどのシークレットが対か、どれがログイン用かは判別できなかった。
- ログインなしで確認できる範囲として、各 ID で Google の認可 URL (リダイレクト
  `http://localhost:<ポート>/callback`、PKCE あり) を開いた結果は次のとおり。

| ID の先頭 | 認可 URL の結果 |
| --- | --- |
| `1071006060591` | ログイン画面に進む (受け付けられる) |
| `884354919052` | ログイン画面に進む (受け付けられる) |
| `1098991175725` | `redirect_uri_mismatch` (localhost のリダイレクトは不可) |

- どちらの ID が Antigravity 用か、トークン交換と `loadCodeAssist` が通るかは**未確認**。
  実際にログインしてトークン交換する確認は、他社 CLI 由来の認証情報を使う操作として、
  自動の許可判定で拒否されたため実施していない。

## 取り扱い上の注意

- `agy` のバイナリから見つけた 3 組のクライアントシークレットは、このドキュメント、
  コード、ログ、会話に転記しない（実装でも使わない）。
- 実機検証では、利用者が自分の環境で `agy` のクライアントを一時的に使った。値は
  リポジトリ・会話・ログに出していない。配布物の既定値には含めない。
- Google の Gemini CLI 利用規約は、Gemini CLI OAuth を使った第三者ソフトウェアからの
  Code Assist 直接アクセスを規約違反と明記している
  (https://www.geminicli.com/docs/resources/tos-privacy)。Gemini CLI のクライアントを
  Antigravity 用に流用しない。
- Antigravity 用に利用許可されたクライアントがある場合だけ、環境変数で明示的に設定する。

## 決定と実装 (2026-11-05)

- 既定は案 A（複数アカウント取り込み）。「アカウント追加」は `agy` の現在のログインを
  アカウント別キーで保存する。これで追加ボタンは設定なしで動く。
- 案 B（アプリ内ログイン）も実装し、実機で動作を確認した。ただし使えるクライアントは
  Google が公開しているものではないため、リポジトリには埋め込まず、利用者が自分の責任で
  用意したクライアントを次のどちらかで渡す。
  - 環境変数 `CODETAS_ANTIGRAVITY_CLIENT_ID` / `CODETAS_ANTIGRAVITY_CLIENT_SECRET`
  - `auth.json` と同じフォルダの `antigravity-oauth-client.json`（`clientId` / `clientSecret`）。
    macOS の GUI アプリはシェルの環境変数を継承しないため、通常はこちら。シークレットが入るので
    0600 にしておく。CODETAS はファイルのモードを確認しない。
- クライアントが未設定なら、ブラウザログインは開始せず取り込みへフォールバックする。

### アプリ内ログイン（クライアント設定時のみ）

- `authorization-code-pkce`。ブラウザで Google にログインし、`http://127.0.0.1:53118/oauth2callback`
  で認可コードを受ける。
- スコープ: `cloud-platform`、`userinfo.email`、`userinfo.profile`、`experimentsandconfigs`。
- トークン交換は `https://oauth2.googleapis.com/token`。`access_type=offline` と `prompt=consent`
  を付けて refresh token を取得する。
- アカウント識別は id_token の `sub` / `email`（なければ `userinfo`）。`google-antigravity#<slug>`
  のキーで保存し、複数アカウントを同時に保持する。
- refresh は保存した refresh token で行うため、アカウント切り替えのために `agy` を操作する必要はない。
- タイムアウトは 10 分。実際の検証では同意操作を含めて約 2 分で完了した。

### OAuth クライアント

- 既定値は持たない。リポジトリにも配布物にもクライアント ID / シークレットを埋め込まない。
- 環境変数かプライベートファイルで ID とシークレットが揃ったときだけ有効（Google は ID と
  シークレットを対で検証する）。環境変数が片方だけならエラーにせず無視し、プライベートファイルが
  あればそちらを使う。ファイルも揃っていなければ未設定として扱う。
- クライアント側で `http://127.0.0.1:<ポート>/oauth2callback` の登録が必要。ポートは
  `53118`。
- macOS の GUI 起動アプリはシェルの環境変数を継承しない。必要なら `launchctl setenv` などで
  アプリのプロセスに渡す。

### アカウント追加（既定の取り込み）

- `agy` の現在のログインを取得する。アクセストークンが期限切れなら `agy models` で更新する。
- 取り込んだセッションは `source: "local-cli"`。更新は `agy models` を実行して行う。
- 同じアカウントをもう一度追加した場合は、既存のキー（ベースまたはアカウント別）を
  更新する（重複しない）。
- 別アカウントを追加するには、`agy` でそのアカウントにログインし直してから「アカウント追加」を押す。
- アカウント別キー (`google-antigravity#<slug>`) の取り込みセッションは、`agy` の現在の
  ログインが同じアカウント（email、なければ refresh token）のときだけ更新する。
  不一致なら別アカウントのトークンで上書きせず、「agy で再ログインしてください」と返す。
- `agy` は同時に 1 アカウントしか保持しないため、複数アカウントの同時自動更新はできない。
- Antigravity のトークン更新と `agy models` は専用のロックで 1 つずつ実行する。キーチェーンの確認や
  `agy` を待つ間も、他のプロバイダーのトークン更新は止まらない。
- 取り込みセッションが 401/403 で拒否されたときの `agy` 経由の更新は 1 分に 1 回まで。直前の更新で
  トークンが替わっていれば、更新せずにそのトークンで 1 回だけリトライする。期限切れによる更新にこの制限はない。

## 検証結果

### クライアントの特定（ログイン不要）

ダミーコードで `https://oauth2.googleapis.com/token` を叩き、`invalid_client`（ペア誤り）
と `invalid_grant`（ペア正しい・コード不正）で判定した。3 ID × 3 シークレットのうち
有効なペアは 3 つ。ローカルリダイレクトを受けるのは 2 つだった。

| ID の先頭 | ローカルリダイレクト | 個人アカウントでの `loadCodeAssist` |
| --- | --- | --- |
| `1071006060591` | 可 | **`free-tier` 対象**。`cloudaicompanionProject: aicode-consumers`、`paidTier: g1-ultra-lite-tier` |
| `884354919052` | 可 | `ineligibleTiers: free-tier` / `GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT`。`standard-tier`（ユーザー指定の GCP プロジェクト必須）のみ |
| `1098991175725` | 不可 (`redirect_uri_mismatch`) | 未実施 |

`agy` が個人アカウントで使っているのは `1071006060591` 系のクライアントと判断できる。
`884354919052` は認可自体は通るが、個人向け無料枠の対象外だった。

### 実機のフル経路（クライアント `1071006060591`）

プライベートファイルだけを置き、環境変数なしで実施。

1. ブラウザ OAuth 同意 → 認可コード → トークン交換: 成功
2. `access_token` / `refresh_token` / `id_token` の `sub`・`email` を取得: 成功
3. refresh token での更新: 成功（`expires_in: 3599`、新しい refresh token は返らず、
   既存を保持する実装で正しい）
4. `v1internal:loadCodeAssist`: 200、`project = aicode-consumers`
5. `v1internal:fetchAvailableModels`: 200、26 モデル
6. `v1internal:generateContent`（`gemini-3.8-flash-low`）: 200、応答 `OK`

再現用の手動テストを `crates/codetas-gateway/src/oauth/providers.rs` に置いた
（`#[ignore]`、既定のテスト実行では動かない）。

```text
cargo test -p codetas-gateway live_antigravity_login_works -- --ignored --nocapture
```

### 分かった制約

- クライアントシークレットなしのトークン交換は `client_secret is missing.` で不可。PKCE だけでは足りない。
- `884354919052` のトークンでも `fetchAvailableModels` に `project: aicode-consumers` を
  渡すと 200 が返る。ただし `loadCodeAssist` が明示的に「このクライアントは個人向け
  無料枠の対象外」と返している以上、そのプロジェクト名を固定で使うのは適格性判定の
  回避になるため実装しない。
- Gemini CLI の公開クライアントを Antigravity 用に流用するのも行わない（規約で禁止）。
- 提供モデルはアカウント/階層で異なる。`gemini-3.1-pro-high` は今回のアカウントで 404 だった。

## 残作業

- 使っているクライアントは `agy` 由来で、Google が第三者利用を正式に認めたものではない。
  アカウント停止などのリスクは利用者側の判断。`antigravity-oauth-client.json` を削除すると、
  以後の「アカウント追加」は取り込み方式に戻る。アプリ内ログインで保存済みのセッションは、更新に
  同じクライアントが要るので期限切れの後は失敗する。`agy` で同じアカウントにログインして
  「アカウント追加」を押すと、取り込みのセッションに置き換わる。
- クライアントを自前で登録する場合、Google Cloud Console で OAuth クライアントを作成し、
  ループバックリダイレクトを登録したうえで、同じファイルに ID / シークレットを置く。
  そのクライアントが `loadCodeAssist` で `free-tier` を得られるかは、クライアントの
  登録内容に依存する（今回の `884354919052` のような例がある）。
- クライアントなしでアカウント切り替えを不要にするには、Google が第三者向けの
  OAuth クライアントを提供するか、`agy` が複数トークンプロファイルを公開するのを待つ。
