#!/usr/bin/env python
from __future__ import annotations

import json
import pathlib
import sys


def fail(message: str, workspace: pathlib.Path) -> None:
    raise SystemExit(f"{message}; workspace kept at {workspace}")

def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} OUTPUT WORKSPACE")

    output = pathlib.Path(sys.argv[1])
    workspace = pathlib.Path(sys.argv[2])

    events = []
    for line in output.read_text().splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            fail(f"non-JSON output in {output}", workspace)
    if not events or events[-1].get("type") != "instance_end":
        fail("instance_end is not the final event", workspace)
    end = events[-1]
    if end.get("status") != "done":
        fail(f"instance status is {end.get('status')!r}", workspace)
    result = end.get("result")
    if not isinstance(result, dict) or result.get("status") != "done":
        fail("result status is not done", workspace)
    if not (end.get("report") or "").strip():
        fail("agent returned no report", workspace)

    errors = [event for event in events if event.get("type") == "error"]
    if errors:
        fail(f"agent emitted {len(errors)} fatal error event(s)", workspace)

    files = [
        path
        for path in workspace.rglob("*")
        if path.is_file() and ".genji" not in path.parts
    ]
    if not files:
        fail("no files generated", workspace)

    # Require actual implementation
    source_suffixes = {
        ".js",
        ".ts",
        ".py",
        ".go",
        ".rs",
        ".java",
        ".c",
        ".cpp",
    }
    sources = [path for path in files if path.suffix.lower() in source_suffixes]
    if not sources:
        fail("no source files generated", workspace)

    text = []
    for path in files:
        try:
            text.append(path.read_text(errors="replace"))
        except OSError:
            pass
    if sum(len(part) for part in text) < 200:
        fail("generated app is implausibly small", workspace)


if __name__ == "__main__":
    main()
