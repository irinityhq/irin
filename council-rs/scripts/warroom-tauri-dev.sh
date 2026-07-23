#!/usr/bin/env bash
# Launch Tauri development on the current worktree's isolated ports.
set -euo pipefail

COUNCIL_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IRIN_ROOT="$(cd "$COUNCIL_ROOT/.." && pwd)"
if [[ -f "$IRIN_ROOT/.irin-worktree.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  . "$IRIN_ROOT/.irin-worktree.env"
  set +a
fi

council_port="${COUNCIL_PORT:-${IRIN_COUNCIL_PORT:-8765}}"
web_port="${WARROOM_WEB_PORT:-${IRIN_WEB_PORT:-3010}}"
gateway_port="${IRIN_GATEWAY_PORT:-18080}"

for value in "$council_port" "$web_port" "$gateway_port"; do
  [[ "$value" =~ ^[0-9]+$ ]] && (( value >= 1 && value <= 65535 )) || {
    printf 'ERROR: invalid worktree port: %s\n' "$value" >&2
    exit 1
  }
done

if lsof -nP -iTCP:"$web_port" -sTCP:LISTEN >/dev/null 2>&1; then
  printf 'ERROR: War Room port %s is already in use; refusing to kill another worktree\n' "$web_port" >&2
  exit 1
fi

web_dir="$COUNCIL_ROOT/warroom/web"
tauri_dir="$COUNCIL_ROOT/warroom-tauri"
[[ -d "$web_dir/node_modules" ]] || (cd "$web_dir" && npm ci)
[[ -d "$tauri_dir/node_modules" ]] || (cd "$tauri_dir" && npm ci)

config="$(python3 - "$council_port" "$web_port" "$gateway_port" <<'PY'
import json, sys
council, web, gateway = sys.argv[1:]
api = f"http://127.0.0.1:{council}"
ws = f"ws://127.0.0.1:{council}"
gateway_url = f"http://127.0.0.1:{gateway}"
before = (
    "cd ../warroom/web && "
    f"NEXT_PUBLIC_API_BASE={api} NEXT_PUBLIC_WS_BASE={ws} "
    f"NEXT_PUBLIC_GATEWAY_BASE={gateway_url} "
    f"./node_modules/.bin/next dev --webpack --hostname 127.0.0.1 --port {web}"
)
csp = (
    "default-src 'self' tauri://localhost https://tauri.localhost; "
    f"connect-src 'self' tauri://localhost https://tauri.localhost {api} {ws} "
    f"{gateway_url} ws://127.0.0.1:{gateway} ipc: http://ipc.localhost; "
    "img-src 'self' asset: https://asset.localhost blob: data:; "
    "style-src 'self' 'unsafe-inline'; font-src 'self' data:; "
    "script-src 'self' 'unsafe-inline' 'unsafe-eval'; object-src 'none'; "
    "base-uri 'none'; frame-ancestors 'none'"
)
print(json.dumps({
    "build": {
        "beforeDevCommand": before,
        "devUrl": f"http://127.0.0.1:{web}",
    },
    "app": {"security": {"csp": csp}},
}))
PY
)"

printf 'Tauri worktree ports: council=%s web=%s gateway=%s\n' \
  "$council_port" "$web_port" "$gateway_port"
cd "$tauri_dir"
npm run tauri -- dev --config "$config"
