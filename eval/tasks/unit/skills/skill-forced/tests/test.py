"""An agent's skills list inlines the skill into its system prompt."""
import json
from pathlib import Path


def check():
    sessions = [json.loads(l) for f in sorted(Path("/genji/sessions").glob("*.jsonl"))
           for l in f.read_text().splitlines()]
    assert any(e["type"] == "system" and "# Skill: secret-word" in e["prompt"] for e in sessions), \
        "no system prompt inlines the skill"
    assert Path("/app/answer.txt").read_text().removesuffix("\n") == "quill-4827-amber", "wrong /app/answer.txt"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
