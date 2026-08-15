#!/usr/bin/env python3
"""Dependency-free MCP server for CODETAS project inspection and media delegation."""

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
from media_tools import (
    document_analyze,
    generated_image_result,
    image_generate,
    video_analyze,
    vision_analyze,
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
    {
        "name": "vision_analyze",
        "description": "Analyze an image with the Vision auxiliary model configured in the CODETAS app.",
        "inputSchema": {"type": "object", "properties": {"image": {"type": "string", "description": "HTTPS URL, data URL, or local image path."}, "question": {"type": "string"}}, "required": ["image"], "additionalProperties": False},
        "annotations": {"readOnlyHint": True, "destructiveHint": False},
    },
    {
        "name": "video_analyze",
        "description": "Process a local video according to the CODETAS video input mode: use the configured auxiliary model in text mode, or attach the original as an MCP resource in native mode.",
        "inputSchema": {"type": "object", "properties": {"videoPath": {"type": "string"}, "question": {"type": "string"}, "sampleFrames": {"type": "integer", "minimum": 1, "maximum": 64}}, "required": ["videoPath"], "additionalProperties": False},
        "annotations": {"readOnlyHint": True, "destructiveHint": False},
    },
    {
        "name": "document_analyze",
        "description": "Process a PDF or image document according to the CODETAS document input mode, including batched page rendering and OCR when auxiliary analysis is selected.",
        "inputSchema": {"type": "object", "properties": {"documentPath": {"type": "string"}, "question": {"type": "string"}, "maxPages": {"type": "integer", "minimum": 1, "maximum": 100}, "ocr": {"type": "boolean"}}, "required": ["documentPath"], "additionalProperties": False},
        "annotations": {"readOnlyHint": True, "destructiveHint": False},
    },
    {
        "name": "image_generate",
        "description": "Generate an image with the image-generation model configured in the CODETAS app.",
        "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string"}, "size": {"type": "string"}, "quality": {"type": "string"}}, "required": ["prompt"], "additionalProperties": False},
        "annotations": {"readOnlyHint": False, "destructiveHint": False},
    },
]


def text_result(value: Any, *, is_error: bool = False) -> dict[str, Any]:
    text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, indent=2)
    result: dict[str, Any] = {"content": [{"type": "text", "text": text}]}
    if is_error:
        result["isError"] = True
    return result


def media_result(value: dict[str, Any]) -> dict[str, Any]:
    if value.get("object") != "codetas.native_input":
        return text_result(value)
    content: list[dict[str, Any]] = [
        {"type": "text", "text": str(value.get("result") or "Native media input is attached.")},
        {
            "type": "resource_link",
            "uri": str(value["uri"]),
            "name": str(value.get("name") or Path(str(value["path"])).name),
            "mimeType": str(value.get("mimeType") or "application/octet-stream"),
            "size": int(value.get("size") or 0),
        },
    ]
    return {"content": content, "structuredContent": value}


def requested_project(arguments: dict[str, Any]) -> Path:
    supplied = arguments.get("projectPath")
    path = Path(str(supplied)).expanduser() if supplied else Path(os.getcwd())
    resolved = path.resolve()
    if not resolved.is_dir():
        raise ValueError("projectPath must point to an existing directory")
    return resolved


def call_tool(name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    try:
        if name == "vision_analyze":
            return text_result(vision_analyze(str(arguments.get("image", "")), str(arguments.get("question", ""))))
        if name == "video_analyze":
            sample_frames = arguments.get("sampleFrames")
            return media_result(video_analyze(str(arguments.get("videoPath", "")), str(arguments.get("question", "")), int(sample_frames) if sample_frames is not None else None))
        if name == "document_analyze":
            max_pages = arguments.get("maxPages")
            ocr = arguments.get("ocr")
            return media_result(document_analyze(str(arguments.get("documentPath", "")), str(arguments.get("question", "")), int(max_pages) if max_pages is not None else None, ocr if isinstance(ocr, bool) else None))
        if name == "image_generate":
            return generated_image_result(image_generate(str(arguments.get("prompt", "")), str(arguments.get("size", "1024x1024")), str(arguments.get("quality", "auto"))))
        start = requested_project(arguments)
    except (OSError, RuntimeError, TypeError, ValueError) as error:
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
            "instructions": "CODETAS project-inspection tools are local and read-only. Media tools delegate to the loopback CODETAS gateway; image_generate may create a provider-owned image result. Credentials and Hermes files are never copied or modified.",
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
    if sys.argv[1:] == ["--health-check"]:
        initialized = handle({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        listed = handle({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        tools = [
            tool.get("name")
            for tool in (((listed or {}).get("result") or {}).get("tools") or [])
            if isinstance(tool, dict)
        ]
        required = {"vision_analyze", "video_analyze", "document_analyze", "image_generate"}
        print(json.dumps({"ok": bool(initialized) and required.issubset(set(tools)), "tools": tools}))
        return 0
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
