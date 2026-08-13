# CODETAS

> Codexに、できることを足す。

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

## Implemented source scope

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

## Repository layout

```text
apps/web                 TypeScript/Vite management interface
apps/desktop             Tauri 2 desktop shell
packages/core            Provider-neutral inspection and sync-plan types
crates/codetas-gateway   Independent local Responses/provider gateway
plugins/codetas          Installable Codex plugin
.agents/plugins          Repository marketplace entry for local development
docs                     Product, architecture, and security notes
```

## Development prerequisites

- Node.js 22 and npm 10
- Rust stable and the Tauri 2 host prerequisites
- Codex desktop app or Codex CLI for plugin integration
- Hermes Agent is optional; CODETAS can inspect compatible project files
  without launching Hermes

Common commands are documented for contributors, but local verification is not
run by the agent in this workspace. CI/CD is the verification source of truth.

```bash
npm install
npm run dev
npm run dev:desktop
```

See [Installation](docs/INSTALLATION.md) for desktop and plugin-only setup, and
[Compatibility](docs/COMPATIBILITY.md) for the exact Hermes feature boundary.
See [Provider Gateway](docs/PROVIDER_GATEWAY.md) for routing and security limits.

## Trust model

CODETAS never silently trusts hooks, commits credentials, or edits Hermes
source files. Local CLI logins may be imported into the user-owned auth store
outside the repository. Users review generated changes and use Codex's normal
hook trust flow.
See [Security](docs/SECURITY.md).

## Status

Pre-alpha. The repository contains runnable desktop, gateway, and plugin source,
but no signed or notarized binary release yet.

## License

Apache License 2.0. See [LICENSE](LICENSE).
