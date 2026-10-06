"""finish accepts only the statuses in the agent's finish list."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    assert any(e["type"] == "instance_start" and e["agent"] == "reporter" for e in events), "reporter did not run"
    end = [e for e in events if e["type"] == "instance_end"][-1]
    status = (end["result"] or {}).get("status")
    assert status == "blocked", f"final status {status!r}"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
