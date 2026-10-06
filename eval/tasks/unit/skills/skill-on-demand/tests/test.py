"""A listed skill is read on demand when the task matches it."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    def reads_skill(e):
        args = e.get("arguments") or {}
        return (e["name"] == "read" and args.get("path", "").endswith("SKILL.md")) or (
            e["name"] == "bash" and "SKILL.md" in args.get("command", ""))

    assert any(e["type"] == "tool_call" and reads_skill(e) for e in events), "SKILL.md was never read"
    assert Path("/app/answer.txt").read_text().removesuffix("\n") == "quill-4827-amber", "wrong /app/answer.txt"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
