# Settings v2

`providers.json` version 2 separates routing configuration from credentials.
The file may contain environment variable names, keychain item identifiers,
OAuth provider ids, and command metadata. It must never contain an API key,
access token, refresh token, cookie, or password. Those values belong in the
sibling user file `auth.json`, the OS keychain, an environment variable, or a
token command.

## Top-level sections

- `registryRevision`: internal one-time registry migration revision; missing values are migrated and persisted automatically
- `providers`: upstream protocol, endpoint, capability, discovery, body/time
  limits, image-history budgeting, and separate request/stream-start retry
  counts
- `modelCatalog`: model metadata used for routing and Codex catalog generation
- `routes`: failover, weighted round robin, and least-usage virtual routes
- `runtime`: bind address, port, startup, service, and bounded graceful-shutdown
  timeout
- `security`: local admission, remote access, DNS pinning, and CORS policy
- `observability`: redaction, retention, and storage budgets
- `accountPool`: account references and switching policy
- `agents`: multi-agent surface, model roster, fallback, effort caps, auxiliary
  input modes, timeout, video frame count, PDF page count, and OCR policy
- `sidecars`: web search, vision, video-input, document/PDF, image-generation,
  video-generation, and realtime model routes
- `codex`: ownership-checked automatic catalog synchronization policy
- `integrations`: Codex, Claude Code, Claude Desktop, OpenCode, and Grok switches

## Migration

Version 1 is parsed as JSON, upgraded in memory, validated as version 2, and
then atomically written back. An unknown future version fails closed. Provider
IDs, endpoints, model IDs, and default provider state are retained.

Hermes-style `agent.image_input_mode` and `auxiliary.*.provider/model` fields are
also imported into the canonical CODETAS `agents` and `sidecars` fields. The app
settings screen writes the canonical form; provider credentials remain outside
this file.

## App workflow

The Agents screen keeps the media workflow self-contained:

- concise `?` tooltips explain only the media modes and auxiliary model fields;
- DeepSeek + GPT Vision, Kimi + GPT Vision, and current-model + GPT Vision
  presets resolve against the enabled, synchronized model catalog instead of
  pinning a stale model ID;
- image, sampled-video, PDF/OCR, and image-generation buttons save the visible
  selections and run a real request through the loopback gateway;
- Codex plugin status checks cached plugin files, starts the MCP health probe,
  verifies the Codex Gateway configuration, and probes the running media API
  before reporting the connection as ready.

Video tests require local `ffmpeg` and `ffprobe`. PDF tests require
`pdftoppm`. The app reports these missing prerequisites directly in the test
result flow.
Token-protected tests use `CODETAS_GATEWAY_TOKEN` or an enabled external key
with `sidecars:write`; the admission secret is sent with `x-codetas-token`.

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
