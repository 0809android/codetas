#!/usr/bin/env python3
"""Regenerate src/registry/published_limits.json from models.dev.

Some providers' `/models` endpoints report no limits, so the gateway ships a
snapshot of the published ones. It is generated here rather than fetched at
runtime, so the app makes no extra request to a third party.

Usage: update_published_limits.py [path/to/models.dev/api.json]
Without a path, it downloads https://models.dev/api.json.
"""

import json
import sys
import urllib.request
from pathlib import Path

SOURCE = "https://models.dev/api.json"
OUTPUT = Path(__file__).resolve().parent.parent / "src/registry/published_limits.json"
# CODETAS provider id -> models.dev provider id. Only providers whose `/models`
# reports no limits belong here. Others state theirs in the registry. `opencode-free`
# and `commandcode` read these tables at lookup: see `insert_published_limits`.
PROVIDERS = {
    "command-code": "commandcode",
    "opencode-go": "opencode-go",
    "opencode-zen": "opencode",
}
# Provider settings accept at most 250 entries per limit map, and the registry
# needs room for its own rows.
MAX_MODELS_PER_PROVIDER = 200
MIN_CONTEXT = 8_192
MAX_CONTEXT = 100_000_000


def load_source(argv):
    if len(argv) > 1:
        return json.loads(Path(argv[1]).read_text(encoding="utf-8"))
    request = urllib.request.Request(SOURCE, headers={"User-Agent": "codetas-limits"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def published_limits(models):
    rows = {}
    for model_id, model in models.items():
        limit = model.get("limit") or {}
        context = limit.get("context")
        if not model.get("tool_call") or not isinstance(context, int):
            continue
        if not MIN_CONTEXT <= context <= MAX_CONTEXT:
            continue
        row = {"context": context}
        model_input = limit.get("input")
        if isinstance(model_input, int) and 0 < model_input < context:
            row["input"] = model_input
        rows[model_id] = row
    return rows


def main(argv):
    source = load_source(argv)
    output = {}
    for provider_id, source_id in PROVIDERS.items():
        rows = published_limits(source[source_id]["models"])
        if not rows or len(rows) > MAX_MODELS_PER_PROVIDER:
            raise SystemExit(f"{provider_id}: unexpected model count {len(rows)}")
        output[provider_id] = rows
    OUTPUT.write_text(
        json.dumps(output, indent=1, sort_keys=True, ensure_ascii=True) + "\n",
        encoding="utf-8",
    )
    counts = {provider_id: len(rows) for provider_id, rows in output.items()}
    print(f"wrote {OUTPUT}: {counts}")


if __name__ == "__main__":
    main(sys.argv)
