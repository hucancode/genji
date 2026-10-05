#!/usr/bin/env bash
# End-to-end smoke tests against a live model.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
CONFIG=${CONFIG:-"$SCRIPT_DIR/config.json"}
CONFIG=$(cd "$(dirname "$CONFIG")" && pwd)/$(basename "$CONFIG")
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/genji-e2e.XXXXXX")

cleanup() {
  local status=$?
  echo "scratch directory: $SCRATCH" >&2
  if (( status == 0 )); then
    rm -rf "$SCRATCH"
  else
    echo "smoke test failed"
  fi
}
trap cleanup EXIT

cargo build --manifest-path "$ROOT/Cargo.toml" --bin genji
GENJI=${GENJI_E2E_BIN:-"$ROOT/target/debug/genji"}

# Runs one genji pass in the tag's workspace and judges its event stream.
# usage: run_pass TAG NAME PROMPT [genji args...]
run_pass() {
  local tag=$1 name=$2 prompt=$3
  shift 3
  local workspace="$SCRATCH/$tag"
  local stdout="$SCRATCH/$tag.$name.jsonl"
  local stderr="$SCRATCH/$tag.$name.log"

  mkdir -p "$workspace/.genji"
  [[ -f "$workspace/.genji/config.json" ]] || cp "$CONFIG" "$workspace/.genji/config.json"

  echo "==> $tag ($name)"
  if ! "$GENJI" build "$prompt" \
    --workspace "$workspace" "$@" >"$stdout" 2>"$stderr"
  then
    cat "$stderr" >&2
    return 1
  fi

  "$SCRIPT_DIR/judge.py" "$stdout" "$workspace"
}

run_smoke() {
  run_pass "$1" run "$2"
}

# A task, then a follow-up on the same session: the follow-up must be built and survive review.
run_greeting() {
  local tag=greeting
  run_pass $tag first \
    "Build a small command-line greeting app. It returns 'Good morning', 'Good afternoon' or 'Good evening' depending on the current time of day, and 'Hello' as the fallback."
  "$SCRIPT_DIR/check_greeting.py" "$SCRATCH/$tag" en

  local id
  id=$(python3 -c 'import json,sys; print(next(e["instance"] for e in map(json.loads, open(sys.argv[1])) if e["type"] == "instance_start"))' "$SCRATCH/$tag.first.jsonl")
  run_pass $tag followup \
    "Now support Japanese: if the app is started with the \`jp\` parameter, return the greetings in Japanese." \
    --resume "$id"
  "$SCRIPT_DIR/check_greeting.py" "$SCRATCH/$tag" jp
}

# usage: smoke.sh [blur] [pokemon] [greeting]   (default: all)
scenarios=("$@")
(( ${#scenarios[@]} )) || scenarios=(blur pokemon greeting)
for scenario in "${scenarios[@]}"; do
  case $scenario in
    blur)
      run_smoke blur \
        "Write an image blur app. It accepts one image and one radius parameter, then blurs the image."
      ;;
    pokemon)
      run_smoke pokemon \
        "Write a pokemon explorer app. It accepts a pokemon id or name, then shows that pokemon's information and pixel art."
      ;;
    greeting) run_greeting ;;
    *) echo "unknown scenario: $scenario" >&2; exit 2 ;;
  esac
done

echo "all smoke tests passed"
