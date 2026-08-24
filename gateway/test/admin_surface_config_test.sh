#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NGINX="$ROOT/nginx.conf"

if grep -Eq '^[[:space:]]*location[[:space:]]+(\^~[[:space:]]+)?/admin/[[:space:]]*\{' "$NGINX"; then
  echo "FAIL: prefix /admin/ nginx location found" >&2
  exit 1
fi

[[ "$(grep -Fxc '        location = /admin/keys {' "$NGINX")" == "1" ]] \
  || { echo "FAIL: nginx must expose exactly /admin/keys" >&2; exit 1; }
[[ "$(grep -Fxc '        location = /admin/keys/revoke {' "$NGINX")" == "1" ]] \
  || { echo "FAIL: nginx must expose exactly /admin/keys/revoke" >&2; exit 1; }

echo "OK: Gateway admin surface exposes two exact paths"
