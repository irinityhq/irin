#!/bin/bash
# =============================================================================
# test_council_644.sh — Verifies the §6.4 aggregation SQL holds against a
# synthetic council session.
#
# Spins up a temp SQLite DB with the audit_events schema, inserts:
#   - 3 leaf rows  (kind=leaf, cost_usd=0.01 each, same council_session_id)
#   - 1 wrapper row (kind=council_wrapper, wrapper_cost_usd=0.05)
#   - 1 replay row  (kind=council_replay, wrapper_cost_usd=0)
# then runs test/sql/council_644_aggregation.sql and asserts:
#   leaf_sum_usd  = 0.03
#   wrapper_cost  = 0.05
#   total_usd     = 0.08
#   replay_count  = 1
#
# Validates the SQL math without requiring a live Council deliberation. The
# full end-to-end test that
# drives an actual gateway → council-rs round-trip is documented in
# COUNCIL_GATEWAY_CONTRACT.md and is gated on the operator setting
# GW_ENABLE_COUNCIL_ENDPOINT=1 + the relevant provider keys.
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SQL_FILE="$REPO_DIR/test/sql/council_644_aggregation.sql"

if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "FATAL: sqlite3 CLI not on PATH (needed to run §6.4 verification)"
    exit 2
fi
if [ ! -f "$SQL_FILE" ]; then
    echo "FATAL: missing $SQL_FILE"
    exit 2
fi

DB="$(mktemp -t council-644-XXXXXX.db)"
trap 'rm -f "$DB"' EXIT

SESSION_ID="sess_p07_test"

sqlite3 "$DB" <<'SCHEMA'
CREATE TABLE IF NOT EXISTS audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    payload TEXT NOT NULL,
    metadata TEXT NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 3,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    signature TEXT NOT NULL
);
SCHEMA

insert_row() {
    local kind="$1"
    local payload="$2"
    local hash="hash_${RANDOM}_${RANDOM}"
    sqlite3 "$DB" <<SQL
INSERT INTO audit_events
  (timestamp, source, target, payload, metadata, schema_version,
   prev_hash, hash, signature)
VALUES
  (strftime('%s','now') * 1000, 'gateway', 'client',
   '$payload',
   '{"action":"outbound_response"}',
   3, 'prev_x', '$hash', 'sig_x');
SQL
}

# 3 seat leaf rows
insert_row leaf '{"kind":"leaf","council_session_id":"'"$SESSION_ID"'","cost_usd":0.01,"request_id":"r_seat1"}'
insert_row leaf '{"kind":"leaf","council_session_id":"'"$SESSION_ID"'","cost_usd":0.01,"request_id":"r_seat2"}'
insert_row leaf '{"kind":"leaf","council_session_id":"'"$SESSION_ID"'","cost_usd":0.01,"request_id":"r_seat3"}'

# Chair wrapper row
insert_row wrapper '{"kind":"council_wrapper","council_session_id":"'"$SESSION_ID"'","cost_usd":0.05,"wrapper_cost_usd":0.05,"chair_tokens":1200,"request_id":"r_chair"}'

# Replay (must contribute zero)
insert_row replay '{"kind":"council_replay","council_session_id":"'"$SESSION_ID"'","cost_usd":0,"wrapper_cost_usd":0,"original_request_id":"r_chair","request_id":"r_replay"}'

# Run the canonical §6.4 SQL — replace :session_id with our fixture.
RESULT=$(sed "s/:session_id/'$SESSION_ID'/g" "$SQL_FILE" | sqlite3 -separator '|' "$DB")

# Expected: session_id|leaf_sum|wrapper|total|leaf_count|wrapper_count|replay_count
echo "result: $RESULT"

IFS='|' read -r SID LEAF_SUM WRAPPER TOTAL LEAF_COUNT WRAPPER_COUNT REPLAY_COUNT <<< "$RESULT"

pass=true
check() {
    local label="$1"; local actual="$2"; local expected="$3"
    # SQLite emits floats with trailing zeros / exponent notation; normalize
    # via awk so 0.03 / 0.030 / 3.0e-2 all compare equal.
    local a=$(awk -v n="$actual" 'BEGIN { printf "%.6f", n }')
    local e=$(awk -v n="$expected" 'BEGIN { printf "%.6f", n }')
    if [ "$a" = "$e" ]; then
        printf "  ✅ %-20s %s\n" "$label" "$actual"
    else
        printf "  ❌ %-20s actual=%s expected=%s\n" "$label" "$actual" "$expected"
        pass=false
    fi
}

check "session_id"   "$SID"          "$SESSION_ID"
check "leaf_sum_usd" "$LEAF_SUM"     "0.03"
check "wrapper_cost" "$WRAPPER"      "0.05"
check "total_usd"    "$TOTAL"        "0.08"
check "leaf_count"   "$LEAF_COUNT"   "3"
check "wrapper_count" "$WRAPPER_COUNT" "1"
check "replay_count" "$REPLAY_COUNT" "1"

if $pass; then
    echo "✅ §6.4 aggregation SQL holds — replay row contributes 0; leaf+wrapper = 0.08"
    exit 0
else
    echo "❌ §6.4 aggregation SQL violated. See diffs above."
    exit 1
fi
