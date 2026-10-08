"""review_threshold = 0 sends every submitted build to a review pass."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
assert any(e["type"] == "instance_start" and e["agent"].endswith(":review") for e in sessions), "no review pass"
assert any(e["type"] == "tool_call" and e["name"] == "verdict" for e in sessions), "the review gave no verdict"
assert Path("/app/hello.txt").read_text().removesuffix("\n") == "hello", "wrong hello.txt"
