# CODETAS

> Add what Codex can do.

[日本語](README.ja.md)

CODETAS is a local-first companion for Codex. It has three pieces:

1. a project-context bridge
2. a Codex plugin
3. an optional local provider gateway

Codex talks to one loopback Responses endpoint. CODETAS routes `provider/model` requests to Responses, Chat Completions, Anthropic Messages, Gemini, and other configured transports. It does not invoke, embed, or require a third-party proxy.

This repository ships a desktop management app and a Codex plugin. The gateway can run inside the app or as a standalone binary. The plugin also works on its own.

CODETAS is an independent community project. It is not an OpenAI or Nous Research product, and is not affiliated with or endorsed by either company.

## Status

Pre-alpha source. The desktop app, gateway, and plugin are runnable from this repository. There is no signed or notarized binary release yet.

## What it does

- Inspect a local project for `.hermes.md` / `HERMES.md`, `AGENTS.md`, skills, and MCP config, then preview a sync plan without changing the source.
- Load Hermes project context into Codex through a reviewable `SessionStart` hook, plus read-only MCP tools for inspection and bounded media (image, sampled video, PDF/OCR, image generation).
- Sign in or import existing CLI logins (Kimi, Claude, Grok, Muse, Qwen, GLM, MiniMax, and others) into a user-owned auth store. API keys stay as references, not values in `providers.json`.
- Route Codex through `http://127.0.0.1:42421/v1`, publish a model catalog, and run failover / weighted / least-usage / account-pool routes.
- Reuse the same routes from Claude Code, Claude Desktop MCP, OpenCode, and Grok without replacing those clients.
- Optionally keep the gateway running via launchd, systemd user units, or Windows Task Scheduler.

Exact behavior and remaining gaps: [feature table](docs/FEATURE_PARITY.md).

## Quick start

Needs Node.js 22, npm 10, stable Rust, and Tauri 2 host prerequisites. Codex Desktop or CLI is required for plugin integration. Hermes Agent is optional.

```bash
git clone https://github.com/0809android/codetas.git
cd codetas
npm install
npm run dev:desktop
```

Then:

1. Open **Connections**. Import an existing CLI login, sign in from the app, or add an API-key reference.
2. Connect Codex. CODETAS backs up the user-level config, then points Codex at the local gateway.
3. Start a new Codex session and use `provider/model`.
4. Optionally add a project, review the sync plan, and install the repository-local Codex plugin from `.agents/plugins`.

Keep CODETAS running while using the route. After catalog changes, fully quit and reopen Codex so the model picker reloads.

Web UI only: `npm run dev`. Plugin-only and service setup: [Installation](docs/INSTALLATION.md).

## Layout

```text
apps/web                 TypeScript/Vite management UI
apps/desktop             Tauri 2 desktop shell
packages/core            Inspection and sync-plan types
crates/codetas-gateway   Local Responses / provider gateway
plugins/codetas          Codex plugin
.agents/plugins          Local marketplace entry
docs                     Product, architecture, and security notes
```

## Trust model

CODETAS does not silently approve hooks, commit credentials, or edit Hermes source files. Users review generated changes and use Codex's normal hook trust flow. See [Security](docs/SECURITY.md).

## Docs

- [Installation](docs/INSTALLATION.md)
- [Compatibility](docs/COMPATIBILITY.md)
- [Provider gateway](docs/PROVIDER_GATEWAY.md)
- [Compatibility Lab](docs/COMPATIBILITY_LAB.md)
- [Contributing](CONTRIBUTING.md)

## License

Apache License 2.0. See [LICENSE](LICENSE).
