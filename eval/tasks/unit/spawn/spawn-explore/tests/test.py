"""spawn runs an explore subagent at depth 1 and returns its report."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
assert any(e["type"] == "instance_start" and e["depth"] == 1 for e in sessions), "no subagent at depth 1"
assert Path("/app/answer.txt").read_text().removesuffix("\n") == "src/util/helpers.py", "wrong /app/answer.txt"
