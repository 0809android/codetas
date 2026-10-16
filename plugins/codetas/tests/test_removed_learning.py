"""Regression checks for the read-only project plugin after learning removal."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import mcp_server


class RemovedLearningTests(unittest.TestCase):
    def test_no_learning_tools_are_advertised(self):
        names = {tool["name"] for tool in mcp_server.TOOLS}
        self.assertTrue({"memory", "skill_manage", "review_complete"}.isdisjoint(names))
        self.assertIn("inspect_project_context", names)
        self.assertIn("vision_analyze", names)

    def test_old_memory_tool_cannot_change_files(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "MEMORY.md"
            path.write_text("keep my saved data")
            result = mcp_server.call_tool("memory", {
                "projectPath": directory, "action": "remove", "target": "memory",
                "content": "keep my saved data",
            })
            self.assertTrue(result.get("isError"))
            self.assertEqual(path.read_text(), "keep my saved data")

    def test_only_read_only_project_start_hook_remains(self):
        manifest = json.loads((ROOT / "hooks/hooks.json").read_text())
        self.assertEqual(set(manifest["hooks"]), {"SessionStart"})
        command = manifest["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        self.assertIn("session_start.py", command)
        self.assertNotIn("learning", command)

    def test_project_inspection_does_not_scan_or_manage_skills(self):
        from unittest.mock import patch
        from project_context import inspect_project
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "HERMES.md").write_text("Project-owned guidance")
            with patch("pathlib.Path.rglob", side_effect=AssertionError("must not walk skills")):
                result = inspect_project(root)
            self.assertEqual(result["contextFile"], str((root / "HERMES.md").resolve()))
            self.assertNotIn("skillCount", result)
            self.assertNotIn("skillDirectories", result)
        self.assertNotIn("list_project_skills", {t["name"] for t in mcp_server.TOOLS})
