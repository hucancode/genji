"""An agent's skills list inlines the skill into its system prompt."""
import json
from pathlib import Path

sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
            for l in f.read_text().splitlines()]
assert any(e["type"] == "system" and "# Skill: secret-word" in e["prompt"] for e in sessions), \
    "no system prompt inlines the skill"
assert Path("/app/answer.txt").read_text().removesuffix("\n") == "quill-4827-amber", "wrong /app/answer.txt"
