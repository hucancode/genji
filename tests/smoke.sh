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

run_smoke() {
  local tag=$1
  local prompt=$2
  local workspace="$SCRATCH/$tag"
  local registry="$SCRATCH/$tag-registry"
  local stdout="$SCRATCH/$tag.stdout.jsonl"
  local stderr="$SCRATCH/$tag.stderr.log"

  mkdir -p "$workspace/.genji" "$registry"
  cp "$CONFIG" "$workspace/.genji/config.json"

  echo "==> $tag"
  if ! GENJI_REGISTRY_DIR="$registry" "$GENJI" \
    plan "$prompt" --follow=6 --no-control --quiet-startup \
    --workspace "$workspace" >"$stdout" 2>"$stderr"
  then
    cat "$stderr" >&2
    return 1
  fi

  "$SCRIPT_DIR/judge.py" "$stdout" "$workspace"
}

run_smoke \
  blur \
  "Write an image blur app. It accepts one image and one radius parameter, then blurs the image."

run_smoke \
  pokemon \
  "Write a pokemon explorer app. It accepts a pokemon id or name, then shows that pokemon's information and pixel art."

echo "all smoke tests passed"
