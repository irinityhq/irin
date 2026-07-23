#!/usr/bin/env bash
# Run a regression command on four fresh loopback ports, isolated from product runtimes.
set -euo pipefail

(( $# > 0 )) || { printf 'usage: %s command [args...]\n' "$0" >&2; exit 2; }
ports="$(python3 - <<'PY'
import socket

reserved = []
try:
    pairs = []
    # These Council ports are intentionally present in the checked-in Web CSP.
    # Each pair also reserves its browser server before selection.
    for council, web in ((8766, 3011), (8767, 3012), (8768, 3013)):
        pair = []
        try:
            for port in (council, web):
                sock = socket.socket()
                sock.bind(("127.0.0.1", port))
                pair.append(sock)
        except OSError:
            for sock in pair:
                sock.close()
            continue
        reserved.extend(pair)
        pairs.append((council, web))
        if len(pairs) == 2:
            break
    if len(pairs) != 2:
        raise SystemExit("need two free CSP-approved regression port pairs")
    print(pairs[0][0], pairs[0][1], pairs[1][0], pairs[1][1])
finally:
    for sock in reserved:
        sock.close()
PY
)"
read -r PW_COUNCIL_PORT PW_WEB_PORT PW_EXPORT_COUNCIL_PORT PW_EXPORT_WEB_PORT <<<"$ports"
export PW_COUNCIL_PORT PW_WEB_PORT PW_EXPORT_COUNCIL_PORT PW_EXPORT_WEB_PORT
printf 'Regression ports: hosted council=%s web=%s; export council=%s web=%s\n' \
  "$PW_COUNCIL_PORT" "$PW_WEB_PORT" "$PW_EXPORT_COUNCIL_PORT" "$PW_EXPORT_WEB_PORT"
exec "$@"
