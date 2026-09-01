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
    dispatch_sidecar_reviews,
    empty_state,
    list_user_skills,
    load_state,
    memory_tool,
    parse_profile_ref,
    persist_sidecar_review_acknowledgements,
    review_complete,
    review_prefix,
    save_state,
    skill_manage,
    state_dir,
)
from project_context import self_improvement_mode_enabled, suspicious_context_reasons

MAX_TRANSCRIPT_CHARS = 24_000
MAX_JSONL_BYTES = 8 * 1024 * 1024
MAX_LINE_BYTES = 256 * 1024
MAX_TOOL_ROUNDS = 4
GATEWAY_TIMEOUT_SECONDS = 90
POLL_SECONDS = 2.0
LIVE_GRACE_SECONDS = 20.0
STALE_SECONDS = 45 * 60
CODETAS_ORIGIN_SENTINEL = "CODETAS-LEARNING-ORIGIN"
START_GATE_ENV = "CODETAS_LEARNING_START_GATE"
START_GATE_PROTOCOL = "stdin-v1"
START_GATE_RELEASE = "start\n"
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
PROFILE_LINE_RE = re.compile(
    r"(?m)^CODETAS-LEARNING-PROFILE:(named|default):([A-Za-z0-9._-]{1,64})\r?$"
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


def inspect_profile_marker(text: str | None) -> tuple[str, str | None]:
    """Return (absent|valid|invalid, profile name).

    A CODETAS origin sentinel without a usable profile line is an identity
    signal, not an invitation to provision from cwd. Multiple conflicting
    markers are invalid.
    """
    if not text or CODETAS_ORIGIN_SENTINEL not in text:
        return "absent", None
    matches = list(PROFILE_MARKER_RE.finditer(text))
    sentinels = text.count(CODETAS_ORIGIN_SENTINEL)
    profile_lines = list(PROFILE_LINE_RE.finditer(text))
    if not matches or sentinels != len(matches) or len(profile_lines) != len(matches):
        return "invalid", None
    parsed: list[tuple[str, str]] = []
    for match in matches:
        kind, name = match.group(1), match.group(2)
        if kind == "default":
            if name != "default":
                return "invalid", None
            parsed.append(("default", "default"))
            continue
        if name in {"default", ".", ".."}:
            return "invalid", None
        parsed.append((kind, name))
    first = parsed[0]
    if any(item != first for item in parsed[1:]):
        return "invalid", None
    return "valid", first[1]


def agent_name_from_instructions(text: str | None) -> str | None:
    status, name = inspect_profile_marker(text)
    return name if status == "valid" else None


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
        self.agent_marker: str = "absent"
        self.ended = False
        self.jsonl_dev: str | None = None
        self.jsonl_ino: str | None = None

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
            "agent_marker": self.agent_marker,
            "ended": self.ended,
            "jsonl_dev": self.jsonl_dev,
            "jsonl_ino": self.jsonl_ino,
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
        marker = data.get("agent_marker")
        if marker in {"absent", "valid", "invalid"}:
            cursor.agent_marker = marker
        elif cursor.agent_name:
            cursor.agent_marker = "valid"
        cursor.ended = bool(data.get("ended"))
        cursor.jsonl_dev = identity_hex(data.get("jsonl_dev"))
        cursor.jsonl_ino = identity_hex(data.get("jsonl_ino"))
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
    if name in {"memory", "skill_manage", "review_complete"}:
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
        status, agent = inspect_profile_marker(text)
        if status != "absent":
            cursor.agent_marker = status
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


def ingest_jsonl(path: Path, cursor: TranscriptCursor) -> tuple[TranscriptCursor, dict[str, int] | None]:
    if not is_regular_file(path):
        return cursor, None
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        identity = jsonl_identity_from_handle(handle)
        generation_changed = False
        live = identity_fields(identity)
        stored = identity_fields({"dev": cursor.jsonl_dev, "ino": cursor.jsonl_ino})
        if cursor.offset > 0 and live and (not stored or stored.keys() != live.keys()):
            generation_changed = True
        if stored.get("dev") is not None and live.get("dev") is not None and stored["dev"] != live["dev"]:
            generation_changed = True
        if stored.get("ino") is not None and live.get("ino") is not None and stored["ino"] != live["ino"]:
            generation_changed = True
        if identity["size"] < cursor.offset or generation_changed:
            cursor = TranscriptCursor()
        if identity["size"] > MAX_JSONL_BYTES:
            # Keep the tail so a huge transcript still yields recent turns.
            cursor.offset = max(cursor.offset, identity["size"] - MAX_JSONL_BYTES)
            cursor.carry = ""
        handle.seek(cursor.offset)
        chunk = handle.read(MAX_JSONL_BYTES)
        cursor.offset = handle.tell()
        identity = jsonl_identity_from_handle(handle)
        cursor.jsonl_dev = identity_hex(identity.get("dev"))
        cursor.jsonl_ino = identity_hex(identity.get("ino"))
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
    return cursor, identity


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
    state.update(dispatch_sidecar_reviews(state, memory_reason=memory_reason, skill_due=skill_due))
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


def acknowledge_sidecar_reviews(sidecar: dict[str, Any], cursor: TranscriptCursor, completed: set[str]) -> None:
    reason = sidecar.get("pending_memory_reason")
    if reason and "memory_review" in completed:
        if reason == "checkpoint":
            sidecar["checkpoint_done"] = True
        sidecar["last_memory_turns"] = cursor.user_turns
        sidecar["pending_memory_reason"] = None
    if sidecar.get("pending_skill") and "skill_review" in completed:
        sidecar["last_skill_turns"] = cursor.user_turns
        sidecar["last_skill_tools"] = cursor.tool_units
        sidecar["pending_skill"] = False
    if reason == "exit" and "memory_review" in completed and not sidecar.get("pending_skill"):
        sidecar["exit_flush_done"] = True


def bind_session_identity(
    session_id: str,
    agent_name: str | None,
    cwd: str | None = None,
    agent_marker: str = "absent",
) -> dict[str, Any]:
    from profile_learning import first_identity_signal, resolve_profile

    event: dict[str, Any] = {}
    if isinstance(cwd, str) and cwd.strip():
        event["cwd"] = cwd.strip()
    if agent_marker != "invalid" and isinstance(agent_name, str) and agent_name.strip():
        event["agent_name"] = agent_name.strip()
    existing = load_state(session_id)
    revoked = existing.get("identity_status") == "revoked"
    bound = (
        existing.get("kind") not in {None, KIND_UNRESOLVED}
        and bool(existing.get("scope_token"))
        and not revoked
    )
    writes_enabled = self_improvement_mode_enabled()
    signal = None if agent_marker == "invalid" else first_identity_signal(event)

    def revoke_existing() -> dict[str, Any]:
        existing["scope_token"] = None
        existing["snapshot_writable"] = False
        existing["identity_status"] = "revoked"
        existing["scope_epoch"] = int(existing.get("scope_epoch") or 0) + 1
        save_state(existing)
        return existing

    if revoked:
        existing["scope_token"] = None
        existing["snapshot_writable"] = False
        existing["identity_status"] = "revoked"
        return existing
    if not writes_enabled:
        return existing
    if agent_marker == "invalid":
        return revoke_existing() if bound or existing.get("kind") not in {None, KIND_UNRESOLVED} else empty_state(session_id)
    elif bound and signal is None:
        return existing
    else:
        # Provision a CODETAS-owned project profile only when nothing explicit
        # was supplied. A present-but-unresolved env/event/marker must not
        # fall back to cwd.
        identity = resolve_profile(
            event,
            allow_provision=not bound and signal is None,
        )
    if identity["kind"] == KIND_UNRESOLVED:
        return revoke_existing() if bound else empty_state(session_id)
    if bound and (
        existing.get("kind") != identity["kind"] or existing.get("profile_name") != identity["name"]
    ):
        return revoke_existing()
    if bound:
        return existing
    from profile_learning import activate_writable_identity, build_snapshot
    return activate_writable_identity(
        session_id,
        str(identity["kind"]),
        identity["name"],
        lambda token: build_snapshot(str(identity["kind"]), identity["name"], token, writable=True),
    )


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
        {
            "type": "function",
            "function": {
                "name": "review_complete",
                "description": "Mark the current dispatched review as nothing_to_save. Requires the bound scopeToken and the review id from the prompt.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scopeToken": {"type": "string"},
                        "reviewId": {"type": "string"},
                        "outcome": {"type": "string", "enum": ["nothing_to_save"]},
                    },
                    "required": ["scopeToken", "reviewId", "outcome"],
                },
            },
        },
    ]


def learning_gateway_available() -> bool:
    try:
        root = gateway_root()
    except RuntimeError:
        return False
    req = request.Request(f"{root}/v1/models", headers=gateway_headers(), method="GET")
    try:
        with request.urlopen(req, timeout=2) as response:
            return 200 <= getattr(response, "status", 200) < 400
    except error.HTTPError as exc:
        return False
    except (error.URLError, TimeoutError, OSError, ValueError):
        return False


def gateway_root() -> str:
    explicit = os.environ.get("CODETAS_LEARNING_GATEWAY_URL")
    if not explicit:
        raise RuntimeError("CODETAS_LEARNING_GATEWAY_URL is required")
    value = explicit.rstrip("/")
    return value[:-3] if value.endswith("/v1") else value


def gateway_headers() -> dict[str, str]:
    headers = {"content-type": "application/json"}
    token = (
        os.environ.get("CODETAS_LEARNING_GATEWAY_TOKEN")
        or os.environ.get("CODETAS_GATEWAY_TOKEN")
        or os.environ.get("CODETAS_CLIENT_TOKEN")
        or ""
    ).strip()
    if token:
        headers["x-codetas-token"] = token
    return headers


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
    if name == "review_complete":
        return review_complete(
            token,
            str(arguments.get("reviewId") or arguments.get("review_id") or ""),
            str(arguments.get("outcome") or "nothing_to_save"),
        )
    return {"success": False, "error": f"unknown tool {name}"}


def mutating_tool_success(name: str, arguments: dict[str, Any], result: dict[str, Any] | None = None) -> bool:
    if name == "memory":
        mutating = str(arguments.get("action") or "") in {"add", "replace", "remove"}
    elif name == "skill_manage":
        mutating = str(arguments.get("action") or "") in {"create", "edit", "patch", "write_file"}
    else:
        return False
    if not mutating:
        return False
    if result is None:
        return True
    return bool(result.get("success")) and result.get("changed") is True


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
        "are continuing the Codex thread. If nothing is worth saving, call "
        "review_complete with the bound scopeToken and the review id."
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
    completed = False
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
            if name == "review_complete" and result.get("success"):
                completed = True
            if mutating_tool_success(name, arguments, result):
                writes += 1
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": call.get("id") or name,
                    "content": json.dumps(result, ensure_ascii=False)[:4000],
                }
            )
    if writes > 0:
        return {"ok": True, "writes": writes, "text": last_text[:500], "outcome": "saved"}
    if completed:
        return {"ok": True, "writes": 0, "text": last_text[:500], "outcome": "nothing_to_save"}
    return {"ok": False, "writes": 0, "error": "review produced no mutating write or explicit no-op", "outcome": "incomplete"}


def sidecar_paths(session_id: str) -> tuple[Path, Path, Path]:
    root = sidecar_dir()
    return root / f"{session_id}.json", root / f"{session_id}.stop", root / f"{session_id}.pause"


def load_sidecar_state(session_id: str) -> dict[str, Any]:
    path, _, _ = sidecar_paths(session_id)
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
        "exit_flush_done": False,
        "revision": 0,
        "work_boundary": {"user_turns": 0, "tool_units": 0, "offset": 0, "id": 0},
    }


def save_sidecar_state(session_id: str, payload: dict[str, Any]) -> None:
    path, _, _ = sidecar_paths(session_id)
    atomic_write_json(path, payload)


def sidecar_revision(sidecar: dict[str, Any]) -> int:
    parsed = json_nonneg_int(sidecar.get("revision"))
    return parsed if parsed is not None else 0


def with_sidecar_lock(session_id: str, mutator):
    from profile_learning import _release_session_file_lock, _with_session_file_lock, session_lock

    with session_lock(session_id):
        handle = _with_session_file_lock(session_id)
        try:
            sidecar = load_sidecar_state(session_id)
            updated = mutator(sidecar)
            if updated is None:
                return sidecar
            if updated is sidecar or updated.get("_dirty"):
                updated.pop("_dirty", None)
                updated["revision"] = sidecar_revision(sidecar) + 1
                save_sidecar_state(session_id, updated)
                return updated
            return sidecar
        finally:
            _release_session_file_lock(session_id, handle)


def update_sidecar_state(session_id: str, mutator) -> dict[str, Any]:
    return with_sidecar_lock(session_id, mutator)


def finished_marker(session_id: str) -> Path:
    return sidecar_dir() / f"{session_id}.finished"


def enable_boundary_marker(session_id: str) -> Path:
    return sidecar_dir() / f"{session_id}.enable-boundary"


def parse_enable_boundary_marker(raw: str) -> dict[str, Any] | None:
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict) or data.get("kind") != "enable-boundary":
        return None
    offset = json_nonneg_int(data.get("offset"))
    size = json_nonneg_int(data.get("jsonlSize"))
    dev = identity_hex(data.get("jsonlDev"))
    ino = identity_hex(data.get("jsonlIno"))
    if offset is None or size is None or offset != size or not dev or not ino:
        return None
    return {"offset": offset, "jsonlSize": size, "jsonlDev": dev, "jsonlIno": ino}


def _scan_pre_target_identity(path: Path, target: int, cursor: TranscriptCursor) -> None:
    if target <= 0 or not is_regular_file(path):
        return
    with path.open("rb") as handle:
        raw = handle.read(target)
    for line in raw.decode("utf-8", errors="replace").splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        try:
            record = json.loads(stripped)
        except json.JSONDecodeError:
            continue
        if not isinstance(record, dict):
            continue
        if record.get("type") in {"session_meta", "turn_context"}:
            consume_record(cursor, record)


def consume_enable_boundary(session_id: str, path: Path) -> dict[str, Any] | None:
    marker_path = enable_boundary_marker(session_id)
    if not marker_path.is_file():
        return None

    applied = {"ok": False, "parsed": False}

    def apply_boundary(sidecar: dict[str, Any]) -> dict[str, Any] | None:
        try:
            raw = marker_path.read_text(encoding="utf-8")
        except OSError:
            return None
        marker = parse_enable_boundary_marker(raw)
        applied["parsed"] = marker is not None
        try:
            identity = jsonl_identity(path) if is_regular_file(path) else None
        except OSError:
            identity = None
        if identity is None:
            return None
        live = identity_fields(identity)
        current_size = int(identity.get("size") or 0)
        if not live.get("dev") or not live.get("ino"):
            return None
        if (
            marker
            and live.get("dev") == marker["jsonlDev"]
            and live.get("ino") == marker["jsonlIno"]
            and current_size >= marker["offset"]
        ):
            target = marker["offset"]
        else:
            target = current_size
        applied["ok"] = True
        cursor = TranscriptCursor()
        cursor.offset = target
        cursor.jsonl_dev = live.get("dev")
        cursor.jsonl_ino = live.get("ino")
        _scan_pre_target_identity(path, target, cursor)
        sidecar["cursor"] = cursor.snapshot()
        sidecar["last_review_at"] = 0
        sidecar["last_memory_turns"] = 0
        sidecar["last_skill_turns"] = 0
        sidecar["last_skill_tools"] = 0
        sidecar["checkpoint_done"] = False
        sidecar["exit_flush_done"] = False
        sidecar["pending_memory_reason"] = None
        sidecar["pending_skill"] = False
        existing = sidecar.get("work_boundary") if isinstance(sidecar.get("work_boundary"), dict) else {}
        boundary_id = int(existing.get("id") or 0) + 1
        sidecar["work_boundary"] = {
            "user_turns": 0,
            "tool_units": 0,
            "offset": target,
            "id": boundary_id,
            "jsonl_size": target,
        }
        if cursor.jsonl_dev:
            sidecar["work_boundary"]["jsonl_dev"] = cursor.jsonl_dev
        if cursor.jsonl_ino:
            sidecar["work_boundary"]["jsonl_ino"] = cursor.jsonl_ino
        sidecar["_dirty"] = True
        return sidecar

    updated = with_sidecar_lock(session_id, apply_boundary)
    if not applied["ok"]:
        return updated
    saved = load_sidecar_state(session_id)
    cursor = saved.get("cursor") if isinstance(saved.get("cursor"), dict) else {}
    work = saved.get("work_boundary") if isinstance(saved.get("work_boundary"), dict) else {}
    saved_ok = (
        json_nonneg_int(cursor.get("offset")) is not None
        and json_nonneg_int(work.get("offset")) is not None
        and json_nonneg_int(work.get("id")) is not None
        and saved.get("pending_memory_reason") is None
        and saved.get("pending_skill") is False
        and json_nonneg_int(cursor.get("offset")) == json_nonneg_int(work.get("offset"))
    )
    if saved_ok and applied["parsed"]:
        try:
            marker_path.unlink()
        except OSError:
            pass
    return updated


def optional_int(value: Any) -> int | None:
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str) and value.strip().lstrip("-").isdigit():
        return int(value.strip())
    return None


U64_MAX = (1 << 64) - 1
HEX_IDENTITY_RE = re.compile(r"^[0-9a-f]{2,32}$")


def json_nonneg_int(value: Any) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0 or value > U64_MAX:
        return None
    return value


def identity_hex(value: Any) -> str | None:
    if not isinstance(value, str) or not HEX_IDENTITY_RE.fullmatch(value):
        return None
    return value


def even_hex(value: int) -> str:
    text = format(int(value), "x")
    if len(text) % 2:
        text = "0" + text
    return text[:32]


def identity_from_stat(stat) -> dict[str, Any]:
    identity: dict[str, Any] = {"size": int(stat.st_size)}
    if hasattr(stat, "st_dev"):
        identity["dev"] = even_hex(stat.st_dev)
    if hasattr(stat, "st_ino"):
        identity["ino"] = even_hex(stat.st_ino)
    return identity


def windows_file_identity(fd: int) -> dict[str, str] | None:
    import ctypes
    from ctypes import wintypes

    class FILE_ID_128(ctypes.Structure):
        _fields_ = [("Identifier", ctypes.c_ubyte * 16)]

    class FILE_ID_INFO(ctypes.Structure):
        _fields_ = [("VolumeSerialNumber", ctypes.c_uint64), ("FileId", FILE_ID_128)]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    get_info = kernel32.GetFileInformationByHandleEx
    get_info.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    get_info.restype = wintypes.BOOL
    import msvcrt

    handle = msvcrt.get_osfhandle(fd)
    info = FILE_ID_INFO()
    if not get_info(handle, 18, ctypes.byref(info), ctypes.sizeof(info)):
        return None
    return {
        "dev": format(int(info.VolumeSerialNumber), "016x"),
        "ino": bytes(info.FileId.Identifier).hex(),
    }


def jsonl_identity_from_handle(handle) -> dict[str, Any]:
    if os.name == "nt":
        identity: dict[str, Any] = {"size": int(os.fstat(handle.fileno()).st_size)}
        try:
            win = windows_file_identity(handle.fileno())
        except OSError:
            win = None
        if not win:
            return identity
        identity["dev"] = win["dev"]
        identity["ino"] = win["ino"]
        return identity
    return identity_from_stat(os.fstat(handle.fileno()))


def jsonl_identity(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return jsonl_identity_from_handle(handle)


def identity_fields(identity: dict[str, Any] | None) -> dict[str, str]:
    if not isinstance(identity, dict):
        return {}
    fields: dict[str, str] = {}
    for key in ("dev", "ino"):
        parsed = identity_hex(identity.get(key))
        if parsed is None:
            continue
        fields[key] = parsed
    return fields


def jsonl_matches_boundary(identity: dict[str, Any], boundary: dict[str, Any]) -> bool:
    if not isinstance(boundary, dict):
        return False
    offset = json_nonneg_int(boundary.get("offset"))
    stored_size = json_nonneg_int(boundary.get("jsonl_size"))
    if stored_size is None:
        stored_size = json_nonneg_int(boundary.get("jsonlSize"))
    live_size = json_nonneg_int(identity.get("size"))
    if offset is None or stored_size is None or live_size is None:
        return False
    if live_size != offset or live_size != stored_size:
        return False
    stored = identity_fields(
        {
            "dev": boundary.get("jsonl_dev", boundary.get("jsonlDev")),
            "ino": boundary.get("jsonl_ino", boundary.get("jsonlIno")),
        }
    )
    live = identity_fields(identity)
    if set(stored) != {"dev", "ino"} or set(live) != {"dev", "ino"}:
        return False
    return stored["dev"] == live["dev"] and stored["ino"] == live["ino"]


def parse_finished_marker(raw: str) -> dict[str, Any] | None:
    try:
        data = json.loads(raw)
    except (json.JSONDecodeError, UnicodeError, TypeError):
        return None
    if not isinstance(data, dict) or data.get("kind") != "finished":
        return None
    boundary = data.get("workBoundary")
    if not isinstance(boundary, dict):
        return None
    offset = json_nonneg_int(boundary.get("offset"))
    top_size = json_nonneg_int(data.get("jsonlSize"))
    revision = json_nonneg_int(data.get("revision"))
    top_dev = identity_hex(data.get("jsonlDev"))
    top_ino = identity_hex(data.get("jsonlIno"))
    if offset is None or top_size is None or revision is None or top_dev is None or top_ino is None:
        return None
    if "jsonlSize" in boundary:
        nested_size = json_nonneg_int(boundary.get("jsonlSize"))
        if nested_size is None or nested_size != top_size:
            return None
    if "jsonlDev" in boundary:
        nest_dev = identity_hex(boundary.get("jsonlDev"))
        if nest_dev is None or nest_dev != top_dev:
            return None
    if "jsonlIno" in boundary:
        nest_ino = identity_hex(boundary.get("jsonlIno"))
        if nest_ino is None or nest_ino != top_ino:
            return None
    return {
        "kind": "finished",
        "revision": revision,
        "offset": offset,
        "jsonlSize": top_size,
        "jsonlDev": top_dev,
        "jsonlIno": top_ino,
    }


def load_finished_marker(session_id: str) -> dict[str, Any] | None:
    path = finished_marker(session_id)
    if not path.exists():
        return None
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return {"kind": "legacy"}
    parsed = parse_finished_marker(raw)
    return parsed if parsed is not None else {"kind": "legacy"}


def mark_finished(session_id: str, jsonl_path: Path | None = None) -> bool:
    if jsonl_path is None:
        return False
    return publish_finished_if_current(session_id, Path(jsonl_path))


def publish_finished_if_current(session_id: str, jsonl_path: Path) -> bool:
    published = {"ok": False}

    def mutate(sidecar: dict[str, Any]) -> dict[str, Any] | None:
        cursor = sidecar.get("cursor") if isinstance(sidecar.get("cursor"), dict) else {}
        if sidecar.get("pending_skill") or not sidecar.get("exit_flush_done") or not cursor.get("ended"):
            return None
        boundary = sidecar.get("work_boundary") if isinstance(sidecar.get("work_boundary"), dict) else {}
        try:
            identity = jsonl_identity(jsonl_path)
        except OSError:
            return None
        if identity.get("dev") is None or identity.get("ino") is None:
            return None
        if not jsonl_matches_boundary(identity, boundary):
            return None
        offset = json_nonneg_int(boundary.get("offset"))
        if offset is None:
            return None
        revision = json_nonneg_int(sidecar.get("revision"))
        if revision is None:
            return None
        payload = {
            "kind": "finished",
            "sessionId": session_id,
            "revision": revision,
            "workBoundary": {
                "id": int(json_nonneg_int(boundary.get("id")) or 0),
                "offset": offset,
                "userTurns": int(json_nonneg_int(boundary.get("user_turns")) or 0),
                "toolUnits": int(json_nonneg_int(boundary.get("tool_units")) or 0),
                "jsonlSize": offset,
                "jsonlDev": identity["dev"],
                "jsonlIno": identity["ino"],
            },
            "jsonlSize": offset,
            "jsonlDev": identity["dev"],
            "jsonlIno": identity["ino"],
        }
        atomic_write_json(finished_marker(session_id), payload)
        published["ok"] = True
        return None

    update_sidecar_state(session_id, mutate)
    return published["ok"]


def is_finished(session_id: str) -> bool:
    return finished_marker(session_id).exists()


def finished_is_stale(session_id: str, jsonl_path: Path) -> bool:
    marker = load_finished_marker(session_id)
    if marker is None or not jsonl_path.exists():
        return False
    try:
        identity = jsonl_identity(jsonl_path)
    except FileNotFoundError:
        return False
    except OSError:
        return True
    if marker.get("kind") != "finished":
        return True
    comparable = {
        "offset": marker.get("offset"),
        "jsonl_size": marker.get("jsonlSize"),
        "jsonl_dev": marker.get("jsonlDev"),
        "jsonl_ino": marker.get("jsonlIno"),
    }
    if not jsonl_matches_boundary(identity, comparable):
        return True
    sidecar = load_sidecar_state(session_id)
    marker_revision = json_nonneg_int(marker.get("revision"))
    if marker_revision is None:
        return True
    return sidecar_revision(sidecar) > marker_revision


def merge_reset_sidecar_lifecycle(session_id: str) -> None:
    """Clear finished/end flags without rewinding a live sidecar cursor."""
    if not looks_like_session_id(session_id):
        return
    def reset(sidecar: dict[str, Any]) -> dict[str, Any]:
        try:
            finished_marker(session_id).unlink()
        except OSError:
            pass
        cursor = sidecar.get("cursor") if isinstance(sidecar.get("cursor"), dict) else {}
        cursor["ended"] = False
        sidecar["cursor"] = cursor
        sidecar["exit_flush_done"] = False
        sidecar["pending_memory_reason"] = None
        sidecar["pending_skill"] = False
        sidecar["_dirty"] = True
        return sidecar

    update_sidecar_state(session_id, reset)


def stop_requested(session_id: str) -> bool:
    _, stop, _ = sidecar_paths(session_id)
    return stop.exists()


def pause_requested(session_id: str) -> bool:
    _, _, pause = sidecar_paths(session_id)
    return pause.exists()


def clear_pause(session_id: str) -> None:
    _, _, pause = sidecar_paths(session_id)
    try:
        pause.unlink()
    except OSError:
        pass



def request_stop(session_id: str) -> None:
    _, stop, _ = sidecar_paths(session_id)
    stop.write_text("stop\n", encoding="utf-8")


def clear_stop(session_id: str) -> None:
    _, stop, _ = sidecar_paths(session_id)
    try:
        stop.unlink()
    except OSError:
        pass


def mark_missed_flush(session_id: str) -> None:
    from profile_learning import persist_flush_incomplete

    persist_flush_incomplete(session_id)


def step_session(session_id: str, jsonl_path: Path, *, ending: bool) -> dict[str, Any]:
    snapshot_holder: dict[str, Any] = {}

    def take_snapshot(sidecar: dict[str, Any]) -> None:
        snapshot_holder["sidecar"] = dict(sidecar)
        snapshot_holder["revision"] = sidecar_revision(sidecar)
        return None

    with_sidecar_lock(session_id, take_snapshot)
    sidecar = snapshot_holder.get("sidecar") or empty_sidecar(session_id)
    expected_revision = int(snapshot_holder.get("revision") or 0)
    cursor = TranscriptCursor.from_snapshot(sidecar.get("cursor") if isinstance(sidecar.get("cursor"), dict) else None)
    cursor, identity = ingest_jsonl(jsonl_path, cursor)
    if identity is None:
        return {
            "session_id": session_id,
            "ending": bool(cursor.ended),
            "reviewed": False,
            "writes": 0,
            "flush_complete": False,
            "error": "jsonl unavailable",
        }
    exit_due = bool(ending or cursor.ended)

    boundary_saved = {"ok": False}

    def persist_work_boundary(latest: dict[str, Any]) -> dict[str, Any] | None:
        if sidecar_revision(latest) != expected_revision:
            return None
        latest["cursor"] = cursor.snapshot()
        latest["work_boundary"] = {
            "user_turns": cursor.user_turns,
            "tool_units": cursor.tool_units,
            "offset": cursor.offset,
            "id": int((latest.get("work_boundary") or {}).get("id") or 0) + 1,
            "jsonl_size": int(identity.get("size") or cursor.offset),
        }
        if identity.get("dev") is not None:
            latest["work_boundary"]["jsonl_dev"] = identity["dev"]
        if identity.get("ino") is not None:
            latest["work_boundary"]["jsonl_ino"] = identity["ino"]
        latest["_dirty"] = True
        boundary_saved["ok"] = True
        return latest

    committed = with_sidecar_lock(session_id, persist_work_boundary)
    if not boundary_saved["ok"]:
        return {
            "session_id": session_id,
            "ending": bool(cursor.ended),
            "reviewed": False,
            "writes": 0,
            "flush_complete": False,
            "error": "sidecar revision conflict",
        }
    sidecar = committed
    expected_revision = sidecar_revision(sidecar)
    result: dict[str, Any] = {
        "session_id": session_id,
        "ending": bool(cursor.ended),
        "reviewed": False,
        "writes": 0,
        "flush_complete": False,
    }
    review = None
    state = None
    if not self_improvement_mode_enabled():
        result["error"] = "self-improvement mode off"
    else:
        state = bind_session_identity(session_id, cursor.agent_name, cursor.cwd, cursor.agent_marker)
        result["kind"] = state.get("kind")
        result["profile_name"] = state.get("profile_name")
        result["user_turns"] = cursor.user_turns
        result["tool_units"] = cursor.tool_units
        if state.get("kind") == KIND_UNRESOLVED or not state.get("scope_token"):
            result["error"] = "unresolved profile"
        else:
            review = sidecar_review_text(sidecar, state, cursor, exit_due)
            if review:
                required = set()
                if sidecar.get("pending_memory_reason"):
                    required.add("memory_review")
                if sidecar.get("pending_skill"):
                    required.add("skill_review")
                outcome = run_review(state, cursor, review)
                result["reviewed"] = True
                result["writes"] = int(outcome.get("writes") or 0)
                result["review_error"] = outcome.get("error")
                result["review_outcome"] = outcome.get("outcome")
                latest = load_state(session_id)
                latest["user_turn_count"] = max(int(latest.get("user_turn_count") or 0), cursor.user_turns)
                save_state(latest)
                persisted, completed = persist_sidecar_review_acknowledgements(session_id)
                acknowledge_sidecar_reviews(sidecar, cursor, completed)
                memory = persisted.get("memory_review") if isinstance(persisted.get("memory_review"), dict) else None
                if (
                    "memory_review" in completed
                    and isinstance(memory, dict)
                    and memory.get("reason") in {"checkpoint", "exit"}
                ):
                    boundary = sidecar.get("work_boundary") if isinstance(sidecar.get("work_boundary"), dict) else {}
                    if "id" in boundary:
                        persisted["flush_acked_revision"] = int(boundary["id"])
                        persisted["flush_acked_offset"] = int(boundary.get("offset") or 0)
                        persisted["flush_acked_tools"] = int(boundary.get("tool_units") or 0)
                        persisted["flush_acked_turns"] = int(boundary.get("user_turns") or cursor.user_turns)
                    save_state(persisted)
                if required - completed:
                    result["review_outcome"] = "incomplete"

    def commit(latest: dict[str, Any]) -> dict[str, Any] | None:
        if sidecar_revision(latest) != expected_revision:
            if exit_due:
                result["flush_complete"] = bool(latest.get("exit_flush_done"))
                if not result["flush_complete"]:
                    mark_missed_flush(session_id)
            return None
        latest["cursor"] = cursor.snapshot()
        existing_boundary = latest.get("work_boundary") if isinstance(latest.get("work_boundary"), dict) else {}
        if existing_boundary:
            latest["work_boundary"] = existing_boundary
        else:
            latest["work_boundary"] = {
                "user_turns": cursor.user_turns,
                "tool_units": cursor.tool_units,
                "offset": cursor.offset,
                "id": sidecar_revision(latest),
                "jsonl_size": int(identity.get("size") or cursor.offset),
            }
            if identity.get("dev") is not None:
                latest["work_boundary"]["jsonl_dev"] = identity["dev"]
            if identity.get("ino") is not None:
                latest["work_boundary"]["jsonl_ino"] = identity["ino"]
        if review:
            latest["last_review_at"] = time.time()
            latest["pending_memory_reason"] = sidecar.get("pending_memory_reason")
            latest["pending_skill"] = sidecar.get("pending_skill")
            latest["last_memory_turns"] = sidecar.get("last_memory_turns")
            latest["last_skill_turns"] = sidecar.get("last_skill_turns")
            latest["last_skill_tools"] = sidecar.get("last_skill_tools")
            latest["checkpoint_done"] = sidecar.get("checkpoint_done")
            latest["exit_flush_done"] = sidecar.get("exit_flush_done")
        if result.get("error") in {"self-improvement mode off", "unresolved profile"} and exit_due:
            mark_missed_flush(session_id)
        if exit_due:
            result["flush_complete"] = bool(latest.get("exit_flush_done")) and not latest.get("pending_skill")
            if not result["flush_complete"]:
                mark_missed_flush(session_id)
        latest["_dirty"] = True
        return latest

    with_sidecar_lock(session_id, commit)
    return result


def await_start_gate() -> bool:
    protocol = os.environ.get(START_GATE_ENV)
    if protocol is None:
        return True
    if protocol != START_GATE_PROTOCOL:
        return False
    try:
        return sys.stdin.readline() == START_GATE_RELEASE
    except (OSError, UnicodeError):
        return False



def apply_enable_boundary_if_present(session_id: str, path: Path) -> str:
    """Consume a pending enable-boundary marker.

    Returns:
        absent: no marker
        consumed: marker applied and removed
        blocked: marker remains (unreadable/malformed/missing identity)
    """
    marker_path = enable_boundary_marker(session_id)
    if not marker_path.is_file():
        return "absent"
    consume_enable_boundary(session_id, path)
    if marker_path.is_file():
        return "blocked"
    return "consumed"


def run_sidecar_loop(session_id: str, jsonl_path: str) -> int:
    path = Path(jsonl_path)
    if not looks_like_session_id(session_id):
        return 2
    if is_finished(session_id):
        if finished_is_stale(session_id, path):
            merge_reset_sidecar_lifecycle(session_id)
        else:
            return 0
    else:
        sidecar = load_sidecar_state(session_id)
        cursor = sidecar.get("cursor") if isinstance(sidecar.get("cursor"), dict) else {}
        if sidecar.get("exit_flush_done") and cursor.get("ended") and not path.exists():
            return 0
        if sidecar.get("exit_flush_done") and cursor.get("ended") and path.exists():
            merge_reset_sidecar_lifecycle(session_id)
    idle_since = time.time()
    last_size = -1
    last_dev = None
    last_ino = None
    startup_boundary = apply_enable_boundary_if_present(session_id, path)
    if startup_boundary == "blocked":
        return 0
    if startup_boundary == "consumed":
        last_size = -1
        last_dev = None
        last_ino = None
    while True:
        loop_boundary = apply_enable_boundary_if_present(session_id, path)
        if loop_boundary == "blocked":
            time.sleep(POLL_SECONDS)
            continue
        if loop_boundary == "consumed":
            last_size = -1
            last_dev = None
            last_ino = None
            continue
        if stop_requested(session_id):
            ended = False
            flush_complete = False
            can_review = (
                is_regular_file(path)
                and self_improvement_mode_enabled()
                and learning_gateway_available()
            )
            if can_review:
                result = step_session(session_id, path, ending=True)
                ended = bool(result.get("ending"))
                flush_complete = bool(result.get("flush_complete"))
            if not flush_complete:
                mark_missed_flush(session_id)
            clear_stop(session_id)
            if ended and flush_complete:
                if publish_finished_if_current(session_id, path):
                    return 0
                last_size = -1
                last_dev = None
                last_ino = None
                continue
            return 0
        if not self_improvement_mode_enabled() or not learning_gateway_available() or pause_requested(session_id):
            sidecar = load_sidecar_state(session_id)
            if not sidecar.get("exit_flush_done"):
                mark_missed_flush(session_id)
            time.sleep(POLL_SECONDS)
            continue
        if not is_regular_file(path):
            time.sleep(POLL_SECONDS)
            continue
        try:
            live_identity = jsonl_identity(path)
        except OSError:
            time.sleep(POLL_SECONDS)
            continue
        size = live_identity["size"]
        mtime = path.stat().st_mtime
        grew = (
            size != last_size
            or live_identity.get("dev") != last_dev
            or live_identity.get("ino") != last_ino
        )
        last_size = size
        last_dev = live_identity.get("dev")
        last_ino = live_identity.get("ino")
        if grew:
            idle_since = time.time()
            result = step_session(session_id, path, ending=False)
            if result.get("ending") and result.get("flush_complete"):
                if publish_finished_if_current(session_id, path):
                    return 0
                last_size = -1
                last_dev = None
                last_ino = None
        elif time.time() - mtime >= STALE_SECONDS:
            time.sleep(POLL_SECONDS)
            continue
        time.sleep(POLL_SECONDS)


def main(argv: list[str] | None = None) -> int:
    args = list(argv if argv is not None else sys.argv[1:])
    if len(args) < 2 or args[0] in {"-h", "--help"}:
        print("usage: session_learning_runtime.py <session_id> <jsonl_path>", file=sys.stderr)
        return 2
    if not looks_like_session_id(args[0]):
        return 2
    if not self_improvement_mode_enabled():
        return 0
    if not await_start_gate():
        return 3
    return run_sidecar_loop(args[0], args[1])


if __name__ == "__main__":
    raise SystemExit(main())
