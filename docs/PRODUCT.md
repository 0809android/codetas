# Product definition

## Subject

CODETAS is a local-first companion for Codex with three cooperating surfaces:
a project-context bridge, a Codex plugin, and an optional local provider
gateway. Hermes-compatible project files remain an input to the bridge; the
gateway is an independent runtime for routing Codex and other compatible
clients to configured providers.

## Single job

For project integration, show what CODETAS found, what Codex can reuse, and
what will change before the user enables it. For provider use, give Codex one
reviewable local endpoint that routes `provider/model` requests without moving
credentials into the plugin or mutating the source project.

## Project-context journey

1. Add a project folder.
2. Inspect `.hermes.md`, `HERMES.md`, `AGENTS.md`, skills, and MCP config.
3. Choose which compatible parts to use.
4. Review the sync plan.
5. Install or update the CODETAS Codex plugin.
6. Approve the plugin hook in Codex.

## Provider-gateway journey

1. Open Connections. Existing Kimi, Claude, and Grok CLI logins are imported
   automatically; otherwise sign in from the app or add an API-key reference.
2. Review the route from Codex through CODETAS to the upstream URL.
3. Connect Codex; CODETAS backs up and updates the user-level configuration.
4. Start a new Codex session and use `provider/model`.

## Visual direction

The interface is a quiet synchronization workbench rather than a metrics
dashboard. Its signature is an addition rail connecting Hermes source material
to Codex capabilities. Color is reserved for state and direction.

### Tokens

- `Porcelain` `#F6F8FC`: primary canvas
- `Paper` `#FFFFFF`: working surfaces
- `Ink` `#172033`: text and navigation
- `Iris` `#5C61E6`: Codex-side actions
- `Lagoon` `#1FA6A8`: compatible source material
- `Apricot` `#F58B62`: review-required state

Typography uses Avenir Next for product hierarchy, the platform UI font for
long text, and the system monospace stack for paths and machine state.
