#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB_DIR="$ROOT/council-rs/warroom/web"
NEXT_ENV="$WEB_DIR/next-env.d.ts"
NEXT_ENV_SNAPSHOT="$(mktemp "${TMPDIR:-/tmp}/irin-next-env.XXXXXX")"

cleanup() {
  cp "$NEXT_ENV_SNAPSHOT" "$NEXT_ENV"
  rm -f "$NEXT_ENV_SNAPSHOT"
}
trap cleanup EXIT INT TERM

cp "$NEXT_ENV" "$NEXT_ENV_SNAPSHOT"
cd "$WEB_DIR"
env \
  WARROOM_TAURI_EXPORT=1 \
  NEXT_PUBLIC_API_BASE=http://127.0.0.1:8765 \
  NEXT_PUBLIC_WS_BASE=ws://127.0.0.1:8765 \
  NEXT_PUBLIC_GATEWAY_BASE="${GATEWAY_URL:-http://127.0.0.1:18080}" \
  npm run build
