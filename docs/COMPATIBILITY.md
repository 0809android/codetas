# Hermes compatibility

CODETAS adapts compatible project knowledge; it does not emulate the entire
Hermes runtime.

| Hermes capability | CODETAS MVP | Codex integration | Safety boundary |
| --- | --- | --- | --- |
| `.hermes.md` / `HERMES.md` | Supported | `SessionStart` context | Size capped and injection-scanned |
| Project `SKILL.md` files | Detected | Compatibility review | Never copied automatically |
| MCP configuration | Detected | Conversion plan | Credentials and environment values excluded |
| Persistent Hermes memory | Not synchronized | Future read-only semantic bridge | No database copying |
| Global Hermes profile | Convert to Codex profiles | Generates `~/.codex/agents/*.toml` | Hermes source is read-only; existing user-owned Codex profiles are never overwritten |
| Cron and scheduled jobs | Not supported | Future explicit task adapter | No background activation |
| Messaging gateways | Not supported | Out of MVP | No account or token import |
| Hermes runtime sessions | Not synchronized | Codex owns its sessions | No transcript mirroring |
| Provider-specific tool calls | Review required | Adapt as a Codex skill or MCP tool | Per-tool approval and testing |

`AGENTS.md` is detected so the UI can show that native Codex project guidance
already exists. CODETAS does not replace or duplicate it. Explicit user
requests, `AGENTS.md`, and higher-priority Codex instructions take precedence
over imported Hermes guidance.

## Model provider compatibility

| Upstream protocol | Current support | Notes |
| --- | --- | --- |
| OpenAI Responses API | Supported | Streaming responses pass through without translation |
| OpenAI Chat Completions | MVP support | Text, images, function tools/results, streaming deltas, tool calls, and usage |
| Anthropic Messages | Supported | Text, images, signed thinking history, function tools/results, usage, and SSE translation |
| Google Gemini generateContent | Supported | Parts, images, function IDs, thought signatures, usage, and SSE translation |
| OpenAI Chat client endpoint | Supported | JSON/SSE, text, images, function tools/results, reasoning and usage |
| Anthropic client endpoint | Supported | JSON/SSE Messages, images, thinking, tools/results, usage and count-token estimate |
| Local OpenAI-compatible servers | MVP support | Requires explicit private-network opt-in |
| OAuth account token | Supported | Imports local CLI logins, runs in-app OAuth for Kimi/Claude/xAI, and refreshes tokens from the user-owned CODETAS auth store |
| Google Vertex / Cloud Code Assist | Supported | Project/location endpoints and Cloud Code Assist envelopes use externally brokered credentials |
| Kiro native transport | Supported | AWS event-stream validation, images, function tools/results, and explicit final-answer control |
| GitHub Copilot transport | Supported | A user-owned GitHub credential is exchanged for a short-lived, memory-only Copilot session |
| Account pools and failover | Supported | Sticky account selection, 429 and server-error failover, and cooldown |
| Quota telemetry | Safety alternative | Provider 429, Retry-After, and standard remaining/limit headers drive account switching without speculative authenticated quota probes |

The gateway is an independent CODETAS implementation. It does not call or
bundle OpenCodex.
