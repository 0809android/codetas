# Feature parity

CODETAS independently implements the user-facing behavior observed in the
reference 2.10.0 tag. It does not invoke, embed, or require external software.
This page is its compact product view.

Status values are `implemented`, `partial`, `alternative`, `missing`, and
`excluded`. A schema or UI placeholder is never counted as implemented.

## Implemented

| Area | Current CODETAS behavior |
| --- | --- |
| Project sync | Read-only Hermes discovery, reviewable project-scoped Codex plan, and Codex profile conversion that embeds SOUL plus a fail-closed learning-loop contract (frozen snapshot, scopeToken writes, Stop continuation reviews) |
| Core protocols | Responses, Chat Completions, Anthropic Messages, Gemini generateContent; JSON/SSE plus Responses and Realtime WebSockets |
| Compaction | Native forwarding plus bounded local envelopes for translated providers |
| Special endpoints | Image generation/edit, search, video submit/status/wait, Realtime call creation, and sideband relay |
| Models | Bounded discovery, enable/disable, custom metadata, client-flavored model lists, Codex catalog publication |
| Routing | Provider namespaces, aliases, failover, weighted round robin, least usage, sticky selection, context/output caps |
| Credentials | Environment, keychain references, CODETAS-owned OAuth store with refresh and Kimi/Claude/Grok CLI import, command/Copilot tokens, caller-header forwarding |
| Accounts | Multi-account references, manual pin/auto mode, quota/round-robin/fill-first strategies, cooldown |
| Agents | Primary/subagent effort caps, model roster/fallback, v1/default/v2 catalog surface, helper interception, evaluation shadows |
| Clients | Codex, Claude Code, OpenCode, Grok launchers; Claude Desktop four-family MCP/inference fragments; review-only Pi fragment; Gemini client API |
| Lifecycle | Desktop and standalone gateway, launchd/systemd/Task Scheduler, launch shim, ownership journal, restore/uninstall, tray controls |
| Security | Redirect-free clients, no ambient proxy, optional DNS pinning, private-target opt-in, scoped admission, exact CORS allowlist, bounded bodies |
| Operations | Liveness/readiness identity, crash-recoverable usage/cost totals, breakdown/follow/trash restore, supervised services, verified signed updates |
| Management CLI | Provider, account, model, route, agent/sidecar, access, observe, system, and config families with validated atomic saves |

## Partial or safety alternatives

| Area | Remaining or intentionally different behavior |
| --- | --- |
| Quota | Safety alternative: 429, Retry-After, and standard headers affect routing without speculative credential-bearing vendor polling |
| Agent injection | Rosters, fallback, effort caps, and trusted turn metadata exist; hidden Codex prompt/default rewriting is intentionally excluded |
| Hosted tools | Dedicated sidecars are implemented; generated media remains provider-owned instead of being copied into an unbounded local artifact cache |
| CLI | Settings and observation/follow families exist; in-app OAuth and OS service mutation remain desktop-owned operations |
| Memory | WebSocket context, routing maps, credential/job caches, frames, and bodies are independently bounded; limits fail closed per surface |

## Deliberate provider boundary

Google Vertex/Cloud Code Assist, Kiro native event/tool wire, and GitHub
Copilot's transient session exchange are implemented independently. Cursor-
style execution of an untrusted local binary and Codex
resume-history rewriting remain outside the CODETAS safety boundary unless a
separate, reviewable design establishes safe ownership and rollback.

## Definition of complete

Parity is complete only when every `partial` or `missing` row in the detailed
audit is implemented, converted to a documented usable alternative, or
explicitly accepted as excluded for a concrete safety reason. CI/CD is the
verification source of truth for configuration migrations and protocol
fixtures.
