---
name: codetas-sync
description: Inspect and reuse Hermes-compatible project context, skills, and MCP connection metadata in Codex through CODETAS. Use when the user asks to sync, migrate, compare, or reuse a project's .hermes.md, HERMES.md, Hermes skills, or Hermes MCP settings without changing the source files.
---

# CODETAS Sync

Use CODETAS as a read-only compatibility bridge between project-owned Hermes
files and Codex.

## Workflow

1. Call `inspect_project_context` with the current project's absolute path to
   establish which supported files exist.
2. Report the exact source paths, reusable skill count, and warnings.
3. If project instructions are needed and the SessionStart hook did not expose
   them, call `read_project_context`.
4. Call `list_project_skills` before proposing skill migration or adaptation.
5. Present a plan that separates directly reusable context from items requiring
   conversion or authentication review.
6. Ask for explicit approval before creating, copying, or editing any target
   file. Never modify the Hermes source.

## Compatibility rules

- `.hermes.md` and `HERMES.md` can be supplied as project guidance.
- `SKILL.md` files are candidates, not automatically compatible. Review their
  frontmatter, tools, path assumptions, and referenced resources first.
- MCP server definitions are review-only. Do not copy tokens, inline secrets,
  environment values, or authentication files.
- Hermes memory databases, cron jobs, messaging gateways, and provider-specific
  runtime state are not directly synchronized in the MVP.
- Explicit user instructions, project `AGENTS.md`, and higher-priority Codex
  instructions always take precedence over imported project guidance.

## Output

Keep the result concrete: detected sources, compatible items, review items,
unsupported items, and the next safe action. State clearly that inspection is
read-only unless the user separately authorizes a target-side change.
