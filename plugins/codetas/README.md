# CODETAS Codex plugin

This repository-local plugin provides the Codex-facing half of CODETAS:

- a `SessionStart` hook that discovers `.hermes.md` or `HERMES.md`;
- read-only MCP tools for project inspection and skill discovery;
- MCP tools for image analysis, sampled-video analysis, PDF/OCR, and image generation;
- skills that guide reviewable Hermes-to-Codex adaptation and media delegation.

The hook is subject to Codex's normal hook trust controls. It rejects context
containing common injection markers or invisible Unicode controls, truncates
large files, and never reads credential files.

The plugin does not modify source projects, import provider authentication, or
synchronize Hermes runtime memory. Media tools call the loopback CODETAS
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
