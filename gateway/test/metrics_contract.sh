#!/bin/bash
# ==========================================================================
# metrics_contract.sh — Rust ↔ Lua ↔ Prometheus metric-name contract test
#
# Scrapes the live /metrics endpoint over the OpenResty gateway container
# and asserts that the watch-plane
# metric names emitted by lua/cost.lua match the WatchStats JSON fields
# served by sidecar-rs. A future rename in
# WatchStats (Rust side) would silently zero a metric on /metrics without
# this gate — the Lua poller's table lookup would miss and no line would
# render. This script catches that.
#
# Required state: gateway + sidecar containers running and reachable at
# ${GW_URL:-http://localhost:18080}. Does NOT require provider API keys.
# Run after `make up`.
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
# Unlabeled families: HELP + TYPE + value line (seeded at 0 before first poll).
# --------------------------------------------------------------------------
UNLABELED_METRICS=(
    "gw_watch_audit_infra_errors_total counter"
    "gw_watch_persist_failures_total counter"
    "gw_watch_pending_pending_records gauge"
    "gw_watch_pending_retry_failures_total counter"
    "gw_watch_pending_oldest_age_ms gauge"
    "gw_council_stored_bytes gauge"
    "gw_watch_recon_cap_breach_total counter"
    "gw_watch_cap_token_rejected_total counter"
    "gw_watch_directive_verify_failed_total counter"
    "gw_watch_cap_token_db_error_deny_total counter"
    "gw_watch_arm_rejected_unauth_total counter"
    "gw_watch_action_production_armed gauge"
    "gw_watch_sentinel_fires_read_failures_total counter"
    "gw_watch_temperature_read_failures_total counter"
)

# --------------------------------------------------------------------------
# Labeled families: HELP + TYPE always present. Value lines appear only when
# a series exists (pack default may have zero sentinels / tenants).
# --------------------------------------------------------------------------
LABELED_HELP_TYPE=(
    "gw_watch_temperature gauge"
    "gw_watch_sentinel_fires_total counter"
    "gw_watch_sentinel_ticks_total counter"
    "gw_watch_sentinel_last_tick_ms gauge"
)

assert_unlabeled() {
    local name="$1"
    local kind="$2"

    if echo "$METRICS_BODY" | grep -qE "^# HELP ${name} "; then
        pass "${name}: # HELP line present"
    else
        fail "${name}: # HELP line MISSING (silent rename in WatchStats?)"
    fi

    if echo "$METRICS_BODY" | grep -qE "^# TYPE ${name} ${kind}$"; then
        pass "${name}: # TYPE ${kind} present"
    else
        fail "${name}: # TYPE ${kind} MISSING (kind drift or rename?)"
    fi

    if echo "$METRICS_BODY" | grep -qE "^${name} [0-9]+(\.[0-9]+)?$"; then
        pass "${name}: value line present"
    else
        fail "${name}: value line MISSING (Lua dispatch dropped the key?)"
    fi
}

assert_labeled_meta() {
    local name="$1"
    local kind="$2"

    if echo "$METRICS_BODY" | grep -qE "^# HELP ${name} "; then
        pass "${name}: # HELP line present"
    else
        fail "${name}: # HELP line MISSING"
    fi

    if echo "$METRICS_BODY" | grep -qE "^# TYPE ${name} ${kind}$"; then
        pass "${name}: # TYPE ${kind} present"
    else
        fail "${name}: # TYPE ${kind} MISSING"
    fi
}

for entry in "${UNLABELED_METRICS[@]}"; do
    name="${entry% *}"
    kind="${entry##* }"
    assert_unlabeled "$name" "$kind"
done

for entry in "${LABELED_HELP_TYPE[@]}"; do
    name="${entry% *}"
    kind="${entry##* }"
    assert_labeled_meta "$name" "$kind"
done

echo ""
echo "------------------------------------------------------------"
echo "metrics-contract: ${PASS_COUNT} pass, ${FAIL_COUNT} fail"
echo "------------------------------------------------------------"

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi
exit 0
