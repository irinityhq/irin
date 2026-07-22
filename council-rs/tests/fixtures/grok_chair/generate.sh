#!/usr/bin/env bash
# Generate a recorded corpus of grok-4.3 canary-chair directive proposals.
# This is a live provider run and incurs provider charges;
# the offline test `grok_chair_fixture_all_pass` validates the corpus forever after.
#
# Usage (from repo root): bash tests/fixtures/grok_chair/generate.sh
# Requires XAI_API_KEY + NVIDIA_API_KEY (the canary cabinet's two seats).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
DIR=tests/fixtures/grok_chair
CAB=cabinets/triage.canary-novertex.yaml
: "${XAI_API_KEY:?export XAI_API_KEY in the login-shell environment}"
: "${NVIDIA_API_KEY:?export NVIDIA_API_KEY in the login-shell environment}"

n=0
while IFS= read -r esc; do
  [ -z "$esc" ] && continue
  n=$((n + 1))
  printf -v idx '%02d' "$n"
  out=$(timeout 150 ./target/release/council -C "$CAB" --blind "$esc" 2>&1) || {
    echo "RUN $idx FAILED (exit $?)"; echo "$out" | tail -5; continue
  }
  # Extract the single ```json ... ``` fence block (chair synthesis).
  fence=$(printf '%s\n' "$out" | awk '/^```json$/{f=1} f{print} /^```$/{if(f>1)exit; f=2}')
  if [ -z "$fence" ]; then
    echo "RUN $idx: no fence extracted"; continue
  fi
  printf '%s\n' "$fence" > "$DIR/$idx.fence.txt"
  verdict=$(printf '%s' "$fence" | grep -o '"verdict":"[^"]*"' | head -1)
  echo "RUN $idx -> $DIR/$idx.fence.txt ($verdict)"
done < "$DIR/escalations.txt"
echo "DONE: $n escalations processed"
ls -1 "$DIR"/*.fence.txt 2>/dev/null | wc -l | xargs echo "fences written:"
