#!/usr/bin/env bash
# council-hermes-seat-adapter — operator-owned Hermes seat surface for council-rs.
#
# Fork this script outside the repo and point council at it:
#   export COUNCIL_HERMES_SEAT_BIN=~/bin/my-hermes-seat.sh
#
# Contract (stable):
#   hermes-seat-adapter.sh --model <wire-model> [--provider <name>]
#   Prompt on stdin → final seat text on stdout; errors on stderr; exit 0 on success.
#
# Council only spawns this binary; Hermes/Nous flags live here under your control.

set -euo pipefail

MODEL=""
PROVIDER="${HERMES_SEAT_PROVIDER:-xai}"

usage() {
  echo "usage: $0 --model <id> [--provider <name>]" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --model)
      [[ $# -ge 2 ]] || usage
      MODEL="$2"
      shift 2
      ;;
    --provider)
      [[ $# -ge 2 ]] || usage
      PROVIDER="$2"
      shift 2
      ;;
    -h | --help)
      usage
      ;;
    *)
      echo "unknown arg: $1" >&2
      usage
      ;;
  esac
done

[[ -n "$MODEL" ]] || usage

if ! command -v hermes >/dev/null 2>&1; then
  echo "hermes-seat-adapter: hermes not on PATH" >&2
  exit 127
fi

PROMPT="$(cat)"

# Seat constraints: one-shot, no personal config/plugins, no tool approval hangs.
exec hermes -z "$PROMPT" \
  --provider "$PROVIDER" \
  -m "$MODEL" \
  --safe-mode \
  --ignore-user-config \
  --ignore-rules
