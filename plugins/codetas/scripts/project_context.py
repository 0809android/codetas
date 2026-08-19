"""Read-only discovery helpers shared by the CODETAS hook and MCP server."""

from __future__ import annotations

import re
import os
import stat
from pathlib import Path
from typing import Any

CONTEXT_NAMES = (".hermes.md", "HERMES.md")

def hermes_context_loading_enabled() -> bool:
    """Read the CODETAS toggle for injecting HERMES.md on SessionStart."""
    try:
        from media_tools import _read_codetas_settings
    except Exception:
        return True
    _, settings = _read_codetas_settings()
    codex = settings.get("codex") if isinstance(settings, dict) else None
    if not isinstance(codex, dict):
        return True
    value = codex.get("loadHermesContext")
    return False if value is False else True


SKILL_DIRECTORIES = (".hermes/skills", "skills", ".agents/skills")
MCP_NAMES = (".mcp.json", "mcp.json", ".hermes/mcp.json")
SENSITIVE_NAMES = (".env", "auth.json", "credentials.json")
MAX_CONTEXT_BYTES = 32_000
MAX_CONTEXT_CHARS = 20_000
MAX_SKILLS = 250
MAX_SKILL_DEPTH = 4

INJECTION_PATTERNS = (
    re.compile(r"\b(?:ignore|disregard|override)\b.{0,80}\b(?:previous|system|developer)\b", re.I | re.S),
    re.compile(r"<\s*(?:script|iframe)\b", re.I),
    re.compile(r"\b(?:reveal|print|exfiltrate)\b.{0,80}\b(?:secret|token|credential|api[-_ ]?key)\b", re.I | re.S),
)
CONTROL_CHARACTERS = ("\u200b", "\u200c", "\u200d", "\u202a", "\u202b", "\u202d", "\u202e", "\u2066", "\u2067", "\u2068", "\u2069")


def existing_directory(path: Path) -> Path:
    resolved = path.resolve()
    return resolved if resolved.is_dir() else resolved.parent


def git_boundary(start: Path) -> Path | None:
    directory = existing_directory(start)
    for candidate in (directory, *directory.parents):
        if (candidate / ".git").exists():
            return candidate
    return None


def search_directories(start: Path) -> tuple[Path, ...]:
    directory = existing_directory(start)
    boundary = git_boundary(directory)
    if boundary is None:
        return (directory,)
    candidates: list[Path] = []
    for candidate in (directory, *directory.parents):
        candidates.append(candidate)
        if candidate == boundary:
            break
    return tuple(candidates)


def project_root(start: Path) -> Path:
    boundary = git_boundary(start)
    if boundary is not None:
        return boundary
    for directory in search_directories(start):
        if any((directory / name).is_file() for name in CONTEXT_NAMES):
            return directory
    return existing_directory(start)


def regular_file_within(path: Path, root: Path) -> Path | None:
    """Return a canonical, non-symlink regular file contained by root."""
    try:
        if path.is_symlink() or not stat.S_ISREG(path.stat(follow_symlinks=False).st_mode):
            return None
        resolved = path.resolve(strict=True)
        resolved.relative_to(root.resolve(strict=True))
        return resolved
    except (OSError, ValueError):
        return None


def regular_directory_within(path: Path, root: Path) -> Path | None:
    """Return a canonical, non-symlink directory contained by root."""
    try:
        if path.is_symlink() or not stat.S_ISDIR(path.stat(follow_symlinks=False).st_mode):
            return None
        resolved = path.resolve(strict=True)
        resolved.relative_to(root.resolve(strict=True))
        return resolved
    except (OSError, ValueError):
        return None


def find_context_file(start: Path) -> Path | None:
    allowed_root = git_boundary(start) or existing_directory(start)
    for directory in search_directories(start):
        for name in CONTEXT_NAMES:
            candidate = regular_file_within(directory / name, allowed_root)
            if candidate is not None:
                return candidate
    return None


def context_preview(path: Path) -> tuple[str, bool]:
    raw = read_regular_bytes(path, MAX_CONTEXT_BYTES + 1)
    truncated = len(raw) > MAX_CONTEXT_BYTES
    content = raw[:MAX_CONTEXT_BYTES].decode("utf-8", errors="replace")
    if len(content) > MAX_CONTEXT_CHARS:
        content = content[:MAX_CONTEXT_CHARS]
        truncated = True
    return content, truncated


def read_regular_bytes(path: Path, limit: int) -> bytes:
    before = path.stat(follow_symlinks=False)
    if path.is_symlink() or not stat.S_ISREG(before.st_mode):
        raise OSError("path is not a regular non-symbolic-link file")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    with os.fdopen(descriptor, "rb") as source:
        opened = os.fstat(source.fileno())
        if not stat.S_ISREG(opened.st_mode) or not os.path.samestat(before, opened):
            raise OSError("path changed while it was being opened")
        return source.read(limit)


def suspicious_context_reasons(content: str) -> list[str]:
    reasons: list[str] = []
    if any(character in content for character in CONTROL_CHARACTERS):
        reasons.append("hidden Unicode control characters")
    if any(pattern.search(content) for pattern in INJECTION_PATTERNS):
        reasons.append("prompt-injection-like wording")
    return reasons


def find_skill_directories(root: Path) -> list[Path]:
    return [
        directory
        for relative in SKILL_DIRECTORIES
        if (directory := regular_directory_within(root / relative, root)) is not None
    ]


def list_skills(root: Path) -> list[dict[str, str]]:
    skills: list[dict[str, str]] = []
    seen: set[Path] = set()
    for directory in find_skill_directories(root):
        for skill_file in skill_files(directory):
            resolved = skill_file.resolve()
            if resolved in seen:
                continue
            seen.add(resolved)
            if len(skills) >= MAX_SKILLS:
                return skills
            name, description = skill_metadata(skill_file)
            skills.append(
                {
                    "name": name,
                    "description": description,
                    "path": str(skill_file),
                }
            )
    return skills


def skill_files(directory: Path) -> list[Path]:
    found: list[Path] = []
    for current, directories, files in os.walk(directory, followlinks=False):
        current_path = Path(current)
        depth = len(current_path.relative_to(directory).parts)
        directories[:] = sorted(
            name
            for name in directories
            if depth < MAX_SKILL_DEPTH and not (current_path / name).is_symlink()
        )
        if "SKILL.md" in files:
            skill_file = regular_file_within(current_path / "SKILL.md", directory)
            if skill_file is not None:
                found.append(skill_file)
        if len(found) >= MAX_SKILLS:
            break
    return sorted(found)


def skill_metadata(path: Path) -> tuple[str, str]:
    fallback = path.parent.name
    try:
        text = read_regular_bytes(path, 8_001)[:8_000].decode("utf-8", errors="replace")
    except OSError:
        return fallback, ""
    if not text.startswith("---"):
        return fallback, ""
    closing = text.find("\n---", 3)
    if closing < 0:
        return fallback, ""
    header = text[3:closing]
    name_match = re.search(r"^name:\s*[\"']?([^\n\"']+)", header, re.M)
    description_match = re.search(r"^description:\s*[\"']?([^\n\"']+)", header, re.M)
    name = name_match.group(1).strip() if name_match else fallback
    description = description_match.group(1).strip() if description_match else ""
    return name, description


def inspect_project(start: Path) -> dict[str, Any]:
    root = project_root(start)
    context = find_context_file(start)
    skill_directories = find_skill_directories(root)
    mcp_files = [
        file
        for name in MCP_NAMES
        if (file := regular_file_within(root / name, root)) is not None
    ]
    sensitive_files = [
        file
        for name in SENSITIVE_NAMES
        if (file := regular_file_within(root / name, root)) is not None
    ]
    warnings: list[str] = []

    if context is None:
        warnings.append("No .hermes.md or HERMES.md was found.")
    else:
        try:
            preview, _ = context_preview(context)
            warnings.extend(suspicious_context_reasons(preview))
        except OSError:
            warnings.append("The Hermes project context exists but could not be read.")
    if mcp_files:
        warnings.append("MCP configuration requires explicit review; credentials are not copied.")
    if sensitive_files:
        warnings.append("Sensitive-looking files were detected and excluded from inspection.")

    return {
        "projectRoot": str(root),
        "contextFile": str(context) if context else None,
        "skillDirectories": [str(path) for path in skill_directories],
        "skillCount": len(list_skills(root)),
        "mcpFiles": [str(path) for path in mcp_files],
        "warnings": warnings,
        "sourceMutation": False,
    }
