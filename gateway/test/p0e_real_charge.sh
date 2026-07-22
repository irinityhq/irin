#!/bin/bash
# ==========================================================================
# p0e_real_charge.sh — P0-E real charge-count integration harness
#                      (Invariant: billed == M across the REAL
#                       CouncilState idem handlers)
#
# WHAT THIS PROVES
#   One escalation travels the full live path (CDC seed -> producer ->
#   dispatcher claim -> council-triage through the gateway -> settle ->
#   signed outbox) while N-1 collider submissions carrying the SAME
#   tenant-scoped Idempotency-Key are fired at the gateway. The REAL
#   CouncilState idem handlers (council_idem_claim, PENDING_TTL=300s)
#   must dedup every collider. Reconciliation: billed == M == 1
#     * exactly ONE spend_ledger settle for the escalation
#       (settled delta == realized_cost_usd, reservation NULLed),
#     * ZERO fresh X-Total-Cost-Usd charges among the colliders
#       (each collider is 409 pending/conflict or 200 + X-Idempotency-Replay),
#     * total realized spend < the $5 P0-E hard cap,
#     * confirmatory out-of-band recon (p0d ReconSource file-export shape)
#       when P0E_RECON_IMPORT_PATH is supplied, else PENDING-OPERATOR.
#
# SPEND SAFETY (the ONLY sanctioned real-spend harness)
#   * REFUSES unless P0E_ENABLE=1 (exit 2).
#   * P0E_MODE=stub (default) is a NO-SPEND rehearsal against the
#     deterministic local council stub (X-Total-Cost-Usd: 0).
#   * P0E_MODE=live additionally REFUSES unless P0E_CONFIRM_REAL_SPEND=yes.
#     Live mode uses the council-triage cabinet (the smallest seat set) and
#     is bounded by: P0E_SPEND_CAP ($5 default, asserted), the p0c
#     per-directive reservation ceiling (WATCH_MAX_FANOUT_COST_USD, pinned
#     to the cap here), and the p0c DAILY_SPEND_CAP in code.
#   * NEVER wired into Makefile / CI / default cargo test (enforced by
#     tests/p0e_real_charge.rs guards; the cargo wrapper is #[ignore] +
#     env-gated).
#
# SERVICES (modeled on test/smoke_phase3.sh)
#   1. gateway + sidecar via docker compose (stack must already exist via
#      `make up` at least once, with a provisioned WATCH_DISPATCHER_GATEWAY_KEY
#      visible in the running sidecar — same operator flow as smoke_phase3).
#   2. council on 127.0.0.1:$P0E_COUNCIL_PORT:
#        stub — started by this script (no spend),
#        live — operator-started Council from the monorepo root:
#               ./target/release/council --serve \
#                   --port 8765 --base-dir council-rs
#               (COUNCIL_GATEWAY_TOKEN in its env must match the gateway's).
#   3. This script rebuilds the sidecar from the CURRENT TREE (local-overlay,
#      same as smoke) so the spend ledger / p0d telemetry assertions see
#      the code under test, then recreates gateway+sidecar with the phase3
#      env. On exit it restores the stack to DEFAULT-OFF gates
#      (P0E_RESTORE_DEFAULT_OFF=true) — the harness never leaves an armed
#      stack behind.
#
# USAGE
#   P0E_ENABLE=1 bash test/p0e_real_charge.sh                  # stub, no spend
#   P0E_ENABLE=1 P0E_MODE=live P0E_CONFIRM_REAL_SPEND=yes \
#       bash test/p0e_real_charge.sh                           # ONE real charge
#   P0E_ENABLE=1 cargo test --test p0e_real_charge -- --ignored # via cargo
# ==========================================================================

set -euo pipefail

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
NC='\033[0m'
pass() { echo -e "${GREEN}PASS${NC}: $1"; }
fail() { echo -e "${RED}FAIL${NC}: $1"; exit 1; }
warn() { echo -e "${YELLOW}WARN${NC}: $1"; }
info() { echo "INFO: $1"; }

# --- HARD GATE: opt-in only -------------------------------------------------
if [ "${P0E_ENABLE:-0}" != "1" ]; then
    echo "REFUSING: P0E_ENABLE=1 is not set." >&2
    echo "This harness orchestrates real services and (P0E_MODE=live) spends" >&2
    echo "real money under a \$${P0E_SPEND_CAP:-5} cap. It is operator opt-in only." >&2
    exit 2
fi

# --- configuration -----------------------------------------------------------
P0E_MODE="${P0E_MODE:-stub}"                 # stub (no spend) | live (REAL SPEND)
P0E_SPEND_CAP="${P0E_SPEND_CAP:-5}"          # harness-local hard cap, USD
P0E_N="${P0E_N:-3}"                          # total same-key submissions (1 dispatcher + N-1 colliders)
P0E_TENANT="p0e-real-charge"                 # pinned by tests/p0e_real_charge.rs idem-key test
P0E_COUNCIL_PORT="${P0E_COUNCIL_PORT:-8765}"
P0E_REBUILD_MODE="${P0E_REBUILD_MODE:-local-overlay}"   # local-overlay | none
P0E_RESTORE_DEFAULT_OFF="${P0E_RESTORE_DEFAULT_OFF:-true}"
P0E_RECON_IMPORT_PATH="${P0E_RECON_IMPORT_PATH:-}"
P0E_RECON_THRESHOLD_USD="${P0E_RECON_THRESHOLD_USD:-1.0}"
P0E_TIMEOUT_SECONDS="${P0E_TIMEOUT_SECONDS:-150}"
P0E_WRITER_CLAIM_STALE_MS="${P0E_WRITER_CLAIM_STALE_MS:-10000}"
GW_URL="${GW_URL:-http://localhost:18080}"
SIDECAR_CONTAINER="${SIDECAR_CONTAINER:-sidecar}"

if [ "$P0E_MODE" = "live" ] && [ "${P0E_CONFIRM_REAL_SPEND:-}" != "yes" ]; then
    echo "REFUSING: P0E_MODE=live requires P0E_CONFIRM_REAL_SPEND=yes (second explicit gate)." >&2
    exit 2
fi
if [ "$P0E_MODE" != "stub" ] && [ "$P0E_MODE" != "live" ]; then
    fail "Unsupported P0E_MODE=$P0E_MODE (expected stub | live)"
fi
if [ "$P0E_N" -lt 2 ]; then
    fail "P0E_N must be >= 2 (need at least one collider); got $P0E_N"
fi

# Docker Desktop on macOS does not put `docker` on login-shell PATH.
if ! command -v docker >/dev/null 2>&1 \
   && [ -x /Applications/Docker.app/Contents/Resources/bin/docker ]; then
    export PATH="/Applications/Docker.app/Contents/Resources/bin:$PATH"
fi
command -v docker >/dev/null 2>&1 || fail "docker CLI not found"

cd "$(dirname "$0")/.."   # gateway component root
GATEWAY_ROOT="$PWD"
MONOREPO_ROOT="$(cd .. && pwd)"
COUNCIL_ROOT="$MONOREPO_ROOT/council-rs"

RUN_ID="p0e-esc-$(date +%s%N | cut -c1-13)"
SENTINEL_NAME="p0e-real-charge-sentinel"
DAY_BUCKET="$(date -u +%F)"
COUNCIL_STUB_PID=""
COUNCIL_STUB_FILE=""
COMPOSE_OVERRIDE=""
PENDING_ID=""
COLLIDER_DIR="$(mktemp -d -t p0e-colliders.XXXXXX)"

DB_PATH="${WATCH_DB_PATH_IN_CONTAINER:-/var/lib/sidecar/watch.db}"

sql() {
    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" "$1" | tr -d '\r'
}

wait_for_gateway_health() {
    for _ in $(seq 1 "${1:-90}"); do
        curl -fsS "$GW_URL/health" >/dev/null 2>&1 && return 0
        sleep 1
    done
    return 1
}

# --- credentials: self-provisioned per run ------------------------------------
# The harness never depends on a prior operator shell session (peppers and
# tokens live only in process env, not in any file). Each run uses an
# ephemeral AUTH_PEPPER + BOOTSTRAP_TOKEN (reused from the environment when
# the operator exports their own) and provisions a FRESH gateway key via the
# documented bootstrap flow. auth_keys.json on the volume is append-only —
# entries hashed with another pepper simply do not validate this run and
# revalidate when the operator restores their pepper.
AUTH_PEPPER="${AUTH_PEPPER:-$(openssl rand -hex 24)}"
BOOTSTRAP_TOKEN="${BOOTSTRAP_TOKEN:-$(openssl rand -hex 24)}"
if [ "$P0E_MODE" = "live" ]; then
    [ -n "${COUNCIL_GATEWAY_TOKEN:-}" ] || fail "P0E_MODE=live requires COUNCIL_GATEWAY_TOKEN exported in this shell
(and council-rs --serve started with the SAME value — the gateway forwards it as X-Gateway-Auth)."
    COUNCIL_TOKEN="$COUNCIL_GATEWAY_TOKEN"
else
    COUNCIL_TOKEN="${COUNCIL_GATEWAY_TOKEN:-p0e-stub-$(openssl rand -hex 8)}"
fi
export AUTH_PEPPER
WATCH_KEY=""

restore_default_off() {
    # Recreate with gates default-OFF. AUTH_PEPPER must be supplied or the
    # fail-closed sidecar refuses to boot at all (which is also fail-closed,
    # but a healthy default-OFF stack is the kinder parked state).
    AUTH_PEPPER="$AUTH_PEPPER" \
    docker compose up -d --no-build --force-recreate gateway sidecar >/dev/null 2>&1
}

# --- teardown ----------------------------------------------------------------
cleanup() {
    set +e
    if [ -n "$COUNCIL_STUB_PID" ]; then
        kill "$COUNCIL_STUB_PID" >/dev/null 2>&1
        wait "$COUNCIL_STUB_PID" >/dev/null 2>&1
    fi
    [ -n "$COUNCIL_STUB_FILE" ] && rm -f "$COUNCIL_STUB_FILE"
    rm -rf "$COLLIDER_DIR"

    if [ "$P0E_RESTORE_DEFAULT_OFF" = "true" ]; then
        info "Restoring stack to DEFAULT-OFF gates (producer/dispatcher/council endpoint)..."
        if restore_default_off; then
            info "Stack recreated with default-OFF gates (WATCH_PRODUCER_ENABLED=false, WATCH_DISPATCHER_ENABLED=false, GW_ENABLE_COUNCIL_ENDPOINT=0)."
        else
            warn "Could not restore default-OFF stack — verify manually with docker compose ps."
        fi
        wait_for_gateway_health 60 || warn "gateway not healthy after default-OFF restore"
    else
        warn "P0E_RESTORE_DEFAULT_OFF=false — the stack is STILL ARMED. Restore manually."
    fi

    if docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'command -v sqlite3' >/dev/null 2>&1; then
        info "Cleaning up p0e tenant rows..."
        docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL 2>/dev/null
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
PRAGMA defer_foreign_keys = ON;
DELETE FROM directive_outbox WHERE tenant='$P0E_TENANT';
DELETE FROM pending_escalations WHERE tenant='$P0E_TENANT';
-- Sanctioned test-only teardown of this harness's own watch_fires rows
-- (tenant-scoped; matches the live smoke audit boundary).
DELETE FROM watch_fires WHERE tenant='$P0E_TENANT';
COMMIT;
SQL
    fi
    [ -n "$COMPOSE_OVERRIDE" ] && rm -f "$COMPOSE_OVERRIDE"
}
trap cleanup EXIT

# --- prerequisites -----------------------------------------------------------
info "P0-E real charge-count harness — mode=$P0E_MODE cap=\$$P0E_SPEND_CAP N=$P0E_N run_id=$RUN_ID"

docker image inspect gateway-sidecar >/dev/null 2>&1 \
    || fail "gateway-sidecar image not found. Run 'make up' once to build the stack."

# --- council on 127.0.0.1:$P0E_COUNCIL_PORT ----------------------------------
if [ "$P0E_MODE" = "stub" ]; then
    if curl -fsS "http://127.0.0.1:$P0E_COUNCIL_PORT/api/health" >/dev/null 2>&1; then
        fail "P0E_MODE=stub but something already listens on 127.0.0.1:$P0E_COUNCIL_PORT. Stop it or use P0E_MODE=live."
    fi
    COUNCIL_STUB_FILE="$(mktemp -t p0e-council-stub.XXXXXX.py)"
    cat >"$COUNCIL_STUB_FILE" <<'PY'
#!/usr/bin/env python3
# Deterministic NO-SPEND council stub for the P0-E rehearsal. Same contract
# as the smoke_phase3 stub: /api/health + /api/deliberate, proposal.v1 fence,
# X-Total-Cost-Usd: 0 always (this stub never bills anything).
import json, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def extract(pattern, text, default):
    m = re.search(pattern, text)
    return m.group(1).strip() if m else default

class Handler(BaseHTTPRequestHandler):
    server_version = "p0e-stub/1"
    def log_message(self, fmt, *args):
        return
    def send_json(self, status, payload, extra=None):
        body = json.dumps(payload, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        if self.path.rstrip("/") == "/api/health":
            self.send_json(200, {"status": "ok", "service": "p0e-stub"})
            return
        self.send_json(404, {"error": "not_found"})
    def do_POST(self):
        if self.path.rstrip("/") != "/api/deliberate":
            self.send_json(404, {"error": "not_found"})
            return
        if not self.headers.get("X-Gateway-Auth"):
            self.send_json(401, {"error": "missing X-Gateway-Auth"})
            return
        n = int(self.headers.get("content-length", "0") or "0")
        prompt = ""
        try:
            payload = json.loads(self.rfile.read(n).decode())
            prompt = "\n".join(str(m.get("content", "")) for m in (payload.get("messages") or [])
                               if isinstance(m, dict))
        except Exception:
            pass
        tenant = extract(r"Escalation tenant:\s*\"?([A-Za-z0-9_.:-]+)\"?", prompt, "p0e-real-charge")
        esc_id = extract(r"Escalation id:\s*\"?([A-Za-z0-9_.:-]+)\"?", prompt,
                         "phase3-startup-probe-v1-00000000000000000000000000000000")
        if "phase3-startup-probe-v1" in prompt:
            in_resp = extract(r'"in_response_to":\s*"([^"]+)"', prompt, esc_id)
            proposal = {"schema": "irin.directive.proposal.v1", "in_response_to": in_resp,
                        "authority": "recommend", "verdict": "Dismiss",
                        "rationale": "Deterministic p0e startup probe response (no spend)."}
        else:
            proposal = {"schema": "irin.directive.proposal.v1", "in_response_to": esc_id,
                        "authority": "recommend", "verdict": "Act",
                        "job": "Record the P0-E rehearsal escalation.",
                        "scope": {"tenant": tenant, "subject": "p0e-real-charge",
                                  # allowlisted verb (read/report/notify/review/escalate per directive_fence); "report" = record
                                  "allowed_actions": ["report"]},
                        "stop_condition": "One signed directive_outbox row exists for this escalation.",
                        "return_expectation": "Return directive status through the gateway watch outbox surface.",
                        "rationale": "Deterministic p0e rehearsal response (no provider spend)."}
        content = "```json\n" + json.dumps(proposal, separators=(",", ":")) + "\n```"
        resp = {"id": "chatcmpl-p0e-stub", "object": "chat.completion",
                "created": int(time.time()), "model": "council-triage",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
        self.send_json(200, resp, {"X-Council-Session-Id": f"p0e-stub-{int(time.time())}",
                                   "X-Total-Cost-Usd": "0", "X-Chair-Tokens": "0"})

if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1])), Handler).serve_forever()
PY
    python3 "$COUNCIL_STUB_FILE" "$P0E_COUNCIL_PORT" &
    COUNCIL_STUB_PID="$!"
    for _ in $(seq 1 50); do
        curl -fsS "http://127.0.0.1:$P0E_COUNCIL_PORT/api/health" >/dev/null 2>&1 && break
        sleep 0.1
    done
    curl -fsS "http://127.0.0.1:$P0E_COUNCIL_PORT/api/health" >/dev/null 2>&1 \
        || fail "no-spend council stub did not become healthy on :$P0E_COUNCIL_PORT"
    pass "No-spend council stub healthy on 127.0.0.1:$P0E_COUNCIL_PORT (X-Total-Cost-Usd always 0)"
else
    HEALTH_JSON=$(curl -fsS "http://127.0.0.1:$P0E_COUNCIL_PORT/api/health" 2>/dev/null) \
        || fail "P0E_MODE=live but no council answers http://127.0.0.1:$P0E_COUNCIL_PORT/api/health.
Start the real Council server from the monorepo root first:
  COUNCIL_GATEWAY_TOKEN=<same token as gateway> \\
  $MONOREPO_ROOT/target/release/council --serve --port $P0E_COUNCIL_PORT --base-dir $COUNCIL_ROOT"
    if echo "$HEALTH_JSON" | grep -q "stub"; then
        fail "P0E_MODE=live but :$P0E_COUNCIL_PORT answers as a STUB ($HEALTH_JSON). Refusing to call a stub 'live'."
    fi
    pass "Live council healthy on 127.0.0.1:$P0E_COUNCIL_PORT (token alignment proven by the dispatcher startup probe)"
fi

# --- rebuild current tree + recreate the stack with the armed env -------------
build_sidecar_local_overlay() {
    info "Building current sidecar via local overlay image (smoke_phase3 pattern)..."
    # sidecar-rs has an in-tree path dependency on sovereign-protocol. Compose
    # builds feed it as an additional_context named `protocol`. Mount the
    # component read-only at /sentinel so the
    # path resolves from /src/sidecar-rs exactly like the Dockerfile layout.
    local sentinel_repo
    sentinel_repo="${P0E_SENTINEL_REPO:-$MONOREPO_ROOT/sentinel}"
    [ -f "$sentinel_repo/sovereign-protocol/Cargo.toml" ] \
        || fail "sovereign-protocol crate not found at $sentinel_repo/sovereign-protocol (set P0E_SENTINEL_REPO)"
    docker run --rm \
        -v "$PWD:/src" \
        -v "$sentinel_repo:/sentinel:ro" \
        -v gateway_phase3_cargo:/root/.cargo \
        -v gateway_phase3_rustup:/root/.rustup \
        -w /src/sidecar-rs \
        --entrypoint /bin/sh \
        gateway-sidecar \
        -c 'apk add --no-cache curl build-base pkgconf >/dev/null &&
            if [ ! -x "$HOME/.cargo/bin/rustc" ]; then
                curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs |
                    sh -s -- -y --profile minimal >/dev/null;
            fi &&
            . "$HOME/.cargo/env" &&
            CARGO_PROFILE_RELEASE_LTO=false
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
            export CARGO_PROFILE_RELEASE_LTO CARGO_PROFILE_RELEASE_CODEGEN_UNITS
            cargo build --release --bin gateway-sidecar'
    docker build -t gateway-sidecar:latest -f - . <<'DOCKERFILE'
FROM gateway-sidecar:latest
COPY sidecar-rs/target/release/gateway-sidecar /usr/local/bin/gateway-sidecar
RUN chmod +x /usr/local/bin/gateway-sidecar
DOCKERFILE
}

case "$P0E_REBUILD_MODE" in
    local-overlay) build_sidecar_local_overlay ;;
    none) info "P0E_REBUILD_MODE=none — reusing the existing gateway-sidecar image (must already carry the code under test)" ;;
    *) fail "Unsupported P0E_REBUILD_MODE=$P0E_REBUILD_MODE (local-overlay | none)" ;;
esac

# Extra env the base compose does not pass through:
#   WATCH_MAX_FANOUT_COST_USD — pin the p0c per-directive reservation ceiling
#     to the P0-E cap so a single fan-out can never exceed it.
#   WRITER_CLAIM_STALE_MS — shorten the p07 single-writer staleness window so
#     a recreated harness instance can take over the previous instance's claim
#     (we deterministically wait it out below).
COMPOSE_OVERRIDE="$(mktemp -t p0e-compose-override.XXXXXX.yaml)"
cat >"$COMPOSE_OVERRIDE" <<OVR
services:
  sidecar:
    environment:
      - WATCH_MAX_FANOUT_COST_USD=$P0E_SPEND_CAP
      - WRITER_CLAIM_STALE_MS=$P0E_WRITER_CLAIM_STALE_MS
OVR

if [ "$(uname)" = "Darwin" ]; then
    BRIDGE_HOST="host.docker.internal"
else
    BRIDGE_HOST="$(docker network inspect bridge -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null || echo '172.17.0.1')"
    [ -n "$BRIDGE_HOST" ] || BRIDGE_HOST="172.17.0.1"
fi

# PHASE A (provision): recreate UNARMED with this run's pepper + bootstrap
# token, then provision a fresh gateway key for the dispatcher through the
# documented bootstrap flow (the same path `make provision-key` uses).
info "Phase A: recreating stack unarmed to provision this run's gateway key..."
BOOTSTRAP_TOKEN="$BOOTSTRAP_TOKEN" \
docker compose up -d --no-build --force-recreate gateway sidecar >/dev/null
wait_for_gateway_health 90 || fail "gateway did not become healthy for provisioning"
docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'command -v sqlite3' >/dev/null 2>&1 \
    || fail "sqlite3 not found inside the sidecar container (rebuild the image)."

PROV_JSON=""
for _ in $(seq 1 30); do
    PROV_JSON=$(curl -sf -X POST "$GW_URL/admin/keys" \
        -H "Content-Type: application/json" \
        -d "{\"budget_key\":\"$P0E_TENANT\",\"tier\":\"default\",\"rpm\":1000,\"admin_key\":\"$BOOTSTRAP_TOKEN\"}" \
        2>/dev/null) && break
    sleep 1
done
WATCH_KEY=$(printf '%s' "$PROV_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("raw_key",""))' 2>/dev/null || true)
[ -n "$WATCH_KEY" ] || fail "could not provision a gateway key via the bootstrap flow (POST /admin/keys)"
pass "Provisioned a fresh gateway key for this run (budget=$P0E_TENANT; value not printed)"

# PHASE B (arm): recreate with the armed phase3 env. Stop + wait out the
# single-writer staleness window first so the new instance can take over the
# p07 writer claim deterministically (claim takeover requires the previous
# holder's heartbeat to be stale).
info "Phase B: stopping sidecar and waiting out the single-writer staleness window (${P0E_WRITER_CLAIM_STALE_MS}ms)..."
docker compose stop sidecar >/dev/null
sleep $(( P0E_WRITER_CLAIM_STALE_MS / 1000 + 3 ))

info "Recreating gateway+sidecar ARMED for this run (council=$P0E_MODE @ http://$BRIDGE_HOST:$P0E_COUNCIL_PORT)..."
WATCH_DISPATCHER_ENABLED=true \
WATCH_PRODUCER_ENABLED=true \
WATCH_DISPATCHER_TICK_INTERVAL_MS=500 \
WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK=2 \
WATCH_DISPATCHER_GATEWAY_KEY="$WATCH_KEY" \
GW_ENABLE_COUNCIL_ENDPOINT=1 \
COUNCIL_GATEWAY_TOKEN="$COUNCIL_TOKEN" \
COUNCIL_BASE_URL="http://${BRIDGE_HOST}:${P0E_COUNCIL_PORT}" \
docker compose -f docker-compose.yml -f "$COMPOSE_OVERRIDE" \
    up -d --no-build --force-recreate gateway sidecar

wait_for_gateway_health 90 || fail "gateway did not become healthy after recreate"

# watch.db migration can lag boot; poll for the table.
table_ok=""
for _ in $(seq 1 30); do
    if sql ".tables" 2>/dev/null | grep -q pending_escalations; then table_ok=yes; break; fi
    sleep 1
done
[ -n "$table_ok" ] || fail "pending_escalations table did not appear at $DB_PATH"
sql ".tables" | grep -q spend_ledger \
    || fail "spend_ledger table missing — the running sidecar predates p0c (rebuild with P0E_REBUILD_MODE=local-overlay)"
pass "Stack recreated; watch.db migrated (pending_escalations + spend_ledger present)"

# --- clean room + ledger snapshot ---------------------------------------------
docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
PRAGMA defer_foreign_keys = ON;
DELETE FROM directive_outbox WHERE tenant='$P0E_TENANT';
DELETE FROM pending_escalations WHERE tenant='$P0E_TENANT';
DELETE FROM watch_fires WHERE tenant='$P0E_TENANT';
COMMIT;
SQL

SETTLED_BEFORE=$(sql "SELECT COALESCE((SELECT settled_usd FROM spend_ledger WHERE day_bucket='$DAY_BUCKET'), 0.0);")
info "spend_ledger settled_usd before (day $DAY_BUCKET): $SETTLED_BEFORE"

# --- seed ONE committed watch_fires row (CDC manual seed, smoke pattern) -------
NOW_MS=$(date +%s000)
STATE_JSON="{\"payload\":{\"run_id\":\"$RUN_ID\",\"observed\":{\"p0e\":true},\"reason\":\"p0e real charge-count escalation\",\"urgency\":\"high\"},\"observed_at\":$NOW_MS}"
FIRE_REASON="p0e real-charge-count escalation run_id=$RUN_ID"
ENVELOPE_JSON="{\"contract\":\"irin.comms.v0.1\",\"kind\":\"Escalation\",\"id\":\"$RUN_ID\",\"tenant\":\"$P0E_TENANT\",\"payload\":{\"sentinel\":\"$SENTINEL_NAME\",\"tier\":\"fast\",\"observed\":{\"p0e\":true},\"reason\":\"p0e real charge-count escalation\",\"urgency\":\"high\",\"proposed_action\":\"ConsultCouncil\",\"run_id\":\"$RUN_ID\"}}"

PREV_HASH=$(sql "SELECT COALESCE((SELECT hash FROM watch_fires WHERE tenant='$P0E_TENANT' ORDER BY id DESC LIMIT 1), '');")
if [ -z "$PREV_HASH" ]; then
    PREV_HASH=0ffed28740318eb8e9fa37cc3d034394c1eea87a273ecdbfcdcb937af502acba
fi
FIRE_HASH=$(python3 - "$P0E_TENANT" "$SENTINEL_NAME" "$NOW_MS" "$STATE_JSON" "$FIRE_REASON" "$PREV_HASH" <<'PY'
import hashlib, sys
tenant, sentinel, fired_at, state_json, reason, prev_hash = sys.argv[1:]
preimage = (
    f"{len(tenant)}:{tenant}|"
    f"{len(sentinel)}:{sentinel}|"
    f"{len(fired_at)}:{fired_at}|"
    f"{len(state_json)}:{state_json}|"
    f"{len(reason)}:{reason}|"
    f"{len(prev_hash)}:{prev_hash}"
)
print(hashlib.sha256(preimage.encode()).hexdigest())
PY
)

docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL
INSERT INTO watch_fires
    (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash, envelope_json, envelope_schema_version)
VALUES
    ('$P0E_TENANT', '$SENTINEL_NAME', $NOW_MS, '$STATE_JSON',
     '$FIRE_REASON', '$PREV_HASH', '$FIRE_HASH', '$ENVELOPE_JSON', 1);
SQL
info "Seeded one committed watch_fires row (run_id=$RUN_ID); waiting for the CDC producer..."

for _ in $(seq 1 "$P0E_TIMEOUT_SECONDS"); do
    PENDING_ID=$(sql "SELECT id FROM pending_escalations WHERE tenant='$P0E_TENANT' AND envelope_json LIKE '%$RUN_ID%' ORDER BY created_at_ms DESC LIMIT 1;")
    [ -n "$PENDING_ID" ] && break
    sleep 1
done
[ -n "$PENDING_ID" ] || fail "CDC producer did not enqueue a pending_escalations row for run_id=$RUN_ID
(producer refused to spawn? check 'docker compose logs sidecar | grep -i \"writer\\|producer\"')"
PENDING_COUNT=$(sql "SELECT COUNT(*) FROM pending_escalations WHERE tenant='$P0E_TENANT' AND envelope_json LIKE '%$RUN_ID%';")
[ "$PENDING_COUNT" -eq 1 ] || fail "expected exactly 1 pending row for run_id=$RUN_ID, got $PENDING_COUNT"
pass "CDC producer enqueued pending row $PENDING_ID"

# --- wait for the dispatcher's claim, then fire the idem colliders ------------
# The dispatcher claims the row (status='claimed'), then POSTs council-triage
# to the gateway with Idempotency-Key "<safe-tenant>:<pending-id>". For the
# p0e charset this is plain "$P0E_TENANT:$PENDING_ID" (formula pinned by
# tests/p0e_real_charge.rs against build_council_triage_headers).
IDEM_KEY="$P0E_TENANT:$PENDING_ID"

CLAIM_SEEN=""
for _ in $(seq 1 "$P0E_TIMEOUT_SECONDS"); do
    ST=$(sql "SELECT status FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID';")
    case "$ST" in
        claimed|council_response_staged|outbox_written|dismissed) CLAIM_SEEN="$ST"; break ;;
        dead_lettered) fail "row dead_lettered before the collider phase (last_error: $(sql "SELECT COALESCE(last_error,'') FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID';"))" ;;
    esac
    sleep 1
done
[ -n "$CLAIM_SEEN" ] || fail "dispatcher never claimed the row (still queued after ${P0E_TIMEOUT_SECONDS}s)"
info "Row left 'queued' (now: $CLAIM_SEEN) — arming colliders on Idempotency-Key (tenant-scoped, derived from run_id=$RUN_ID)"

# the old fixed `sleep 2` grace was a heuristic — on a
# slow compose the dispatcher's POST could still lose the CouncilState claim
# race to a collider, making the harness flaky in exactly the way it exists
# to catch. Poll the durable idem mirror (council_idem.db write-ahead row,
# written BEFORE the in-memory claim is granted) until the dispatcher's key
# appears, bounded by P0E_TIMEOUT_SECONDS. Fail CLOSED: if the entry never
# appears we abort BEFORE firing any collider (a collider that won the claim
# would itself deliberate = a real second charge in live mode).
COUNCIL_IDEM_DB_PATH_IN_CONTAINER="${COUNCIL_IDEM_DB_PATH_IN_CONTAINER:-/var/lib/sidecar/council_idem.db}"
IDEM_SEEN=""
for _ in $(seq 1 "$P0E_TIMEOUT_SECONDS"); do
    IDEM_COUNT=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 \
        "$COUNCIL_IDEM_DB_PATH_IN_CONTAINER" \
        "SELECT COUNT(*) FROM council_idem WHERE idempotency_key='$IDEM_KEY';" \
        2>/dev/null | tr -d '\r' || echo "")
    if [ "${IDEM_COUNT:-0}" -ge 1 ] 2>/dev/null; then
        IDEM_SEEN=1
        break
    fi
    sleep 1
done
[ -n "$IDEM_SEEN" ] \
    || fail "dispatcher's Idempotency-Key '$IDEM_KEY' never appeared in $COUNCIL_IDEM_DB_PATH_IN_CONTAINER within ${P0E_TIMEOUT_SECONDS}s — refusing to fire colliders (one could WIN the claim and cause a real second deliberation)"
pass "Dispatcher owns the CouncilState idem claim for $IDEM_KEY (durable mirror row present)"

# Collider body is INTENTIONALLY different from the dispatcher's prompt: a
# correct dedup must 409 it (pending within PENDING_TTL, or stored-conflict
# after) — and a same-body replay would be 200 + X-Idempotency-Replay. All
# three outcomes are "deduped". The ONLY failure is a fresh deliberation.
COLLIDER_BODY='{"model":"council-triage","messages":[{"role":"user","content":"P0-E idem collider — this body must NEVER reach a cabinet; the real CouncilState idem handlers must dedup it."}]}'

N_COLLIDERS=$(( P0E_N - 1 ))
for i in $(seq 1 "$N_COLLIDERS"); do
    HTTP_CODE=$(curl -sS -o "$COLLIDER_DIR/body.$i" -D "$COLLIDER_DIR/headers.$i" \
        -w '%{http_code}' \
        -H "Authorization: Bearer $WATCH_KEY" \
        -H "Idempotency-Key: $IDEM_KEY" \
        -H "Content-Type: application/json" \
        -X POST "$GW_URL/v1/chat/completions" \
        --data "$COLLIDER_BODY" || echo "000")
    echo "$HTTP_CODE" > "$COLLIDER_DIR/code.$i"
    info "collider $i/$N_COLLIDERS -> HTTP $HTTP_CODE"
done

FRESH_COLLIDERS=0
DEDUPED_COLLIDERS=0
for i in $(seq 1 "$N_COLLIDERS"); do
    CODE=$(cat "$COLLIDER_DIR/code.$i")
    REPLAY=$(grep -i '^X-Idempotency-Replay:' "$COLLIDER_DIR/headers.$i" 2>/dev/null | tr -d '\r' || true)
    COST=$(grep -i '^X-Total-Cost-Usd:' "$COLLIDER_DIR/headers.$i" 2>/dev/null | awk '{print $2}' | tr -d '\r' || true)
    if [ "$CODE" = "409" ]; then
        # a bare 409 is not proof of dedup — capacity
        # and shape gates also 409. Only the idem handlers' 409s carry
        # ERR_IDEMPOTENCY_CONFLICT in the body; anything else is NOT a dedup
        # and must count against billed==M.
        if grep -q 'ERR_IDEMPOTENCY_CONFLICT' "$COLLIDER_DIR/body.$i" 2>/dev/null; then
            DEDUPED_COLLIDERS=$((DEDUPED_COLLIDERS + 1))
        else
            FRESH_COLLIDERS=$((FRESH_COLLIDERS + 1))
            warn "collider $i got 409 WITHOUT ERR_IDEMPOTENCY_CONFLICT (body: $(head -c 300 "$COLLIDER_DIR/body.$i" 2>/dev/null)) — not an idem dedup!"
        fi
    elif [ "$CODE" = "200" ] && [ -n "$REPLAY" ]; then
        DEDUPED_COLLIDERS=$((DEDUPED_COLLIDERS + 1))
        info "collider $i was a stored replay (X-Idempotency-Replay, original cost header: ${COST:-n/a} — NOT a new charge)"
    else
        FRESH_COLLIDERS=$((FRESH_COLLIDERS + 1))
        warn "collider $i got HTTP $CODE WITHOUT a replay marker (cost header: ${COST:-n/a}) — a fresh deliberation!"
    fi
done
[ "$FRESH_COLLIDERS" -eq 0 ] \
    || fail "billed==M VIOLATED: $FRESH_COLLIDERS collider(s) produced a fresh deliberation. The CouncilState idem handlers did not dedup."
pass "All $N_COLLIDERS colliders deduped by the real CouncilState idem handlers (409 pending/conflict or replay)"

# --- wait for terminal status --------------------------------------------------
START=$(date +%s)
while true; do
    ST=$(sql "SELECT status FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID';")
    case "$ST" in
        outbox_written|dismissed) pass "Row reached terminal status: $ST"; break ;;
        dead_lettered) fail "row dead_lettered (last_error: $(sql "SELECT COALESCE(last_error,'') FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID';"))" ;;
    esac
    [ $(( $(date +%s) - START )) -gt "$P0E_TIMEOUT_SECONDS" ] && fail "timed out waiting for terminal status (last: $ST)"
    sleep 1
done

# --- billed == M == 1 reconciliation -------------------------------------------
REALIZED=$(sql "SELECT realized_cost_usd FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID';")
[ -n "$REALIZED" ] || fail "realized_cost_usd is NULL — no settle was recorded for the escalation"

RESERVATION_LEFT=$(sql "SELECT COUNT(*) FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID' AND reserved_estimate_usd IS NOT NULL;")
[ "$RESERVATION_LEFT" -eq 0 ] || fail "reservation stamp not NULLed after settle (double-settle hazard)"

SETTLED_AFTER=$(sql "SELECT COALESCE((SELECT settled_usd FROM spend_ledger WHERE day_bucket='$DAY_BUCKET'), 0.0);")

python3 - "$SETTLED_BEFORE" "$SETTLED_AFTER" "$REALIZED" "$P0E_SPEND_CAP" "$P0E_MODE" <<'PY' || exit 1
import sys
before, after, realized, cap, mode = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4]), sys.argv[5]
delta = after - before
eps = 1e-6
# Exactly ONE settle: the day-bucket delta must equal this escalation's
# realized cost. A dup settle would make delta ~= 2x realized.
assert abs(delta - realized) < eps, (
    f"spend_ledger settled delta ({delta}) != realized_cost_usd ({realized}) — "
    f"not exactly one settle for the escalation (dup charge or orphan settle)")
assert realized < cap, f"realized cost {realized} exceeded the P0-E cap {cap}"
if mode == "live":
    assert realized > 0.0, "live mode but realized cost is 0 — no real charge was reconciled"
else:
    assert realized == 0.0, f"stub mode but realized cost is {realized} — the no-spend stub billed?"
print(f"PASS: exactly one spend_ledger settle (delta {delta:.6f} == realized {realized:.6f}); "
      f"total {realized:.6f} < cap {cap}")
PY

OUTBOX_COUNT=$(sql "SELECT COUNT(*) FROM directive_outbox WHERE tenant='$P0E_TENANT' AND in_response_to='$PENDING_ID';")
FINAL_STATUS=$(sql "SELECT status FROM pending_escalations WHERE tenant='$P0E_TENANT' AND id='$PENDING_ID';")
# Code reality (dispatcher.rs stage_directive path): BOTH verdicts write
# exactly ONE signed outbox row — outbox status 'staged' for Act (pending row
# 'outbox_written'), 'dismissed' for Dismiss (pending row 'dismissed'). The
# verdict-independent invariant is one signed row per deliberation.
[ "$OUTBOX_COUNT" -eq 1 ] || fail "expected exactly 1 directive_outbox row regardless of verdict, got $OUTBOX_COUNT (terminal: $FINAL_STATUS)"
OUTBOX_SIG=$(sql "SELECT signature_b64 FROM directive_outbox WHERE tenant='$P0E_TENANT' AND in_response_to='$PENDING_ID';")
[ -n "$OUTBOX_SIG" ] || fail "directive_outbox.signature_b64 is empty"
pass "Exactly one signed directive_outbox row (terminal: $FINAL_STATUS)"

pass "billed == M == 1 reconciled: 1 dispatcher deliberation (realized \$$REALIZED), 0 fresh colliders, $DEDUPED_COLLIDERS deduped"

# --- confirmatory out-of-band recon (p0d ReconSource file-export shape) --------
if [ -n "$P0E_RECON_IMPORT_PATH" ] && [ -f "$P0E_RECON_IMPORT_PATH" ]; then
    python3 - "$P0E_RECON_IMPORT_PATH" "$DAY_BUCKET" "$SETTLED_AFTER" "$P0E_RECON_THRESHOLD_USD" <<'PY' || exit 1
import json, sys
path, day, local, threshold = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
raw = open(path).read()
external = None
# Mirrors sidecar-rs/src/watch/recon.rs parse_import: JSON map, JSON
# single-day form, then CSV "day,usd" lines.
try:
    v = json.loads(raw)
    if isinstance(v, dict):
        if day in v:
            external = float(v[day])
        elif v.get("day_bucket") == day and "total_usd" in v:
            external = float(v["total_usd"])
except Exception:
    pass
if external is None:
    for line in raw.splitlines():
        parts = line.split(",", 1)
        if len(parts) == 2 and parts[0].strip() == day:
            try:
                external = float(parts[1].strip()); break
            except ValueError:
                continue
assert external is not None, f"no entry for day {day} in recon import {path}"
divergence = abs(external - local)
assert divergence <= threshold, (
    f"RECON DIVERGENCE: provider export {external} vs local settled {local} "
    f"(|diff| {divergence} > threshold {threshold})")
print(f"PASS: out-of-band recon agrees (external {external} vs local settled {local}, threshold {threshold})")
PY
else
    warn "RECON: confirmatory out-of-band check PENDING-OPERATOR — no P0E_RECON_IMPORT_PATH supplied.
      Provider usage APIs lag same-day; the immediate billed==M proof above is the
      X-Total-Cost-Usd header + spend_ledger settle delta (per the P0-E design risk note).
      To close the loop later: drop a provider export ({\"$DAY_BUCKET\": <usd>} or CSV) and re-check
      against spend_ledger.settled_usd for $DAY_BUCKET."
fi

echo ""
echo "======================================================================"
echo "P0-E REAL CHARGE-COUNT HARNESS PASSED (mode=$P0E_MODE)"
echo "======================================================================"
echo "Escalation $PENDING_ID (run_id=$RUN_ID, tenant=$P0E_TENANT)"
echo "Submissions: $P0E_N total on one Idempotency-Key -> billed == M == 1"
echo "  1 real deliberation (dispatcher), realized cost: \$$REALIZED"
echo "  $DEDUPED_COLLIDERS colliders deduped by the real CouncilState idem handlers"
echo "Ledger: exactly one spend_ledger settle for $DAY_BUCKET (delta == realized)"
echo "Cap: \$$REALIZED < \$$P0E_SPEND_CAP (P0-E hard cap)"
echo "======================================================================"
exit 0
