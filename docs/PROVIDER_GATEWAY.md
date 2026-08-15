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
Claude Code, Grok, Antigravity (`agy`), Qwen Code, Kimi, OpenCode, Gemini, and
Kiro.

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
and cooldown.

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
assistant messages. For Codex clients, when a translated model repeatedly
returns reasoning plus tool calls without visible content, the gateway emits a
short Responses `output_text` progress message on the first such turn and then
at a bounded cadence. The message is fixed text and never incorporates
reasoning or tool arguments. Empty-content tool-only turns are counted
in-process and recorded in the opt-in debug log without response content.

`anthropicMessages` converts content blocks, images, function tools/results,
signed thinking history, usage, and SSE events. `geminiGenerateContent`
converts parts, images, function IDs, thought signatures, usage, and SSE events.
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
local `codetas1:` compaction envelope with exactly one `compaction` output
item for that synthetic path. The required `encrypted_content` field is a
versioned local transport envelope, not ciphertext, and is expanded only by
CODETAS before translation.

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

CODETAS atomically generates Codex's optional model catalog. Disabled model
metadata is omitted and virtual routes appear as selectable models. With
ownership-checked automatic synchronization enabled, provider, model, route,
and agent-surface saves publish the catalog in the same rollback boundary.

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
