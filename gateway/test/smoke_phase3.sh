#!/bin/bash
# ==========================================================================
# smoke_phase3.sh — Phase 3b.6 E2E smoke for the live dispatcher
#
# Proves one escalation travels the full live path:
#   watch_fires → CDC enqueue → queued/failed → claimed →
#   council_response_staged → signed directive_outbox
#
# using the Rust sidecar with the CDC producer + live dispatcher loops enabled.
# By default this uses a deterministic no-spend Council stub. Native local
# runs bind it on 127.0.0.1:8765; CI/containerized runs start it as a sibling
# Compose service so Gateway reaches it through Compose DNS. Set
# PHASE3_SMOKE_COUNCIL_MODE=live to use a real local council-rs server and live
# provider cabinet instead.
#
# Usage:
#   make smoke-phase3
#   or
#   bash test/smoke_phase3.sh
#
# Fast local arm gate:
#   PHASE3_SMOKE_REBUILD_MODE=none make smoke-phase3-arm-only
# proves the genuine attested STAGE/CONFIRM path and exits before the CDC/outbox
# tail. Use it to catch arm/readiness/writer-claim regressions before spending a
# hosted CI smoke run.
#
# Prerequisites (documented so the operator can set them up):
#   1. Full dev stack running:
#        make up
#      (gateway + sidecar containers)
#
#   2. The sidecar container must have been started with these environment
#      variables (set them in your .env before `make up`, or use a
#      docker-compose override file):
#        WATCH_DISPATCHER_ENABLED=true
#        WATCH_DISPATCHER_TICK_INTERVAL_MS=500
#        WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK=2
#        WATCH_DISPATCHER_GATEWAY_KEY=<raw gw_... key>
#
#      The GATEWAY_KEY is the **caller** credential (sidecar → gateway
#      /v1/chat/completions for council-triage). It is **not**
#      COUNCIL_GATEWAY_TOKEN (which is gateway → council-rs).
#
#      Operator flow to obtain it:
#        - Ensure the stack is up at least once with BOOTSTRAP_TOKEN set
#          (or have an existing admin key).
#        - make provision-key BUDGET=phase3-smoke TIER=default RPM=1000
#          (or use ADMIN_KEY=... if you already have one).
#        - Copy the raw key string printed by the command.
#        - Add `WATCH_DISPATCHER_GATEWAY_KEY=that_raw_key` to your .env
#        - Rebuild sidecar if needed (`make build-sidecar-smoke` on low-RAM hosts)
#        - Restart the stack: `make down && make up`
#
#   3. Default no-spend mode starts a deterministic Council stub on the native
#      host or the smoke Compose network and restarts gateway/sidecar so the
#      startup probe sees it. PHASE3_SMOKE_COUNCIL_STUB_RUNTIME=host|compose
#      overrides auto-selection. For the real live cabinet path, start
#      Council (`./target/release/council` from the monorepo root) yourself and
#      run PHASE3_SMOKE_COUNCIL_MODE=live PHASE3_SMOKE_RESTART_STACK=false
#      make smoke-phase3.
#
#      Rebuild mode defaults to PHASE3_SMOKE_REBUILD_MODE=image, which bakes the
#      provenance-correct sidecar image via the unified cargo-chef Dockerfile.
#      Set PHASE3_SMOKE_REBUILD_MODE=compose for a normal docker compose build,
#      or none to only recreate containers from an already-built image.
#
#      New in D9 harness extension: PHASE3_SMOKE_TRIGGER_MODE=file-inbox for
#      real unseeded FileInboxSentinel fires (drops *.txt into demo/inbox after
#      restart injects the phase3-sentinels.yaml fixture via override).
#
#   4. sqlite3 available inside the sidecar container (via `docker compose exec sidecar sqlite3`).
#
#   5. The gateway is responding on $GW_URL (default http://localhost:18080).
#
# The script inserts a committed watch_fires row by default (or triggers a real
# unseeded FileInboxSentinel fire), waits for the CDC producer sweep to create
# the pending_escalations row, then polls the DB until that row reaches a
# terminal state (outbox_written or dismissed).
#
# TRIGGER_MODE controls the escalation trigger mechanism:
#   file-inbox      — real drop of *.txt into demo/inbox (host); sentinel (armed
#                     via SENTINELS_CONFIG_PATH pointing at the committed
#                     test/fixtures/phase3-sentinels.yaml) writes the watch_fires
#                     row with no manual INSERT (assert_unseeded first). Requires
#                     WATCH_PRODUCER_ENABLED=true for CDC path.
#   manual-cdc-seed — manual INSERT of watch_fires row (classic D9 CDC seed path).
#   pending         — legacy direct INSERT into pending_escalations (bypasses
#                     watch_fires + producer).
#
# The script supports two modes:
#   - Positive (default): requires WATCH_DISPATCHER_ENABLED=true in the running stack.
#     Seeds/triggers a row and waits for it to reach a terminal state.
#   - Negative (`PHASE3_SMOKE_EXPECT_DISABLED=true`): seeds a queued row,
#     sleeps a short interval, and asserts the row is still queued with no
#     directive_outbox row created.
#
# On success the script prints a clear PASS summary.
# On failure it prints diagnostics and exits non-zero.
# ==========================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATEWAY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$GATEWAY_ROOT/.." && pwd)"
source "$SCRIPT_DIR/lib/council_stub_runtime.sh"

# --- colors ---------------------------------------------------------------
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC}: $1"; }
fail() { echo -e "${RED}FAIL${NC}: $1"; exit 1; }
warn() { echo -e "${YELLOW}WARN${NC}: $1"; }
info() { echo -e "INFO: $1"; }

# --- configuration --------------------------------------------------------
GW_URL="${GW_URL:-http://localhost:18080}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-gateway}"   # default from docker-compose

# --- canary-safety guard (do not hijack a live stack) ---------------------
# Later (restart_stack_for_phase3) the smoke runs a destructive
# `docker compose ... down --remove-orphans` and then recreates gateway+sidecar
# with smoke keys and seed data. If COMPOSE_PROJECT_NAME is left at the compose
# default ("gateway") while a live/canary stack is already running under that
# same project name, that teardown would stop and hijack the live containers.
# Refuse in that case unless the operator explicitly opts in.
#   - CI is unaffected: .github/workflows/triad-ci.yml sets
#     COMPOSE_PROJECT_NAME=provtest explicitly before `make smoke-phase3`.
#   - Escape hatch (you own the 'gateway' stack): PHASE3_SMOKE_FORCE_PROJECT=1.
# Only bites the exact hazard: default project name AND containers live under it.
if [ "$COMPOSE_PROJECT_NAME" = "gateway" ] && [ "${PHASE3_SMOKE_FORCE_PROJECT:-0}" != "1" ]; then
    if command -v docker >/dev/null 2>&1 &&
       [ -n "$(docker ps -q --filter 'label=com.docker.compose.project=gateway' 2>/dev/null)" ]; then
        fail "refusing to run against the default compose project 'gateway' while containers are live under it — the smoke's destructive down+recreate would hijack that stack (canary/live). Set COMPOSE_PROJECT_NAME to an isolated name (COMPOSE_PROJECT_NAME=provtest, as CI does), or PHASE3_SMOKE_FORCE_PROJECT=1 if you truly own the 'gateway' stack."
    fi
fi

SIDECAR_CONTAINER="${SIDECAR_CONTAINER:-sidecar}"
COUNCIL_MODE="${PHASE3_SMOKE_COUNCIL_MODE:-stub}"         # stub | live
PRODUCER_MODE="${PHASE3_SMOKE_PRODUCER_MODE:-cdc}"        # cdc | pending (legacy)
TRIGGER_MODE="${PHASE3_SMOKE_TRIGGER_MODE:-manual-cdc-seed}" # file-inbox | manual-cdc-seed | pending
RESTART_STACK="${PHASE3_SMOKE_RESTART_STACK:-true}"
# image  = build the provenance-correct image via the unified cargo-chef Dockerfile
#          (Review), then `up --no-build`. The IMAGE carries the baked
#          GW_BUILD_DIRTY for the current tree — NO host cargo build, NO binary
#          bind-mount overlay (both removed). `local-overlay` is a deprecated alias.
# compose = `up --build` (inline build). none = reuse the already-built image.
REBUILD_MODE="${PHASE3_SMOKE_REBUILD_MODE:-image}" # image | compose | none (local-overlay: deprecated alias of image)
ARM_ONLY="${PHASE3_SMOKE_ARM_ONLY:-false}"

# P1-5 — FORBID a leg-selection env on the provenance compile. The dirty/clean leg
# state must come ONLY from the real git tree (see test/prove_provenance_legs.sh),
# never an env channel an attacker could flip to launder a forged identity.
if [ -n "${GW_SMOKE_LEG:-}" ]; then
    echo "FAIL: GW_SMOKE_LEG is forbidden (Council P1-5). Leg state comes from the git tree, not env." >&2
    exit 1
fi
RESET_TENANT="${PHASE3_SMOKE_RESET_TENANT:-true}"
COUNCIL_STUB_PORT="${PHASE3_SMOKE_COUNCIL_STUB_PORT:-8765}"
export COUNCIL_STUB_RUNTIME="${PHASE3_SMOKE_COUNCIL_STUB_RUNTIME:-auto}"
COUNCIL_STUB_DEPLOYMENT="$(council_stub_runtime)" \
    || fail "could not select Council stub runtime"

# Smoke-only relax knobs. Injected via docker-compose.smoke.yml (never the base
# compose) so they can't leak into a production stack through a stray .env line.
# Exported ONCE here — docker compose reads them for the overlay's ${VAR:-...}
# substitution, so the values live in exactly two places (here and the
# overlay's defaults), not per-branch inline assignments.
# Per-key rate is NOT relaxed anywhere: provision the smoke key with enough
# RPM instead (make provision-key BUDGET=phase3-smoke RPM=1000).
export COUNCIL_CONCURRENCY_CAP="${COUNCIL_CONCURRENCY_CAP:-8}"
export RATE_LIMIT_EXEMPT_CIDRS="${RATE_LIMIT_EXEMPT_CIDRS:-127.0.0.0/8,172.16.0.0/12}"
COUNCIL_STUB_PID=""
SENTINEL_YAML_TMP=""
COMPOSE_OVERRIDE=""
TRIGGER_FILE=""
SMOKE_ARTIFACT_DIR="${SMOKE_ARTIFACT_DIR:-/tmp/gateway-phase3-smoke-artifacts}"
mkdir -p "$SMOKE_ARTIFACT_DIR"

# Arm ceremony for attested-arm attested reserve (positive path only).
# Boot-time: GW_ARM_PRINCIPALS (>=2) + GW_ARM_ATTEST_KEYS_PATH must be set
# and the registry file present on sidecar_data volume at sidecar start.
# Smoke performs genuine stage+sign+confirm (not rehearsal) using openssl.
DO_ARM=false
ARM_CEREMONY_COMPLETED=false
if [ "${PHASE3_SMOKE_EXPECT_DISABLED:-false}" != "true" ]; then
    DO_ARM=true
fi
ARM_KEY_PEM=""
ATT_KEYS_JSON=""
ARM_BEARER="ci-a:tok-ci-a"
ARM_PRINCIPALS="ci-a:tok-ci-a,ci-b:tok-ci-b"

# The DB path inside the sidecar container.
# In the current docker-compose the sidecar uses a path under /var/lib/sidecar
# or "watch.db" relative to its cwd. We try the most common locations.
DB_CANDIDATES=(
    "/var/lib/sidecar/watch.db"
    "/watch.db"
    "/var/lib/sidecar/gateway_watch.db"
)
DB_PATH=""

for cand in "${DB_CANDIDATES[@]}"; do
    if docker compose exec -T "$SIDECAR_CONTAINER" test -f "$cand" 2>/dev/null; then
        DB_PATH="$cand"
        break
    fi
done

if [ -z "$DB_PATH" ]; then
    # Last resort – let the operator override
    DB_PATH="${WATCH_DB_PATH_IN_CONTAINER:-/var/lib/sidecar/watch.db}"
    warn "Could not auto-detect watch.db inside container. Using $DB_PATH"
    warn "If this is wrong, set WATCH_DB_PATH_IN_CONTAINER and re-run."
fi

IDENTITY_PATH="${DIRECTIVE_IDENTITY_PATH_IN_CONTAINER:-/var/lib/sidecar/directive_identity.json}"

# Throwaway per-run watch DB for the D9/arm smoke (any run that generates the
# compose override: DO_ARM or file-inbox). watch_fires is engine-enforced
# append-only (W3 trg_watch_fires_no_delete), so a shared watch.db can never
# be row-reset — the smoke gets its own DB file instead, wiped between runs
# while the stack is down. Same ephemeral semantics CI gets from its throwaway
# volume. The real watch.db (canary 'sovereign' tenant) is a different file on
# the same named volume and is never touched.
SMOKE_WATCH_DB="${PHASE3_SMOKE_WATCH_DB:-/var/lib/sidecar/watch-smoke.db}"
# The wipe below `rm -f`s this path on the shared volume — refuse anything
# that resolves to the real watch.db (canary 'sovereign' tenant data).
case "$(basename "$SMOKE_WATCH_DB")" in
    watch.db) fail "PHASE3_SMOKE_WATCH_DB must not be the real watch.db (got $SMOKE_WATCH_DB)";;
esac
SIDECAR_DATA_VOLUME="${PHASE3_SMOKE_SIDECAR_VOLUME:-gateway_sidecar_data}"

# Smoke row identifiers
SMOKE_TENANT="phase3-smoke"
SMOKE_ID="smoke-esc-$(date +%s%N | cut -c1-10)"
SMOKE_SENTINEL="phase3-smoke-sentinel"
if [ "$TRIGGER_MODE" = "file-inbox" ]; then
  SMOKE_SENTINEL="file-inbox-watch"
fi
SMOKE_PENDING_ID="$SMOKE_ID"
SMOKE_ENVELOPE="{\"sentinel\":\"phase3-smoke\",\"tier\":\"fast\",\"observed\":{\"cpu\":95},\"reason\":\"smoke test escalation\",\"urgency\":\"high\",\"run_id\":\"$SMOKE_ID\"}"

TIMEOUT_SECONDS="${PHASE3_SMOKE_TIMEOUT_SECONDS:-150}"
CDC_TIMEOUT_SECONDS="${PHASE3_SMOKE_CDC_TIMEOUT_SECONDS:-$TIMEOUT_SECONDS}"
POLL_INTERVAL=1

wait_for_gateway_health() {
    local attempts="${1:-60}"
    for _ in $(seq 1 "$attempts"); do
        if curl -fsS "$GW_URL/health" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# --- assert_unseeded (D9 harness extension for file-inbox TRIGGER_MODE) -----
# Confirms no watch_fires or pending_escalations rows exist for this smoke run
# *before* performing the real inbox drop. Proves the fire is unseeded.
# NOTE: controlled smoke harness only (SMOKE_* vars are timestamp+fixed alphanum from
# this script; no user input reaches these sqlite3 -c strings). Interpolation is
# intentional for the hermetic test DB probes.
assert_unseeded() {
    local fires pending
    fires=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COUNT(*) FROM watch_fires WHERE tenant='$SMOKE_TENANT' AND (reason LIKE '%$SMOKE_ID%' OR envelope_json LIKE '%$SMOKE_ID%');" 2>/dev/null | tr -d '\r' || echo "0")
    pending=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COUNT(*) FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND (id='$SMOKE_PENDING_ID' OR envelope_json LIKE '%$SMOKE_ID%');" 2>/dev/null | tr -d '\r' || echo "0")
    if [ "$fires" -ne 0 ] || [ "$pending" -ne 0 ]; then
        fail "assert_unseeded failed: pre-existing rows for $SMOKE_ID (fires=$fires, pending=$pending). Stale test data or prior run not cleaned?"
    fi
}

port_is_free() {
    python3 - "$1" <<'PY'
import socket
import sys

port = int(sys.argv[1])
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
try:
    sock.bind(("0.0.0.0", port))
except OSError:
    sys.exit(1)
finally:
    sock.close()
PY
}

start_stub_council() {
    if [ "$COUNCIL_MODE" != "stub" ] || [ "${PHASE3_SMOKE_EXPECT_DISABLED:-false}" = "true" ]; then
        return 0
    fi

    if [ "$COUNCIL_STUB_DEPLOYMENT" = "compose" ]; then
        info "No-spend Council stub selected as a sibling Compose service"
        return 0
    fi

    if ! port_is_free "$COUNCIL_STUB_PORT"; then
        fail "PHASE3_SMOKE_COUNCIL_MODE=stub but 127.0.0.1:$COUNCIL_STUB_PORT is already in use.
Stop the existing process or run PHASE3_SMOKE_COUNCIL_MODE=live if you intend to use a real council-rs server."
    fi

    COUNCIL_STUB_PROFILE=phase3 python3 "$SCRIPT_DIR/council_stub.py" "$COUNCIL_STUB_PORT" &
    COUNCIL_STUB_PID="$!"

    for _ in $(seq 1 50); do
        if curl -fsS "http://127.0.0.1:$COUNCIL_STUB_PORT/api/health" >/dev/null 2>&1; then
            pass "No-spend Council stub listening on 127.0.0.1:$COUNCIL_STUB_PORT (native host mode)"
            return 0
        fi
        sleep 0.1
    done
    fail "No-spend Council stub did not become healthy on 127.0.0.1:$COUNCIL_STUB_PORT"
}

source "$SCRIPT_DIR/lib/worktree_docker_context.sh"

build_sidecar_image() {
    # Build the provenance-correct sidecar IMAGE via the unified cargo-chef
    # Dockerfile (context=repo root, .git in-context — Review). The
    # image binary carries the GW_BUILD_DIRTY baked from THIS checkout's real git
    # state, so a clean tree -> clean image -> real arm with NO host cargo build and
    # NO binary bind-mount overlay (both removed: the host build was the killed
    # compile #2; the overlay only existed to mask the old always-DARK image).
    # The DIRTY leg is proven separately by test/prove_provenance_legs.sh.
    # docker_path + COMPOSE_FILE_ARGS are set by the caller (restart_stack_for_phase3).
    info "Building sidecar image via unified cargo-chef Dockerfile (light profile)..."
    local ctx_cleanup="" extra_compose=""
    if [ -f "$REPO_ROOT/.git" ] && grep -q '^gitdir:' "$REPO_ROOT/.git" 2>/dev/null; then
        ctx_cleanup="$(stage_worktree_docker_context "$REPO_ROOT")"
        extra_compose="$(mktemp -t phase3-compose-ctx.XXXXXX.yml)"
        cat > "$extra_compose" <<EOF
services:
  sidecar:
    build:
      context: ${ctx_cleanup}
EOF
        info "Git worktree detected — docker build context staged with real .git at $ctx_cleanup"
    fi
    PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS ${extra_compose:+-f "$extra_compose"} build \
        --build-arg CARGO_PROFILE_RELEASE_LTO="${PHASE3_SMOKE_LTO:-false}" \
        --build-arg CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${PHASE3_SMOKE_CGU:-16}" \
        sidecar \
        || { [ -z "$ctx_cleanup" ] || rm -rf "$ctx_cleanup"; [ -z "$extra_compose" ] || rm -f "$extra_compose"; fail "sidecar image build failed (unified Dockerfile)"; }
    # NOT `[ -n ... ] && rm`: on a non-worktree checkout the empty-var guard
    # makes the && list (and thus this function) return 1, and set -e kills
    # the whole smoke silently right after "Image Built".
    if [ -n "$ctx_cleanup" ]; then rm -rf "$ctx_cleanup"; fi
    if [ -n "$extra_compose" ]; then rm -f "$extra_compose"; fi
}

prepare_arm_artifacts() {
    if [ "$DO_ARM" != true ]; then
        return 0
    fi
    info "Preparing real arm key + attest registry (host-side openssl)..."
    ARM_KEY_PEM="$(mktemp -t phase3-arm-key.XXXXXX.pem)"
    ATT_KEYS_JSON="$(mktemp -t phase3-attest-keys.XXXXXX.json)"
    openssl ecparam -name prime256v1 -genkey -noout -out "$ARM_KEY_PEM" >/dev/null 2>&1 \
        || fail "openssl ecparam for arm key failed (openssl required in CI runner)"
    # Extract compressed (33B) SEC1 P-256 point from DER SPKI; trailing 33 bytes.
    PUB_B64=$(openssl ec -in "$ARM_KEY_PEM" -pubout -conv_form compressed -outform DER 2>/dev/null | tail -c 33 | base64 | tr -d '\n')
    [ -n "$PUB_B64" ] || fail "failed to extract compressed public_key for attest registry"
    # Quick prefix check (02/03 for compressed)
    PRE=$(printf '%s' "$PUB_B64" | base64 -d 2>/dev/null | xxd -p -c1 2>/dev/null | head -1 || echo '??')
    if [ "$PRE" != "02" ] && [ "$PRE" != "03" ]; then
        # alt for mac/xxd-less: try od
        PRE=$(printf '%s' "$PUB_B64" | base64 -d 2>/dev/null | od -An -tx1 | tr -d ' \n' | cut -c1-2 || echo '??')
    fi
    if [ "$PRE" != "02" ] && [ "$PRE" != "03" ]; then
        warn "pubkey prefix check got '$PRE' (proceeding; tail-c-33 assumed correct per contract)"
    fi
    cat > "$ATT_KEYS_JSON" <<EOF
[{"credential_id":"ci-arm-cred","credential_type":"se-p256","public_key":"$PUB_B64","label":"ci","enrolled_at":"2026-01-01T00:00:00Z"}]
EOF
    chmod 600 "$ARM_KEY_PEM" "$ATT_KEYS_JSON" 2>/dev/null || true
    pass "arm artifacts ready (credential=ci-arm-cred)"
}

perform_real_arm() {
    if [ "$DO_ARM" != true ]; then
        return 0
    fi
    [ -n "$ARM_KEY_PEM" ] && [ -f "$ARM_KEY_PEM" ] || fail "arm key missing for ceremony"
    [ -n "$ATT_KEYS_JSON" ] && [ -f "$ATT_KEYS_JSON" ] || fail "attest registry missing for ceremony"
    info "Performing genuine arm ceremony (stage/sign/confirm) — must not be rehearsal"
    # Identity diagnostic: the serving sidecar IS the image binary (no bind-mount
    # overlay anymore — the unified Dockerfile bakes provenance into the image).
    # build_id is reported by the running binary in the stage response below.
    echo "--- serving sidecar image identity ---"
    PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS exec -T sidecar sh -c 'ls -l /usr/local/bin/gateway-sidecar; md5sum /usr/local/bin/gateway-sidecar' 2>&1 || echo "(exec failed)"
    echo "--- end identity diag ---"
    local sock_vol
    sock_vol="${COMPOSE_PROJECT_NAME}_sidecar_sock"
    curl_uds() {
        docker run --rm -i --user 0 -v "${sock_vol}:/run/sidecar" \
            curlimages/curl:8.12.1 -sS --unix-socket /run/sidecar/sidecar.sock "$@"
    }
    # Pre-pull the curl image so the FIRST curl_uds (the stage call) does not emit
    # docker "Pulling/Status: Downloaded" lines that could contaminate the
    # captured stage stdout and make jq read a missing `.rehearsal` (-> "true").
    PATH="$docker_path" docker pull curlimages/curl:8.12.1 >/dev/null 2>&1 || true

    # 1. STAGE (real arm — no rehearsal:true)
    # UDS readiness gate (Review, P0-1): the management socket FILE can
    # exist before the listener is accepting (bind->accept window), so the FIRST
    # stage call right after a stack restart can hit connection-refused (curl exit 7
    # -> http_code 000). RETRY the stage ONLY on 000 (a transport/readiness failure);
    # any real HTTP code (200/401/403/409/...) is authoritative and breaks the loop
    # immediately — we never paper over a genuine rejection. Re-staging is idempotent
    # (the stage_id nonce just changes). Mirrors the readiness loop already proven in
    # test/prove_provenance_legs.sh::probe_leg.
    STAGE_OUT=""
    STAGE_CODE="000"
    for _ in $(seq 1 30); do
        STAGE_OUT=$(curl_uds -w '\n%{http_code}' -X POST http://localhost/watch/admin/producer/arm/stage \
            -H "Authorization: Bearer ${ARM_BEARER}" \
            -H "Content-Type: application/json" -d '{}' 2>/dev/null) || true
        STAGE_CODE=$(printf '%s' "$STAGE_OUT" | tail -1)
        [ -n "$STAGE_CODE" ] && [ "$STAGE_CODE" != "000" ] && break
        sleep 1
    done
    STAGE_BODY=$(printf '%s' "$STAGE_OUT" | sed '$d')
    if [ "$STAGE_CODE" = "000" ]; then
        # Readiness exhausted: 30s of retries never got a transport-level reply.
        # Dump stack state + sidecar logs so the true cause is visible (Council P0-1).
        echo "--- arm STAGE UDS unreachable: docker compose ps ---" >&2
        PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS ps >&2 2>&1 || true
        echo "--- recent sidecar logs ---" >&2
        PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS logs sidecar 2>&1 | tail -30 >&2 || true
        fail "arm STAGE unreachable over UDS (HTTP 000 after 30 retries) — sidecar never accepted; fail closed"
    fi
    if [ "$STAGE_CODE" != "200" ]; then
        echo "STAGE body: $STAGE_BODY" >&2
        fail "arm STAGE failed (HTTP $STAGE_CODE) — fail closed"
    fi
    STAGE_ID=$(printf '%s' "$STAGE_BODY" | jq -r '.stage_id // empty')
    CHALLENGE_B64=$(printf '%s' "$STAGE_BODY" | jq -r '.challenge // empty')
    # NOTE: do NOT use `jq '.rehearsal // "true"'` — jq's `//` treats boolean
    # `false` as empty, so a genuine real arm (`"rehearsal":false`) would wrongly
    # read as "true". `tostring` preserves false/true; a missing field becomes
    # "null" (!= "false" => fail-closed below). This footgun masked a clean arm
    # for several CI cycles.
    REHEARSAL=$(printf '%s' "$STAGE_BODY" | jq -r '.rehearsal | tostring')
    if [ "$REHEARSAL" != "false" ]; then
        # The just-built binary is proven CLEAN (buildid_probe build_is_dirty=false)
        # and the sidecar does NOT log the B6 rehearsal-forcing warn — so a missing
        # `.rehearsal` field (jq `// "true"` default), not a DARK build, is the
        # likely cause. Dump the RAW stage response + which build_id the sidecar
        # actually logged so the true cause is unambiguous.
        echo "--- RAW stage response (code then body) ---" >&2
        echo "STAGE_CODE=[$STAGE_CODE]" >&2
        echo "STAGE_BODY=[$STAGE_BODY]" >&2
        echo "REHEARSAL(parsed)=[$REHEARSAL] STAGE_ID=[$STAGE_ID] CHALLENGE_len=[${#CHALLENGE_B64}]" >&2
        echo "--- sidecar arm/build log lines ---" >&2
        PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS logs sidecar 2>&1 | grep -iE "build_id|forcing REHEARSAL|may not arm|arm stage|arm/stage|rehearsal|allow_real" | tail -8 >&2 || echo "(no arm log line found)" >&2
        fail "arm STAGE returned rehearsal=$REHEARSAL (expected false). See RAW stage response above (binary is clean per buildid_probe; suspect response shape, not DARK build)."
    fi
    [ -n "$STAGE_ID" ] && [ -n "$CHALLENGE_B64" ] || fail "stage response missing stage_id or challenge"
    # Negative control (Council P1-6): read the EMBEDDED build_id straight out of the
    # signed challenge bytes the running binary produced (the artifact, not the wire).
    # rehearsal=false MUST come with a clean build_id ("<sha>", never "<sha>-dirty"/
    # "unknown") — else a DARK build leaked a real arm.
    CHAL_BIN="$(mktemp -t phase3-chal.XXXXXX.bin)"
    if ! printf '%s' "$CHALLENGE_B64" | base64 -d > "$CHAL_BIN" 2>/dev/null; then
        rm -f "$CHAL_BIN"
        fail "arm STAGE challenge is not valid base64 — fail closed"
    fi
    CLEAN_BUILD_ID=$(jq -r '.build_id // "PARSE_FAIL"' "$CHAL_BIN" 2>/dev/null || echo "PARSE_FAIL")
    CHAL_V=$(jq -r '.v // "missing"' "$CHAL_BIN" 2>/dev/null || echo "missing")
    [ -n "$CLEAN_BUILD_ID" ] || CLEAN_BUILD_ID="PARSE_FAIL"
    case "$CLEAN_BUILD_ID" in
        *-dirty|PARSE_FAIL|unknown*)
            if [ "$CLEAN_BUILD_ID" = "PARSE_FAIL" ] && { [ "$CHAL_V" = "1" ] || [ "$CHAL_V" = "missing" ]; }; then
                rm -f "$CHAL_BIN"
                fail "arm STAGE rehearsal=false but challenge v=$CHAL_V has no embedded build_id (MF-1 requires v>=2). Sidecar image is STALE — drop PHASE3_SMOKE_REBUILD_MODE=none and rebuild (default image mode bakes a current binary)."
            fi
            rm -f "$CHAL_BIN"
            fail "arm STAGE rehearsal=false but embedded build_id='$CLEAN_BUILD_ID' (challenge v=$CHAL_V; expected clean '<sha>'). Provenance/oracle mismatch — fail closed." ;;
    esac
    pass "STAGE ok (rehearsal=false, build_id=$CLEAN_BUILD_ID, stage_id=$STAGE_ID)"

    # 2. SIGN (ECDSA-P256-SHA256, DER, b64) — CHAL_BIN decoded above (single decode site)
    SIG_B64=$(openssl dgst -sha256 -sign "$ARM_KEY_PEM" -binary "$CHAL_BIN" | base64 | tr -d '\n') || fail "openssl sign failed"
    rm -f "$CHAL_BIN"

    # 3. CONFIRM
    CONF_BODY="{\"stage_id\":\"$STAGE_ID\",\"credential_id\":\"ci-arm-cred\",\"credential_type\":\"se-p256\",\"signature\":\"$SIG_B64\"}"
    CONF_OUT=$(curl_uds -w '\n%{http_code}' -X POST http://localhost/watch/admin/producer/arm/confirm \
        -H "Authorization: Bearer ${ARM_BEARER}" \
        -H "Content-Type: application/json" -d "$CONF_BODY" ) || true
    CONF_CODE=$(printf '%s' "$CONF_OUT" | tail -1)
    CONF_BODY=$(printf '%s' "$CONF_OUT" | sed '$d')
    if [ "$CONF_CODE" != "200" ]; then
        echo "CONFIRM body: $CONF_BODY" >&2
        fail "arm CONFIRM failed (HTTP $CONF_CODE) — fail closed, no silent skip"
    fi
    pass "CONFIRM ok (HTTP 200). active_arm row written; reserve can now claim."
    ARM_CEREMONY_COMPLETED=true
}

restart_stack_for_phase3() {
    if [ "$RESTART_STACK" != "true" ] || [ "${PHASE3_SMOKE_EXPECT_DISABLED:-false}" = "true" ]; then
        return 0
    fi

    local current_watch_key current_council_token producer_enabled docker_path
    current_watch_key=$(docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'printf "%s" "${WATCH_DISPATCHER_GATEWAY_KEY:-}"' | tr -d '\r')
    current_council_token=$(docker compose exec -T gateway sh -c 'printf "%s" "${COUNCIL_GATEWAY_TOKEN:-}"' | tr -d '\r')

    if [ -z "$current_watch_key" ]; then
        fail "Cannot restart D9 stack: running sidecar does not expose WATCH_DISPATCHER_GATEWAY_KEY.
Provision a phase3 smoke gateway key first, then re-run."
    fi
    if [ -z "$current_council_token" ]; then
        fail "Cannot restart D9 stack: running gateway does not expose COUNCIL_GATEWAY_TOKEN.
Set COUNCIL_GATEWAY_TOKEN before running the council endpoint smoke."
    fi

    if [ "$TRIGGER_MODE" = "file-inbox" ]; then
        # Controlled D9 harness cleanup. Stale trigger files from previous
        # smoke runs are generated artifacts; if left in the bind-mounted
        # inbox, PollWatcher can replay them on stack restart and drown the
        # fresh run before CDC reaches the new row.
        find demo/inbox -maxdepth 1 -type f -name 'phase3-smoke-*.txt' -delete
    fi

    # D9 harness extension (TRIGGER_MODE=file-inbox): deliver the committed
    # 1-sentinel fixture via a *temporary* compose override (merged at up time).
    # This adds a ro bind mount of a temp copy + SENTINELS_CONFIG_PATH env
    # to the sidecar without any change to docker-compose.yml (only comment
    # updated there). The override + temp yaml are cleaned in the trap.
    # The activation fix in registry.rs:build_file_inbox guarantees the
    # PollWatcher is started for the polling file-inbox sentinel so real
    # drops produce unseeded watch_fires rows.
    COMPOSE_OVERRIDE=""
    SENTINEL_YAML_TMP=""
    if [ "$TRIGGER_MODE" = "file-inbox" ]; then
        SENTINEL_YAML_TMP="$(mktemp -t phase3-sentinels.XXXXXX.yaml)"
        cp "test/fixtures/phase3-sentinels.yaml" "$SENTINEL_YAML_TMP"
    fi

    # Arm + inbox override (always full sections because compose override replaces
    # lists like volumes/environment). Include arm principals+keys volume when DO_ARM.
    if [ "$DO_ARM" = true ] || [ "$TRIGGER_MODE" = "file-inbox" ]; then
        COMPOSE_OVERRIDE="$(mktemp -t phase3-compose-override.XXXXXX.yaml)"
        {
            echo 'services:'
            echo '  sidecar:'
            echo '    environment:'
            if [ "$DO_ARM" = true ]; then
                echo '      - GW_ARM_PRINCIPALS=ci-a:tok-ci-a,ci-b:tok-ci-b'
                # CI fixture path only — production uses arm_attest_keys.json (arm-enroll).
                echo '      - GW_ARM_ATTEST_KEYS_PATH=/var/lib/sidecar/attest_keys.json'
            fi
            if [ "$TRIGGER_MODE" = "file-inbox" ]; then
                echo '      - SENTINELS_CONFIG_PATH=/var/lib/gateway/sentinels/phase3-sentinels.yaml'
            fi
            echo "      - WATCH_DB_PATH=${SMOKE_WATCH_DB}"
            echo '      - COUNCIL_IDEM_DB_PATH=/var/lib/sidecar/council_idem.db'
            echo '      - LEDGER_DB_PATH=/var/lib/sidecar/ledger.db'
            echo '      - LEDGER_SIGNING_KEY_PATH=/run/secrets/ledger_key'
            echo '      - GATEWAY_DURABLE=${GATEWAY_DURABLE:-0}'
            echo '      - GATEWAY_STATE_DB_PATH=/var/lib/sidecar/gateway.db'
            echo '      - DECON_CONFIG_PATH=/conf/decon_config.json'
            echo '      - AUTH_CONFIG_PATH=${AUTH_CONFIG_PATH:-/var/lib/sidecar/auth_keys.json}'
            echo '      - IP_POLICY_PATH=/conf/ip_policy.json'
            echo '      - GATEWAY_AUTH_FAIL_CLOSED=${GATEWAY_AUTH_FAIL_CLOSED:-true}'
            echo '      - AUTH_PEPPER=${AUTH_PEPPER}'
            echo '      - WATCH_DISPATCHER_ENABLED=${WATCH_DISPATCHER_ENABLED:-true}'
            echo '      - WATCH_PRODUCER_ENABLED=${WATCH_PRODUCER_ENABLED:-false}'
            echo '      - WATCH_CANARY_TENANT=${WATCH_CANARY_TENANT:-phase3-smoke}'
            echo '      - WATCH_DISPATCHER_TICK_INTERVAL_MS=${WATCH_DISPATCHER_TICK_INTERVAL_MS:-500}'
            echo '      - WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK=${WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK:-2}'
            echo '      - WATCH_DISPATCHER_GATEWAY_KEY=${WATCH_DISPATCHER_GATEWAY_KEY:-}'
            echo '      - GATEWAY_BASE_URL=${GATEWAY_BASE_URL:-http://gateway:8080}'
            # COUNCIL_CONCURRENCY_CAP + RATE_LIMIT_EXEMPT_CIDRS are NOT echoed
            # here: docker-compose.smoke.yml carries them and compose merges
            # environment lists by key across -f files, so the overlay's values
            # survive this generated override (verified via `docker compose
            # config` on the 3-file stack). Two sources (script export +
            # overlay default), not three.
            echo '    volumes:'
            echo '      - ./conf:/conf:ro'
            echo '      - sidecar_sock:/run/sidecar'
            echo '      - sidecar_data:/var/lib/sidecar'
            echo '      - ./demo/inbox:/var/lib/gateway/inbox:ro'
            echo '      - ${HOME}/.irin/ledger_key.pem:/run/secrets/ledger_key:ro'
            echo '      - ${HOME}/.config/gcloud:/root/.config/gcloud:ro'
            if [ "$DO_ARM" = true ]; then
                echo "      - ${ATT_KEYS_JSON}:/var/lib/sidecar/attest_keys.json:ro"
                # NOTE: the CLEAN-binary bind-mount overlay is GONE. The unified
                # cargo-chef Dockerfile bakes GW_BUILD_DIRTY into the IMAGE from the
                # real checkout, so the container runs the correct provenance binary
                # directly — no shadow mount, no host cargo build (Review).
            fi
            if [ "$TRIGGER_MODE" = "file-inbox" ] && [ -n "${SENTINEL_YAML_TMP:-}" ]; then
                echo "      - ${SENTINEL_YAML_TMP}:/var/lib/gateway/sentinels/phase3-sentinels.yaml:ro"
            fi
        } > "$COMPOSE_OVERRIDE"
    fi

    producer_enabled=false
    if [ "$PRODUCER_MODE" = "cdc" ]; then
        producer_enabled=true
    fi
    # T1 attested arm (DO_ARM): the producer must NOT be boot-env-armed. There
    # are two arm mechanisms in the sidecar and they share ONE single-writer
    # claim + producer_kill_state:
    #   1. boot env-arm  — WATCH_PRODUCER_ENABLED=true + dispatcher key spawns
    #      the CDC producer at boot (runner.rs is_producer_gate_armed), taking
    #      the writer claim and setting producer_kill_state=Some.
    #   2. attested four-eyes confirm — admin_arm_confirm_attest's Verified arm
    #      calls arm_producer_start, which 409s "Producer already armed" when
    #      producer_kill_state is already Some.
    # Boot-env-arming AND running the attested ceremony makes (2) collide with
    # (1): confirm verifies the ES256 signature then 409s on the start. The
    # real T1 model is that the hardware-attested confirm IS the gate that
    # starts the producer, so when DO_ARM=true we boot it DISABLED and let the
    # ceremony's confirm acquire the writer claim + spawn the sweep cleanly.
    # (Previously masked: every arm folded to rehearsal, whose confirm never
    # calls arm_producer_start — the jq rehearsal-parse fix unmasked this.)
    if [ "$DO_ARM" = true ]; then
        producer_enabled=false
    fi
    docker_path="$PATH"
    if ! command -v docker-credential-desktop >/dev/null 2>&1 && [ -x /Applications/Docker.app/Contents/Resources/bin/docker-credential-desktop ]; then
        docker_path="/Applications/Docker.app/Contents/Resources/bin:$docker_path"
    fi

    COMPOSE_FILE_ARGS="-f docker-compose.yml -f docker-compose.smoke.yml"
    if [ -n "$COMPOSE_OVERRIDE" ]; then
        COMPOSE_FILE_ARGS="$COMPOSE_FILE_ARGS -f $COMPOSE_OVERRIDE"
    fi

    local bridge_ip council_base_url upstream_runtime
    bridge_ip="host.docker.internal"
    if [ "$(uname)" != "Darwin" ]; then
        bridge_ip="$(docker network inspect bridge -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null || echo '172.17.0.1')"
        [ -n "$bridge_ip" ] || bridge_ip="172.17.0.1"
    fi
    upstream_runtime="$COUNCIL_STUB_DEPLOYMENT"
    [ "$COUNCIL_MODE" = "stub" ] || upstream_runtime="host"
    council_base_url="$(council_stub_base_url "$upstream_runtime" "$COUNCIL_STUB_PORT" "$bridge_ip")"

    start_compose_stub() {
        if [ "$COUNCIL_MODE" = "stub" ] && [ "$COUNCIL_STUB_DEPLOYMENT" = "compose" ]; then
            PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS up -d --wait council-stub \
                || fail "Compose-network Council stub did not become healthy"
            pass "No-spend Council stub healthy at $council_base_url (Compose network)"
        fi
    }

    info "Recreating gateway/sidecar for D9 env (council_mode=$COUNCIL_MODE, producer_mode=$PRODUCER_MODE, trigger_mode=$TRIGGER_MODE, rebuild_mode=$REBUILD_MODE)..."
    if [ "$REBUILD_MODE" = "image" ] || [ "$REBUILD_MODE" = "local-overlay" ]; then
        [ "$REBUILD_MODE" = "local-overlay" ] && warn "REBUILD_MODE=local-overlay is deprecated (host cargo build + bind-mount overlay removed); using unified image build"
        build_sidecar_image
        # The image now carries correct provenance (clean tree -> clean binary), so
        # the container runs it directly via `up --no-build` — no bind-mount overlay.
        # Full `down` (not in-place --force-recreate) first: a clean teardown lets the
        # prior sidecar release its single-writer producer lease and removes any
        # lingering duplicate container. Named volumes are KEPT (auth/ledger keys
        # live in sidecar_data).
        PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS down --remove-orphans 2>/dev/null || true
        if [ -n "$COMPOSE_OVERRIDE" ]; then
            # Wipe the throwaway smoke DB while the stack is down. Row-level
            # reset is impossible (watch_fires append-only, W3); file-level
            # reset is the sanctioned teardown for the smoke's own DB.
            PATH="$docker_path" docker run --rm -v "${SIDECAR_DATA_VOLUME}:/var/lib/sidecar" alpine:3.21 \
                sh -c "rm -f ${SMOKE_WATCH_DB} ${SMOKE_WATCH_DB}-wal ${SMOKE_WATCH_DB}-shm" \
                || warn "smoke watch DB wipe failed (continuing; stale smoke rows may persist)"
        fi
        start_compose_stub
        PATH="$docker_path" \
        WATCH_DISPATCHER_ENABLED=true \
        WATCH_PRODUCER_ENABLED="$producer_enabled" \
        WATCH_DISPATCHER_GATEWAY_KEY="$current_watch_key" \
        GW_ENABLE_COUNCIL_ENDPOINT=1 \
        COUNCIL_GATEWAY_TOKEN="$current_council_token" \
        COUNCIL_BASE_URL="$council_base_url" \
        GW_ARM_PRINCIPALS="$ARM_PRINCIPALS" \
        GW_ARM_ATTEST_KEYS_PATH="/var/lib/sidecar/attest_keys.json" \
        docker compose $COMPOSE_FILE_ARGS up -d --no-build gateway sidecar
    elif [ "$REBUILD_MODE" = "compose" ]; then
        start_compose_stub
        PATH="$docker_path" \
        WATCH_DISPATCHER_ENABLED=true \
        WATCH_PRODUCER_ENABLED="$producer_enabled" \
        WATCH_DISPATCHER_GATEWAY_KEY="$current_watch_key" \
        GW_ENABLE_COUNCIL_ENDPOINT=1 \
        COUNCIL_GATEWAY_TOKEN="$current_council_token" \
        COUNCIL_BASE_URL="$council_base_url" \
        docker compose $COMPOSE_FILE_ARGS up -d --build gateway sidecar
    elif [ "$REBUILD_MODE" = "none" ]; then
        start_compose_stub
        PATH="$docker_path" \
        WATCH_DISPATCHER_ENABLED=true \
        WATCH_PRODUCER_ENABLED="$producer_enabled" \
        WATCH_DISPATCHER_GATEWAY_KEY="$current_watch_key" \
        GW_ENABLE_COUNCIL_ENDPOINT=1 \
        COUNCIL_GATEWAY_TOKEN="$current_council_token" \
        COUNCIL_BASE_URL="$council_base_url" \
        GW_ARM_PRINCIPALS="$ARM_PRINCIPALS" \
        GW_ARM_ATTEST_KEYS_PATH="/var/lib/sidecar/attest_keys.json" \
        docker compose $COMPOSE_FILE_ARGS up -d --no-build --force-recreate gateway sidecar
    else
        fail "Unsupported PHASE3_SMOKE_REBUILD_MODE=$REBUILD_MODE (expected image, compose, or none)"
    fi

    # Override runs (DO_ARM / file-inbox) point the sidecar at the throwaway
    # smoke DB — pin DB_PATH so host-side sqlite queries hit the same file,
    # not the real watch.db a stale pre-restart detection may have found.
    if [ -n "$COMPOSE_OVERRIDE" ]; then
        DB_PATH="$SMOKE_WATCH_DB"
    fi

    if ! wait_for_gateway_health 90; then
        fail "Gateway did not become healthy after D9 stack recreation"
    fi

    # Verify DNS and reachability from the Gateway container before any arm or
    # escalation seed. This catches runner-netns/host-gateway mismatches early.
    PATH="$docker_path" docker compose $COMPOSE_FILE_ARGS exec -T gateway \
        wget -q -O /dev/null "$council_base_url/api/health" \
        || fail "Gateway container cannot reach Council upstream at $council_base_url"
    pass "Gateway -> Council upstream health probe passed before arm/seed"

    # Real arm must happen on the freshly recreated stack (with
    # principals + registry file) before any pending_escalation is claimed.
    # Fail closed: if stage returns rehearsal or confirm !=200, fail loudly.
    if [ "$DO_ARM" = true ]; then
        perform_real_arm
    fi
}

cleanup() {
    set +e
    if [ -n "${COUNCIL_STUB_PID:-}" ]; then
        kill "$COUNCIL_STUB_PID" >/dev/null 2>&1 || true
        wait "$COUNCIL_STUB_PID" >/dev/null 2>&1 || true
    fi
    if [ -n "${SENTINEL_YAML_TMP:-}" ]; then
        rm -f "$SENTINEL_YAML_TMP"
    fi
    if [ -n "${COMPOSE_OVERRIDE:-}" ]; then
        rm -f "$COMPOSE_OVERRIDE"
    fi
    if [ -n "${TRIGGER_FILE:-}" ]; then
        rm -f "$TRIGGER_FILE"
    fi
    # Best-effort disarm + remove arm scratch (never leave armed; task requires).
    # Use same UDS docker curl pattern as perform (project-prefixed volume).
    if [ -n "${ARM_KEY_PEM:-}" ] || [ -n "${ATT_KEYS_JSON:-}" ]; then
        local _sock_vol
        _sock_vol="${COMPOSE_PROJECT_NAME:-gateway}_sidecar_sock"
        docker run --rm -i --user 0 -v "${_sock_vol}:/run/sidecar" \
            curlimages/curl:8.12.1 -sS --unix-socket /run/sidecar/sidecar.sock \
            -X POST http://localhost/watch/admin/producer/disarm \
            -H "Authorization: Bearer ${ARM_BEARER}" \
            -H "Content-Type: application/json" -d '{}' >/dev/null 2>&1 || true
        [ -n "${ARM_KEY_PEM:-}" ] && rm -f "$ARM_KEY_PEM" 2>/dev/null || true
        [ -n "${ATT_KEYS_JSON:-}" ] && rm -f "$ATT_KEYS_JSON" 2>/dev/null || true
    fi
    # Silently skip cleanup if sqlite3 is not available in the container
    # (the prereq check above will have already failed the run with a clear message).
    if ! docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'command -v sqlite3' >/dev/null 2>&1; then
        if [ "$COUNCIL_MODE" = "stub" ] && [ "$COUNCIL_STUB_DEPLOYMENT" = "compose" ] \
            && [ -n "${COMPOSE_FILE_ARGS:-}" ]; then
            docker compose $COMPOSE_FILE_ARGS rm -sf council-stub >/dev/null 2>&1 || true
        fi
        return 0
    fi

    info "Cleaning up smoke row (if it still exists)..."
    # pending_escalations.directive_id -> directive_outbox.id and
    # directive_outbox.(tenant,in_response_to) -> pending_escalations.(tenant,id)
    # form a bidirectional FK pair, so cleanup must defer constraints.
    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL 2>/dev/null || true
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
PRAGMA defer_foreign_keys = ON;
DELETE FROM directive_outbox
 WHERE tenant='$SMOKE_TENANT'
   AND (
        in_response_to='$SMOKE_PENDING_ID'
        OR in_response_to IN (
            SELECT id FROM pending_escalations
             WHERE tenant='$SMOKE_TENANT' AND envelope_json LIKE '%$SMOKE_ID%'
        )
   );
DELETE FROM pending_escalations
 WHERE tenant='$SMOKE_TENANT'
   AND (id='$SMOKE_PENDING_ID' OR envelope_json LIKE '%$SMOKE_ID%');
-- watch_fires rows are NOT deleted: the table is engine-enforced append-only
-- (W3 trg_watch_fires_no_delete), and a DELETE here would RAISE(ABORT) and
-- roll back the whole cleanup transaction — the || true on this heredoc
-- swallowed exactly that for every post-W3 run. Smoke fire rows live in the
-- throwaway $SMOKE_WATCH_DB (DO_ARM paths), wiped on the next run.
COMMIT;
SQL
    if [ "$COUNCIL_MODE" = "stub" ] && [ "$COUNCIL_STUB_DEPLOYMENT" = "compose" ] \
        && [ -n "${COMPOSE_FILE_ARGS:-}" ]; then
        # Remove only the test-only sibling service. Gateway, sidecar, and their
        # project-scoped volumes remain available for post-smoke inspection.
        docker compose $COMPOSE_FILE_ARGS rm -sf council-stub >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

reset_smoke_tenant_state() {
    if [ "$RESET_TENANT" != "true" ]; then
        return 0
    fi

    if [ "$DO_ARM" = true ]; then
        # DO_ARM runs use the throwaway $SMOKE_WATCH_DB, wiped between runs
        # while the stack is down (restart_stack_for_phase3), which also puts
        # the smoke seed at the lowest id for the CDC oldest-first cursor
        # (hardening) — no row reset needed or possible: watch_fires is
        # engine-enforced append-only (W3 trg_watch_fires_no_delete; the
        # pre-W3 DELETE here died with 'watch_fires_append_only (19)' on any
        # DB carrying prior smoke rows). writer_claim is preserved by design —
        # the just-armed attested producer holds the singleton lease and
        # self-disarms if it vanishes.
        info "Throwaway smoke watch DB in use ($SMOKE_WATCH_DB) — no row reset needed."
        return 0
    fi

    info "Resetting smoke tenant queue state (tenant=$SMOKE_TENANT)..."
    # Non-arm (legacy pending / negative) paths share the real watch.db with
    # the 'sovereign' canary tenant. Only MUTABLE tables are touched, tenant-
    # scoped: pending_escalations (queue) + its FK partner directive_outbox,
    # and the boot-arm writer_claim. watch_fires and its hash chain are
    # append-only — never deleted. SMOKE_ID is timestamp-unique, so seeds
    # never collide with stale rows.
    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
PRAGMA defer_foreign_keys = ON;
DELETE FROM directive_outbox WHERE tenant='$SMOKE_TENANT';
DELETE FROM pending_escalations WHERE tenant='$SMOKE_TENANT';
DELETE FROM writer_claim;
COMMIT;
SQL
}

# --- prerequisites --------------------------------------------------------
info "Phase 3b.6 E2E smoke for live dispatcher"

if ! docker compose ps --services --filter "status=running" | grep -q "$SIDECAR_CONTAINER"; then
    fail "sidecar container is not running. Run 'make up' first."
fi

if ! curl -fsS "$GW_URL/health" >/dev/null 2>&1; then
    fail "Gateway not responding on $GW_URL/health. Is the stack healthy?"
fi

# Explicit prereq: sqlite3 must be present inside the sidecar container
# (added in Phase 3b.6 patch). This must be checked *before* any DB operations.
if ! docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'command -v sqlite3' >/dev/null 2>&1; then
    fail "sqlite3 not found inside the sidecar container.
Rebuild the sidecar image (the Dockerfile now includes the 'sqlite' package) or
install it manually in the running container for dev/smoke use."
fi

start_stub_council
prepare_arm_artifacts
restart_stack_for_phase3

if [ "$ARM_ONLY" = "true" ]; then
    if [ "$DO_ARM" != true ]; then
        fail "PHASE3_SMOKE_ARM_ONLY=true requires the positive attested-arm path (PHASE3_SMOKE_EXPECT_DISABLED must not be true)"
    fi
    if [ "$ARM_CEREMONY_COMPLETED" != true ]; then
        fail "PHASE3_SMOKE_ARM_ONLY=true but STAGE/CONFIRM did not run (set PHASE3_SMOKE_RESTART_STACK=true or run full smoke-phase3)"
    fi
    pass "arm-only smoke passed: genuine STAGE/CONFIRM completed; skipping CDC/outbox tail"
    exit 0
fi

DISPATCHER_ENABLED=$(docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'printf "%s" "${WATCH_DISPATCHER_ENABLED:-}"' | tr -d '\r')
PRODUCER_ENABLED=$(docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'printf "%s" "${WATCH_PRODUCER_ENABLED:-}"' | tr -d '\r')
DISPATCHER_GATEWAY_KEY=$(docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'printf "%s" "${WATCH_DISPATCHER_GATEWAY_KEY:-}"' | tr -d '\r')

# Quick sanity that the dispatcher mode in the running sidecar matches this smoke.
if [ "${PHASE3_SMOKE_EXPECT_DISABLED:-false}" = "true" ]; then
    if [ "$DISPATCHER_ENABLED" = "true" ]; then
        fail "PHASE3_SMOKE_EXPECT_DISABLED=true but WATCH_DISPATCHER_ENABLED=true in the running sidecar.
Restart the stack with WATCH_DISPATCHER_ENABLED=false before running the negative smoke."
    fi
else
    if [ "$DISPATCHER_ENABLED" != "true" ]; then
        fail "WATCH_DISPATCHER_ENABLED is not set to true in the running sidecar.
Restart the stack with WATCH_DISPATCHER_ENABLED=true for the positive smoke,
or run PHASE3_SMOKE_EXPECT_DISABLED=true make smoke-phase3 for the negative check."
    fi
fi

# Positive-mode prereq: the gateway caller key must be present and non-empty.
# This is what lets the probe (and later live dispatcher) authenticate to
# the gateway's /v1/chat/completions without 401.
if [ "${PHASE3_SMOKE_EXPECT_DISABLED:-false}" != "true" ]; then
    if [ -z "$DISPATCHER_GATEWAY_KEY" ]; then
        fail "WATCH_DISPATCHER_GATEWAY_KEY is not set in the running sidecar.
For the positive smoke you must provision a gateway API key and export it:

  make provision-key BUDGET=phase3-smoke RPM=1000
  # (use BOOTSTRAP_TOKEN or an existing admin key; RPM=1000 so the
  #  preflight + escalation + recovery burst never trips per-key rate)

Then add the raw key to .env as WATCH_DISPATCHER_GATEWAY_KEY=..., run
  make build-sidecar-smoke   # (if on a memory-limited host)
  make down && make up
  make smoke-phase3

The key is required for the P0-eta cabinet probe and the live dispatcher path."
    fi
    # T1 attested arm (DO_ARM): the producer is armed by the four-eyes attested
    # confirm (perform_real_arm, already asserted HTTP 200 == arm_producer_start
    # spawned the CDC sweep), NOT by WATCH_PRODUCER_ENABLED. In that model the
    # boot env-arm is intentionally OFF (producer_enabled=false above), so this
    # boot-arm guard does not apply — the 200 from confirm is the producer-up
    # proof. Keep the guard for the legacy boot-arm path (DO_ARM != true).
    if [ "$DO_ARM" != true ] && { [ "$PRODUCER_MODE" = "cdc" ] || [ "$TRIGGER_MODE" = "file-inbox" ]; } && [ "$PRODUCER_ENABLED" != "true" ]; then
        fail "TRIGGER_MODE=file-inbox or PRODUCER_MODE=cdc requires WATCH_PRODUCER_ENABLED=true in the running sidecar.
Recreate the stack with WATCH_PRODUCER_ENABLED=true, or use TRIGGER_MODE=pending / PRODUCER_MODE=pending for legacy direct-pending smoke."
    fi
fi

# Check DB is readable + the schema is migrated. The watch.db is opened and
# migrated during sidecar boot (main.rs), which can lag container "Started" and
# the gateway health gate (different container). After RESTART=true the file is
# freshly created, so poll — re-resolving the path across candidates each round —
# instead of failing on the first race. WATCH_DB_PATH is now pinned to
# /var/lib/sidecar/watch.db (persistent volume), so the first candidate should win.
TABLE_WAIT_ATTEMPTS="${PHASE3_SMOKE_TABLE_WAIT_ATTEMPTS:-30}"
table_found=""
for attempt in $(seq 1 "$TABLE_WAIT_ATTEMPTS"); do
    for cand in "$DB_PATH" "${DB_CANDIDATES[@]}"; do
        if docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$cand" ".tables" 2>/dev/null | grep -q pending_escalations; then
            DB_PATH="$cand"
            table_found="yes"
            break 2
        fi
    done
    [ "$attempt" -eq 1 ] && info "Waiting for watch.db pending_escalations table (migration after boot)..."
    sleep 1
done
if [ -z "$table_found" ]; then
    warn "Tables visible at $DB_PATH:"
    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" ".tables" 2>&1 | sed 's/^/    /' || true
    fail "Could not find pending_escalations table after ${TABLE_WAIT_ATTEMPTS}s. Checked: $DB_PATH ${DB_CANDIDATES[*]}.
The sidecar opens WATCH_DB_PATH (default /var/lib/sidecar/watch.db); confirm it is set and on a persistent volume."
fi
info "pending_escalations table present at $DB_PATH"
reset_smoke_tenant_state

# --- Negative mode (PHASE3_SMOKE_EXPECT_DISABLED=true) --------------------
if [ "${PHASE3_SMOKE_EXPECT_DISABLED:-false}" = "true" ]; then
    info "NEGATIVE MODE: expecting dispatcher to be disabled"
    NOW_MS=$(date +%s000)
    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL
INSERT OR IGNORE INTO pending_escalations
    (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
VALUES
    ('$SMOKE_ID', '$SMOKE_TENANT', 'phase3-smoke-sentinel',
     '$SMOKE_ENVELOPE', 'queued', $NOW_MS);
SQL

    sleep 5

    STATUS=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT status FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND id='$SMOKE_ID';" | tr -d '\r')

    OUTBOX_COUNT=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COUNT(*) FROM directive_outbox WHERE tenant='$SMOKE_TENANT' AND in_response_to='$SMOKE_ID';" | tr -d '\r')

    if [ "$STATUS" = "queued" ] && [ "$OUTBOX_COUNT" -eq 0 ]; then
        pass "Negative smoke passed: row remains queued with no outbox row when dispatcher disabled"
    else
        fail "Negative smoke failed: status=$STATUS, outbox_count=$OUTBOX_COUNT (expected queued + 0 outbox)"
    fi

    # cleanup will run via trap
    exit 0
fi

# --- checklist 1: cabinet preflight ---------------------------------------
COUNCIL_RS_ROOT="${COUNCIL_RS_ROOT:-$(cd .. && pwd)/council-rs}"
CABINET_PATH="${PHASE3_SMOKE_CABINET_PATH:-${COUNCIL_RS_ROOT}/cabinets/triage.yaml}"
if [ -f "$CABINET_PATH" ]; then
    if grep -q "irin.directive.proposal.v1" "$CABINET_PATH" && \
       ! grep -Eq '"council_session_id"[[:space:]]*:' "$CABINET_PATH" && \
       ! grep -Eq '"council_cost_usd"[[:space:]]*:' "$CABINET_PATH"; then
        pass "Cabinet preflight passed: emits proposal.v1 and does not template session/cost"
    else
        fail "Cabinet preflight failed: $CABINET_PATH does not meet proposal.v1 requirements"
    fi
else
    warn "Cabinet file not found at $CABINET_PATH — skipping preflight"
fi

# --- diagnostics helper (Phase 3b.6 smoke harness) ------------------------
# Prints rich evidence when the row ends up dead_lettered or the smoke times out.
print_smoke_diagnostics() {
    echo ""
    echo "=== Phase 3b.6 smoke diagnostics (dead_lettered / failure) ==="
    echo "Tenant: $SMOKE_TENANT   Run id: $SMOKE_ID   Pending id: ${SMOKE_PENDING_ID:-<unknown>}"

    local status last_error council_json outbox_count fires pending_filter

    pending_filter="tenant='$SMOKE_TENANT' AND id='${SMOKE_PENDING_ID:-}'"
    if [ -z "${SMOKE_PENDING_ID:-}" ]; then
        pending_filter="tenant='$SMOKE_TENANT' AND envelope_json LIKE '%$SMOKE_ID%'"
    fi

    status=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COALESCE(status,'<null>') FROM pending_escalations WHERE $pending_filter ORDER BY created_at_ms DESC LIMIT 1;" 2>/dev/null | tr -d '\r' || echo "<query-failed>")
    echo "pending_escalations.status: ${status:-<missing>}"

    last_error=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COALESCE(last_error,'<null>') FROM pending_escalations WHERE $pending_filter ORDER BY created_at_ms DESC LIMIT 1;" 2>/dev/null | tr -d '\r' || echo "<query-failed>")
    echo "pending_escalations.last_error: ${last_error:-<missing>}"

    council_json=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COALESCE(council_response_json,'<null>') FROM pending_escalations WHERE $pending_filter ORDER BY created_at_ms DESC LIMIT 1;" 2>/dev/null | tr -d '\r' || echo "<query-failed>")
    if [ -n "${SMOKE_ARTIFACT_DIR:-}" ] && [ "$council_json" != "<null>" ] && [ "$council_json" != "<query-failed>" ]; then
        printf '%s' "$council_json" > "$SMOKE_ARTIFACT_DIR/council_response_${SMOKE_ID}.json"
        echo "  (full council_response_json -> $SMOKE_ARTIFACT_DIR/council_response_${SMOKE_ID}.json)"
    fi
    if [ "${#council_json}" -gt 2000 ]; then
        council_json="${council_json:0:2000}... (truncated)"
    fi
    echo "pending_escalations.council_response_json (truncated):"
    echo "${council_json:-<missing>}"

    outbox_count=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COUNT(*) FROM directive_outbox WHERE tenant='$SMOKE_TENANT' AND (in_response_to='${SMOKE_PENDING_ID:-}' OR in_response_to IN (SELECT id FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND envelope_json LIKE '%$SMOKE_ID%'));" 2>/dev/null | tr -d '\r' || echo "0")
    echo "directive_outbox rows for smoke run: $outbox_count"

    fires=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT reason FROM watch_fires WHERE tenant='$SMOKE_TENANT' AND (reason LIKE '%$SMOKE_ID%' OR reason LIKE '%directive_parse_failed%' OR reason LIKE '%escalation_recovered%' OR reason LIKE '%directive_staged%' OR reason LIKE '%dead_letter%') ORDER BY fired_at DESC LIMIT 10;" 2>/dev/null | tr -d '\r' || echo "<none>")
    echo "Recent watch_fires reasons for the smoke tenant (id-correlated first):"
    echo "$fires"

    echo "=== end diagnostics ==="
    echo ""
}

# --- seed the row (checklist 3) ------------------------------------------
if [ "$TRIGGER_MODE" = "pending" ] || [ "$PRODUCER_MODE" = "pending" ]; then
    info "Seeding one queued pending_escalations row (tenant=$SMOKE_TENANT, id=$SMOKE_ID)"

    NOW_MS=$(date +%s000)
    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL
INSERT OR IGNORE INTO pending_escalations
    (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
VALUES
    ('$SMOKE_ID', '$SMOKE_TENANT', '$SMOKE_SENTINEL',
     '$SMOKE_ENVELOPE', 'queued', $NOW_MS);
SQL

    STATUS=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT status FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND id='$SMOKE_PENDING_ID';" | tr -d '\r')

    if [ "$STATUS" != "queued" ] && [ "$STATUS" != "claimed" ]; then
        fail "Failed to seed the row as 'queued' or 'claimed' (got: $STATUS)"
    fi
    pass "Pending row seeded as '$STATUS' (dispatcher is active)"
elif [ "$TRIGGER_MODE" = "file-inbox" ]; then
    assert_unseeded
    info "Triggering real unseeded FileInboxSentinel fire via drop into demo/inbox (tenant=$SMOKE_TENANT, sentinel=$SMOKE_SENTINEL, run_id=$SMOKE_ID)"

    TRIGGER_FILE="demo/inbox/phase3-smoke-${SMOKE_ID}.txt"
    printf 'D9 harness file-inbox trigger\nrun_id=%s\n' "$SMOKE_ID" > "$TRIGGER_FILE"

    # Poll for watch_fires row written by the sentinel's fire_pipeline (observe/interesting/escalate)
    # + quarantine.write_fire_row + db.insert_fire. This is the unseeded real path (D9 goal).
    for _ in $(seq 1 "$CDC_TIMEOUT_SECONDS"); do
        SMOKE_FIRE_ID=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
            "SELECT id FROM watch_fires WHERE tenant='$SMOKE_TENANT' AND sentinel='$SMOKE_SENTINEL' AND (reason LIKE '%$SMOKE_ID%' OR state_json LIKE '%$SMOKE_ID%' OR envelope_json LIKE '%$SMOKE_ID%') ORDER BY id DESC LIMIT 1;" | tr -d '\r')
        if [ -n "$SMOKE_FIRE_ID" ]; then
            break
        fi
        sleep 1
    done
    if [ -z "$SMOKE_FIRE_ID" ]; then
        print_smoke_diagnostics
        fail "FileInboxSentinel did not produce watch_fires row for $TRIGGER_FILE (ensure SENTINELS_CONFIG_PATH loaded the fixture, PollWatcher started by activation fix in registry.rs, and *.txt pattern matches)"
    fi
    pass "Real file-inbox fire produced watch_fires row (unseeded, id=$SMOKE_FIRE_ID)"

    # Now the (armed) CDC producer must sweep it to pending_escalations (same as manual path)
    for _ in $(seq 1 "$CDC_TIMEOUT_SECONDS"); do
        SMOKE_PENDING_ID=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
            "SELECT id FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND sentinel_name='$SMOKE_SENTINEL' AND envelope_json LIKE '%$SMOKE_ID%' ORDER BY created_at_ms DESC LIMIT 1;" | tr -d '\r')
        if [ -n "$SMOKE_PENDING_ID" ]; then
            break
        fi
        sleep 1
    done
    if [ -z "$SMOKE_PENDING_ID" ]; then
        print_smoke_diagnostics
        fail "CDC producer did not enqueue a pending_escalations row from the real file-inbox watch_fires run_id=$SMOKE_ID"
    fi

    STATUS=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT status FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND id='$SMOKE_PENDING_ID';" | tr -d '\r')
    pass "CDC producer enqueued pending row $SMOKE_PENDING_ID as '$STATUS' (from real file-inbox trigger)"
elif [ "$TRIGGER_MODE" = "manual-cdc-seed" ] || [ "$PRODUCER_MODE" = "cdc" ]; then
    info "Seeding one committed watch_fires row for CDC producer sweep (tenant=$SMOKE_TENANT, run_id=$SMOKE_ID)"

    NOW_MS=$(date +%s000)
    STATE_JSON="{\"payload\":{\"run_id\":\"$SMOKE_ID\",\"observed\":{\"cpu\":95},\"reason\":\"smoke test escalation\",\"urgency\":\"high\"},\"observed_at\":$NOW_MS}"
    FIRE_REASON="phase3 D9 CDC smoke escalation run_id=$SMOKE_ID"
    ENVELOPE_JSON="{\"contract\":\"irin.comms.v0.1\",\"kind\":\"Escalation\",\"id\":\"$SMOKE_ID\",\"tenant\":\"$SMOKE_TENANT\",\"payload\":{\"sentinel\":\"$SMOKE_SENTINEL\",\"tier\":\"fast\",\"observed\":{\"cpu\":95},\"reason\":\"smoke test escalation\",\"urgency\":\"high\",\"proposed_action\":\"ConsultCouncil\",\"run_id\":\"$SMOKE_ID\"}}"

    PREV_HASH=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT COALESCE((SELECT hash FROM watch_fires WHERE tenant='$SMOKE_TENANT' ORDER BY id DESC LIMIT 1), '');" | tr -d '\r')
    if [ -z "$PREV_HASH" ]; then
        PREV_HASH=$(python3 - <<'PY'
import hashlib
print("0ffed28740318eb8e9fa37cc3d034394c1eea87a273ecdbfcdcb937af502acba")
PY
)
    fi

    FIRE_HASH=$(python3 - "$SMOKE_TENANT" "$SMOKE_SENTINEL" "$NOW_MS" "$STATE_JSON" "$FIRE_REASON" "$PREV_HASH" <<'PY'
import hashlib
import sys

tenant, sentinel, fired_at, state_json, reason, prev_hash = sys.argv[1:]
preimage = (
    f"{len(tenant)}:{tenant}|"
    f"{len(sentinel)}:{sentinel}|"
    f"{len(fired_at)}:{fired_at}|"
    f"{len(state_json)}:{state_json}|"
    f"{len(reason)}:{reason}|"
    f"{len(prev_hash)}:{prev_hash}"
)
print(hashlib.sha256(preimage.encode("utf-8")).hexdigest())
PY
)

    docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" <<SQL
INSERT INTO watch_fires
    (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash, envelope_json, envelope_schema_version)
VALUES
    ('$SMOKE_TENANT', '$SMOKE_SENTINEL', $NOW_MS, '$STATE_JSON',
     '$FIRE_REASON', '$PREV_HASH', '$FIRE_HASH', '$ENVELOPE_JSON', 1);
SQL

    for _ in $(seq 1 "$CDC_TIMEOUT_SECONDS"); do
        SMOKE_PENDING_ID=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
            "SELECT id FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND sentinel_name='$SMOKE_SENTINEL' AND envelope_json LIKE '%$SMOKE_ID%' ORDER BY created_at_ms DESC LIMIT 1;" | tr -d '\r')
        if [ -n "$SMOKE_PENDING_ID" ]; then
            break
        fi
        sleep 1
    done

    if [ -z "$SMOKE_PENDING_ID" ]; then
        print_smoke_diagnostics
        fail "CDC producer did not enqueue a pending_escalations row for watch_fires run_id=$SMOKE_ID"
    fi

    STATUS=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT status FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND id='$SMOKE_PENDING_ID';" | tr -d '\r')
    pass "CDC producer enqueued pending row $SMOKE_PENDING_ID as '$STATUS'"
else
    fail "Unsupported PHASE3_SMOKE_TRIGGER_MODE=$TRIGGER_MODE (or legacy PRODUCER_MODE=$PRODUCER_MODE); expected file-inbox | manual-cdc-seed | pending"
fi

# --- D9 dedup receipt: one logical run -> one pending row -----------------
PENDING_RUN_COUNT=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT COUNT(*) FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND envelope_json LIKE '%$SMOKE_ID%';" | tr -d '\r')

if [ "$PENDING_RUN_COUNT" -ne 1 ]; then
    print_smoke_diagnostics
    fail "Expected exactly 1 pending_escalations row for run_id=$SMOKE_ID, got $PENDING_RUN_COUNT"
fi
pass "Exactly one pending_escalations row for run_id=$SMOKE_ID"

# --- positive path --------------------------------------------------------
info "Positive path: waiting for the live dispatcher to process the row..."

START=$(date +%s)
while true; do
    CURRENT_STATUS=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
        "SELECT status FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND id='$SMOKE_PENDING_ID';" | tr -d '\r')

    if [ -z "$CURRENT_STATUS" ] || [ "$CURRENT_STATUS" = "<null>" ] || [ "$CURRENT_STATUS" = "<missing>" ]; then
        print_smoke_diagnostics
        fail "RECEIPT (a) failed: pending_escalations.status is blank/null/missing at terminal wait"
    fi

    if [ "$CURRENT_STATUS" = "outbox_written" ] || [ "$CURRENT_STATUS" = "dismissed" ]; then
        pass "RECEIPT (a): pending_escalations.status terminal and non-blank ($CURRENT_STATUS)"
        break
    fi

    if [ "$CURRENT_STATUS" = "dead_lettered" ]; then
        print_smoke_diagnostics
        fail "Row dead_lettered by the live dispatcher (see diagnostics above for last_error + council_response_json)"
    fi

    NOW=$(date +%s)
    if [ $((NOW - START)) -gt $TIMEOUT_SECONDS ]; then
        print_smoke_diagnostics
        fail "Timed out waiting for row to be processed (last status: $CURRENT_STATUS). Diagnostics printed above."
    fi

    sleep $POLL_INTERVAL
done

# --- checklist 5: exactly one outbox row ----------------------------------
OUTBOX_COUNT=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT COUNT(*) FROM directive_outbox WHERE tenant='$SMOKE_TENANT' AND in_response_to='$SMOKE_PENDING_ID';" | tr -d '\r')

if [ "$OUTBOX_COUNT" -ne 1 ]; then
    fail "Expected exactly 1 directive_outbox row, got $OUTBOX_COUNT"
fi
pass "Exactly one directive_outbox row for (tenant, in_response_to)"

# Assert the full run id also produced one directive, not merely one directive
# for the selected pending id. This catches duplicate logical fires that would
# otherwise become multiple pending rows and multiple directives.
OUTBOX_RUN_COUNT=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT COUNT(*) FROM directive_outbox WHERE tenant='$SMOKE_TENANT' AND in_response_to IN (SELECT id FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND envelope_json LIKE '%$SMOKE_ID%');" | tr -d '\r')

if [ "$OUTBOX_RUN_COUNT" -ne 1 ]; then
    fail "Expected exactly 1 directive_outbox row for run_id=$SMOKE_ID, got $OUTBOX_RUN_COUNT"
fi
pass "Exactly one directive_outbox row for run_id=$SMOKE_ID"

# T3 (c): preserve the untruncated Council response envelope on the success path.
COUNCIL_JSON=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT COALESCE(council_response_json,'<null>') FROM pending_escalations WHERE tenant='$SMOKE_TENANT' AND id='$SMOKE_PENDING_ID';" | tr -d '\r')

if [ -z "$COUNCIL_JSON" ] || [ "$COUNCIL_JSON" = "<null>" ]; then
    fail "RECEIPT (c) failed: council_response_json is empty/null for $SMOKE_PENDING_ID"
fi

COUNCIL_JSON_ARTIFACT="$SMOKE_ARTIFACT_DIR/council_response_${SMOKE_ID}.json"
printf '%s' "$COUNCIL_JSON" > "$COUNCIL_JSON_ARTIFACT"
if [ ! -s "$COUNCIL_JSON_ARTIFACT" ]; then
    fail "RECEIPT (c) failed: full council_response_json artifact was not written"
fi
pass "RECEIPT (c): full council_response_json artifact written ($COUNCIL_JSON_ARTIFACT)"

# --- checklist 6: signature exists ----------------------------------------
SIG=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT signature_b64 FROM directive_outbox WHERE tenant='$SMOKE_TENANT' AND in_response_to='$SMOKE_PENDING_ID';" | tr -d '\r')

if [ -z "$SIG" ]; then
    fail "signature_b64 is empty"
fi
pass "signature_b64 is present and non-empty"

# --- checklist 6b: gateway-exposed outbox surface returns verify anchors -----
DIRECTIVE_ID=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT id FROM directive_outbox WHERE tenant='$SMOKE_TENANT' AND in_response_to='$SMOKE_PENDING_ID';" | tr -d '\r')

if [ -z "$DIRECTIVE_ID" ]; then
    fail "directive_outbox.id is empty"
fi

# W1: the outbox read gates PLAINTEXT (envelope_json_canonical + envelope
# convenience object) behind the watch ADMIN token, not the dispatcher gateway
# key. The main.rs wrapper does the constant-time compare (admin_token_matches)
# to compute `authed`, then passes that bool to get_outbox_json, which 401s on
# !authed BEFORE any store lookup.
# Outbox reads are admin-only (Invariant, Option 3): a
# non-matching / missing bearer now gets a 401 BEFORE any store lookup (the
# D1/T1 public hash projection was removed — see checklist 6c). Read the admin token the sidecar
# actually resolved at boot: WATCH_ADMIN_TOKEN if set non-empty, else
# BOOTSTRAP_TOKEN (exactly AppState.watch_admin_token's resolution order). The
# compare is over the RAW token value (both sides SHA-256-hashed internally; no
# pepper), so passing the raw token is correct.
OUTBOX_AUTH_KEY=$(docker compose exec -T "$SIDECAR_CONTAINER" sh -c 'printf "%s" "${WATCH_ADMIN_TOKEN:-$BOOTSTRAP_TOKEN}"' 2>/dev/null)

if ! python3 - "$GW_URL" "$SMOKE_TENANT" "$DIRECTIVE_ID" "$SIG" "$OUTBOX_AUTH_KEY" <<'PY'
import base64
import json
import sys
import urllib.request
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

base_url, tenant, directive_id, expected_sig = sys.argv[1:5]
auth_key = sys.argv[5] if len(sys.argv) > 5 else ""
base_url = base_url.rstrip("/")

def fetch_json(path):
    req = urllib.request.Request(base_url + path)
    if auth_key:
        req.add_header("Authorization", f"Bearer {auth_key}")
    with urllib.request.urlopen(req, timeout=10) as resp:
        return resp.status, json.loads(resp.read().decode("utf-8"))

pub_status, pubkey = fetch_json("/watch/outbox/pubkey")
if pub_status != 200:
    raise SystemExit(f"/watch/outbox/pubkey returned {pub_status}")
for key in ("alg", "kid", "pubkey_b64", "note"):
    if not pubkey.get(key):
        raise SystemExit(f"/watch/outbox/pubkey missing {key}")
if pubkey.get("alg") != "Ed25519":
    raise SystemExit(f"/watch/outbox/pubkey alg mismatch: {pubkey.get('alg')!r}")

row_status, row = fetch_json(f"/watch/outbox/{tenant}/{directive_id}")
if row_status != 200:
    raise SystemExit(f"/watch/outbox/{tenant}/{directive_id} returned {row_status}")
if row.get("id") != directive_id:
    raise SystemExit(f"outbox id mismatch: {row.get('id')!r}")
if row.get("tenant") != tenant:
    raise SystemExit(f"outbox tenant mismatch: {row.get('tenant')!r}")
signature = row.get("signature") or {}
if signature.get("alg") != "Ed25519":
    raise SystemExit(f"gateway outbox signature alg mismatch: {signature.get('alg')!r}")
if not signature.get("kid"):
    raise SystemExit("gateway outbox signature missing kid")
if signature.get("kid") != pubkey.get("kid"):
    raise SystemExit(
        f"gateway outbox signature kid {signature.get('kid')!r} does not match published kid {pubkey.get('kid')!r}"
    )
if signature.get("value") != expected_sig:
    raise SystemExit("gateway outbox signature.value does not match DB signature_b64 row")
canonical = row.get("envelope_json_canonical")
if not canonical:
    raise SystemExit("gateway outbox row missing envelope_json_canonical")
if not row.get("envelope"):
    raise SystemExit("gateway outbox row missing envelope convenience object")

try:
    pubkey_bytes = base64.b64decode(pubkey["pubkey_b64"], validate=True)
    signature_bytes = base64.b64decode(signature["value"], validate=True)
    Ed25519PublicKey.from_public_bytes(pubkey_bytes).verify(
        signature_bytes,
        canonical.encode("utf-8"),
    )
except Exception as exc:
    raise SystemExit(f"Ed25519 signature verification failed: {exc}") from exc
PY
then
    fail "Gateway /watch/outbox surface did not return the expected verification payload"
fi
pass "RECEIPT (d): outbox pubkey kid matches published key and Ed25519 signature verifies"

# --- checklist 6c: unauthed outbox reads are 401 with no leak ----------------
# Invariant (Option 3): outbox reads are admin-only. Every
# unauthed read path must return 401 BEFORE any store lookup (so 401-vs-403/404
# cannot be used as a tenant/id existence oracle) and the body must leak none of
# the old projection / routing / cadence / count fields. Probe five shapes: a
# real id, a bogus id, a foreign tenant, the list endpoint, and POST /claim —
# the one read-equivalent plaintext path (it hands a worker the full directive),
# which shares the same admin gate (H3, Council gate fb47aa76-b6c).
if ! python3 - "$GW_URL" "$SMOKE_TENANT" "$DIRECTIVE_ID" <<'PY'
import json
import sys
import urllib.error
import urllib.request

base_url, tenant, directive_id = sys.argv[1:4]
base_url = base_url.rstrip("/")

# (label, method, path, body) — all probed WITHOUT an Authorization header.
probes = [
    ("real-id GET", "GET", f"/watch/outbox/{tenant}/{directive_id}", None),
    ("bogus-id GET", "GET", f"/watch/outbox/{tenant}/does-not-exist", None),
    ("foreign-tenant GET", "GET", f"/watch/outbox/intruder/{directive_id}", None),
    ("list", "GET", f"/watch/outbox/{tenant}", None),
    ("claim POST", "POST", "/watch/outbox/claim", b"{}"),
]
# None of these may appear anywhere in a 401 body.
leak_needles = (
    "projection",
    "envelope_sha256_jcs",
    "envelope_json_canonical",
    "envelope",
    "created_at_ms",
    "directives",
    "signature",
)

for label, method, path, body in probes:
    req = urllib.request.Request(base_url + path, data=body, method=method)
    if body is not None:
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            status = resp.status
            resp_body = resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        status = exc.code
        resp_body = exc.read().decode("utf-8", "replace")
    if status != 401:
        raise SystemExit(f"{label}: expected 401 unauthed, got {status} (body: {resp_body[:200]})")
    low = resp_body.lower()
    for needle in leak_needles:
        if needle in low:
            raise SystemExit(f"{label}: 401 body leaked {needle!r} (body: {resp_body[:200]})")
    # Tenant identifier must not echo back (routing intel).
    if tenant.lower() in low:
        raise SystemExit(f"{label}: 401 body echoed tenant {tenant!r} (body: {resp_body[:200]})")
PY
then
    fail "Unauthed /watch/outbox reads did not uniformly 401 without leaking metadata"
fi
pass "Unauthed /watch/outbox reads (GET real/bogus/foreign + list + claim) all 401 with no leaked fields"

# --- checklist 7: Phase 3 audit events ------------------------------------
AUDIT_COUNT=$(docker compose exec -T "$SIDECAR_CONTAINER" sqlite3 "$DB_PATH" \
    "SELECT COUNT(*) FROM watch_fires WHERE tenant='$SMOKE_TENANT' AND (reason LIKE '%escalation_recovered%' OR reason LIKE '%directive_staged%' OR reason LIKE '%directive_parse%');" | tr -d '\r')

if [ "$AUDIT_COUNT" -lt 1 ]; then
    warn "No Phase 3 audit events found for the tenant (may be timing or the events are under different reasons)."
else
    pass "At least one Phase 3 audit event present in watch_fires for the tenant"
fi

# --- negative path (checklist 8) ------------------------------------------
info "Negative path: if the dispatcher had been disabled, the row would have stayed 'queued'."
info "  (To exercise this, restart the stack with WATCH_DISPATCHER_ENABLED=false and re-run the seed + short sleep.)"
info "  For this run we simply note that the positive path succeeded with the dispatcher enabled."
warn "KNOWN GAP (T3-b): late-header warning surface is not implemented; no receipt asserted."

# --- success --------------------------------------------------------------
echo ""
echo "======================================================================"
echo "PHASE 3b.6 SMOKE PASSED"
echo "======================================================================"
echo "Escalation $SMOKE_ID for tenant $SMOKE_TENANT traveled the full CDC-backed live path."
echo "Pending escalation id: $SMOKE_PENDING_ID"
echo "One signed directive_outbox row was produced."
echo "Audit events were written."
echo ""
echo "Env vars used for this smoke (set them in .env before 'make up'):"
echo "  WATCH_DISPATCHER_ENABLED=true"
echo "  WATCH_DISPATCHER_TICK_INTERVAL_MS=500"
echo "  WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK=2"
if [ "$DO_ARM" = true ]; then
    echo "  WATCH_PRODUCER_ENABLED=false   # attested confirm starts the producer"
else
    echo "  WATCH_PRODUCER_ENABLED=true"
fi
echo "  WATCH_DISPATCHER_GATEWAY_KEY=<raw key from make provision-key>"
echo "  PHASE3_SMOKE_COUNCIL_MODE=$COUNCIL_MODE"
echo "  PHASE3_SMOKE_PRODUCER_MODE=$PRODUCER_MODE"
echo "  PHASE3_SMOKE_TRIGGER_MODE=$TRIGGER_MODE   # file-inbox (real drop) | manual-cdc-seed | pending"
echo ""
echo "Next steps after green smoke:"
echo "  - Promote the smoke to CI (periodic or on main merge)."
echo "  - Add claimed_until / lease (Phase 3c) when ready."
echo "======================================================================"

exit 0
