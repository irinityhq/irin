#!/usr/bin/env bash
# Focused regression for packaged webview capture targeting + marker predicate.
# Does not launch the GUI app. Optional REJECT_PNG proves a known-bad screenshot fails.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER="$ROOT/packaging/webview-evidence.swift"
[[ -f "$HELPER" ]] || { echo "ERROR: missing $HELPER" >&2; exit 1; }
[[ "$(uname -s)" == "Darwin" ]] || { echo "ERROR: macOS only" >&2; exit 1; }

REJECT_PNG="${REJECT_PNG:-}"
# Prefer an explicit false-positive sample if the caller left one for the gate.
if [[ -z "$REJECT_PNG" && -f /tmp/kimi-false-webview-smoke.png ]]; then
  REJECT_PNG=/tmp/kimi-false-webview-smoke.png
fi

ARGS=(selftest)
if [[ -n "$REJECT_PNG" ]]; then
  [[ -f "$REJECT_PNG" ]] || { echo "ERROR: REJECT_PNG not found: $REJECT_PNG" >&2; exit 1; }
  ARGS+=(--reject "$REJECT_PNG")
  echo "reject_sample=$REJECT_PNG"
fi

swift "$HELPER" "${ARGS[@]}"
echo "test-webview-evidence: PASS"
