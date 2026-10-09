"""Runs the genji command in argv and kills it right after its 2nd tool call, as a crash."""
import json
import subprocess
import sys

genji = subprocess.Popen(sys.argv[1:], stdout=subprocess.PIPE, text=True)
calls = 0
for line in genji.stdout:
    sys.stdout.write(line)
    sys.stdout.flush()
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if event.get("type") == "tool_call":
        calls += 1
        if calls >= 2:
            genji.kill()
            break
sys.exit(genji.wait())
