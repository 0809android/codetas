#!/usr/bin/env python3
"""Sidecar learning runtime for live Codex session transcripts.

CODETAS Desktop watches ~/.codex/sessions for live rollout JSONL files.
Each live Codex thread gets one sidecar process that tails the transcript,
reviews durable facts, and writes the bound Hermes profile through the
existing memory / skill_manage tools. The sidecar exits when the Codex
session file stops being live. It never injects turns into Codex.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import sys
import time
from pathlib import Path
from typing import Any
from urllib import error, request

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from memory_store import scan_memory_content
from profile_learning import (
    CHECKPOINT_PROMPT,
    COMBINED_REVIEW_PROMPT,
    EXIT_FLUSH_PROMPT,
    FLUSH_MIN_TURNS,
    KIND_UNRESOLVED,
    MEMORY_REVIEW_PROMPT,
    SKILL_REVIEW_PROMPT,
    bind_scope,
    empty_state,
    list_user_skills,
    load_state,
    memory_tool,
    parse_profile_ref,
    review_prefix,
    save_state,
    skill_manage,
    state_dir,
)
from project_context import suspicious_context_reasons

MAX_TRANSCRIPT_CHARS = 24_000
MAX_JSONL_BYTES = 8 * 1024 * 1024
MAX_LINE_BYTES = 256 * 1024
MAX_TOOL_ROUNDS = 4
GATEWAY_TIMEOUT_SECONDS = 90
POLL_SECONDS = 2.0
LIVE_GRACE_SECONDS = 20.0
STALE_SECONDS = 45 * 60
CODETAS_ORIGIN_SENTINEL = "CODETAS-LEARNING-ORIGIN"
UUID_RE = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)


def learning_state_root() -> Path:
    override = os.environ.get("CODETAS_LEARNING_STATE_DIR")
    if override:
        return Path(override).expanduser()
    return state_dir()


def sidecar_dir() -> Path:
    path = learning_state_root() / "sidecars"
    path.mkdir(parents=True, exist_ok=True)
    return path


def looks_like_session_id(value: str) -> bool:
    return bool(value) and bool(UUID_RE.fullmatch(value))


def session_id_from_path(path: Path) -> str | None:
    name = path.name
    if not name.endswith(".jsonl"):
        return None
    stem = name[: -len(".jsonl")]
    if len(stem) < 36:
        return None
    candidate = stem[-36:]
    return candidate.lower() if looks_like_session_id(candidate) else None


def is_regular_file(path: Path) -> bool:
    try:
        return path.is_file() and not path.is_symlink()
    except OSError:
        return False


def atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    os.replace(tmp, path)


def extract_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        for key in ("text", "prompt", "content", "message"):
            inner = value.get(key)
            text = extract_text(inner)
            if text:
                return text
        return ""
    if isinstance(value, list):
        parts = [extract_text(item) for item in value]
        return "\n".join(part for part in parts if part)
    return ""


PROFILE_MARKER_RE = re.compile(
    r"(?m)^CODETAS-LEARNING-ORIGIN\r?\nCODETAS-LEARNING-PROFILE:(named|default):([A-Za-z0-9._-]{1,64})\r?$"
)


def developer_instructions_text(value: Any) -> str | None:
    # Only the structured developer_instructions member may bind identity.
    if not isinstance(value, dict):
        return None
    for key in ("developer_instructions", "developerInstructions"):
        inner = value.get(key)
        if isinstance(inner, str) and inner.strip():
            return inner
        if isinstance(inner, dict):
            text = inner.get("text")
            if isinstance(text, str) and text.strip():
                return text
    return None


def agent_name_from_instructions(text: str | None) -> str | None:
    if not text or CODETAS_ORIGIN_SENTINEL not in text:
        return None
    match = PROFILE_MARKER_RE.search(text)
    if not match:
        return None
    kind, name = match.group(1), match.group(2)
    if kind == "default":
        return "default" if name == "default" else None
    if name in {"default", ".", ".."}:
        return None
    return name


class TranscriptCursor:
    def __init__(self) -> None:
        self.offset = 0
        self.carry = ""
        self.user_turns = 0
        self.tool_units = 0
        self.seen_tool_ids: list[str] = []
        self.messages: list[str] = []
        self.cwd: str | None = None
        self.originator: str | None = None
        self.agent_name: str | None = None
        self.ended = False

    def snapshot(self) -> dict[str, Any]:
        return {
            "offset": self.offset,
            "carry": self.carry,
            "user_turns": self.user_turns,
            "tool_units": self.tool_units,
            "seen_tool_ids": list(self.seen_tool_ids[-200:]),
            "messages": list(self.messages[-40:]),
            "cwd": self.cwd,
            "originator": self.originator,
            "agent_name": self.agent_name,
            "ended": self.ended,
        }

    @classmethod
    def from_snapshot(cls, data: dict[str, Any] | None) -> "TranscriptCursor":
        cursor = cls()
        if not isinstance(data, dict):
            return cursor
        cursor.offset = int(data.get("offset") or 0)
        cursor.carry = str(data.get("carry") or "")
        cursor.user_turns = int(data.get("user_turns") or 0)
        cursor.tool_units = int(data.get("tool_units") or 0)
        seen = data.get("seen_tool_ids")
        if isinstance(seen, list):
            cursor.seen_tool_ids = [str(item) for item in seen if isinstance(item, str)][-200:]
        messages = data.get("messages")
        if isinstance(messages, list):
            cursor.messages = [str(item) for item in messages if isinstance(item, str)][-40:]
        for key in ("cwd", "originator", "agent_name"):
            value = data.get(key)
            if isinstance(value, str) and value.strip():
                setattr(cursor, key, value.strip())
        cursor.ended = bool(data.get("ended"))
        return cursor


def _append_message(cursor: TranscriptCursor, role: str, text: str) -> None:
    cleaned = " ".join(text.split())
    if not cleaned:
        return
    if len(cleaned) > 800:
        cleaned = cleaned[:800] + "…"
    cursor.messages.append(f"{role}: {cleaned}")
    if len(cursor.messages) > 40:
        cursor.messages = cursor.messages[-40:]


def _note_tool(cursor: TranscriptCursor, tool_id: str | None, tool_name: str | None) -> None:
    name = (tool_name or "").lower()
    if name in {"memory", "skill_manage"}:
        return
    if tool_id:
        if tool_id in cursor.seen_tool_ids:
            return
        cursor.seen_tool_ids.append(tool_id)
        if len(cursor.seen_tool_ids) > 200:
            cursor.seen_tool_ids = cursor.seen_tool_ids[-200:]
    cursor.tool_units += 1


def consume_record(cursor: TranscriptCursor, record: dict[str, Any]) -> None:
    record_type = record.get("type")
    payload = record.get("payload") if isinstance(record.get("payload"), dict) else {}
    if record_type == "session_meta":
        cwd = payload.get("cwd")
        if isinstance(cwd, str) and cwd.strip():
            cursor.cwd = cwd.strip()
        originator = payload.get("originator")
        if isinstance(originator, str) and originator.strip():
            cursor.originator = originator.strip()
        instructions = payload.get("base_instructions")
        text = developer_instructions_text(instructions)
        agent = agent_name_from_instructions(text)
        if agent:
            cursor.agent_name = agent
        return
    if record_type == "turn_context":
        cwd = payload.get("cwd")
        if isinstance(cwd, str) and cwd.strip():
            cursor.cwd = cwd.strip()
        return
    if record_type == "event_msg":
        event_type = payload.get("type")
        if event_type in {"session_end", "thread_closed", "thread_archived"}:
            cursor.ended = True
        if event_type == "user_message":
            _append_message(cursor, "user", extract_text(payload.get("message") or payload.get("text") or payload))
        if event_type == "agent_message":
            _append_message(cursor, "assistant", extract_text(payload.get("message") or payload.get("text") or payload))
        return
    if record_type != "response_item":
        return
    item_type = payload.get("type")
    role = payload.get("role")
    if item_type == "message" and role == "user":
        cursor.user_turns += 1
        _append_message(cursor, "user", extract_text(payload.get("content")))
        return
    if item_type == "message" and role == "assistant":
        _append_message(cursor, "assistant", extract_text(payload.get("content")))
        return
    if item_type in {"custom_tool_call", "function_call"}:
        tool_id = payload.get("call_id") or payload.get("id")
        _note_tool(cursor, str(tool_id) if tool_id else None, payload.get("name") or payload.get("tool_name"))
        return
    if item_type == "custom_tool_call_output":
        return


def ingest_jsonl(path: Path, cursor: TranscriptCursor) -> TranscriptCursor:
    if not is_regular_file(path):
        return cursor
    size = path.stat().st_size
    if size < cursor.offset:
        cursor = TranscriptCursor()
    if size > MAX_JSONL_BYTES:
        # Keep the tail so a huge transcript still yields recent turns.
        cursor.offset = max(cursor.offset, size - MAX_JSONL_BYTES)
        cursor.carry = ""
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        handle.seek(cursor.offset)
        chunk = handle.read(MAX_JSONL_BYTES)
        cursor.offset = handle.tell()
    text = cursor.carry + chunk
    lines = text.splitlines(keepends=True)
    if lines and not lines[-1].endswith("\n"):
        cursor.carry = lines.pop()
        if len(cursor.carry) > MAX_LINE_BYTES:
            cursor.carry = ""
    else:
        cursor.carry = ""
    for line in lines:
        raw = line.strip()
        if not raw:
            continue
        try:
            record = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            consume_record(cursor, record)
    return cursor


def transcript_excerpt(cursor: TranscriptCursor) -> str:
    text = "\n".join(cursor.messages)
    if len(text) > MAX_TRANSCRIPT_CHARS:
        return text[-MAX_TRANSCRIPT_CHARS:]
    return text


def sidecar_review_text(sidecar: dict[str, Any], state: dict[str, Any], cursor: TranscriptCursor, ending: bool) -> str | None:
    """Drive reviews from transcript counters. Do not touch plugin hook flags."""
    last_memory = int(sidecar.get("last_memory_turns") or 0)
    last_skill_turns = int(sidecar.get("last_skill_turns") or 0)
    last_skill_tools = int(sidecar.get("last_skill_tools") or 0)
    checkpoint_done = bool(sidecar.get("checkpoint_done"))
    memory_reason = None
    skill_due = False
    if ending:
        memory_reason = "exit"
    elif not checkpoint_done and cursor.user_turns >= FLUSH_MIN_TURNS:
        memory_reason = "checkpoint"
    elif cursor.user_turns - last_memory >= 10:
        memory_reason = "nudge"
    if (cursor.tool_units - last_skill_tools) >= 15 or (cursor.user_turns - last_skill_turns) >= 15:
        skill_due = True
    if memory_reason is None and not skill_due:
        return None
    sidecar["pending_memory_reason"] = memory_reason
    sidecar["pending_skill"] = skill_due
    prefix = review_prefix(state)
    if memory_reason and skill_due:
        return prefix + COMBINED_REVIEW_PROMPT
    if memory_reason == "exit":
        return prefix + EXIT_FLUSH_PROMPT
    if memory_reason == "checkpoint":
        return prefix + CHECKPOINT_PROMPT
    if memory_reason:
        return prefix + MEMORY_REVIEW_PROMPT
    return prefix + SKILL_REVIEW_PROMPT


def acknowledge_sidecar_reviews(sidecar: dict[str, Any], cursor: TranscriptCursor) -> None:
    reason = sidecar.get("pending_memory_reason")
    if reason in {"checkpoint", "exit"}:
        sidecar["checkpoint_done"] = True
    if reason:
        sidecar["last_memory_turns"] = cursor.user_turns
    if sidecar.get("pending_skill"):
        sidecar["last_skill_turns"] = cursor.user_turns
        sidecar["last_skill_tools"] = cursor.tool_units
    sidecar["pending_memory_reason"] = None
    sidecar["pending_skill"] = False


def bind_session_identity(session_id: str, agent_name: str | None) -> dict[str, Any]:
    identity = parse_profile_ref(agent_name)
    existing = load_state(session_id)
    bound = (
        existing.get("kind") not in {None, KIND_UNRESOLVED}
        and existing.get("scope_token")
    )
    if identity["kind"] == KIND_UNRESOLVED:
        state = empty_state(session_id)
        state["kind"] = KIND_UNRESOLVED
        state["profile_name"] = None
        return state
    if bound and (
        existing.get("kind") != identity["kind"] or existing.get("profile_name") != identity["name"]
    ):
        state = empty_state(session_id)
        state["kind"] = KIND_UNRESOLVED
        state["profile_name"] = None
        return state
    if bound:
        return existing
    state = empty_state(session_id)
    state["kind"] = identity["kind"]
    state["profile_name"] = identity["name"]
    state["scope_token"] = secrets.token_urlsafe(24)
    save_state(state)
    return state


def tool_specs() -> list[dict[str, Any]]:
    return [
        {
            "type": "function",
            "function": {
                "name": "memory",
                "description": "Add, replace, or remove a durable MEMORY.md or USER.md entry for the bound Hermes profile.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scopeToken": {"type": "string"},
                        "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                        "target": {"type": "string", "enum": ["memory", "user"]},
                        "content": {"type": "string"},
                        "old_text": {"type": "string"},
                    },
                    "required": ["scopeToken", "action", "target"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "skill_manage",
                "description": "Create or edit a class-level skill under this profile's skills/user/.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scopeToken": {"type": "string"},
                        "action": {
                            "type": "string",
                            "enum": ["view", "list", "create", "edit", "patch", "write_file"],
                        },
                        "name": {"type": "string"},
                        "content": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "file_path": {"type": "string"},
                        "file_content": {"type": "string"},
                    },
                    "required": ["scopeToken", "action"],
                },
            },
        },
    ]


def gateway_root() -> str:
    explicit = os.environ.get("CODETAS_LEARNING_GATEWAY_URL")
    if not explicit:
        raise RuntimeError("CODETAS_LEARNING_GATEWAY_URL is required")
    value = explicit.rstrip("/")
    return value[:-3] if value.endswith("/v1") else value


def gateway_headers() -> dict[str, str]:
    return {"content-type": "application/json"}


def selected_learning_model() -> str:
    return os.environ.get("CODETAS_LEARNING_MODEL") or "gpt-5.6-luna"


def post_chat(messages: list[dict[str, Any]]) -> dict[str, Any]:
    payload = {
        "model": selected_learning_model(),
        "messages": messages,
        "tools": tool_specs(),
        "tool_choice": "auto",
        "temperature": 0,
    }
    req = request.Request(
        f"{gateway_root()}/v1/chat/completions",
        data=json.dumps(payload).encode("utf-8"),
        headers=gateway_headers(),
        method="POST",
    )
    try:
        with request.urlopen(req, timeout=GATEWAY_TIMEOUT_SECONDS) as response:
            body = response.read(2 * 1024 * 1024)
    except error.HTTPError as exc:
        detail = exc.read(16 * 1024).decode("utf-8", "replace")
        raise RuntimeError(f"gateway HTTP {exc.code}: {detail}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"gateway unreachable: {exc}") from exc
    data = json.loads(body.decode("utf-8"))
    if not isinstance(data, dict):
        raise RuntimeError("gateway returned a non-object")
    return data


def dispatch_tool(name: str, arguments: dict[str, Any], bound_token: str) -> dict[str, Any]:
    token = arguments.get("scopeToken") or arguments.get("scope_token")
    if not isinstance(token, str) or token != bound_token:
        return {"success": False, "error": "scopeToken does not match the bound sidecar session."}
    if name == "memory":
        return memory_tool(
            token,
            str(arguments.get("action") or ""),
            str(arguments.get("target") or ""),
            arguments.get("content") if isinstance(arguments.get("content"), str) else None,
            arguments.get("old_text") if isinstance(arguments.get("old_text"), str) else None,
        )
    if name == "skill_manage":
        return skill_manage(
            token,
            str(arguments.get("action") or ""),
            str(arguments.get("name") or ""),
            arguments.get("content") if isinstance(arguments.get("content"), str) else None,
            arguments.get("old_string") if isinstance(arguments.get("old_string"), str) else None,
            arguments.get("new_string") if isinstance(arguments.get("new_string"), str) else None,
            arguments.get("file_path") if isinstance(arguments.get("file_path"), str) else None,
            arguments.get("file_content") if isinstance(arguments.get("file_content"), str) else None,
        )
    return {"success": False, "error": f"unknown tool {name}"}


def parse_tool_arguments(raw: Any) -> dict[str, Any]:
    if isinstance(raw, dict):
        return raw
    if isinstance(raw, str) and raw.strip():
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {}


def run_review(state: dict[str, Any], cursor: TranscriptCursor, review: str) -> dict[str, Any]:
    bound = bind_scope(state.get("scope_token") if isinstance(state.get("scope_token"), str) else None)
    if bound.get("success") is False:
        return {"ok": False, "error": bound.get("error")}
    skills = list_user_skills(str(state.get("kind")), state.get("profile_name") if isinstance(state.get("profile_name"), str) else None)
    skill_lines = ", ".join(item["name"] for item in skills) or "none"
    system = (
        "You are the CODETAS sidecar learning agent. You are not the Codex "
        "conversation. Review the transcript excerpt and persist durable facts "
        "with memory / skill_manage. Do not answer the user. Do not claim you "
        "are continuing the Codex thread. If nothing is worth saving, say "
        "'Nothing to save.'"
    )
    user = (
        f"{review}\n\n"
        f"Existing user skills: {skill_lines}\n\n"
        f"Transcript excerpt:\n{transcript_excerpt(cursor)}"
    )
    if suspicious_context_reasons(user) or scan_memory_content(user):
        return {"ok": False, "error": "transcript injection scan failed", "writes": 0}
    token = state.get("scope_token")
    if not isinstance(token, str) or not token:
        return {"ok": False, "error": "missing bound scopeToken", "writes": 0}
    messages: list[dict[str, Any]] = [
        {"role": "system", "content": system},
        {"role": "user", "content": user},
    ]
    try:
        return _run_review_loop(messages, token)
    except Exception as exc:  # noqa: BLE001 — sidecar must not die on one bad model call
        return {"ok": False, "error": str(exc), "writes": 0}


def _run_review_loop(messages: list[dict[str, Any]], bound_token: str) -> dict[str, Any]:
    writes = 0
    last_text = ""
    for _ in range(MAX_TOOL_ROUNDS):
        data = post_chat(messages)
        choices = data.get("choices") if isinstance(data.get("choices"), list) else []
        if not choices or not isinstance(choices[0], dict):
            break
        message = choices[0].get("message") if isinstance(choices[0].get("message"), dict) else {}
        tool_calls = message.get("tool_calls") if isinstance(message.get("tool_calls"), list) else []
        content = message.get("content")
        if isinstance(content, str):
            last_text = content
        messages.append(message)
        if not tool_calls:
            break
        for call in tool_calls:
            if not isinstance(call, dict):
                continue
            function = call.get("function") if isinstance(call.get("function"), dict) else {}
            name = str(function.get("name") or "")
            arguments = parse_tool_arguments(function.get("arguments"))
            result = dispatch_tool(name, arguments, bound_token)
            if result.get("success"):
                writes += 1
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": call.get("id") or name,
                    "content": json.dumps(result, ensure_ascii=False)[:4000],
                }
            )
    return {"ok": True, "writes": writes, "text": last_text[:500]}


def sidecar_paths(session_id: str) -> tuple[Path, Path]:
    root = sidecar_dir()
    return root / f"{session_id}.json", root / f"{session_id}.stop"


def load_sidecar_state(session_id: str) -> dict[str, Any]:
    path, _ = sidecar_paths(session_id)
    if not path.exists():
        return empty_sidecar(session_id)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return empty_sidecar(session_id)
    return data if isinstance(data, dict) else empty_sidecar(session_id)


def empty_sidecar(session_id: str) -> dict[str, Any]:
    return {
        "session_id": session_id,
        "cursor": TranscriptCursor().snapshot(),
        "last_review_at": 0,
        "last_memory_turns": 0,
        "last_skill_turns": 0,
        "last_skill_tools": 0,
        "checkpoint_done": False,
    }


def save_sidecar_state(session_id: str, payload: dict[str, Any]) -> None:
    path, _ = sidecar_paths(session_id)
    atomic_write_json(path, payload)


def stop_requested(session_id: str) -> bool:
    _, stop = sidecar_paths(session_id)
    return stop.exists()


def request_stop(session_id: str) -> None:
    _, stop = sidecar_paths(session_id)
    stop.write_text("stop\n", encoding="utf-8")


def clear_stop(session_id: str) -> None:
    _, stop = sidecar_paths(session_id)
    try:
        stop.unlink()
    except OSError:
        pass


def step_session(session_id: str, jsonl_path: Path, *, ending: bool) -> dict[str, Any]:
    sidecar = load_sidecar_state(session_id)
    cursor = TranscriptCursor.from_snapshot(sidecar.get("cursor") if isinstance(sidecar.get("cursor"), dict) else None)
    cursor = ingest_jsonl(jsonl_path, cursor)
    state = bind_session_identity(session_id, cursor.agent_name)
    result: dict[str, Any] = {
        "session_id": session_id,
        "kind": state.get("kind"),
        "profile_name": state.get("profile_name"),
        "user_turns": cursor.user_turns,
        "tool_units": cursor.tool_units,
        "ending": ending or cursor.ended,
        "reviewed": False,
        "writes": 0,
    }
    if state.get("kind") == KIND_UNRESOLVED or not state.get("scope_token"):
        sidecar["cursor"] = cursor.snapshot()
        save_sidecar_state(session_id, sidecar)
        result["error"] = "unresolved profile"
        return result
    review = sidecar_review_text(sidecar, state, cursor, ending or cursor.ended)
    if review:
        outcome = run_review(state, cursor, review)
        result["reviewed"] = True
        result["writes"] = int(outcome.get("writes") or 0)
        result["review_error"] = outcome.get("error")
        sidecar["last_review_at"] = time.time()
        if outcome.get("ok"):
            acknowledge_sidecar_reviews(sidecar, cursor)
    sidecar["cursor"] = cursor.snapshot()
    save_sidecar_state(session_id, sidecar)
    return result


def finished_marker(session_id: str) -> Path:
    return sidecar_dir() / f"{session_id}.finished"


def mark_finished(session_id: str) -> None:
    finished_marker(session_id).write_text("finished\n", encoding="utf-8")


def is_finished(session_id: str) -> bool:
    return finished_marker(session_id).exists()


def run_sidecar_loop(session_id: str, jsonl_path: str) -> int:
    path = Path(jsonl_path)
    if not looks_like_session_id(session_id):
        return 2
    if is_finished(session_id):
        return 0
    clear_stop(session_id)
    idle_since = time.time()
    last_size = -1
    while True:
        if stop_requested(session_id):
            if is_regular_file(path):
                step_session(session_id, path, ending=True)
            clear_stop(session_id)
            mark_finished(session_id)
            return 0
        if not is_regular_file(path):
            if time.time() - idle_since >= LIVE_GRACE_SECONDS:
                mark_finished(session_id)
                return 0
            time.sleep(POLL_SECONDS)
            continue
        size = path.stat().st_size
        mtime = path.stat().st_mtime
        grew = size != last_size
        last_size = size
        if grew:
            idle_since = time.time()
            result = step_session(session_id, path, ending=False)
            if result.get("ending"):
                mark_finished(session_id)
                return 0
        elif time.time() - mtime >= STALE_SECONDS:
            step_session(session_id, path, ending=True)
            mark_finished(session_id)
            return 0
        time.sleep(POLL_SECONDS)


def main(argv: list[str] | None = None) -> int:
    args = list(argv if argv is not None else sys.argv[1:])
    if len(args) < 2 or args[0] in {"-h", "--help"}:
        print("usage: session_learning_runtime.py <session_id> <jsonl_path>", file=sys.stderr)
        return 2
    return run_sidecar_loop(args[0], args[1])


if __name__ == "__main__":
    raise SystemExit(main())
