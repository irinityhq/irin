#!/bin/bash
# ==========================================================================
# metrics_contract.sh — Rust ↔ Lua ↔ Prometheus metric-name contract test
#
# Scrapes the live /metrics endpoint over the OpenResty gateway container
# and asserts that the watch-plane
# metric names emitted by lua/cost.lua match the WatchStats JSON fields
# served by sidecar-rs/src/main.rs::watch_stats. A future rename in
# WatchStats (Rust side) would silently zero a metric on /metrics without
# this gate — the Lua poller's table lookup would miss and no line would
# render. This script catches that.
#
# Required state: gateway + sidecar containers running and reachable at
# ${GW_URL:-http://localhost:18080}. Does NOT require provider API keys,
# the watch plane emits unlabeled values that are present from sidecar
# startup. Run after `make up`.
#
# Wired into `make metrics-contract`. Not part of `make validate` because
# validate is config-check only (no live container).
# ==========================================================================
set -uo pipefail

GW_URL="${GW_URL:-http://localhost:18080}"
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'
PASS_COUNT=0
FAIL_COUNT=0

pass() { echo -e "${GREEN}PASS${NC}: $1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo -e "${RED}FAIL${NC}: $1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

echo "Scraping ${GW_URL}/metrics..."
METRICS_BODY=$(curl -sS "${GW_URL}/metrics") || {
    echo -e "${RED}FATAL${NC}: could not reach ${GW_URL}/metrics — is the stack up? (make up)"
    exit 2
}

if [ -z "$METRICS_BODY" ]; then
    echo -e "${RED}FATAL${NC}: ${GW_URL}/metrics returned an empty body"
    exit 2
fi

# --------------------------------------------------------------------------
# Contract table: (metric_name, kind) — covers watch-plane + council
# metrics emitted by lua/cost.lua::prometheus() (fresh-start seeded).
# Includes the Council stored-bytes gauge.
# --------------------------------------------------------------------------
WATCH_METRICS=(
    "gw_watch_audit_infra_errors_total counter"
    "gw_watch_persist_failures_total counter"
    "gw_watch_pending_pending_records gauge"
    "gw_watch_pending_retry_failures_total counter"
    "gw_watch_pending_oldest_age_ms gauge"
    "gw_council_stored_bytes gauge"
)

assert_metric() {
    local name="$1"
    local kind="$2"

    # 1) HELP line — any text, just must be present so future renames break.
    if echo "$METRICS_BODY" | grep -qE "^# HELP ${name} "; then
        pass "${name}: # HELP line present"
    else
        fail "${name}: # HELP line MISSING (silent rename in WatchStats?)"
    fi

    # 2) TYPE line — exact kind match (counter vs gauge).
    if echo "$METRICS_BODY" | grep -qE "^# TYPE ${name} ${kind}$"; then
        pass "${name}: # TYPE ${kind} present"
    else
        fail "${name}: # TYPE ${kind} MISSING (kind drift or rename?)"
    fi

    # 3) Value line — unlabeled, integer or float. The watch-plane metrics
    #    are emitted whenever the cached WatchStats fold has run; the Lua
    #    poller runs on a timer at startup, so a freshly-up gateway emits
    #    these as 0 until the first poll. We allow any non-negative number.
    if echo "$METRICS_BODY" | grep -qE "^${name} [0-9]+(\.[0-9]+)?$"; then
        pass "${name}: value line present"
    else
        fail "${name}: value line MISSING (Lua dispatch dropped the key?)"
    fi
}

for entry in "${WATCH_METRICS[@]}"; do
    name="${entry% *}"
    kind="${entry##* }"
    assert_metric "$name" "$kind"
done

echo ""
echo "------------------------------------------------------------"
echo "metrics-contract: ${PASS_COUNT} pass, ${FAIL_COUNT} fail"
echo "------------------------------------------------------------"

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi
exit 0
