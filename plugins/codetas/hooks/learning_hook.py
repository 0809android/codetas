#!/usr/bin/env python3
"""Dispatch Codex lifecycle hooks into the Hermes-compatible learning loop.

SessionStart and SessionEnd stay on this Python entry. Per-turn UserPromptSubmit,
PostToolUse, and Stop use hooks/learning_fast.sh so Codex does not cold-start
Python on counter increments or no-op Stop. This module remains the fallback
and the SessionStart snapshot builder.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys

HOOKS_DIR = Path(__file__).resolve().parent
SCRIPT_DIR = HOOKS_DIR.parent / "scripts"
sys.path.insert(0, str(HOOKS_DIR))
sys.path.insert(0, str(SCRIPT_DIR))

from profile_learning import (  # noqa: E402
    hook_output,
    on_post_tool_use,
    on_prompt_submit,
    on_session_end,
    on_session_start,
    on_stop,
)
from project_context import hermes_context_loading_enabled  # noqa: E402
from session_start import project_context_message  # noqa: E402


def read_event() -> dict:
    try:
        event = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        return {}
    return event if isinstance(event, dict) else {}


def emit(event_name: str, additional_context: str | None) -> int:
    if additional_context:
        print(json.dumps(hook_output(event_name, additional_context), ensure_ascii=False))
    return 0


def main() -> int:
    event_name = sys.argv[1] if len(sys.argv) > 1 else "SessionStart"
    event = read_event()
    if event_name == "SessionStart":
        parts: list[str] = []
        if hermes_context_loading_enabled():
            cwd_value = event.get("cwd") if isinstance(event.get("cwd"), str) else os.getcwd()
            project_message = project_context_message(Path(cwd_value).expanduser())
            if project_message:
                if project_message.startswith("CODETAS could not read") or project_message.startswith("CODETAS did not load"):
                    print(json.dumps({"systemMessage": project_message}, ensure_ascii=False))
                else:
                    parts.append(project_message)
        snapshot = on_session_start(event)
        if snapshot:
            parts.append(snapshot)
        return emit("SessionStart", "\n\n".join(parts) if parts else None)
    if event_name == "UserPromptSubmit":
        return emit("UserPromptSubmit", on_prompt_submit(event))
    if event_name == "PostToolUse":
        on_post_tool_use(event)
        return 0
    if event_name == "Stop":
        return emit("Stop", on_stop(event))
    if event_name == "SessionEnd":
        on_session_end(event)
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
