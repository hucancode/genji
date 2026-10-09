"""The resumed session finishes the tool the killed one started."""
import json
import subprocess
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
assert Path("/app/textstats.py").is_file(), "textstats.py is missing"
Path("/tmp/sample.txt").write_text("The cat, the DOG.\nthe end!\n\nCat dog cat\n")
out = subprocess.run(["python3", "textstats.py", "/tmp/sample.txt"], cwd="/app", capture_output=True, text=True, timeout=30)
got = json.loads(out.stdout)
want = {"lines": 4, "words": 9, "chars": 40, "top_word": "the"}
for key, value in want.items():
    assert got.get(key) == value, f"{key}: got {got.get(key)!r}, want {value!r}"
assert any(e["type"] == "instance_start" and e["resumed"] for e in events), "the session was not resumed"
