# Provider gateway

CODETAS Gateway is an independent local compatibility layer for Codex model
providers. It does not depend on a third-party proxy process.

## Client contract

The desktop app starts the gateway at `http://127.0.0.1:42421/v1` and
automatically converges the user-level Codex configuration to:

```toml
model_provider = "openai"
model = "local/code-model"
openai_base_url = "http://127.0.0.1:42421/v1"
model_catalog_json = "/Users/example/.codex/codetas-model-catalog.json"
```

`openai_base_url` is the loopback Design-B transport override used by Codex
Desktop's built-in provider. The integration keeps the native `openai` provider identity, so
existing and new desktop sessions remain in one thread list without rewriting or
moving any session JSONL. On upgrade, a recognized CODETAS-owned legacy
`model_providers.codetas_gateway` table and matching `chatgpt_base_url` are
removed automatically. User-owned provider or base-URL settings are never
silently overwritten. Qualified `provider/model` entries route through CODETAS;
native OpenAI models use the caller's Codex login. This mode refuses remote binds
and local admission-token settings.

The published Codex catalog keeps native GPT slugs unqualified so the Codex App
picker can show the same **Fast** speed toggle as the official models cache
(`additional_speed_tiers = ["fast"]`, service tier id `priority`). Routed models
keep `provider/model` slugs and do not advertise that ChatGPT-only tier. After
CODETAS writes `codetas-model-catalog.json`, ChatGPT / Codex App must be fully
quit and reopened before the picker reloads the new list.

The CODETAS model screen itself updates immediately when a model is published,
hidden, renamed, or regrouped. It also supports provider-wide publication
toggles and the catalog display-name formats `default`, `custom`, `modelId`,
`providerModel`, and `providerIdModel`. These settings are written to the
generated catalog immediately when automatic catalog sync is enabled. If
automatic sync is disabled, use **Sync to Codex** after changing these settings.
The existing Codex App picker remains subject to the reload behavior above
because its current session does not expose a catalog-refresh operation through
the integration.

On first connection, CODETAS scans GUI-safe executable locations (the process
`PATH`, `~/.local/bin`, `~/.cargo/bin`, vendor home bins such as
`~/.kimi-code/bin` and `~/.claude/bin`, Homebrew, and standard system
locations). Authenticated Codex stays on the existing forward-auth `openai`
provider. An authenticated GitHub CLI adds GitHub Models with an absolute-path
`gh auth token` command credential; that token is resolved only at request time
and is never written to `providers.json`. Existing provider definitions are
never replaced by CLI detection.

Kimi, Claude, and Grok logins are imported when present. CODETAS copies the
access and refresh tokens into the user-owned auth store (`auth.json` beside
`providers.json`, mode 0600), enables the matching provider (including an
existing disabled environment-variable stub that has no value), and refreshes
the session before expiry. If no local login exists, the Connections view **使う**
or **Kimi/Claude/Grokでログイン** button starts an in-app browser or device
login. Values never reach the renderer, logs, git, or `providers.json`.

The desktop uses one shared local-CLI registry for both its lightweight startup
scan and the manual **登録確認** action in the 接続 view. The startup scan checks
executable presence and version only, so opening CODETAS never sends a billable
model request. Loading settings may import an already-present CLI login without
calling the model. The manual scan may run a fixed, tool-free non-interactive
probe to distinguish installed clients from headless authentication failures,
quota exhaustion, and TTY-only clients. The registry currently covers Codex,
Claude Code, Grok, Antigravity (`agy`), Qwen Code, Kimi, OpenCode, Gemini,
Kiro, Muse, Z.AI / GLM, and MiniMax.

Codex project-local configuration cannot select or redefine this transport, so
the change belongs to the user configuration. CODETAS creates a private backup
and ownership journal before applying it. A pre-existing model catalog is backed
up as well. Restore reverts only unchanged CODETAS-owned fields/files, restores
the prior catalog when present, and reports user edits as conflicts.

## Routing

Models use `provider/model`. For example, a provider with ID `local` and model
`code-model` is exposed as `local/code-model`. Native OpenAI models remain
unprefixed and route to the forward-auth OpenAI definition; other unprefixed
models use the configured default provider. Virtual route IDs can apply ordered failover,
weighted round robin, least-usage selection, sticky accounts, quota thresholds,
and cooldown. A route may also include a human-readable `description`; CODETAS
shows it in the routing editor and publishes it as the Codex model description.

For example, a vision fallback route can document its purpose directly in the
configuration:

```json
{
  "id": "vision-fallback",
  "name": "画像認識フォールバック",
  "description": "画像認識の第一候補が利用できない場合に、次の候補へ自動で切り替えます。",
  "strategy": "failover",
  "targets": [
    { "model": "openai/gpt-vision-primary", "weight": 1 },
    { "model": "local/vision-backup", "weight": 1 }
  ],
  "enabled": true
}
```

The gateway exposes:

- `GET /healthz`
- `GET /readyz`
- `GET /v1/models`
- `POST /v1/responses`
- `GET /v1/responses` (WebSocket upgrade)
- `POST /v1/responses/compact`
- `POST /v1/chat/completions`
- `POST /v1/messages`
- `POST /v1/messages/count_tokens`
- `GET /v1beta/models`
- `POST /v1beta/models/{model}:generateContent`
- `POST /v1beta/models/{model}:streamGenerateContent`
- `POST /v1/images/generations`
- `POST /v1/images/edits` (JSON or bounded multipart form data)
- `POST /v1/alpha/search`
- `POST /v1/videos/generations`
- `GET /v1/videos/{request_id}`
- `POST /v1/sidecars/web-search`
- `POST /v1/sidecars/vision`
- `GET /v1/sidecars/config`
- `POST /v1/sidecars/video-analysis`
- `POST /v1/sidecars/document`
- `POST /v1/sidecars/image`
- `POST /v1/sidecars/video`
- `GET /v1/sidecars/video/{request_id}`
- `POST /v1/live`
- `POST /v1/realtime/calls`
- `GET /v1/live/{call_id}` (WebSocket upgrade)
- `GET /v1/realtime/calls/{call_id}` (WebSocket upgrade)
- `GET /v1/realtime?call_id=...` (WebSocket upgrade)

Before routing a Responses request, the gateway applies the selected model's
input budget with a modality-aware estimate. Ordinary JSON text, remote image
URLs, and image metadata use the conservative byte-based text estimate. A
syntactically valid `data:image/...;base64,` payload is replaced by a bounded
marker for text measurement and charged a separate capped image cost based on
`detail`; malformed data URLs remain ordinary text. Responses-lite
`additional_tools` items remain excluded. Budget rejections report text and
image estimates, image count, total, limit, provider, and model without logging
prompt or image content. Subagent history shrinking uses the same estimator.

## Adapters

`responses` forwards a Responses request and event stream after replacing the
routed model ID.

`chatCompletions` converts Responses instructions, messages, images, function
tools, function calls, and tool outputs to Chat Completions. JSON responses and
SSE text/tool-call deltas are converted back into Responses objects and events
with monotonic sequence numbers. Empty Chat `content` deltas do not create empty
assistant messages. When a Chat response contains both visible assistant text
and tool calls, Responses stores them as adjacent output items; replay merges
those adjacent items back into one Chat assistant message before the matching
tool result. This preserves provider-required reasoning content on the same
message as `tool_calls` and prevents strict providers such as Kimi from
rejecting the continuation history. For Codex clients, when a translated model
repeatedly returns reasoning plus tool calls without visible content, the
gateway emits a short Responses `output_text` progress message on the first
such turn and then at a bounded cadence. The message is fixed text and never
incorporates reasoning or tool arguments. Empty-content tool-only turns are
counted in-process and recorded in the opt-in debug log without response
content.

`anthropicMessages` converts content blocks, images, function tools/results,
signed thinking history, usage, and SSE events. `geminiGenerateContent`
converts parts, images, function IDs, thought signatures, usage, and SSE events.
The built-in `meta` preset targets Meta Model API at `https://api.meta.ai/v1` over
Chat Completions, with Muse Spark models, a 1M context window, and `none`
reasoning mapped to `minimal`. Saved `meta-ai` / `muse` IDs keep the same contract.

For translated providers, Codex app/plugin connector namespaces are flattened to
stable `namespace__tool` wire names and restored to Responses `function_call`
items with their original `namespace`. Distinct tools that flatten to the same
name receive a deterministic suffix instead of being dropped. Custom tools use
a reversible `{input}` wrapper, and client-executed `tool_search` calls retain
their type while dynamically returned tools are re-injected for the next turn.
Kiro uses its native event-stream wire with CRC and size validation. Kiro API
keys and profile authentication use distinct headers/origin fields; client
tool calls are validated and a private completion tool prevents progress text
from being mistaken for the final answer. GitHub Copilot exchanges a user-
owned GitHub bearer credential for a short-lived session token kept only in a
bounded, zeroized process cache. Vertex and Cloud Code Assist use their native
project/location and envelope forms.

Tool-result images are carried on each provider's native vision surface instead
of being serialized as giant text tool outputs: Chat Completions receives a
follow-up user image message after the tool-result group, Anthropic receives
image blocks inside `tool_result`, Gemini receives `inlineData`/`fileData`
parts beside `functionResponse`, and Kiro receives native image entries on the
same user turn. Before translation, CODETAS applies an age-ordered image
history budget. Newer inline images are preserved, while older Base64 images
whose encoded payload exceeds their tier are first re-encoded as progressively
smaller PNGs (newest six at up to 2000px,
the next fourteen at up to 1024px, and older images at up to 700px), then
replaced with explicit text markers only when the selected provider's
`maxRequestBytes` and protocol budget still cannot be respected. Non-PNG image
formats retain their original bytes until the budget requires omission.
Anthropic additionally
uses its 5 MiB per-image and 20 MiB aggregate image guards; Gemini and Kiro use
their aggregate inline-image guards. A final serialized-size pass may omit
additional oldest images when JSON framing, instructions, or tool schemas still
push the request over `maxRequestBytes`. If an upstream still returns HTTP 413,
the gateway performs one provider-neutral tightened retry with one additional
oldest image omitted; non-image 413 responses are returned without retry.

Azure OpenAI providers can set `azureDeployment` and `azureApiVersion` instead
of embedding deployment paths or query strings in `baseUrl`. CODETAS validates
the Azure resource host, constructs the Responses/Chat deployment path, and
adds `api-version` only as a request query parameter. The preset uses Azure's
`api-key` credential header without persisting the value.

Kiro text-only streams are decoded frame-by-frame and emitted as Responses
`output_text.delta` events. Tool-enabled turns remain bounded until the private
completion/tool-call contract has been validated, preventing progress text from
being exposed as a final answer.

Unsupported hosted tools and stateful response references are rejected rather
than silently omitted or emulated.

OpenAI uses remote compaction on both credential paths. Public API-key
providers send dedicated requests to `/responses/compact`. The OpenAI
Codex-login forwarding provider sends `compaction_trigger` to its normal
Responses endpoint because the subscription backend does not expose the
public API path. All non-OpenAI providers use bounded local synthetic
compaction, regardless of whether their normal protocol is Responses, Chat
Completions, Anthropic Messages, or Gemini generateContent. CODETAS returns a
local rolling summary with retained recent turns inside one `compaction`
output item for that synthetic path. New envelopes use the `codetas2:`
prefix; `codetas1:` and legacy `ocx1:` remain readable. The required
`encrypted_content` field is a versioned local transport envelope, not
ciphertext and not an authenticated instruction. CODETAS expands those
envelopes before every upstream hop into an assistant-handoff checkpoint
followed by retained recent turns: translated adapters, the normal
Responses sanitizer, synthetic compaction, and native OpenAI compact/trigger
forwarding. The native backends never receive a `codetas1:` or `codetas2:`
payload. Local generation can roll back to `codetas1:` without disabling the
v2 decoder. Synthetic compaction uses a checkpoint prompt and refuses to
install an empty, too-short, heading-invalid, or control-token-leaking
summary (`<|eos|>`, `<file_end>`, `<tool_call>`). Replayed local summaries
are framed so a later user message outranks the checkpoint if they disagree.
Opaque OpenAI `gAAAAA` blobs become a short unread-compaction note instead of
being forwarded to translated models. Synthetic compaction replaces image
content with a short marker so historical Base64 pixels do not consume the
compaction request budget.

The generated Codex model catalog derives `auto_compact_token_limit` from the
model's usable input budget, including configured input/output limits. For
Codex-login OpenAI models, the registry explicitly defines a 372,000-token
context and 272,000-token maximum input, leaving 100,000 tokens outside the
input budget without injecting a maximum-output value into normal requests.
CODETAS advertises 90% of the usable input budget. No token contract is inferred
from a model name or UI capability. Existing saved provider settings are
backfilled with missing registry input limits without overwriting explicit user
values. OpenAI Responses routes rely on provider usage and remote compaction
instead of a gateway token-admission estimate. Local-compaction routes use
estimation only
as a fail-open pathological-input guard at 2.5 times the usable input budget;
normal context management remains with Codex. Compaction requests bypass that
guard. Upstream context-window failures are normalized to the standard
`context_length_exceeded` shape so Codex can enter its compaction recovery path;
the normalizer accepts bounded structured errors from OpenAI-, Anthropic-,
Gemini-, and Kiro-style providers as well as bounded plain-text errors without
scanning echoed request content.

On the HTTP Responses path, Codex continues turns with `previous_response_id`
plus a delta `input`. Many upstreams reject that field, so CODETAS expands the
locally cached history before routing and strips the id after a successful
expand. When the cache misses, CODETAS does **not** fail closed with HTTP 400:
it drops the stale id, forwards the delta, and records the successful turn as a
new checkpoint so later turns are not permanently delta-only. Checkpointing
applies on `force_record` routes (translated protocols, Codex-login forward
auth, and stateless Responses), and is also forced for a locally recovered or
rebased continuation. The observability ledger records recovery kinds in
occurrence order; gateway-owned continuation recovery is de-duplicated, while
provider retry metadata may contain repeated kinds. `recoveryKind` keeps the
first kind (`continuation-rebase:{reason}` such as `unknown_id`,
`unavailable`, `empty`, `lease_limit`; or `continuation-lossy`). WebSocket
continuation is not tagged this way. OpenCodex-style fail-closed is
intentionally not used here because CODETAS owns the only continuation store
clients can rely on.

Virtual routes calculate each target's usable input budget independently before
taking the minimum, avoiding an artificially early threshold assembled from
limits that belong to different failover targets.

To prevent a successful tool call from consuming an entire context window,
the gateway also detects when the reconstructed history ends with eight or
more consecutive completed calls to the same function. Only that function is
removed from the next request, other tools remain available, and a short
synthetic user message instructs the model to continue without repeating it.
The guard is request-local and resets when a new real user message begins.

The client-facing Chat Completions and Anthropic Messages endpoints translate
requests into the same internal Responses flow, then translate JSON or SSE
results back to the calling protocol. They therefore share provider routing,
account pools, failover, usage accounting, limits, and content-free failure
observation. Unsupported fields such as multiple Chat choices or stop
sequences are rejected explicitly.

Configured sidecars appear as `codetas-sidecar/web-search`, `vision`,
`video-analysis`, `document`, `image`, `video`, and `live`. Saving settings verifies that every target in a
sidecar failover route advertises the required capability. Subagent requests
identified by Codex turn metadata can use an ordered model roster and fallback
list without persisting that metadata; primary and subagent reasoning effort
caps are applied before provider-specific translation.

The web-search sidecar requires a Standard Responses route with native hosted
web search, so a Chat-only route cannot be saved as a target. Vision validates
credential-free HTTPS or bounded inline image input. When the selected primary
model cannot accept images, `agents.imageInputMode=auto` delegates them to the
configured Vision sidecar and replaces each image part with its textual analysis.
`video-analysis` accepts bounded sampled frames, while `document` accepts bounded
rendered pages for PDF/OCR workflows. Each request accepts at most four items and
20 MiB of encoded media; the Codex plugin resizes and batches larger configured
frame/page counts before calling these endpoints. Native input mode returns the
original path to the active model instead of invoking the auxiliary route. Video generation can return a job
immediately or poll for up to ten minutes and exposes the provider-owned
artifact URL when complete; CODETAS does not retain third-party media bytes.
The non-secret sidecar config endpoint lets the Codex plugin honor the frame,
page, and OCR values saved in the app. The gateway itself enforces the selected
input modes and auxiliary timeout.
Video IDs are opaque, process-local CODETAS job IDs mapped to the exact
provider/account candidate. After a gateway restart an old job fails closed as
unknown instead of being polled against a possibly different provider.

Realtime call creation accepts bounded JSON or multipart requests. CODETAS
rewrites the configured model per route candidate and converts multipart SDP
to the ChatGPT backend JSON shape only for a reviewed backend URL. Only safe
protocol headers are relayed; response headers are limited to content type and
location.

Realtime sideband WebSockets connect to a pre-resolved, policy-checked address
while preserving the original hostname for TLS verification. Text, binary,
ping, and pong frames are relayed without content logging; frame/message,
write-buffer, connect, and idle bounds apply in both directions. A provider may
set `realtimeWsBaseUrl` for a reviewed alternate host. Plaintext WebSockets are
accepted only for an explicitly allowed private destination.

## Credentials

`providers.json` stores references only: an environment-variable name, a
keychain item id, an OAuth provider id, or a reviewed token command. It never
contains an API key, access token, or refresh token.

OAuth for Kimi, Claude, and xAI uses the public OAuth client IDs of the
official Kimi CLI, Claude Code, and Grok CLI (PKCE, no client secret). These
are bundled so existing CLI logins can be imported and refreshed without extra
setup. A deployment that registers its own OAuth app can override them with
`CODETAS_KIMI_CLIENT_ID`, `CODETAS_ANTHROPIC_CLIENT_ID`, or
`CODETAS_XAI_CLIENT_ID`. Token storage and refresh are owned by CODETAS.

| Source | What CODETAS does |
| --- | --- |
| Kimi CLI `~/.kimi-code/credentials/kimi-code.json` | Import on startup, then refresh |
| Claude Code Keychain or `~/.claude/.credentials.json` | Import on startup, then refresh |
| Grok CLI `~/.grok/auth.json` | Import on startup, then refresh |
| In-app login | Browser or device flow, then the same auth store |
| GitHub CLI | `gh auth token` at request time; token not stored |
| Google Cloud | `gcloud auth print-access-token` at request time |
| Google Antigravity CLI | Imports the `agy` secure-store session, refreshes through `agy models`, sends the required `antigravity` user agent, and resolves the managed Cloud Code Assist project through `loadCodeAssist`; tokens are not persisted in `providers.json` |
| Muse CLI `~/.config/muse/auth.json` | Import on startup. Prefer `api_key`, otherwise the Meta-account `access_token`. Re-read that file instead of a refresh token; values are not persisted in `providers.json` |
| Qwen Code `~/.qwen/settings.json` | Import the selected ModelStudio key and map `baseUrl` to Token Plan intl/Beijing, Coding Plan, or DashScope. GLM or MiniMax models billed on that same Alibaba key stay on the Qwen provider. The live request re-reads the file instead of keeping a stale copy |
| Z.AI / GLM | Import only a dedicated `ZAI_API_KEY` / `ZHIPU_API_KEY` / `GLM_API_KEY` from Qwen settings, or `api_key` from `~/.z.ai/auth.json` |
| MiniMax | Import only a dedicated `MINIMAX_API_KEY` from Qwen settings, or `api_key` from `~/.minimax/auth.json` |
| API key providers | Environment variable or keychain reference |

The auth store is `auth.json` next to `providers.json` (mode 0600, outside the
git repo). The gateway reads it at request time, refreshes before expiry, and
never returns the value to the renderer. Command credentials run directly
without a shell, have a timeout, discard stderr, and cap stdout at 64 KiB.
Account pools contain references only.

Uninstall removes the CODETAS-owned `auth.json` beside settings. Environment
variables must be visible to the desktop process. On macOS, an app launched
from Finder does not necessarily inherit a shell export; use the auth store,
keychain, or launch CODETAS from that shell.

## Runtime

The desktop embeds the gateway for the normal app flow. The same crate also
builds a standalone `codetas-gateway --config <providers.json>` executable for
external service managers. The desktop can explicitly register its own
headless mode with launchd, a systemd user unit, or Windows Task Scheduler.
Definitions contain only executable/config/data paths, are ownership-marked,
and use the platform restart policy. A gateway server-task exit is propagated
to the process so the supervisor can restart it. Registration and removal are
never automatic, and an unmarked same-name definition is left untouched.
Configured request and stream-start retry counts apply to every HTTP provider
relay, including native Kiro/Copilot and special endpoints. Retries are limited to
timeouts, 429, server failures, or transport failures, honor bounded
`Retry-After`, and rebuild authentication for each attempt.

Responses history translated to Chat Completions is assembled as complete tool
rounds: assistant text, reasoning, and parallel calls share one assistant
message; every matching tool result follows immediately; result images and
intervening user messages are released only after the round closes. Missing
results receive an explicit unknown-status tool result instead of leaving an
invalid dangling call. Anthropic, Gemini, and Kiro translations normalize
unambiguous call/result batches at the Responses-item layer. Native Responses
providers opt into the same adjacency normalization with
`requiresAdjacentResponsesToolResults`; the built-in DeepSeek preset enables it
and registry migration applies it to existing installations. Models listed in
`preserveReasoningContentModels` also retain a minimal reasoning field on
tool-call continuations when replayed reasoning is unavailable, preventing
strict thinking-mode endpoints from rejecting an otherwise complete round.
Providers can narrow that fallback with `requiresReasoningPlaceholderModels`;
an explicit empty array disables fabricated placeholders while preserving real
reasoning replay.

CODETAS atomically generates Codex's optional model catalog. Disabled model
metadata is omitted and virtual routes appear as selectable models. With
ownership-checked automatic synchronization enabled, provider, model, route,
and agent-surface saves publish the catalog in the same rollback boundary. Each
generated `base_instructions` must identify its own exact catalog slug; catalog
validation rejects an entry that claims another model. Every generated
`base_instructions` entry, and any optional `instructions_template`, also
includes a skill-and-investigation contract: skills stay available, a matching
`SKILL.md` may be read once per turn, and `create_thread` is not a substitute
for doing the work. The catalog sets `include_skills_usage_instructions` to
true so non-OpenAI models receive the same skill surface. The compatibility hash
also includes the display name, generated instructions, and optional
instruction template so Codex reloads corrected model identity metadata instead
of retaining stale cached metadata.

The standalone executable also exposes settings-file management families:
`provider`, `account`, `models`, `route`, `agent`, `access`, `observe`, and
`system`. Mutations validate the complete configuration, create an
owner-private backup, and atomically replace the settings file. Credential
commands accept references and broker metadata only; there is no plaintext
API-key option.

The desktop and standalone gateway keep a local observability ledger. Each
completed request stores only a random request ID, provider/model/route/account
identifiers, outcome, latency, attempt count, streaming flag, usage counters,
and an optional cost estimate derived from validated model-catalog prices.
Responses, Chat Completions, Anthropic, and Gemini usage are collected from
both JSON responses and terminal stream events. Prompt content, response
content, HTTP headers, and credential values are structurally absent. Owner-
private JSONL segments are pruned by retention days and total byte budget.
Lifetime totals use monotonic event sequences and per-segment byte checkpoints;
startup replays an uncheckpointed suffix and truncates only an incomplete final
record. SIGTERM, desktop stop, update, and uninstall paths gracefully drain the
server and reserve part of the configured shutdown timeout for pending writes.
The UI/CLI expose aggregate and daily/provider/model/surface breakdowns,
bounded debug scopes, follow mode, and reversible ownership-checked trash.

Local admission authentication is optional. When enabled, both processes read
`CODETAS_GATEWAY_TOKEN`; Codex sends it through `env_http_headers`, and CODETAS
compares it without persisting or logging the value. It is off by default until
the signed launcher can provision the environment consistently.

## Other clients

CODETAS generates optional, ownership-marked launchers in its own application
data directory. `codetas-claude` uses the Anthropic Messages endpoint,
`codetas-opencode` injects an in-memory OpenCode provider configuration, and
`codetas-grok` uses Grok's custom OpenAI-compatible models endpoint. None
replaces the original command or edits a client config. Claude Desktop receives
a reviewable MCP fragment pointing to the desktop binary's native, read-only
`--mcp-server` mode; CODETAS deliberately does not read or rewrite the
existing Claude Desktop configuration because it may contain unrelated
secrets. Claude Desktop additionally receives a reviewable four-family
inference profile. Its generated aliases are resolved only when the request
carries the owned `x-codetas-client: claude-desktop` marker, so normal Codex or
Gemini model names cannot be captured globally.
