#!/usr/bin/env node
// Cross-platform dispatcher for Codex plugin hooks.
// Locates Python 3.10+ and forwards to hooks/learning_hook.py.
const { spawnSync } = require("child_process");
const path = require("path");

const hook = path.join(__dirname, "learning_hook.py");
const hookArgs = process.argv.slice(2);
const candidates =
  process.platform === "win32"
    ? [
        { bin: "python", args: [] },
        { bin: "python3", args: [] },
        { bin: "py", args: ["-3"] },
      ]
    : [
        { bin: "python3", args: [] },
        { bin: "python", args: [] },
      ];

const probeCode =
  "import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 1)";

for (const candidate of candidates) {
  const probe = spawnSync(
    candidate.bin,
    [...candidate.args, "-c", probeCode],
    { stdio: "ignore" },
  );
  if (probe.error) {
    if (probe.error.code === "ENOENT") {
      continue;
    }
    // Unexpected spawn failure; try the next candidate.
    continue;
  }
  if (probe.status !== 0) {
    continue;
  }
  const result = spawnSync(
    candidate.bin,
    [...candidate.args, hook, ...hookArgs],
    { stdio: "inherit" },
  );
  if (result.error) {
    process.stderr.write(
      `CODETAS: failed to launch Python for learning hook: ${result.error.message}\n`,
    );
    process.exit(127);
  }
  process.exit(result.status == null ? 1 : result.status);
}

process.stderr.write("CODETAS: Python 3.10+ not found\n");
process.exit(127);
