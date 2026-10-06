"""The first request is built and finished."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    end = [e for e in events if e["type"] == "instance_end"][-1]
    assert (end["result"] or {}).get("status") == "done", f"final status {end['result']}"
    assert not [e for e in events if e["type"] == "error"], "fatal error events"
    skip = {".git", ".genji", "__pycache__"}
    text = "\n".join(p.read_text(errors="ignore") for p in Path("/app").rglob("*")
                     if p.is_file() and not skip & set(p.relative_to("/app").parts)).lower()
    for greeting in ("good morning", "good afternoon", "good evening", "hello"):
        assert greeting in text, f"no file under /app says {greeting!r}"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
