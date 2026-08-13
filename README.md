# CODETAS

> Codexに、できることを足す。

CODETAS は、Codex のための実験的なローカル完結型コンパニオンです。Hermes の
プロジェクトコンテキストを検出し、再利用できる内容を提示して、スキル・ライフ
サイクルフック・MCP サーバーからなるレビュー可能な Codex 統合を提供します。

独立したローカルプロバイダーゲートウェイも含みます。Codex は
Responses 互換のループバックエンドポイントに1つ接続するだけで、CODETAS が
Responses / Chat Completions / Anthropic Messages / Gemini generateContent の各
プロバイダーへモデルを振り分けます。OpenCodex の実行・組み込み・依存はしません。

このリポジトリから配布されるのは、インストール可能なデスクトップ管理アプリと、
プロジェクト単位の統合を行う Codex プラグインの2つです。プラグイン単体でも
使えます。

CODETAS は独立したコミュニティプロジェクトです。OpenAI や Nous Research の
製品ではなく、いずれの企業とも提携・承認関係にありません。

## 実装範囲

- ローカルプロジェクトを追加し、`.hermes.md` / `HERMES.md` / `AGENTS.md` /
  スキル / MCP 設定を検出。
- 元のプロジェクトを変更せずに同期プランをプレビュー。
- レビュー可能な `SessionStart` フックで Hermes のプロジェクトコンテキストを
  Codex に読み込み。
- ローカル MCP サーバーでプロジェクトコンテキスト／スキル検査ツールを公開。
- Codex 統合をリポジトリローカルのプラグインとしてパッケージ化。
- API キーの値を `providers.json` に保存せずにプロバイダー定義を追加。
- 既存の Kimi / Claude / Grok CLI ログインをユーザー所有の auth ストアへ
  取り込むか、アプリからログイン。リフレッシュトークンは git に入れない。
- `provider/model` リクエストを独立したループバックゲートウェイで振り分け。
- Responses ストリームを透過し、Chat Completions のテキスト・ツール呼び出し・
  usage・SSE を Responses イベントへ変換。
- Anthropic / Gemini のテキスト・画像・ツール呼び出し・usage・推論サマリ・
  SSE を変換。
- 幅広いプロバイダーレジストリから選択し、モデルを検出して Codex の
  モデルカタログを公開。
- フェイルオーバー・重み付け・最小使用量・アカウントプールの経路を実行。
- コンテンツを含まないリクエスト結果・アダプターのトークン使用量・カタログ
  価格に基づく概算コストを、保持・容量制限付きのプライベートなローカル台帳に
  記録。
- launchd / systemd ユーザーユニット / Windows Task Scheduler でゲートウェイを
  任意に常駐化し、所有権安全な codetas-codex 起動 shim を利用。
- プロトコルアダプターと生成ランチャーで、Claude Code / Claude Desktop MCP /
  OpenCode / Grok から同じ経路を再利用（元のクライアントは置き換えない）。
- ユーザーレベルの Codex プロバイダー設定をレビュー後にバックアップ・更新し、
  所有権を尊重した復元を提供。
- スタンドアロン管理 CLI でプロバイダー・アカウント・モデル・経路・エージェント・
  サイドカー・アクセスキー参照・observability・システム設定を管理。
- 変換プロバイダー間で Responses の compact 状態を再利用し、クライアント別の
  モデルカタログを提供。画像生成／編集・検索・動画生成を能力チェック済みの
  経路で中継。

OpenCodex 2.10.0 の正確な挙動・残ギャップ・意図的な安全策の代替は
[機能表](docs/FEATURE_PARITY.md)にまとめています。

## リポジトリ構成

```text
apps/web                 TypeScript/Vite 管理画面
apps/desktop             Tauri 2 デスクトップシェル
packages/core            プロバイダー非依存の検査・同期プラン型
crates/codetas-gateway   独立したローカル Responses／プロバイダーゲートウェイ
plugins/codetas          インストール可能な Codex プラグイン
.agents/plugins          ローカル開発用のリポジトリマーケットプレイス登録
docs                     製品・アーキテクチャ・セキュリティのメモ
```

## 開発要件

- Node.js 22 と npm 10
- Rust stable と Tauri 2 のホスト前提条件
- Codex デスクトップアプリまたは Codex CLI（プラグイン統合用）
- Hermes Agent は任意。CODETAS は Hermes を起動せずに互換ファイルを検査可能

```bash
npm install
npm run dev
npm run dev:desktop
```

デスクトップ版とプラグイン単体のセットアップは
[インストール](docs/INSTALLATION.md)、Hermes の正確な対応範囲は
[互換性](docs/COMPATIBILITY.md)、経路とセキュリティ制限は
[プロバイダーゲートウェイ](docs/PROVIDER_GATEWAY.md)を参照してください。

## 信頼モデル

CODETAS はフックを黙って信用せず、資格情報をコミットせず、Hermes のソース
ファイルを編集しません。ローカル CLI のログインはリポジトリ外のユーザー所有
auth ストアへ取り込まれます。生成された変更はユーザーが確認し、Codex 標準の
フック信頼フローを使います。[セキュリティ](docs/SECURITY.md)を参照。

## ステータス

プレアルファ。デスクトップ・ゲートウェイ・プラグインのソースは実行可能ですが、
署名・公証済みのバイナリリリースはまだありません。

## ライセンス

Apache License 2.0。[LICENSE](LICENSE) を参照。

---

## English

CODETAS is an experimental, local-first companion for Codex. It discovers
Hermes project context, shows what can be reused, and pairs with a reviewable
Codex integration made from skills, lifecycle hooks, and an MCP server.

It also includes an independent local provider gateway. Codex connects to one
Responses-compatible loopback endpoint while CODETAS routes models to native
Responses, Chat Completions, Anthropic Messages, or Gemini generateContent
providers. CODETAS does not invoke, embed, or require OpenCodex.

The product has two pieces distributed from this repository: an installable
desktop management app and a Codex plugin that performs the project-scoped
integration. The plugin can also be used on its own.

CODETAS is an independent community project. It is not an OpenAI or Nous
Research product and is not affiliated with or endorsed by either company.

### Implemented source scope

- Add a local project and detect `.hermes.md`, `HERMES.md`, `AGENTS.md`, skills,
  and MCP configuration.
- Preview a sync plan without changing the source project.
- Load Hermes project context into Codex through a reviewable `SessionStart`
  hook.
- Expose project-context and skill-inspection tools through a local MCP server.
- Package the Codex integration as a repository-local plugin.
- Add provider definitions without storing API-key values in `providers.json`.
- Import existing Kimi, Claude, and Grok CLI logins into a user-owned auth
  store, or sign in from the app; refresh tokens stay out of git.
- Route `provider/model` requests through an independent loopback gateway.
- Pass through Responses streams and adapt Chat Completions text, tool calls,
  usage, and server-sent events back into Responses events.
- Adapt native Anthropic and Gemini text, images, tool calls, usage, reasoning
  summaries, and server-sent events.
- Select from a broad provider registry, discover models, and publish a Codex
  model catalog.
- Run failover, weighted, least-usage, and account-pool routes.
- Track content-free request outcomes, adapter token usage, and catalog-priced
  cost estimates in a private, retention- and capacity-bounded local ledger.
- Optionally supervise the gateway through launchd, a systemd user unit, or
  Windows Task Scheduler and use an ownership-safe codetas-codex launch shim.
- Reuse the same routes from Claude Code, Claude Desktop MCP, OpenCode, and
  Grok through protocol adapters and generated launchers that do not replace
  the original clients.
- Back up and update the user-level Codex provider configuration after review,
  with ownership-aware restoration.
- Use the standalone management CLI for provider, account, model, route, agent,
  sidecar, access-key reference, observability, and system settings.
- Reuse Responses compact state across translated providers, serve client-
  flavored model catalogs, and relay image generation/edit, search, and video
  generation endpoints through capability-checked routes.

Exact OpenCodex 2.10.0 behavior, remaining gaps, and deliberate safety
alternatives are summarized in the
[feature table](docs/FEATURE_PARITY.md).

### Repository layout

```text
apps/web                 TypeScript/Vite management interface
apps/desktop             Tauri 2 desktop shell
packages/core            Provider-neutral inspection and sync-plan types
crates/codetas-gateway   Independent local Responses/provider gateway
plugins/codetas          Installable Codex plugin
.agents/plugins          Repository marketplace entry for local development
docs                     Product, architecture, and security notes
```

### Development prerequisites

- Node.js 22 and npm 10
- Rust stable and the Tauri 2 host prerequisites
- Codex desktop app or Codex CLI for plugin integration
- Hermes Agent is optional; CODETAS can inspect compatible project files
  without launching Hermes

```bash
npm install
npm run dev
npm run dev:desktop
```

See [Installation](docs/INSTALLATION.md) for desktop and plugin-only setup, and
[Compatibility](docs/COMPATIBILITY.md) for the exact Hermes feature boundary.
See [Provider Gateway](docs/PROVIDER_GATEWAY.md) for routing and security limits.

### Trust model

CODETAS never silently trusts hooks, commits credentials, or edits Hermes
source files. Local CLI logins may be imported into the user-owned auth store
outside the repository. Users review generated changes and use Codex's normal
hook trust flow.
See [Security](docs/SECURITY.md).

### Status

Pre-alpha. The repository contains runnable desktop, gateway, and plugin source,
but no signed or notarized binary release yet.

### License

Apache License 2.0. See [LICENSE](LICENSE).
