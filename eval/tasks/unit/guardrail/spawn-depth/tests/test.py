"""max_subagent_depth refuses a spawn past the limit."""
import json
from pathlib import Path


def check():
    sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
           for l in f.read_text().splitlines()]
    assert any(e["type"] == "tool_result" and e["name"] == "spawn" and "depth limit reached" in str(e["result"])
               for e in sessions), "no spawn refused for depth"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
