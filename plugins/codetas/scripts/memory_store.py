"""Bounded Hermes-compatible MEMORY.md / USER.md store.

Mirrors the Hermes MemoryStore contract that CODETAS can implement locally:
frozen snapshot at session start, live disk writes, §-delimited entries,
character limits, injection scan, atomic replace, and refuse-to-wipe on
unreadable files. External memory providers and Honcho are out of scope.
"""

from __future__ import annotations

import os
import stat
import time
from pathlib import Path
from typing import Any

from project_context import suspicious_context_reasons

ENTRY_DELIMITER = "\n§\n"
MEMORY_CHAR_LIMIT = 2200
USER_CHAR_LIMIT = 1375
MAX_CONSOLIDATION_FAILURES = 3
MEMORY_BLOCK_HEADERS = {
    "memory": "MEMORY (your personal notes)",
    "user": "USER PROFILE (who the user is)",
}
_READ_FAILED = object()


def scan_memory_content(content: str) -> str | None:
    reasons = suspicious_context_reasons(content)
    if not reasons:
        return None
    return "Memory content blocked: " + "; ".join(reasons)


def parse_entries(raw: str) -> list[str]:
    if not raw.strip():
        return []
    return [entry.strip() for entry in raw.split(ENTRY_DELIMITER) if entry.strip()]


def is_regular_file(path: Path) -> bool:
    try:
        metadata = path.lstat()
    except OSError:
        return False
    return stat.S_ISREG(metadata.st_mode) and not path.is_symlink()


def read_raw_checked(path: Path) -> tuple[str, bool]:
    if not path.exists():
        return "", True
    if not is_regular_file(path):
        return "", False
    try:
        return path.read_text(encoding="utf-8-sig"), True
    except (OSError, UnicodeDecodeError):
        return "", False


def atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and not is_regular_file(path):
        raise OSError(f"{path} is not a regular file")
    temp = path.with_name(f".mem_{path.name}.{os.getpid()}.tmp")
    try:
        temp.write_text(content, encoding="utf-8")
        os.replace(temp, path)
    except OSError:
        if temp.exists():
            temp.unlink(missing_ok=True)
        raise


class MemoryStore:
    def __init__(
        self,
        memory_dir: Path,
        memory_char_limit: int = MEMORY_CHAR_LIMIT,
        user_char_limit: int = USER_CHAR_LIMIT,
    ) -> None:
        self.memory_dir = memory_dir
        self.memory_entries: list[str] = []
        self.user_entries: list[str] = []
        self.memory_char_limit = memory_char_limit
        self.user_char_limit = user_char_limit
        self._system_prompt_snapshot = {"memory": "", "user": ""}
        self._consolidation_failures = 0
        self._consolidation_by_target = {"memory": 0, "user": 0}

    def path_for(self, target: str) -> Path:
        name = "USER.md" if target == "user" else "MEMORY.md"
        return self.memory_dir / name

    def char_limit(self, target: str) -> int:
        return self.user_char_limit if target == "user" else self.memory_char_limit

    def entries_for(self, target: str) -> list[str]:
        return self.user_entries if target == "user" else self.memory_entries

    def set_entries(self, target: str, entries: list[str]) -> None:
        if target == "user":
            self.user_entries = entries
        else:
            self.memory_entries = entries

    def char_count(self, target: str) -> int:
        entries = self.entries_for(target)
        return len(ENTRY_DELIMITER.join(entries)) if entries else 0

    def set_consolidation_failures(self, values: dict[str, int] | None) -> None:
        self._consolidation_failures = 0
        self._consolidation_by_target = {
            "memory": int((values or {}).get("memory") or 0),
            "user": int((values or {}).get("user") or 0),
        }

    def consolidation_failures(self) -> dict[str, int]:
        by_target = getattr(self, "_consolidation_by_target", {"memory": 0, "user": 0})
        return {"memory": int(by_target.get("memory") or 0), "user": int(by_target.get("user") or 0)}

    def load_from_disk(self) -> None:
        if self.memory_dir.exists() and (self.memory_dir.is_symlink() or not self.memory_dir.is_dir()):
            raise OSError(f"{self.memory_dir} is not a regular directory")
        self.memory_dir.mkdir(parents=True, exist_ok=True)
        self.memory_entries = list(dict.fromkeys(parse_entries(read_raw_checked(self.path_for("memory"))[0])))
        self.user_entries = list(dict.fromkeys(parse_entries(read_raw_checked(self.path_for("user"))[0])))
        self._system_prompt_snapshot = {
            "memory": self.render_block("memory", self._sanitize(self.memory_entries, "MEMORY.md")),
            "user": self.render_block("user", self._sanitize(self.user_entries, "USER.md")),
        }

    def format_for_system_prompt(self, target: str) -> str | None:
        block = self._system_prompt_snapshot.get(target, "")
        return block or None

    def add(self, target: str, content: str) -> dict[str, Any]:
        content = content.strip()
        if not content:
            return {"success": False, "error": "Content cannot be empty."}
        scan_error = scan_memory_content(content)
        if scan_error:
            return {"success": False, "error": scan_error}
        if self._reload(target, skip_drift=False) is _READ_FAILED:
            return self._read_failed(target)
        entries = self.entries_for(target)
        if content in entries:
            return self._success(target, "Entry already exists (no duplicate added).")
        new_entries = entries + [content]
        new_total = len(ENTRY_DELIMITER.join(new_entries))
        limit = self.char_limit(target)
        if new_total > limit:
            return self._consolidation_failure(
                {
                    "success": False,
                    "error": (
                        f"Memory at {self.char_count(target):,}/{limit:,} chars. "
                        f"Adding this entry ({len(content)} chars) would exceed the limit. "
                        "Consolidate now: use 'replace' to merge overlapping entries or "
                        "'remove' stale entries, then retry this add — all in this turn."
                    ),
                    "current_entries": list(entries),
                    "usage": f"{self.char_count(target):,}/{limit:,}",
                }
            )
        entries.append(content)
        self.set_entries(target, entries)
        self.save(target)
        return self._success(target, "Entry added.")

    def replace(self, target: str, old_text: str, new_content: str) -> dict[str, Any]:
        old_text = old_text.strip()
        new_content = new_content.strip()
        if not old_text:
            return {"success": False, "error": "old_text cannot be empty."}
        if not new_content:
            return {"success": False, "error": "new_content cannot be empty. Use 'remove' to delete entries."}
        scan_error = scan_memory_content(new_content)
        if scan_error:
            return {"success": False, "error": scan_error}
        drift = self._reload(target)
        if drift is _READ_FAILED:
            return self._read_failed(target)
        if drift:
            return self._drift_error(target, drift)
        entries = self.entries_for(target)
        matches = [(index, entry) for index, entry in enumerate(entries) if old_text in entry]
        if not matches:
            return self._consolidation_failure(
                {
                    "success": False,
                    "error": f"No entry matched '{old_text}'. Check current_entries and retry with exact text.",
                    "current_entries": list(entries),
                }
            )
        unique = {entry for _, entry in matches}
        if len(unique) > 1:
            return {
                "success": False,
                "error": f"Multiple entries matched '{old_text}'. Be more specific.",
                "matches": [entry[:80] for entry in unique],
            }
        index = matches[0][0]
        trial = list(entries)
        trial[index] = new_content
        new_total = len(ENTRY_DELIMITER.join(trial))
        limit = self.char_limit(target)
        if new_total > limit:
            return self._consolidation_failure(
                {
                    "success": False,
                    "error": (
                        f"Replacement would put memory at {new_total:,}/{limit:,} chars. "
                        "Shorten the new content or remove other entries, then retry."
                    ),
                    "current_entries": list(entries),
                    "usage": f"{self.char_count(target):,}/{limit:,}",
                }
            )
        entries[index] = new_content
        self.set_entries(target, entries)
        self.save(target)
        return self._success(target, "Entry replaced.")

    def remove(self, target: str, old_text: str) -> dict[str, Any]:
        old_text = old_text.strip()
        if not old_text:
            return {"success": False, "error": "old_text cannot be empty."}
        drift = self._reload(target)
        if drift is _READ_FAILED:
            return self._read_failed(target)
        if drift:
            return self._drift_error(target, drift)
        entries = self.entries_for(target)
        matches = [(index, entry) for index, entry in enumerate(entries) if old_text in entry]
        if not matches:
            return self._consolidation_failure(
                {
                    "success": False,
                    "error": f"No entry matched '{old_text}'.",
                    "current_entries": list(entries),
                }
            )
        unique = {entry for _, entry in matches}
        if len(unique) > 1:
            return {
                "success": False,
                "error": f"Multiple entries matched '{old_text}'. Be more specific.",
                "matches": [entry[:80] for entry in unique],
            }
        entries.pop(matches[0][0])
        self.set_entries(target, entries)
        self.save(target)
        return self._success(target, "Entry removed.")

    def save(self, target: str) -> None:
        atomic_write_text(self.path_for(target), ENTRY_DELIMITER.join(self.entries_for(target)))

    def render_block(self, target: str, entries: list[str]) -> str:
        if not entries:
            return ""
        limit = self.char_limit(target)
        content = ENTRY_DELIMITER.join(entries)
        current = len(content)
        percent = min(100, int((current / limit) * 100)) if limit else 0
        header = f"{MEMORY_BLOCK_HEADERS['user' if target == 'user' else 'memory']} [{percent}% — {current:,}/{limit:,} chars]"
        separator = "═" * 46
        return f"{separator}\n{header}\n{separator}\n{content}"

    def _sanitize(self, entries: list[str], filename: str) -> list[str]:
        sanitized: list[str] = []
        for entry in entries:
            if not entry or entry.startswith("[BLOCKED:"):
                sanitized.append(entry)
                continue
            reasons = suspicious_context_reasons(entry)
            if reasons:
                sanitized.append(
                    f"[BLOCKED: {filename} entry contained threat pattern(s): "
                    f"{', '.join(reasons)}. Removed from system prompt; "
                    "use memory(action=remove) to delete the original.]"
                )
            else:
                sanitized.append(entry)
        return sanitized

    def _reload(self, target: str, skip_drift: bool = False) -> object | str | None:
        raw, ok = read_raw_checked(self.path_for(target))
        if not ok:
            return _READ_FAILED
        if not skip_drift:
            drift = self._detect_drift(target, raw)
            if drift:
                return drift
        self.set_entries(target, parse_entries(raw))
        return None

    def _detect_drift(self, target: str, raw: str) -> str | None:
        if not raw.strip():
            return None
        parsed = parse_entries(raw)
        roundtrip = ENTRY_DELIMITER.join(parsed)
        max_entry = max((len(entry) for entry in parsed), default=0)
        if raw.strip() == roundtrip and max_entry <= self.char_limit(target):
            return None
        backup = self.path_for(target).with_suffix(self.path_for(target).suffix + f".bak.{int(time.time())}")
        try:
            backup.write_text(raw, encoding="utf-8")
            return str(backup)
        except OSError:
            return str(backup) + " (BACKUP FAILED — file unchanged on disk)"

    def _read_failed(self, target: str) -> dict[str, Any]:
        path = self.path_for(target)
        return {
            "success": False,
            "error": (
                f"Refusing to write {path.name}: the file exists but could not be read. "
                "Treating it as empty would wipe memory, so the write is refused."
            ),
        }

    def _drift_error(self, target: str, backup: str) -> dict[str, Any]:
        path = self.path_for(target)
        return {
            "success": False,
            "error": (
                f"Refusing to write {path.name}: on-disk content would not round-trip "
                f"through the memory tool. A snapshot was saved to {backup}."
            ),
            "drift_backup": backup,
        }

    def _consolidation_failure(self, response: dict[str, Any], target: str | None = None) -> dict[str, Any]:
        by_target = getattr(self, "_consolidation_by_target", None)
        if by_target is None:
            by_target = {"memory": 0, "user": 0}
            self._consolidation_by_target = by_target
        key = target or "memory"
        by_target[key] = int(by_target.get(key) or 0) + 1
        self._consolidation_failures = int(by_target.get(key) or 0)
        if self._consolidation_failures <= MAX_CONSOLIDATION_FAILURES:
            return response
        return {
            "success": False,
            "done": True,
            "error": (
                f"Memory consolidation failed {self._consolidation_failures} times this turn. "
                "Stop retrying memory calls and continue with the user reply."
            ),
        }

    def _success(self, target: str, message: str) -> dict[str, Any]:
        self._consolidation_failures = 0
        current = self.char_count(target)
        limit = self.char_limit(target)
        percent = min(100, int((current / limit) * 100)) if limit else 0
        return {
            "success": True,
            "done": True,
            "target": target,
            "message": message,
            "usage": f"{percent}% — {current:,}/{limit:,} chars",
            "entry_count": len(self.entries_for(target)),
            "note": "Write saved. This update is complete — do not repeat it.",
        }
