# CODETAS Codex plugin

This repository-local plugin provides the Codex-facing half of CODETAS:

- a `SessionStart` hook that discovers `.hermes.md` or `HERMES.md`;
- read-only MCP tools for project inspection and skill discovery;
- a skill that guides reviewable Hermes-to-Codex adaptation.

The hook is subject to Codex's normal hook trust controls. It rejects context
containing common injection markers or invisible Unicode controls, truncates
large files, and never reads credential files.

The plugin does not modify source projects, import provider authentication, or
synchronize Hermes runtime memory. Provider logins belong to the desktop
gateway and its user-owned auth store, not this plugin.
