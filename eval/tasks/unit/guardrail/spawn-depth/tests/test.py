"""max_subagent_depth refuses a spawn past the limit."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
assert any(e["type"] == "tool_result" and e["name"] == "spawn" and "depth limit reached" in str(e["result"])
           for e in sessions), "no spawn refused for depth"
