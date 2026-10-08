"""A custom agent file in the agents directory runs as its own subcommand."""
import json
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
assert any(e["type"] == "instance_start" and e["agent"] == "greeter" for e in events), "greeter did not run"
assert Path("/app/greeting.txt").read_text().removesuffix("\n") == "hello from greeter", "wrong greeting.txt"
