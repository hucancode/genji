"""The first request is built and finished."""
import json
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
end = [e for e in events if e["type"] == "instance_end"][-1]
assert (end["result"] or {}).get("status") == "done", f"final status {end['result']}"
assert not [e for e in events if e["type"] == "error"], "fatal error events"
skip = {".git", ".genji", "__pycache__"}
text = "\n".join(p.read_text(errors="ignore") for p in Path("/app").rglob("*")
                 if p.is_file() and not skip & set(p.relative_to("/app").parts)).lower()
for greeting in ("good morning", "good afternoon", "good evening", "hello"):
    assert greeting in text, f"no file under /app says {greeting!r}"
