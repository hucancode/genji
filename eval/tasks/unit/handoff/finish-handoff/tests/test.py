"""A top-level finish handoff continues in the same process to the named agent, linked by parent."""
import json
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
starts = [e for e in events if e["type"] == "instance_start"]
assert len(starts) >= 2, f"{len(starts)} instance(s), expected a handoff to a second"
assert starts[1]["parent"] == starts[0]["instance"], "the second instance is not the first's child"

assert Path("/app/result.txt").read_text().removesuffix("\n") == "handed", "wrong result.txt"
