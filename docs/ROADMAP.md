# Roadmap

> **Current product shape (August 2026):** CODETAS is a local-first Codex
> companion with three cooperating surfaces: the project-context bridge, the
> Codex plugin, and the optional local provider gateway. The 0.1–0.3 items
> below are the historical delivery sequence; the operational expansion is
> now part of the product rather than an unreleased side track.

## 0.1 — Read-only bridge

- Desktop project registration and inspection
- Reviewable synchronization plans
- Repository-local Codex plugin
- Safe `SessionStart` context loader
- Read-only project and skill MCP tools

## 0.2 — Independent provider gateway

- Responses-compatible loopback endpoint
- Responses passthrough adapter
- Chat Completions request/response/SSE adapter
- Environment-variable credential references
- Reviewable provider management UI
- Backed-up Codex user configuration updates

## 0.3 — Project activation

- Install and update the bundled plugin from the desktop app
- Per-project enablement state
- Hook trust guidance and health checks
- Exportable sync-plan artifacts

## Implemented after the original 0.4 plan

- Anthropic Messages and Gemini generateContent adapters
- 40-plus built-in and custom provider presets
- Live model discovery and Codex catalog publication
- Failover, weighted, least-usage, and account-pool routing
- Read-only OS keychain references and bounded command credentials
- Optional local admission token from `CODETAS_GATEWAY_TOKEN`
- Ownership-aware Codex configuration restoration
- DNS resolution validation and connection pinning
- Connection probes, doctor diagnostics, response limits, and idle timeouts
- Standalone gateway executable and conflict-safe integration cleanup

## Implemented operational expansion

- Google Vertex/Cloud Code Assist, Kiro, and GitHub Copilot native transports
- Verified interactive desktop OAuth with CODETAS-owned refresh in the user auth store
- launchd, systemd user, and Task Scheduler registration with restart supervision
- Gemini-compatible inbound API and scoped sidecar API
- Crash-recoverable usage/cost checkpoints, breakdowns, follow, reversible trash, and graceful flush
- Claude Desktop four-family profile and ownership-marked client launchers
- Signed artifact download, cross-manifest verification, gateway restoration, and app restart

## Next

- CI conformance fixtures and replay coverage for every provider wire
- Additional standard quota-header normalization without credential-bearing probe traffic
- Provider-neutral adapter SDK
- Reviewed artifact caching only if a bounded ownership and deletion contract is accepted

## Later

- Approved durable-memory promotion
- Read-only Hermes memory search through MCP
- Explicit adapters for scheduled work and subagent patterns
- Signed desktop releases and automatic update metadata
- Additional in-app OAuth providers using the same user-owned auth store

Messaging gateways, credential migration, silent hook approval, and full
runtime/session mirroring are not planned as automatic synchronization.
