"""review_threshold = 0 sends every submitted build to a review pass."""
import json
from pathlib import Path

def check():
    sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
           for l in f.read_text().splitlines()]
    assert any(e["type"] == "instance_start" and e["agent"].endswith(":review") for e in sessions), "no review pass"
    assert any(e["type"] == "tool_call" and e["name"] == "verdict" for e in sessions), "the review gave no verdict"
    assert Path("/app/hello.txt").read_text().removesuffix("\n") == "hello", "wrong hello.txt"

try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
