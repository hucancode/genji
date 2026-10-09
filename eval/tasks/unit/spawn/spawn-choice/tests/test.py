"""With every tool available, the agent delegates to a named agent through spawn."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
top = {e["instance"] for e in sessions if e["type"] == "instance_start" and e["depth"] == 0}
tools = {t["function"]["name"] for e in sessions if e["type"] == "system" and e["instance"] in top
         for t in e["tools"]}
assert {"read", "write", "ls", "bash", "spawn"} <= tools, f"the agent lacks the full toolset: {sorted(tools)}"
assert any(e["type"] == "tool_call" and e["instance"] in top and e["name"] == "spawn"
           and e["arguments"].get("agent") == "explore" for e in sessions), "explore was not spawned"
