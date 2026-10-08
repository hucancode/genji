"""finish accepts only the statuses in the agent's finish list."""
import json
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
assert any(e["type"] == "instance_start" and e["agent"] == "reporter" for e in events), "reporter did not run"
status = {e["id"]: e["arguments"].get("status") for e in events if e["type"] == "tool_call" and e["name"] == "finish"}
results = [e for e in events if e["type"] == "tool_result" and e["name"] == "finish"]
accepted = [status[e["id"]] for e in results if not e["is_error"]]
assert set(accepted) <= {"blocked"}, f"finish accepted {accepted}"
refused = [status[e["id"]] for e in results if e["is_error"] and status[e["id"]] != "blocked"]
assert refused or accepted, "finish was never called"
