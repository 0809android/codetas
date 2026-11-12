# Security

CODETAS handles agent instructions and may eventually manage durable memory.
Those inputs can influence future model behavior, so the default is read-only
and review-first.

## Guarantees for the MVP

- Hermes markdown sync writes only after an explicit preview and confirmation.
  Existing targets are copied to a sibling backup first. `.env`, `auth.json`,
  SQLite, and session databases are never part of this sync.
- `.env`, `auth.json`, credential files, and common private-key formats are not
  committed, logged, or copied into the repository. Local CLI logins may be
  imported into the user-owned CODETAS auth store.
- Hook output is capped before it is sent to Codex.
- Project context is scanned for common prompt-injection and credential-
  exfiltration patterns before injection.
- Plugin hooks use Codex's normal trust review and are never silently approved.
- MCP tools in the MVP do not write project configuration. The `memory` and
  `skill_manage` tools require a SessionStart `scopeToken` and write only that
  session's Hermes profile `memories/` and `skills/user/` after injection
  scanning, character limits, and path checks. Unresolved profile identity
  never falls back to default. Bundled, hub, and external skills are not
  writable. Plugin hooks are never silently approved. The Desktop learning
  sidecar receives only a confirmed Gateway URL and does not read ambient
  client or gateway tokens.
- The provider gateway listens on loopback by default. Non-loopback binding
  requires explicit remote access plus local-token or scoped external-key
  authentication.
- Provider configuration stores environment-variable names, OS keychain
  references, OAuth provider ids, or reviewed command definitions, never
  credential values.
- OAuth access and refresh tokens live only in the user-owned CODETAS auth
  store next to `providers.json`. They are never written to git, logs, or the
  renderer. CODETAS refreshes them before expiry.
- Request and response bodies are not logged. Two stores do keep conversation
  content on disk, in owner-only files:
  - the continuation cache (`response-state/sessions/` next to
    `providers.json`), which lets Codex continue a turn after a gateway
    restart. It is bounded to 4 MiB per session and 64 MiB in total, and a
    session's file is removed when the session leaves the cache.
  - compaction archives (`~/.codetas/compaction-archives/`), written when a
    compaction checkpoint or its output is too large to inline. They are
    immutable and not garbage-collected, because later checkpoints can refer
    to them. Delete them by hand when the conversations they hold are no
    longer needed.
- The debug log (`~/.codex/codetas-debug.log`, or `CODETAS_DEBUG_LOG`) records
  identifiers, counts, and error messages, not bodies. It is owner-only and
  rotates at 8 MiB. Raw upstream error bodies are written only when
  `CODETAS_LOG_UPSTREAM_ERRORS` is set, to `codetas-upstream-errors.log` next
  to the debug log, never to a shared temporary directory. Neither log
  follows a symbolic link at its path.
- The observability ledger has no schema field for content or headers. It stores
  bounded identifiers, status, latency, adapter usage counters, and optional
  catalog-derived cost estimates in owner-private files.
- Observability segments are removed by both retention age and a hard byte
  ceiling. Disabling `redactContent` is rejected instead of weakening the
  storage contract.
- Upstream redirects are disabled, and URLs containing credentials are rejected.
- Plaintext upstream HTTP is restricted to explicitly enabled localhost or
  private literal-IP development endpoints; public provider traffic requires TLS.
- Literal loopback, link-local, and private IP destinations require an explicit
  per-provider opt-in. IPv6 forms that reach a private IPv4 address (NAT64,
  6to4, IPv4-compatible) count as private; Teredo, local-use NAT64,
  benchmarking, discard-only, and site-local IPv6 ranges are always private.
- A gateway bound to loopback answers only requests addressed to an IP literal
  or a `localhost` name. A web page that rebinds its own DNS name to 127.0.0.1
  therefore cannot read even the GET endpoints, which the CORS check alone
  would not stop because same-origin GET requests carry no `Origin`.
- The desktop window cannot change settings that decide what runs or where
  secrets go without a native confirmation dialog: a new or changed credential
  command, a credential sent to a new endpoint or a new secret sent to an
  existing one, private-network access for a provider, `allowRemote`, a
  non-loopback listen address, turning off the local token or DNS pinning, new
  CORS origins or external access keys, and the update manifest, installer
  endpoint, or their public keys.
- The media MCP tools let ffmpeg and ffprobe open only the named local file and
  refuse containers that list other inputs (HLS, concat, image sequences,
  filter graphs), so a planted "video" cannot pull other files into the frames
  sent for analysis.
- Before changing user-level Codex configuration, CODETAS creates a private,
  unique backup and an ownership journal. A pre-existing Codex model catalog is
  backed up too. Restore changes only fields/files that still equal the values
  CODETAS installed; without the journal, a matching file name is never treated
  as proof of ownership or deleted.
- Optional local admission authentication reads `CODETAS_GATEWAY_TOKEN` and
  compares the `x-codetas-token` header without persisting the token.
- Generated OS service definitions never contain provider environment
  variables or token values. Service registration is rejected until enabled
  environment-backed credentials are migrated to the CODETAS auth store, a
  user-managed keychain, or a reviewed command broker.
- Service definitions and the optional `codetas-codex` shim carry an ownership
  marker. CODETAS refuses to overwrite or remove an unmarked same-name file and
  never edits `PATH` or replaces the original `codex` executable.

## Non-goals

CODETAS hooks are not a security boundary. They provide context and guardrails
but do not replace Codex sandboxing, operating-system permissions, or repository
review.

With DNS pinning enabled, CODETAS resolves provider hostnames before sending,
rejects private, local, documentation, benchmark, multicast, and reserved
results unless the provider explicitly allows private networking, then reuses
that exact address set in reqwest. Every request is validated this way; a
client is reused only for a request that validated to the same address set, so
connection reuse never skips the check. Disabling DNS pinning weakens this
boundary and should be limited to reviewed development environments.

Loopback alone is not local-process authentication. Local token enforcement is
available but off by default so a fresh install does not fail without an
environment variable, and the history-preserving Codex connection cannot use
it at all. A loopback gateway therefore looks up which OS account owns each
connecting socket (`/proc/net/tcp` on Linux, `netstat` and `ps` on macOS) and
drops connections owned by another account, so other accounts on a shared
machine cannot use this user's providers and logins. The check is best effort:
when the owner cannot be determined (Windows, a failed or slow lookup, a
socket that already closed) the connection is accepted, so a lookup problem
never locks the user out. Other processes of the same account can still use
the gateway; that is the same trust boundary as reading the auth store. Set
`CODETAS_PEER_CHECK=off` to disable the check. Signed releases should make
token provisioning part of the launcher. The desktop renderer
(`https://tauri.localhost`, `tauri://localhost`, and the Vite dev origin) may
call the loopback gateway; other browser origins stay denied unless listed in
`corsAllowOrigins`.

## Provider terms

Importing another client's login (Claude Code, Antigravity, Kimi, Qwen, GitHub
Copilot, DeepAI Code, and similar) or signing in from CODETAS the way that
client does sends requests as that client. A provider's terms may not allow
this, and the provider may restrict or suspend the account. The desktop app
asks once in a native dialog before any automatic import, and again before an
import or sign-in the user starts while that consent is missing. The decision
is stored in `cli-login-consent.json` next to `providers.json`. When the user
declines, nothing new is imported; connections that already exist keep
working. The GitHub CLI token command is meant for this use and is not gated.

## Reporting

Report vulnerabilities privately through GitHub's private vulnerability
reporting on the repository's Security tab. See [`SECURITY.md`](../SECURITY.md).
Do not include source repositories, prompts, credentials, or personal memory in
public reports.
