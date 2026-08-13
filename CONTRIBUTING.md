# Contributing to CODETAS

CODETAS welcomes focused issues and pull requests. Keep the desktop app, shared
core, and Codex plugin independently understandable.

## Development

```bash
npm install
npm run dev
```

Use `npm run dev:desktop` for the native shell. CI is the required verification
gate for TypeScript, Rust, JSON manifests, and Python plugin scripts.

## Pull requests

- State which Hermes capability is being adapted and which Codex mechanism is
  used.
- Preserve the read-only source boundary unless an approved design explicitly
  introduces a target-side write.
- Never include credentials, `auth.json`, private prompts, memory databases, or
  copied user profiles in fixtures or commits.
- Update `docs/COMPATIBILITY.md` when support status changes.
- Include screenshots for visible desktop UI changes.

By contributing, you agree that your contribution is licensed under Apache-2.0.
