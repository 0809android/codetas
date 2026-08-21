# CODETAS

> Codexに、できることを足す。

[English](README.md)

CODETAS は Codex 向けのローカル完結型コンパニオンです。次の3面で動きます。

1. プロジェクトコンテキストの橋渡し
2. Codex プラグイン
3. 任意のローカルプロバイダーゲートウェイ

Codex はループバックの Responses エンドポイントに1つ接続します。CODETAS が `provider/model` を Responses / Chat Completions / Anthropic Messages / Gemini などへ振り分けます。外部プロキシの実行・組み込み・依存はありません。

このリポジトリから配るのはデスクトップ管理アプリと Codex プラグインです。ゲートウェイはアプリ内蔵でも単体バイナリでも動かせます。プラグインだけでも使えます。

CODETAS は独立したコミュニティプロジェクトです。OpenAI や Nous Research の製品ではなく、提携・承認関係もありません。

## ステータス

プレアルファのソース配布です。デスクトップ・ゲートウェイ・プラグインはリポジトリから実行できます。署名・公証済みのバイナリリリースはまだありません。

## できること

- ローカルプロジェクトから `.hermes.md` / `HERMES.md`、`AGENTS.md`、スキル、MCP 設定を検出し、元ファイルを変えずに同期プランをプレビューする。
- レビュー可能な `SessionStart` フックで Hermes のプロジェクトコンテキストを Codex に読み込む。検査とメディア（画像、サンプリング動画、PDF/OCR、画像生成）は MCP 経由。
- 既存の CLI ログイン（Kimi / Claude / Grok / Muse / Qwen / GLM / MiniMax など）をユーザー所有の auth ストアへ取り込むか、アプリからログインする。API キーの値は `providers.json` に保存しない。
- Codex を `http://127.0.0.1:42421/v1` 経由で接続し、モデルカタログを公開する。フェイルオーバー、重み付け、最小使用量、アカウントプールの経路を使える。
- Claude Code、Claude Desktop MCP、OpenCode、Grok からも同じ経路を再利用する（元クライアントは置き換えない）。
- 必要なら launchd / systemd ユーザーユニット / Windows Task Scheduler でゲートウェイを常駐させる。

正確な挙動と残ギャップは[機能表](docs/FEATURE_PARITY.md)。

## 使い方

Node.js 22、npm 10、Rust stable、Tauri 2 のホスト前提条件が必要です。プラグイン連携には Codex Desktop または CLI が必要です。Hermes Agent は任意です。

```bash
git clone https://github.com/0809android/codetas.git
cd codetas
npm install
npm run dev:desktop
```

そのあと:

1. **接続**を開く。既存 CLI ログインの取り込み、アプリからのログイン、または API キー参照の追加。
2. Codex へ接続する。CODETAS はユーザー設定をバックアップしてから、ローカルゲートウェイへ向ける。
3. 新しい Codex セッションを開始し、`provider/model` を使う。
4. 必要ならプロジェクトを追加し、同期プランを確認して、`.agents/plugins` のリポジトリローカルプラグインを入れる。

経路を使う間は CODETAS を起動したままにしてください。カタログ変更後は Codex を完全終了して開き直し、モデル一覧を再読み込みします。

Web UI のみ: `npm run dev`。プラグイン単体と常駐化は[インストール](docs/INSTALLATION.md)。

## 構成

```text
apps/web                 TypeScript/Vite 管理画面
apps/desktop             Tauri 2 デスクトップシェル
packages/core            検査と同期プランの型
crates/codetas-gateway   ローカル Responses / プロバイダーゲートウェイ
plugins/codetas          Codex プラグイン
.agents/plugins          ローカル開発用マーケットプレイス登録
docs                     製品・アーキテクチャ・セキュリティのメモ
```

## 信頼モデル

フックの黙認、資格情報のコミット、Hermes ソースの編集はしません。変更はユーザーが確認し、Codex 標準のフック信頼フローを使います。[セキュリティ](docs/SECURITY.md)を参照。

## ドキュメント

- [インストール](docs/INSTALLATION.md)
- [互換性](docs/COMPATIBILITY.md)
- [プロバイダーゲートウェイ](docs/PROVIDER_GATEWAY.md)
- [Compatibility Lab（日本語）](docs/COMPATIBILITY_LAB.ja.md)
- [コントリビューション](CONTRIBUTING.md)

## ライセンス

Apache License 2.0。[LICENSE](LICENSE) を参照。
