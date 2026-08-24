#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NGINX="$ROOT/nginx.conf"

if ! awk '
  /^[[:space:]]*location[[:space:]]/ && /(^|[^[:alnum:]_\/])\/admin\// {
    allowed = ($1 == "location" && $2 == "=" &&
               ($3 == "/admin/keys" || $3 == "/admin/keys/revoke") &&
               $4 == "{" && NF == 4)
    if (!allowed) {
      print "FAIL: unexpected /admin/ nginx location: " $0 > "/dev/stderr"
      invalid = 1
    }
  }
  END { exit invalid }
' "$NGINX"; then
  exit 1
fi

[[ "$(grep -Fxc '        location = /admin/keys {' "$NGINX")" == "1" ]] \
  || { echo "FAIL: nginx must expose exactly /admin/keys" >&2; exit 1; }
[[ "$(grep -Fxc '        location = /admin/keys/revoke {' "$NGINX")" == "1" ]] \
  || { echo "FAIL: nginx must expose exactly /admin/keys/revoke" >&2; exit 1; }

echo "OK: Gateway admin surface exposes two exact paths"
