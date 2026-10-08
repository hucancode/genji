"""A run over its token budget stops with reason token_limit and exit code 2."""
import json
from pathlib import Path


def check():
    events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
    code = int(Path("/genji/trace/exit_code").read_text())
    end = [e for e in events if e["type"] == "instance_end"][-1]
    assert end["reason"] == "token_limit", f"stopped for {end['reason']!r}"
    assert code == 2, f"exit code {code}"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
