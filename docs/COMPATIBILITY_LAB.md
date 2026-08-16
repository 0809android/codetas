# Compatibility Lab and policy routing

CODETAS keeps provider compatibility as an executable contract rather than a
model-name heuristic. The provider registry declares protocol capabilities,
and CI runs positive and negative request fixtures through every preset's
Responses, Chat Completions, Anthropic, or Gemini adapter.

The desktop Routing page and `GET /v1/compatibility` expose the same result
matrix read-only. `GET /v1/routes/dry-run?model=<route>` shows every configured
target and account, including excluded candidates and reasons such as missing
capabilities, price limits, disabled or paused accounts, cooldown, and quota.
The result rank preserves the configured candidate position while `selected`
uses the exact next strategy decision. Evaluation runs on a runtime snapshot;
it does not advance round-robin cursors or clean live failure/cooldown state.
These endpoints require `compatibility:read` or `routing:read` when scoped
external access is used.

Translations: [日本語運用ガイド](COMPATIBILITY_LAB.ja.md) and a
[한국어 운영 요약](COMPATIBILITY_LAB.ko.md). This English document is the
canonical complete contract; the Korean page is intentionally a concise guide.

## Settings v2 additions

The settings file remains version 2. New fields use defaults during backfill;
future versions and registry revisions still fail closed.

- `catalog.selectedModels` is the public allowlist. Empty means all generated
  models. `catalog.modelPickerOrder` controls stable picker priority. Native
  OpenAI slugs and their `openai/`-qualified IDs are aliases of the same model
  on catalog surfaces; route IDs and aliases remain exact-match identifiers.
- Provider capabilities include structured output, service tier, custom tools,
  tool search, MCP namespaces, and provider-owned opaque metadata.
- Provider limits separate request pacing, transport retry, 429 retry, and
  empty-completion retry. Pacing uses a provider-wide shared next-slot queue,
  so concurrent requests do not resume and start upstream together.
- Policy routes combine required capabilities with health, cost, quota, and
  context weights and optional price ceilings.
- Accounts support priority, pause deadline, and one pinned account per
  provider. Pin is the explicit first choice; otherwise quota, round-robin, and
  fill-first ordering applies within one priority tier before lower tiers are
  used as failover. Credentials remain references; literal API keys are prohibited.
- `runtime.memoryBudgetBytes` and `runtime.maxInflightRequests` bound admission.
  `GET /v1/management/memory` reports content-free counters.
  Changing the memory budget restarts an embedded Gateway transactionally so
  the Axum body limit and reported effective limit remain identical; failure
  restores the prior settings, catalog, and runtime. `/healthz` and `/readyz`
  bypass inference admission so capacity exhaustion cannot hide process health.

`/healthz` is process liveness. `/readyz` additionally requires an enabled
provider, a usable default, and a synchronized non-empty published catalog.

## Management and client ownership

Desktop exposes a field-scoped management IPC for catalog ordering, memory
limits, and account priority/pause/pin changes. Each call reloads the latest
validated settings and changes only the fields present in the patch; the static
registry migration revision is never treated as a settings-generation lock.
Full provider edits still use the validated settings transaction and rollback path.

OpenCode, Pi, Claude Desktop, and Hermes integration outputs carry a CODETAS
ownership marker and only describe the fields CODETAS owns. CODETAS does not
read or rewrite unrelated client configuration. The OAuth descriptor registry
is available in the desktop UI and with `codetas-gateway provider
oauth-registry`; adding a provider extends that registry without changing the
credential-store boundary.

Signed reasoning metadata from Anthropic, Gemini, and Kiro is retained only in
provider-owned metadata fields needed for replay. Observability remains
content-free and never stores prompts, API keys, OAuth tokens, or signatures.
