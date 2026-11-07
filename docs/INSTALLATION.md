# Installation

CODETAS is pre-alpha source software. There is no signed or notarized desktop
binary yet.

## Desktop app from source

Prerequisites are Node.js 22, npm 10, stable Rust, and the platform requirements
for Tauri 2.

```bash
git clone <repository-url> codetas
cd codetas
npm install
npm run dev:desktop
```

The desktop app registers project folders locally, detects supported files, and
shows a sync plan. It does not modify a Hermes source file.

## Provider gateway from the desktop app

1. Open **接続**. If Kimi, Claude, or Grok is already logged in on this
   machine, CODETAS imports that login automatically. Otherwise press **使う**
   or **Kimi/Claude/Grokでログイン**. Direct API keys (DeepSeek, GLM, MiniMax,
   and others) are saved as a Keychain or environment-variable reference.
2. Confirm the imported or added provider is enabled and, if you want, make it
   the default route.
3. Select **接続する**. CODETAS starts the local gateway, backs up the
   existing user-level `config.toml`, and redirects Codex's built-in OpenAI
   transport to the loopback Responses gateway. The provider identity stays
   `openai`, preserving existing desktop history. **解除する** restores official
   Codex and stops the gateway.
4. Start a new Codex session and keep CODETAS open while using the route.
   If CODETAS later crashes, Codex is temporarily restored to the official
   OpenAI transport and is pointed back at the local gateway when it returns.
   Turn this off in Settings if you do not want that fallback.

For the OpenAI (Codex login) model refresh and connection test, CODETAS checks
`@openai/codex` on the official npm registry and downloads the latest native CLI
into its own cache when needed. It verifies the archive's SHA-512 integrity and
uses that version for discovery and Codex-owned login refresh. This does not
update Codex App or your global CLI, and requires neither npm nor a separate CLI
installation. The current Codex ChatGPT login is still required. If the latest
version cannot be checked or downloaded, the operation fails explicitly rather
than silently using an older CLI; the existing model list is preserved.

Tokens live in the user-owned `auth.json` beside `providers.json`. They are not
written to git, logs, or the window. See [Provider Gateway](PROVIDER_GATEWAY.md)
for the import paths and refresh rules.

The gateway starts with the desktop UI and listens on
`http://127.0.0.1:42421/v1`. It is an independent CODETAS implementation; no
third-party proxy command or runtime is required. See
[Provider Gateway](PROVIDER_GATEWAY.md) before using the pre-alpha gateway with
paid API credentials.

The repository also builds a standalone `codetas-gateway` executable. Service
managers call it with `--config <providers.json>` or the `CODETAS_SETTINGS`
environment variable. The executable validates settings before binding and
stops cleanly on the platform termination signal.

The Connections screen can explicitly register the desktop binary in headless
gateway mode as a macOS LaunchAgent, a systemd user unit, or a Windows Task
Scheduler task. CODETAS refuses to overwrite an unmarked definition. It also
refuses generated-service mode while an enabled provider/account depends on an
environment-variable secret, because those values are never copied into the
service definition. Use the CODETAS auth store, a user-managed keychain
reference, or a reviewed token command first. The optional `codetas-codex` shim asks the service manager to
start the gateway and then executes the original `codex`; it is placed in a
CODETAS data folder and does not edit `PATH` or replace the original command.

## Codex plugin only

The repository contains a local marketplace at
`.agents/plugins/marketplace.json`. Open that marketplace in the Codex app,
install `codetas`, and review the requested plugin capabilities.

When Codex first encounters the `SessionStart` hook, use Codex's normal trust
flow to review it. CODETAS does not approve hooks on the user's behalf.

After installation, open Codex in a project containing `.hermes.md` or
`HERMES.md`. The hook searches only the current directory up to its Git root.
The MCP tools accept the current project's absolute path and remain read-only.

## Intended release distribution

Tagged releases will provide a desktop installer and keep the matching Codex
plugin in the same repository marketplace. App and plugin versions move
together so the UI can explain exactly which integration it manages.

The first public macOS binary will require signing and notarization. Windows
and Linux packages follow after the project-scoped activation flow is stable.

### Publishing a macOS release

`.github/workflows/release.yml` builds, signs, notarizes, staples, and
publishes the macOS app and dmg when a `v<version>` tag is pushed, or when the
workflow is dispatched with an existing tag. The tag must match
`apps/desktop/src-tauri/tauri.conf.json`, so bump the version before tagging.

The workflow fails before building unless every credential below is set as a
repository secret. It never publishes an unsigned or unnotarized bundle.

| Secret | Value |
| --- | --- |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: <name> (<TEAMID>)` |
| `APPLE_CERTIFICATE` | base64 of the Developer ID `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password of that `.p12` |
| `APPLE_API_KEY` | App Store Connect API key ID (the `AuthKey_<ID>.p8` suffix) |
| `APPLE_API_ISSUER` | App Store Connect issuer ID |
| `APPLE_API_KEY_P8` | contents of `AuthKey_<ID>.p8` |
| `KEYCHAIN_PASSWORD` | any password for the temporary CI keychain |

Create the certificate secret with
`base64 -i DeveloperID.p12 | pbcopy`. The API key needs the **Developer ID
Installer** or **App Manager** role; notarization rejects other roles.

`APPLE_API_KEY` and `APPLE_API_KEY_PATH` are the names Tauri itself reads, so
the workflow stores the key ID as `APPLE_API_KEY` and writes the `.p8` contents
to a temporary file named `AuthKey_<ID>.p8` for `APPLE_API_KEY_PATH`.

To publish:

```sh
# bump the version in apps/desktop/src-tauri/tauri.conf.json first
git tag v0.1.2 && git push origin v0.1.2
```

The workflow verifies the signature with `codesign`, validates the stapled
ticket with `stapler`, runs `spctl --assess`, and uploads the dmg, a zipped app,
and `SHA256SUMS.txt`. Because `createUpdaterArtifacts` is still `false`, the
release carries no Tauri Updater manifest; in-app updates stay disabled until
an endpoint, public key, and signed manifest are added.
