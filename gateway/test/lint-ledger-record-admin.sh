#!/usr/bin/env bash
# ==========================================================================
# lint-ledger-record-admin.sh — guard the /ledger/record writer contract.
#
# ProjectMem #0093: OpenResty ledger_record was posting /ledger/record without
# X-Admin-Key and treating a decoded 401 JSON body as success. That silently
# dropped every governed-call audit write.
#
# Contract (see gateway/docs/gateway-core-surfaces.md):
#   1. writers send admin-tier X-Admin-Key (LEDGER_ADMIN_KEY / ADMIN_KEY)
#   2. non-2xx status is failure (decoded error body alone is not success)
#   3. success requires recorded=true (defense-in-depth in lib/ledger.lua)
#   4. nginx must preserve LEDGER_ADMIN_KEY/ADMIN_KEY via top-level env
#
# This lint is structural so a refactor cannot reintroduce the silent-drop path
# without failing CI. It does not spin OpenResty.
# ==========================================================================

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SIDECAR="$ROOT/lua/sidecar.lua"
LEDGER="$ROOT/lua/lib/ledger.lua"
NGINX="$ROOT/nginx.conf"
EXIT=0

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    EXIT=1
}

[[ -f "$SIDECAR" ]] || { echo "missing $SIDECAR" >&2; exit 1; }
[[ -f "$LEDGER" ]] || { echo "missing $LEDGER" >&2; exit 1; }
[[ -f "$NGINX" ]] || { echo "missing $NGINX" >&2; exit 1; }

# Function body through the next top-level _M. function (exclusive).
ledger_record_body="$(
    sed -n '/^function _M\.ledger_record(/,/^function _M\./p' "$SIDECAR" \
        | sed '$d'
)"
[[ -n "$ledger_record_body" ]] || fail "could not extract _M.ledger_record from sidecar.lua"

printf '%s\n' "$ledger_record_body" | grep -q 'X-Admin-Key' \
    || fail "ledger_record must send X-Admin-Key header"

printf '%s\n' "$ledger_record_body" | grep -Eq 'LEDGER_ADMIN_KEY|ADMIN_KEY' \
    || fail "ledger_record / init path must reference LEDGER_ADMIN_KEY or ADMIN_KEY"

# Non-2xx: both bounds required independently so dropping one still fails the lint.
printf '%s\n' "$ledger_record_body" | grep -Eq 'status[[:space:]]*<[[:space:]]*200' \
    || fail "ledger_record must reject status < 200"
printf '%s\n' "$ledger_record_body" | grep -Eq 'status[[:space:]]*>=[[:space:]]*300' \
    || fail "ledger_record must reject status >= 300"

# Match the actual boolean predicate, not the word "recorded" in error strings.
printf '%s\n' "$ledger_record_body" | grep -Eq 'result\.recorded[[:space:]]*~=[[:space:]]*true' \
    || fail "ledger_record must require result.recorded ~= true (or equivalent) on success"

# Module init must load the key (not hardcode it).
grep -q 'os.getenv("LEDGER_ADMIN_KEY")' "$SIDECAR" \
    || fail "sidecar.init must load LEDGER_ADMIN_KEY from env"
grep -q 'os.getenv("ADMIN_KEY")' "$SIDECAR" \
    || fail "sidecar.init must fall back to ADMIN_KEY from env"

# Retry helper must not treat a bare decoded table as success.
grep -q 'result.recorded == true' "$LEDGER" \
    || fail "lib/ledger.lua record_with_retry must require result.recorded == true"

# nginx strips inherited env unless listed; without these, compose wiring is a no-op.
grep -Eq '^env[[:space:]]+LEDGER_ADMIN_KEY;' "$NGINX" \
    || fail "nginx.conf must declare env LEDGER_ADMIN_KEY; so Lua can read it"
grep -Eq '^env[[:space:]]+ADMIN_KEY;' "$NGINX" \
    || fail "nginx.conf must declare env ADMIN_KEY; so Lua can read the fallback"

# /vertex/token is a secret-bearing sibling of /ledger/*: same X-Admin-Key gate.
vertex_token_body="$(
    sed -n '/^function _M\.vertex_token(/,/^function _M\./p' "$SIDECAR" \
        | sed '$d'
)"
[[ -n "$vertex_token_body" ]] || fail "could not extract _M.vertex_token from sidecar.lua"
printf '%s\n' "$vertex_token_body" | grep -q 'X-Admin-Key' \
    || fail "vertex_token must send X-Admin-Key header"
printf '%s\n' "$vertex_token_body" | grep -q 'LEDGER_ADMIN_KEY == ""' \
    || fail "vertex_token must fail closed when LEDGER_ADMIN_KEY is empty"

if [[ "$EXIT" -ne 0 ]]; then
    echo
    echo "❌ lint-ledger-record-admin: contract violations above (ProjectMem #0093)"
    exit 1
fi
echo "✅ lint-ledger-record-admin: X-Admin-Key + non-2xx fail-closed + recorded=true + nginx env intact"
