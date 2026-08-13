# CODETAS project instructions

## Product boundary

- CODETAS is an independent, unofficial companion for Codex.
- Keep the desktop app, sync core, and Codex plugin separable.
- Treat `.hermes.md`, `HERMES.md`, Hermes profiles, and Codex configuration as user-owned inputs.
- Never overwrite source Hermes files.
- Generated changes must be reviewable before they are applied.

## Security

- Never expose, log, or commit `.env`, `auth.json`, OAuth tokens, API keys, cookies, or private keys.
- Local OAuth sessions may be stored in the user-owned CODETAS auth store (outside the git repo) so existing CLI logins work without re-registration and tokens refresh automatically. Values must never reach the renderer, logs, git, or `providers.json`.
- Memory and skill writes require an explicit user action.
- Project discovery is read-only by default.
- Do not silently trust Codex hooks or bypass Codex hook review.

## Development

- Use TypeScript for the web UI and shared sync logic.
- Keep Tauri commands narrow and return structured errors.
- Keep provider adapters independent from the desktop UI and from third-party proxy runtimes.
- Prefer platform-neutral paths and avoid hard-coded user directories.
- Keep UI language plain: describe user-visible actions, not internal implementation.
- Local builds, tests, lint, formatting, and package-manager verification are left to CI/CD unless explicitly requested.

## Brand

- Product name: `CODETAS`.
- Japanese reading: `コデタス`.
- Tagline: `Codexに、できることを足す。`
- Always state that CODETAS is not affiliated with or endorsed by OpenAI or Nous Research.
