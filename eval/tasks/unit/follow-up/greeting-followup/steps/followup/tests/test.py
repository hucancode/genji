"""The follow-up is built on the resumed session."""
import json
import re
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

assert any(e["type"] == "instance_start" and e["resumed"] for e in events), "the session was not resumed"
assert "jp" in text, "no file under /app mentions jp"
assert re.search("[\u3040-\u30ff\u4e00-\u9fff]", text), "no Japanese text under /app"
