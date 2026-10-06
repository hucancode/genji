"""An agent only gets the tools its definition lists."""
import json
from pathlib import Path


def check():
    sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
           for l in f.read_text().splitlines()]
    started = {e["instance"] for e in sessions if e["type"] == "instance_start" and e["agent"] == "reader"}
    assert started, "reader did not run"
    tools = {t["function"]["name"] for e in sessions if e["type"] == "system" and e["instance"] in started
             for t in e["tools"]}
    assert "read" in tools, f"reader has no read tool: {sorted(tools)}"
    assert not tools & {"write", "bash"}, f"reader got tools it does not list: {sorted(tools)}"
    assert not Path("/app/x.txt").exists(), "/app/x.txt was created"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
