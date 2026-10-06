"""A custom agent file in the agents directory runs as its own subcommand."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    assert any(e["type"] == "instance_start" and e["agent"] == "greeter" for e in events), "greeter did not run"
    assert Path("/app/greeting.txt").read_text().removesuffix("\n") == "hello from greeter", "wrong greeting.txt"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
