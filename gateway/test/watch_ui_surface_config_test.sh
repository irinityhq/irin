#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NGINX="$ROOT/nginx.conf"
LUA="$ROOT/lua/sidecar.lua"

[[ "$(grep -Fxc '        location ~ ^/watch/ui-snapshot/[^/]+$ {' "$NGINX")" == "1" ]] \
  || { echo "FAIL: nginx must expose exactly one ui-snapshot path pattern" >&2; exit 1; }

if grep -Eq '^[[:space:]]*location[[:space:]]+(\^~[[:space:]]+)?/watch/?[[:space:]]*\{' "$NGINX"; then
  echo "FAIL: broad /watch nginx location found" >&2
  exit 1
fi

grep -Fq 'function _M.watch_ui_snapshot_proxy()' "$LUA" \
  || { echo "FAIL: ui snapshot proxy missing" >&2; exit 1; }
grep -Fq 'ngx.req.get_method() ~= "GET"' "$LUA" \
  || { echo "FAIL: ui snapshot proxy is not GET-only" >&2; exit 1; }
grep -Fq 'ngx.var.uri:match("^/watch/ui%-snapshot/[^/]+$")' "$LUA" \
  || { echo "FAIL: ui snapshot proxy path is not exact" >&2; exit 1; }

echo "OK: Watch UI nginx/Lua surface is one exact GET projection"
