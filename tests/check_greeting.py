#!/usr/bin/env python
"""Static check of a generated greeting app: STAGE is `en` or `jp`."""
from __future__ import annotations

import pathlib
import re
import sys

JAPANESE = re.compile(r"[぀-ヿ一-鿿]")

def main() -> None:
    if len(sys.argv) != 3 or sys.argv[2] not in ("en", "jp"):
        raise SystemExit(f"usage: {sys.argv[0]} WORKSPACE en|jp")
    workspace = pathlib.Path(sys.argv[1])
    stage = sys.argv[2]

    text = ""
    for path in workspace.rglob("*"):
        if not path.is_file() or ".genji" in path.parts or "__pycache__" in path.parts:
            continue
        if path.suffix.lower() in {".pyc", ".class", ".o"}:
            continue
        text += path.read_text(errors="replace") + "\n"
    lower = text.lower()

    missing = [
        g for g in ("good morning", "good afternoon", "good evening", "hello") if g not in lower
    ]
    if missing:
        raise SystemExit(f"greetings missing from the app: {missing}; workspace kept at {workspace}")
    if stage == "jp":
        if "jp" not in lower:
            raise SystemExit(f"no `jp` parameter in the app; workspace kept at {workspace}")
        if not JAPANESE.search(text):
            raise SystemExit(f"no Japanese greetings in the app; workspace kept at {workspace}")


if __name__ == "__main__":
    main()
