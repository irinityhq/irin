#!/bin/bash
set -euo pipefail

# Hybrid asset smoke: lint/typecheck, Next static export → warroom-web-dist,
# compiled-surface marker checks, and Rust unit tests. Interactive webview
# acceptance remains a separate Gate 5 check.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COUNCIL_RS_ROOT="$(cd "$ROOT/.." && pwd)"
WARROOM_WEB="$COUNCIL_RS_ROOT/warroom/web"
DIST="$ROOT/warroom-web-dist"

echo "=== War Room Tauri hybrid build smoke ==="
echo "warroom-tauri: $ROOT"
echo "warroom/web:   $WARROOM_WEB"

if [ ! -d "$WARROOM_WEB" ]; then
  echo "ERROR: warroom/web missing at $WARROOM_WEB"
  exit 1
fi

if [ "${WARROOM_SMOKE_SKIP_WEB_LINT:-}" != "1" ]; then
  pushd "$WARROOM_WEB" >/dev/null
  echo "Running lint + typecheck..."
  npm run lint
  npm run typecheck
  popd >/dev/null
else
  echo "Skipping lint/typecheck (WARROOM_SMOKE_SKIP_WEB_LINT=1)"
fi

bash "$ROOT/scripts/build-warroom-assets.sh"

if [ ! -f "$DIST/index.html" ]; then
  echo "ERROR: $DIST/index.html not produced"
  exit 1
fi

echo "Checking compiled surface markers in warroom-web-dist..."
assert_markers() {
  local pattern="$1"
  local label="$2"
  if grep -q "$pattern" "$DIST/index.html" 2>/dev/null; then
    echo "  OK: $label in index.html"
    return 0
  fi
  if grep -rq "$pattern" "$DIST/_next" 2>/dev/null; then
    echo "  OK: $label in _next chunks"
    return 0
  fi
  echo "ERROR: expected $label (pattern: $pattern) in export"
  exit 1
}

assert_markers "Council War Room" "War Room title"
assert_markers "Outbox" "Outbox surface asset"
assert_markers "Librarian" "Librarian surface asset"
assert_markers "Drift" "Drift surface asset"

if [ "${WARROOM_SMOKE_SKIP_TAURI_TESTS:-}" = "1" ]; then
  echo "Skipping Tauri Rust tests (covered by the dedicated macOS Tauri lane)"
else
  echo "Running Rust unit tests (src-tauri)..."
  (cd "$ROOT/src-tauri" && cargo test)
fi

echo "OK: requested hybrid export and Tauri checks succeeded"
