"""The request id of the single ERROR line is found and nothing else is changed."""
import subprocess
from pathlib import Path


def check():
    assert Path("/app/answer.txt").read_text().removesuffix("\n") == "a3f9c2e1", "wrong /app/answer.txt"
    subprocess.run(["git", "-C", "/app", "add", "-A"], check=True)
    changed = subprocess.run(["git", "-C", "/app", "diff", "--cached", "--name-only", "HEAD"],
                             capture_output=True, text=True, check=True).stdout.split()
    assert set(changed) <= {"answer.txt"}, f"changed {changed}, allowed only answer.txt"


try:
    check()
    reward, note = 1, "PASS"
except Exception as e:  # a failed assert, a missing file or trace, bad JSON: all fail the task
    reward, note = 0, f"FAIL: {e!r}"
print(note)
out = Path("/logs/verifier")
out.mkdir(parents=True, exist_ok=True)
(out / "reward.txt").write_text(f"{reward}\n")
