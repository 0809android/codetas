# Settings v2

`providers.json` version 2 separates routing configuration from credentials.
The file may contain environment variable names, keychain item identifiers,
OAuth provider ids, and command metadata. It must never contain an API key,
access token, refresh token, cookie, or password. Those values belong in the
sibling user file `auth.json`, the OS keychain, an environment variable, or a
token command.

## Top-level sections

- `providers`: upstream protocol, endpoint, capability, discovery, body/time
  limits, and separate request/stream-start retry counts
- `modelCatalog`: model metadata used for routing and Codex catalog generation
- `routes`: failover, weighted round robin, and least-usage virtual routes
- `runtime`: bind address, port, startup, service, and bounded graceful-shutdown
  timeout
- `security`: local admission, remote access, DNS pinning, and CORS policy
- `observability`: redaction, retention, and storage budgets
- `accountPool`: account references and switching policy
- `agents`: multi-agent surface, model roster, fallback, and effort caps
- `sidecars`: web search, vision, image, and video model routes
- `codex`: ownership-checked automatic catalog synchronization policy
- `integrations`: Codex, Claude Code, Claude Desktop, OpenCode, and Grok switches

## Migration

Version 1 is parsed as JSON, upgraded in memory, validated as version 2, and
then atomically written back. An unknown future version fails closed. Provider
IDs, endpoints, model IDs, and default provider state are retained.

## Secret boundary

Static `Authorization`, API-key, token, secret, password, and key query fields
are rejected during validation. Use `apiKeyEnv`, `envHeaders`, a user-managed
keychain reference, a CODETAS OAuth provider id (`source=oAuth` plus
`reference=<providerId>`), or a bounded command. CODETAS never writes secret
values to `providers.json`.

OAuth access and refresh tokens for Kimi, Claude, and xAI are stored in the
sibling user file `auth.json` (mode 0600), imported from the matching CLI when
present, and refreshed before expiry. Uninstall removes that file when it sits
beside CODETAS settings. Keychain usernames use `credential:<reference>` for
API keys and `oauth:<reference>` for legacy keychain-backed access tokens under
service `jp.kinocode.codetas`.
