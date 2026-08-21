# CODETAS Codex plugin

This repository-local plugin provides the Codex-facing half of CODETAS:

- a `SessionStart` hook that discovers `.hermes.md` or `HERMES.md` and injects a frozen Hermes profile memory snapshot (compact/resume reuse that snapshot; they do not rebuild it);
- `UserPromptSubmit` / `PostToolUse` / `Stop` hooks that run the learning loop through a POSIX fast path (no Python cold start on no-op increments or no-op Stop). Memory nudge every 10 user turns, skill nudge every 15 observed tool units or user turns, 6-turn checkpoint, session scopeToken. Reviews inject `additionalContext` on the triggering or next user turn and never set Stop `decision: block`;
- MCP tools for project inspection, skill discovery, and profile-scoped `memory` / `skill_manage` writes;
- MCP tools for image analysis, sampled-video analysis, PDF/OCR, and image generation;
- skills that guide reviewable Hermes-to-Codex adaptation and media delegation.

Project inspection remains read-only. `memory` and `skill_manage` write only the
active Hermes profile's `memories/` and `skills/user/` after injection scanning
and size limits. `image_generate` creates a provider-owned image result through
the gateway and does not modify the inspected project.

## Learning-loop hook contract

- `SessionStart` may be heavy: it resolves identity (fail-closed; never falls
  back to default), freezes `MEMORY.md` / `USER.md` plus a `skills/user` index,
  and binds a session `scopeToken`. `compact` / `resume` reuse that snapshot.
- `UserPromptSubmit`, `PostToolUse`, and `Stop` run `hooks/learning_fast.sh`.
  The script updates a tiny sidecar and does not start Python. A turn with
  several tool calls therefore does not pay repeated interpreter + import cost
  from `PLUGIN_ROOT` (often hundreds of milliseconds on an external volume;
  locally the POSIX path is ~4ms vs ~25ms for a Python no-op).
- No-op Stop exits without output. A due memory or skill review is injected as
  `additionalContext` on the triggering or next `UserPromptSubmit`. Stop never
  sets `decision: block`, so Codex is not forced into an extra review turn.
- `memory` and `skill_manage` still require the session `scopeToken` and write
  only that profile's `memories/` and `skills/user/`.

The hook is subject to Codex's normal hook trust controls. It rejects context
containing common injection markers or invisible Unicode controls, truncates
large files, and never reads credential files.

The plugin does not modify inspected source projects or import provider
authentication. Profile learning writes stay inside the active Hermes profile.
Media tools call the loopback CODETAS
gateway at `CODETAS_GATEWAY_URL` (default `http://127.0.0.1:42421`) and use the
models selected in the app. Provider logins belong to the desktop gateway and
its user-owned auth store, not this plugin. Local video analysis requires
`ffmpeg` and `ffprobe`; local PDF rendering requires `pdftoppm`.
Without an explicit URL, the plugin follows the app's configured or dynamically
assigned loopback port. Video and PDF modes are read from the app. Native mode
attaches the original file as an MCP resource; auxiliary mode resizes and sends media in bounded
batches so large files do not become one oversized Gateway request. Admission
tokens use the dedicated `x-codetas-token` header. Besides the standard token
variables, the plugin resolves enabled `sidecars:write` external-key environment
variables from the app's user-owned settings file.

Use the desktop app's Agents screen to apply a recommended model combination,
verify the Codex plugin/Gateway status, and run image, video, PDF/OCR, or
image-generation tests. Help icons beside the media settings provide concise
setup guidance in the app.
