#!/bin/bash
. /tests/lib.sh
cd /app || fail "no /app"
[ -f textstats.py ] || fail "textstats.py is missing"
[ -f test_textstats.py ] || fail "test_textstats.py is missing"
printf 'The cat, the DOG.\nthe end!\n\nCat dog cat\n' > /tmp/sample.txt
python3 - <<'PY' || fail "wrong output"
import json, subprocess, sys
out = subprocess.run([sys.executable, "textstats.py", "/tmp/sample.txt"], capture_output=True, text=True, timeout=30)
got = json.loads(out.stdout)
want = {"lines": 4, "words": 9, "chars": 40, "top_word": "the"}
for k, v in want.items():
    if got.get(k) != v:
        sys.exit(f"{k}: got {got.get(k)!r}, want {v!r}")
PY
python3 -m unittest test_textstats >/dev/null 2>&1 || fail "the agent's unit tests fail"
# The finished run resumed the killed session instead of starting over.
if [ -f "$TRACE" ]; then
  has_event instance_start '.resumed == true'
fi
pass
