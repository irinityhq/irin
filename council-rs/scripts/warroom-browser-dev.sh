#!/usr/bin/env bash
# Clean browser War Room stack: council --serve :8765 + Next.js on :3010
# Usage: ./scripts/warroom-browser-dev.sh
# Stop: Ctrl+C (stops council if this script started it)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COUNCIL_PORT="${COUNCIL_PORT:-8765}"
WEB_PORT="${WARROOM_WEB_PORT:-3010}"
PID_DIR="$ROOT/warroom/.runtime"
PID_FILE="$PID_DIR/council-serve.pid"
NEXT_ENV_FILE="$ROOT/warroom/web/next-env.d.ts"
NEXT_ENV_SNAPSHOT=""
NEXT_PID=""
WE_STARTED_COUNCIL=0

# The binary lands in this repo's target/ on a standalone checkout, but an
# enclosing workspace (the irin mono-repo) hoists it to the workspace root's
# target/. Ask cargo where the workspace actually is.
locate_council_bin() {
  local target_dir ws_manifest
  if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    target_dir="$CARGO_TARGET_DIR"
  else
    ws_manifest="$(cd "$ROOT" && cargo locate-project --workspace --message-format plain 2>/dev/null || true)"
    target_dir="$(dirname "${ws_manifest:-$ROOT/Cargo.toml}")/target"
  fi
  echo "$target_dir/release/council"
}
COUNCIL_BIN="$(locate_council_bin)"

mkdir -p "$PID_DIR"
cd "$ROOT"

if [[ ! -x "$COUNCIL_BIN" ]]; then
  echo "→ Building council (release)…"
  cargo build --release
fi

if [[ ! -x "$COUNCIL_BIN" ]]; then
  echo "✗ Missing $COUNCIL_BIN after build."
  echo "  Run from repo root: cargo build --release"
  exit 1
fi

ENV_FILE="$ROOT/warroom/web/.env.local"
# Align War Room API/WS bases with COUNCIL_PORT without clobbering other local
# settings (auth token, gateway base, etc.). Match only the active env keys —
# not comments or unrelated lines that happen to mention a port.
create_env_local_from_example() {
  sed "s|127\.0\.0\.1:8765|127.0.0.1:${COUNCIL_PORT}|g" \
    "$ROOT/warroom/web/env.local.example" >"$ENV_FILE"
}

# True when NEXT_PUBLIC_API_BASE and NEXT_PUBLIC_WS_BASE both target COUNCIL_PORT.
env_local_matches_council_port() {
  local api_line ws_line
  api_line="$(grep -E '^[[:space:]]*NEXT_PUBLIC_API_BASE=' "$ENV_FILE" 2>/dev/null | tail -n1 || true)"
  ws_line="$(grep -E '^[[:space:]]*NEXT_PUBLIC_WS_BASE=' "$ENV_FILE" 2>/dev/null | tail -n1 || true)"
  [[ -n "$api_line" && -n "$ws_line" ]] \
    && [[ "$api_line" == *":${COUNCIL_PORT}"* || "$api_line" == *":${COUNCIL_PORT}/"* ]] \
    && [[ "$ws_line" == *":${COUNCIL_PORT}"* || "$ws_line" == *":${COUNCIL_PORT}/"* ]]
}

# In-place port rewrite on the two bases only (preserves auth token / gateway / etc.).
patch_env_local_port() {
  # macOS/BSD sed needs -i ''; GNU sed accepts -i''. Use a temp for portability.
  local tmp
  tmp="$(mktemp)"
  sed -E \
    -e "s|^([[:space:]]*NEXT_PUBLIC_API_BASE=.*)127\\.0\\.0\\.1:[0-9]+|\\1127.0.0.1:${COUNCIL_PORT}|" \
    -e "s|^([[:space:]]*NEXT_PUBLIC_WS_BASE=.*)127\\.0\\.0\\.1:[0-9]+|\\1127.0.0.1:${COUNCIL_PORT}|" \
    "$ENV_FILE" >"$tmp"
  mv "$tmp" "$ENV_FILE"
}

if [[ ! -f "$ENV_FILE" ]]; then
  create_env_local_from_example
  echo "→ Created warroom/web/.env.local (API :${COUNCIL_PORT})"
elif ! env_local_matches_council_port; then
  patch_env_local_port
  if env_local_matches_council_port; then
    echo "→ Updated NEXT_PUBLIC_API_BASE/WS_BASE in .env.local to :${COUNCIL_PORT} (other keys preserved)"
  else
    # File had no standard keys — fall back to example so the UI is usable.
    create_env_local_from_example
    echo "→ Regenerated warroom/web/.env.local from example (API :${COUNCIL_PORT}; no standard API/WS keys found)"
  fi
fi

council_healthy() {
  curl -sf "http://127.0.0.1:${COUNCIL_PORT}/api/health" >/dev/null 2>&1
}

start_council() {
  if council_healthy; then
    if [[ "${WARROOM_ADOPT:-0}" == "1" ]]; then
      echo "✓ Adopting existing council on :${COUNCIL_PORT} (WARROOM_ADOPT=1)"
      return 0
    fi
    echo "✗ A council is already listening on :${COUNCIL_PORT} (canary?)."
    echo "  Refusing to silently adopt it — you would not be testing a fresh stack."
    echo "  • Use the existing one:  WARROOM_ADOPT=1 make warroom"
    echo "  • Run a fresh stack:     COUNCIL_PORT=8767 WARROOM_WEB_PORT=3011 make warroom"
    exit 1
  fi
  if lsof -nP -iTCP:"$COUNCIL_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "✗ Port ${COUNCIL_PORT} is in use but /api/health failed."
    echo "  Free it: kill \$(lsof -t -iTCP:${COUNCIL_PORT} -sTCP:LISTEN)"
    exit 1
  fi
  echo "→ Starting council --serve --port ${COUNCIL_PORT}"
  "$COUNCIL_BIN" --serve --port "$COUNCIL_PORT" &
  echo $! >"$PID_FILE"
  WE_STARTED_COUNCIL=1
  for _ in $(seq 1 40); do
    if council_healthy; then
      echo "✓ Council ready"
      return 0
    fi
    sleep 0.25
  done
  echo "✗ Council did not become healthy in time"
  exit 1
}

cleanup() {
  trap - EXIT INT TERM
  if [[ -n "$NEXT_PID" ]] && kill -0 "$NEXT_PID" 2>/dev/null; then
    kill "$NEXT_PID" 2>/dev/null || true
    wait "$NEXT_PID" 2>/dev/null || true
  fi
  if [[ "$WE_STARTED_COUNCIL" -eq 1 && -f "$PID_FILE" ]]; then
    pid="$(cat "$PID_FILE")"
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      echo "→ Stopped council (pid $pid)"
    fi
    rm -f "$PID_FILE"
  fi
  if [[ -n "$NEXT_ENV_SNAPSHOT" && -f "$NEXT_ENV_SNAPSHOT" ]]; then
    cp "$NEXT_ENV_SNAPSHOT" "$NEXT_ENV_FILE"
    rm -f "$NEXT_ENV_SNAPSHOT"
  fi
}
handle_signal() {
  exit "$1"
}
trap cleanup EXIT
trap 'handle_signal 130' INT
trap 'handle_signal 143' TERM

if [[ -f "$NEXT_ENV_FILE" ]]; then
  NEXT_ENV_SNAPSHOT="$(mktemp "${TMPDIR:-/tmp}/irin-next-env.XXXXXX")"
  cp "$NEXT_ENV_FILE" "$NEXT_ENV_SNAPSHOT"
fi

start_council

# Also guard the Next dev port (3010). The browser script and Tauri dev share it.
PIDS=$(lsof -tiTCP:"$WEB_PORT" -sTCP:LISTEN 2>/dev/null || true)
if [ -n "$PIDS" ]; then
  echo "→ Port ${WEB_PORT} in use — killing previous Next listener..."
  kill -9 $PIDS 2>/dev/null || true
  sleep 0.3
fi

echo ""
echo "┌────────────────────────────────────────────────────────┐"
echo "│  Council War Room (browser)                            │"
echo "│  Open:  http://127.0.0.1:${WEB_PORT}                        │"
echo "│  API:   http://127.0.0.1:${COUNCIL_PORT}/api/health             │"
echo "│  History → click a session in the left rail              │"
echo "│  (ws:// URLs are for the app — not the browser bar)      │"
echo "└────────────────────────────────────────────────────────┘"
echo ""

cd "$ROOT/warroom/web"
if [[ ! -d node_modules ]]; then
  echo "→ npm ci…"
  npm ci
fi
# Mirrors package.json "dev:local" but with the port as a real knob —
# npm-script args can't take WARROOM_WEB_PORT through reliably.
./node_modules/.bin/next dev --webpack --hostname 127.0.0.1 --port "$WEB_PORT" &
NEXT_PID=$!
wait "$NEXT_PID"
NEXT_PID=""
