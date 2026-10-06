# Verifier helpers for hand-authored eval tasks. Each task's tests/lib.sh is a copy of this
# file (`cargo test -p genji-eval` checks they match). Source it from tests/test.sh:
#
#   . /tests/lib.sh
#   file_eq /app/answer.txt "a3f9c2e1"
#   tool_count spawn -ge 1
#   pass
#
# Every check fails the step with a message; `pass` records reward 1. The trace is every
# step's genji events, written by genji-drive.

TRACE=/genji/trace/all.jsonl
APP=${APP:-/app}
mkdir -p /logs/verifier

pass() { echo "PASS"; echo 1 > /logs/verifier/reward.txt; exit 0; }
fail() { echo "FAIL: $*"; echo 0 > /logs/verifier/reward.txt; exit 0; }

events() { if [ -f "$TRACE" ]; then cat "$TRACE"; fi; }

# final_status STATUS: the last verdict (`finish`/review) has this status (done, blocked, handoff).
final_status() {
  local s
  s=$(events | jq -r 'select(.type=="instance_end") | .result.status // "none"' | tail -1)
  [ "$s" = "$1" ] || fail "final status is '${s:-none}', expected '$1'"
}

# tool_count NAME OP N: the number of NAME tool calls compares to N with test's OP (-ge, -eq, ...).
tool_count() {
  local c
  c=$(events | jq -r --arg n "$1" 'select(.type=="tool_call" and .name==$n) | .id' | wc -l)
  [ "$c" "$2" "$3" ] || fail "$c '$1' call(s), expected $2 $3"
}

# has_event TYPE [JQ_FILTER]: some event of TYPE (matching the optional jq filter) was emitted.
has_event() {
  local f=${2:-true}
  events | jq -e --arg t "$1" "select(.type==\$t) | select($f)" >/dev/null || fail "no '$1' event${2:+ matching $2}"
}

# changed_files: paths changed since the task's baseline commit, tracked or not.
changed_files() {
  git -C "$APP" add -A >/dev/null 2>&1
  git -C "$APP" diff --cached --name-only HEAD
}

# changed_only PATH...: nothing outside the given paths (files or directory prefixes) changed.
changed_only() {
  local f p ok
  for f in $(changed_files); do
    ok=
    for p in "$@"; do case "$f" in "$p" | "$p"/*) ok=1 ;; esac; done
    [ -n "$ok" ] || fail "changed $f, allowed only: $*"
  done
}

# untouched PATH...: these paths did not change.
untouched() {
  local p
  for p in "$@"; do
    git -C "$APP" diff --quiet HEAD -- "$p" 2>/dev/null || fail "$p was changed"
  done
}

# file_eq FILE TEXT: FILE holds exactly TEXT, ignoring one trailing newline.
file_eq() {
  [ -f "$1" ] || fail "$1 is missing"
  local got
  got=$(cat "$1")
  [ "$got" = "$2" ] || fail "$1 is '$got', expected '$2'"
}

# img_close A B MAX: images A and B differ by at most MAX normalized RMSE (0..1).
img_close() {
  [ -f "$1" ] || fail "$1 is missing"
  local d
  d=$(compare -metric RMSE "$1" "$2" null: 2>&1 | sed -n 's/.*(\(.*\)).*/\1/p')
  [ -n "$d" ] || fail "cannot compare $1 with $2"
  awk -v d="$d" -v m="$3" 'BEGIN { exit !(d <= m) }' || fail "$1 differs from $2 by $d (max $3)"
}
