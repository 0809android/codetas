# Architecture

CODETAS is split into independently useful layers.

```text
Desktop UI
    │
    ├── project inspection ──> shared core
    ├── provider settings ───> CODETAS Gateway ───> upstream model APIs
    │                              ▲
    │                              └── Codex Responses API
    └── integration status ──> Codex plugin
                                   ├── lifecycle hooks
                                   ├── skills
                                   └── local MCP server
```

## Desktop app

The Tauri shell owns native folder selection and read-only project inspection.
The TypeScript/Vite UI stores the user's project list locally and presents sync plans.

## Shared core

The core expresses provider-neutral inspection results and synchronization
plans. Source adapters can be added without changing the UI contract.

## Codex plugin

The plugin is the Codex-facing runtime. Its `SessionStart` hook discovers and
injects `.hermes.md` or `HERMES.md` plus a frozen Hermes profile memory
snapshot and a `skills/user` index. `UserPromptSubmit`, `PostToolUse`, and
`Stop` run the profile-scoped learning loop: turn-based memory nudges, observed
tool-unit or turn-based skill nudges, an early six-turn checkpoint, and a Stop
continuation review. The MCP `memory` and `skill_manage` tools require the
session `scopeToken` and write only that profile's `memories/` and
`skills/user/`. Unresolved profile identity never falls back to default. The
media generation tool may create a provider-owned result, but does not write to
the inspected project.

## Provider gateway

The Rust gateway binds to `127.0.0.1:42421` by default. Explicit remote binding
requires admission authentication. Codex sees a Responses API provider named
`openai`, with its built-in transport redirected through the loopback
`openai_base_url` override, so
existing desktop history remains under the native provider identity. Routes use
`provider/model`; native OpenAI model IDs remain unprefixed and other
unprefixed IDs use the configured default provider.

Provider adapters are protocol modules, not UI branches. Responses passthrough,
OpenAI Chat Completions, Anthropic Messages, and Gemini generateContent are
implemented. A separate routing runtime selects providers and account
references without inspecting prompt content.

OpenAI Chat Completions and Anthropic Messages are also exposed as client
protocols in front of the same Responses routing core. Capability-checked
sidecar aliases and subagent-aware model rosters are resolved by the routing
layer, not by the desktop UI.

Provider definitions persist in the platform application-data directory as
`providers.json`. That file contains environment-variable names, keychain
references, OAuth provider ids, and reviewed command metadata — never API-key
or OAuth-token values. Access and refresh tokens for Kimi, Claude, and xAI live
in the sibling user file `auth.json` (mode 0600). The desktop can embed the
gateway, while the standalone binary provides the service boundary.

## State ownership

- Hermes source files remain project-owned.
- Codex configuration remains user-owned.
- CODETAS runtime state belongs under the platform application-data directory.
- Provider credentials remain in the process environment, a user-managed OS
  credential store, an external command broker, or the user-owned CODETAS auth
  store. `providers.json` stores references only.
- Repository-local plugin files are distributable source, not user state.
- Generated client launchers and Claude Desktop MCP fragments are CODETAS state
  with explicit ownership markers; existing client configs remain user-owned.
