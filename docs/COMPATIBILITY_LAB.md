# Compatibility Lab and policy routing

CODETAS keeps provider compatibility as an executable contract rather than a
model-name heuristic. The provider registry declares protocol capabilities,
and CI runs positive and negative request fixtures through every preset's
Responses, Chat Completions, Anthropic, or Gemini adapter.

The desktop Routing page and `GET /v1/compatibility` expose the same result
matrix read-only. Every row is the pass, fail, or skip result of a local pure
fixture executed against the current provider protocol, effective capabilities,
normalizers, repair policies, pacing queue, and retry policy. It never performs
a production upstream probe. `GET /v1/routes/dry-run?model=<route>` shows every configured
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
  tool search, MCP namespaces, and provider-owned opaque metadata. Registry
  presets declare these adapter capabilities explicitly. Omitted advanced
  capabilities on legacy or custom providers default to false, and tool
  sub-capabilities cannot be enabled when the base `tools` capability is off.
- Provider limits separate request pacing, transport retry, 429 retry, and
  empty-completion retry. Pacing uses a provider-wide shared next-slot queue,
  so concurrent requests do not resume and start upstream together.
- Policy routes combine required capabilities with health, cost, quota, and
  context weights and optional price ceilings. Health is derived from each
  candidate's consecutive-failure and active-cooldown state, and the same
  percentage is exposed by route dry-run.
- Accounts support priority, pause deadline, and one pinned account per
  provider. Pin is the explicit first choice; otherwise quota, round-robin, and
  fill-first ordering applies within one priority tier before lower tiers are
  used as failover. Credentials remain references; literal API keys are prohibited.
- `runtime.memoryBudgetBytes` and `runtime.maxInflightRequests` bound admission.
  `GET /v1/management/memory` reports content-free counters.
  Admission measures the decoded streamed body instead of trusting compressed
  `Content-Length`; decoded chunks reserve the shared budget atomically before
  the request is rebuilt for the handler. The admission reservation remains
  attached to the returned HTTP body until that body completes, errors, or is
  dropped, including SSE responses; data frames, errors, and trailers are passed
  through unchanged.
  The admission middleware is the sole request-body limiter and reads the
  current budget for every request, so a field-scoped memory-budget update
  changes the effective limit without relying on a stale Axum startup limit.
  Desktop may still use its transactional embedded-Gateway restart path and
  rollback, while direct `GatewayHandle` updates apply the same dynamic limit.
  `/healthz` and `/readyz` bypass inference admission so capacity exhaustion
  cannot hide process health.

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
provider-owned metadata fields needed for replay. Gemini signatures are emitted
at the canonical content-part level; legacy nested signatures are read only as
a compatibility fallback. Anthropic-compatible EOF tolerance flushes a complete
undelimited final SSE frame through the strict parser, but rejects truncated
JSON. Observability remains
content-free and never stores prompts, API keys, OAuth tokens, or signatures.

Ordinary Responses snapshot repair reconstructs indexed items from a complete,
untainted `output_item.added` lifecycle only when terminal `output` is missing or
is not an array. An explicit array is authoritative, including `[]` and partial
arrays; ordinary continuation repair never merges collected items into it.
Compaction has a separate, endpoint-specific partial-output merge contract and
must not be used to justify ordinary Responses repair. Lifecycle conflicts,
index gaps, oversized collectors, and open non-injectable tool items remain
fail-closed. For byte-preserving Responses passthrough, already-canonical frames
remain exact and only frames requiring an enabled compatibility repair are
reserialized.
