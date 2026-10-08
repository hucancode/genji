"""tool_result_max_bytes clips a long bash output and spills the full text to a file."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
assert any(e["type"] == "tool_result" and e["name"] == "bash" and "written to" in str(e["result"])
           for e in sessions), "no clipped bash result"
assert Path("/app/answer.txt").read_text().removesuffix("\n") == "100000", "wrong /app/answer.txt"
