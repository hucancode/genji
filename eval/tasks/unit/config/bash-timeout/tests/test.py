"""bash_timeout_secs kills a long command."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
assert any(e["type"] == "tool_result" and e["name"] == "bash" and "timed out after 2s" in str(e["result"])
           for e in sessions), "no bash result that timed out after 2s"
assert Path("/app/answer.txt").read_text().removesuffix("\n") == "ok", "wrong /app/answer.txt"
