"""hand_off continues in the same process with a fresh instance of another agent."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    assert any(e["type"] == "tool_call" and e["name"] == "hand_off" for e in events), "hand_off was not called"
    starts = [e for e in events if e["type"] == "instance_start"]
    assert len(starts) >= 2, f"{len(starts)} instance(s), expected a handoff to a second"
    assert starts[1]["parent"] == starts[0]["instance"], "the second instance is not the first's child"

    assert Path("/app/result.txt").read_text().removesuffix("\n") == "routed", "wrong result.txt"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
