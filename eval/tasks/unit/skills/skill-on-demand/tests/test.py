"""A listed skill is read on demand when the task matches it."""
import json
from pathlib import Path

# Every instance counts: the agent may have a subagent read the skill.
events = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
          for l in f.read_text().splitlines()]
def reads_skill(e):
    args = e.get("arguments") or {}
    return (e["name"] == "read" and args.get("path", "").endswith("SKILL.md")) or (
        e["name"] == "bash" and "SKILL.md" in args.get("command", ""))

assert any(e["type"] == "tool_call" and reads_skill(e) for e in events), "SKILL.md was never read"
assert Path("/app/answer.txt").read_text().removesuffix("\n") == "quill-4827-amber", "wrong /app/answer.txt"
