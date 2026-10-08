"""/stop on the control socket stops the run gracefully."""
import json
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
code = int(Path("/genji/trace/exit_code").read_text())
end = [e for e in events if e["type"] == "instance_end"][-1]
assert end["reason"] == "user", f"stopped for {end['reason']!r}"
assert code == 2, f"exit code {code}"
