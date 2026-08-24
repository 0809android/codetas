## Skill policy for this repository

Skills are available. Use them when they help. Do not reload them.

Do not use the `openai-docs` skill for ordinary CODETAS work.

This includes compaction, gateway routing, catalog, provider adapters,
envelope design, private `.ai/` notes, implementation TODOs, and handoff
prompts. Those tasks are local software work. Official OpenAI documentation
search is not a required first action and must not delay reading or editing
this repository.

Use `openai-docs` only when the user explicitly asks for current official
OpenAI or Codex product documentation, pricing, model migration, or a cited
API contract.

Do not use `japanese-tech-writing` for implementation TODOs, design memos,
handoffs, or investigation notes. That skill is for book and article prose
only.

## Skill Load Guard

This section overrides Codex default skill-trigger behavior.

- Read a selected `SKILL.md` at most once per turn. Then do the work.
- Do not re-read the same skill after a tool result, compaction, or a short
  follow-up such as "続けて".
- Do not re-announce a skill after the first announcement in the same turn.
- "Do not carry skills across turns" means drop an irrelevant skill. It does
  not mean reload `SKILL.md` from scratch on every hop.
- If the work is writing or implementing, write. Re-search is not a substitute.

## Thread and investigation guard

- Do not call `create_thread`, `fork_thread`, or `handoff_thread` unless the
  user explicitly asks for a separate Codex task or names that tool.
- After the files named in the user request have been read once, implement or
  answer. Re-running `git status` or re-reading the same file is not progress.

## Past-fix review (do not regress)

Before changing continuation, compaction, WebSocket merge/context, or the
repeated-tool guard, read `.ai/HISTORY_LOSS_AND_LOOPS.md` if it exists.

That file is gitignored on purpose. It records bugs that look like “the model
got lost” but were CODETAS regressions. Do not “simplify” them back.

In particular:

- Do not treat an empty ChatGPT `response.completed.output` as “no assistant
  text” for continuation history. Fill continuation context from streamed
  `output_item.done` items.
- Do not drop the last user message or last assistant question from a local
  compact tail to meet a token budget.
- Do not strip `previous_response_id` on expand miss for stateful official
  OpenAI Responses. Do not save a truncated miss as the new Exact checkpoint.
- Do not limit the repeated-tool loop guard to `function_call` with identical
  arguments. Codex loops on `exec` reads of the same files with slightly
  different commands.

If `.ai/HISTORY_LOSS_AND_LOOPS.md` is missing, say so and do not invent a
replacement policy.
