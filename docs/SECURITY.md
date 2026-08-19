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
- MCP tools in the MVP do not write memory, skills, or project configuration.
- The provider gateway listens on loopback by default. Non-loopback binding
  requires explicit remote access plus local-token or scoped external-key
  authentication.
- Provider configuration stores environment-variable names, OS keychain
  references, OAuth provider ids, or reviewed command definitions, never
  credential values.
- OAuth access and refresh tokens live only in the user-owned CODETAS auth
  store next to `providers.json`. They are never written to git, logs, or the
  renderer. CODETAS refreshes them before expiry.
- Request and response bodies are not logged or persisted.
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
  per-provider opt-in.
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
that exact address set in reqwest. Disabling DNS pinning weakens this boundary
and should be limited to reviewed development environments.

Loopback alone is not local-process authentication. Local token enforcement is
available but off by default so a fresh install does not fail without an
environment variable. Signed releases should make token provisioning part of
the launcher and add strict origin checks.

## Reporting

Do not include source repositories, prompts, credentials, or personal memory in
public reports. A private security-reporting channel will be documented before
the first public binary release.
