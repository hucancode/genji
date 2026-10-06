"""spawn runs an explore subagent at depth 1 and returns its report."""
import json
from pathlib import Path


def check():
    sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
           for l in f.read_text().splitlines()]
    assert any(e["type"] == "instance_start" and e["depth"] == 1 for e in sessions), "no subagent at depth 1"
    assert Path("/app/answer.txt").read_text().removesuffix("\n") == "src/util/helpers.py", "wrong /app/answer.txt"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
