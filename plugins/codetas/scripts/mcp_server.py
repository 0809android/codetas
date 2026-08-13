#!/usr/bin/env python3
"""Dependency-free, read-only MCP server for CODETAS project inspection."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
from typing import Any

from project_context import (
    context_preview,
    find_context_file,
    inspect_project,
    list_skills,
    project_root,
    suspicious_context_reasons,
)

SERVER_INFO = {"name": "codetas-project", "version": "0.1.0"}

TOOLS = [
    {
        "name": "inspect_project_context",
        "description": "Inspect the current project for Hermes context, reusable skills, and MCP metadata without modifying files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "projectPath": {
                    "type": "string",
                    "description": "Absolute path to the project. Defaults to the server working directory.",
                }
            },
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True, "destructiveHint": False},
    },
    {
        "name": "list_project_skills",
        "description": "List names, descriptions, and paths of Hermes-compatible SKILL.md files in the current project.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "projectPath": {
                    "type": "string",
                    "description": "Absolute path to the project. Defaults to the server working directory.",
                }
            },
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True, "destructiveHint": False},
    },
    {
        "name": "read_project_context",
        "description": "Read the current project's .hermes.md or HERMES.md after a local injection-safety check.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "projectPath": {
                    "type": "string",
                    "description": "Absolute path to the project. Defaults to the server working directory.",
                }
            },
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True, "destructiveHint": False},
    },
]


def text_result(value: Any, *, is_error: bool = False) -> dict[str, Any]:
    text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, indent=2)
    result: dict[str, Any] = {"content": [{"type": "text", "text": text}]}
    if is_error:
        result["isError"] = True
    return result


def requested_project(arguments: dict[str, Any]) -> Path:
    supplied = arguments.get("projectPath")
    path = Path(str(supplied)).expanduser() if supplied else Path(os.getcwd())
    resolved = path.resolve()
    if not resolved.is_dir():
        raise ValueError("projectPath must point to an existing directory")
    return resolved


def call_tool(name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    try:
        start = requested_project(arguments)
    except (OSError, ValueError) as error:
        return text_result(str(error), is_error=True)

    if name == "inspect_project_context":
        return text_result(inspect_project(start))
    if name == "list_project_skills":
        root = project_root(start)
        return text_result({"projectRoot": str(root), "skills": list_skills(root)})
    if name == "read_project_context":
        context_file = find_context_file(start)
        if context_file is None:
            return text_result("No .hermes.md or HERMES.md was found.", is_error=True)
        try:
            content, truncated = context_preview(context_file)
        except OSError as error:
            return text_result(f"Could not read {context_file}: {error}", is_error=True)
        reasons = suspicious_context_reasons(content)
        if reasons:
            return text_result(
                {"blocked": True, "path": str(context_file), "reasons": reasons},
                is_error=True,
            )
        return text_result(
            {"path": str(context_file), "content": content, "truncated": truncated}
        )
    return text_result(f"Unknown tool: {name}", is_error=True)


def handle(message: dict[str, Any]) -> dict[str, Any] | None:
    method = message.get("method")
    request_id = message.get("id")
    if request_id is None:
        return None

    if method == "initialize":
        params = message.get("params") or {}
        protocol_version = params.get("protocolVersion", "2024-11-05")
        result = {
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": SERVER_INFO,
            "instructions": "CODETAS tools are local and read-only. They never copy credentials or modify Hermes files.",
        }
    elif method == "ping":
        result = {}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        params = message.get("params") or {}
        arguments = params.get("arguments") or {}
        result = call_tool(str(params.get("name", "")), arguments)
    else:
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": f"Method not found: {method}"},
        }
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def main() -> int:
    for line in sys.stdin:
        try:
            message = json.loads(line)
            response = handle(message)
        except Exception as error:  # Keep the stdio server alive and return protocol-safe errors.
            response = {
                "jsonrpc": "2.0",
                "id": None,
                "error": {"code": -32603, "message": str(error)},
            }
        if response is not None:
            print(json.dumps(response, ensure_ascii=False), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
