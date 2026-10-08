"""The request id of the single ERROR line is found and nothing else is changed."""
import subprocess
from pathlib import Path

assert Path("/app/answer.txt").read_text().removesuffix("\n") == "a3f9c2e1", "wrong /app/answer.txt"
subprocess.run(["git", "-C", "/app", "add", "-A"], check=True)
changed = subprocess.run(["git", "-C", "/app", "diff", "--cached", "--name-only", "HEAD"],
                         capture_output=True, text=True, check=True).stdout.split()
assert set(changed) <= {"answer.txt"}, f"changed {changed}, allowed only answer.txt"
