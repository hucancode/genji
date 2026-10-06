"""max_tool_iterations stops a run that needs more tool turns."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    assert any(e["type"] == "instance_start" and e["agent"] == "stepper" for e in events), "stepper did not run"
    end = [e for e in events if e["type"] == "instance_end"][-1]
    assert end["reason"] == "max_iterations", f"stopped for {end['reason']!r}"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
