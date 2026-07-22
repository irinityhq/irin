#!/usr/bin/env bash
# ==========================================================================
# demo.sh — the public self-demo: fresh clone -> signed mock directive.
#
#   make verify      # bring the isolated stack up and prove the closed loop
#   make verify-down # tear the isolated stack + all its state down
#
# Proves the whole signal chain end to end with NO real provider keys and NO
# Touch-ID hardware arm:
#
#   watch_fires (seeded) -> CDC producer -> pending_escalation ->
#   dispatcher claims -> gateway routes `model: council-triage` ->
#   a local no-spend Council STUB returns a canned directive ($0) ->
#   sidecar signs it (Ed25519 over RFC 8785 JCS) into directive_outbox
#
# and prints the signed row plus the elapsed time.
#
# Everything is isolated so it can run on a machine already hosting a live
# `make up` / canary stack WITHOUT TOUCHING IT:
#   - own compose project name (COMPOSE_PROJECT_NAME=irin-demo) => own
#     containers + own named volumes (irin-demo_sidecar_data / _sock)
#   - own host port (DEMO_GW_PORT, default 28080; never 18080)
#   - own no-spend Council stub service on the same Compose network
#   - own EPHEMERAL ledger key under .demo-state/ (never ~/.irin/…)
#   - own auto-generated dev secrets in .env.demo (NEVER writes/reads .env)
#
# The "arm" here is a SOFTWARE P-256 key generated on the fly (the same
# mechanism CI uses) — it is a mock of the hardware-attested arm, not a real
# Secure-Enclave / Touch-ID arm. The armed/Touch-ID path is a documented Day-2
# operator setup (docs/runbooks/arming-authorization.md), not this fresh-clone
# path.
# ==========================================================================
set -euo pipefail

# --- locate the repo root (this script lives in <root>/test) --------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$ROOT_DIR/.." && pwd)"
cd "$ROOT_DIR"
source "$SCRIPT_DIR/lib/worktree_docker_context.sh"

# --- config (all overridable via env) -------------------------------------
PROJECT="${COMPOSE_PROJECT_NAME:-irin-demo}"
# Hard refusal: the demo tears down its project with `down -v`. If PROJECT
# resolves to the live stack's name (e.g. COMPOSE_PROJECT_NAME=gateway exported
# in the shell), that teardown would destroy the live/canary containers AND
# volumes. No escape hatch — pick a different name.
case "$PROJECT" in
    gateway|gateway_*|gateway-*)
        echo "FATAL: refusing to run the verify stack under compose project '$PROJECT' —" >&2
        echo "       that name belongs to a live/canary stack and the verify stack's" >&2
        echo "       'down -v' teardown would destroy it. Unset COMPOSE_PROJECT_NAME" >&2
        echo "       or choose a non-'gateway*' name." >&2
        exit 1
        ;;
esac
DEMO_GW_PORT="${DEMO_GW_PORT:-28080}"
GW_URL="http://localhost:${DEMO_GW_PORT}"
DEMO_TENANT="irin-demo"
DEMO_SENTINEL="demo-sentinel"
STATE_DIR="$ROOT_DIR/.demo-state"
ENV_FILE="$ROOT_DIR/.env.demo"
CURL_IMG="curlimages/curl:8.12.1"
POLL_TIMEOUT="${DEMO_POLL_TIMEOUT:-90}"
# Overridable so a fresh-build proof can use its own tags without touching the
# live stack's gateway-sidecar/gateway-gateway images (shared-daemon safety).
SIDECAR_IMAGE_NAME="${DEMO_SIDECAR_IMAGE_NAME:-gateway-sidecar}"
GATEWAY_IMAGE_NAME="${DEMO_GATEWAY_IMAGE_NAME:-gateway-gateway}"
# Optional published images. Default verify path does NOT pull them
# (DEMO_PULL=0): Hub tags can lag the source tip, and a stranger must not
# silently run pre-tip binaries against a newer tree. Prefer local images
# or DEMO_ALLOW_BUILD=1 for exact-source proof. DEMO_PULL=1 is opt-in only.
PULL_SIDECAR_IMAGE="${DEMO_PULL_SIDECAR_IMAGE:-irinity/irin-sidecar:v0.3.1}"
PULL_GATEWAY_IMAGE="${DEMO_PULL_GATEWAY_IMAGE:-irinity/irin-gateway:v0.3.1}"

# --- colors / logging -----------------------------------------------------
if [ -t 1 ]; then G='\033[0;32m'; R='\033[0;31m'; Y='\033[0;33m'; B='\033[1m'; N='\033[0m'; else G=''; R=''; Y=''; B=''; N=''; fi
step() { echo -e "${B}==>${N} $1"; }
ok()   { echo -e "${G}ok${N}   $1"; }
warn() { echo -e "${Y}warn${N} $1"; }
die()  { echo -e "${R}FAIL${N} $1" >&2; exit 1; }

# --- docker credential helper PATH shim (macOS Docker Desktop) ------------
DOCKER_PATH="$PATH"
if ! command -v docker-credential-desktop >/dev/null 2>&1 \
   && [ -x /Applications/Docker.app/Contents/Resources/bin/docker-credential-desktop ]; then
    DOCKER_PATH="/Applications/Docker.app/Contents/Resources/bin:$PATH"
fi

COMPOSE_FILES=(-f docker-compose.yml -f docker-compose.demo.yml)
# All up/config/exec/build invocations go through here: fixed project, fixed
# file set, and the env-file once it exists. NOTE: written for macOS stock bash
# 3.2 — no empty-array expansion under `set -u` (it aborts there), so we branch
# on the env-file instead of splatting a maybe-empty array.
dc() {
    if [ -f "$ENV_FILE" ]; then
        PATH="$DOCKER_PATH" docker compose -p "$PROJECT" "${COMPOSE_FILES[@]}" --env-file "$ENV_FILE" "$@"
    else
        PATH="$DOCKER_PATH" docker compose -p "$PROJECT" "${COMPOSE_FILES[@]}" "$@"
    fi
}
# Teardown uses the BASE compose only: the demo overlay references ${DEMO_*}
# vars that may be unset at teardown time (compose hard-errors on the empty
# volume spec). Volumes are project-scoped and named identically in the base
# file, so `-p irin-demo -f docker-compose.yml down -v` removes exactly the
# demo project's own containers + volumes and nothing else.
dc_teardown() {
    PATH="$DOCKER_PATH" docker compose -p "$PROJECT" -f docker-compose.yml down -v --remove-orphans "$@"
}
SIDECAR_SOCK_VOL="${PROJECT}_sidecar_sock"
# curl the sidecar's management Unix socket from a throwaway container that
# mounts the (project-scoped) socket volume — the socket is inside a named
# volume, unreachable from the host FS.
curl_uds() {
    PATH="$DOCKER_PATH" docker run --rm -i --user 0 -v "${SIDECAR_SOCK_VOL}:/run/sidecar" \
        "$CURL_IMG" -sS --unix-socket /run/sidecar/sidecar.sock "$@"
}

# ==========================================================================
# down — tear the demo stack + all its state down (project-scoped only)
# ==========================================================================
demo_down() {
    step "Tearing down the verify stack (project '$PROJECT' only — the live stack is untouched)"
    # -v removes the demo project's named volumes (sidecar_data/_sock); other
    # projects' volumes are never named here, so they are never touched.
    dc_teardown 2>/dev/null || true
    rm -rf "$STATE_DIR"
    rm -f "$ENV_FILE"
    ok "verify stack down; .demo-state/ and .env.demo removed"
}

if [ "${1:-up}" = "down" ]; then
    demo_down
    exit 0
fi

# ==========================================================================
# up — the full demo
# ==========================================================================
cleanup_on_fail() {
    local rc=$?
    if [ -n "${BUILD_CONTEXT:-}" ] && [ -d "$BUILD_CONTEXT" ]; then rm -rf "$BUILD_CONTEXT"; fi
    if [ -n "${BUILD_CONTEXT_COMPOSE:-}" ] && [ -f "$BUILD_CONTEXT_COMPOSE" ]; then rm -f "$BUILD_CONTEXT_COMPOSE"; fi
    if [ "$rc" -ne 0 ]; then
        warn "verify failed (exit $rc) — leaving the stack up for inspection."
        warn "logs:  docker compose -p $PROJECT ${COMPOSE_FILES[*]} logs --tail=80 sidecar"
        warn "clean: make verify-down"
    fi
}
trap cleanup_on_fail EXIT

# --- preflight ------------------------------------------------------------
step "Preflight"
for tool in docker openssl python3 jq; do
    command -v "$tool" >/dev/null 2>&1 || die "'$tool' is required but not found on PATH."
done
PATH="$DOCKER_PATH" docker compose version >/dev/null 2>&1 || die "'docker compose' (v2+) is required."

# Clean any PRIOR demo run FIRST — a previous failed run leaves its stack up
# for inspection. Demo project ONLY; this is
# also why the demo never accumulates append-only watch_fires rows.
dc_teardown >/dev/null 2>&1 || true

# Now the ports must be free of anything ELSE (a real/canary stack, another app).
port_is_taken() ( python3 - "$1" <<'PY'
import socket,sys
s=socket.socket()
try: s.bind(("127.0.0.1",int(sys.argv[1]))); s.close()
except OSError: sys.exit(0)   # taken
sys.exit(1)                   # free
PY
)
if port_is_taken "$DEMO_GW_PORT"; then die "gateway demo port $DEMO_GW_PORT is in use (not by a prior demo). Set DEMO_GW_PORT to a free port."; fi
ok "docker/openssl/python3/jq present; prior verify stack (if any) cleaned; port $DEMO_GW_PORT free"

# --- generate dev secrets + ephemeral keys (never touch .env / ~/.irin) --
step "Generating dev-safe secrets + ephemeral keys under .demo-state/"
mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR"
AUTH_PEPPER="$(openssl rand -hex 32)"
BOOTSTRAP_TOKEN="$(openssl rand -hex 32)"
COUNCIL_GATEWAY_TOKEN="$(openssl rand -hex 32)"

LEDGER_KEY="$STATE_DIR/ledger_key.pem"
openssl rand -out "$LEDGER_KEY" 32
chmod 600 "$LEDGER_KEY"

# Synthetic (software) arm key + attest registry — the mock of the hardware arm.
ARM_KEY="$STATE_DIR/arm-key.pem"
ATTEST_KEYS="$STATE_DIR/attest_keys.json"
openssl ecparam -name prime256v1 -genkey -noout -out "$ARM_KEY" >/dev/null 2>&1 \
    || die "openssl ecparam (arm key) failed."
chmod 600 "$ARM_KEY"
ARM_PUB_B64="$(openssl ec -in "$ARM_KEY" -pubout -conv_form compressed -outform DER 2>/dev/null | tail -c 33 | base64 | tr -d '\n')"
[ -n "$ARM_PUB_B64" ] || die "failed to derive compressed P-256 public key for the attest registry."
cat > "$ATTEST_KEYS" <<JSON
[{"credential_id":"demo-arm-cred","credential_type":"se-p256","public_key":"$ARM_PUB_B64","label":"demo","enrolled_at":"2026-01-01T00:00:00Z"}]
JSON
chmod 600 "$ATTEST_KEYS"
ok "ledger key + software arm key generated (ephemeral, .demo-state/, 0600)"

# --- resolve images (reuse if present; NEVER build unattended) ------------
# Safety: the sidecar image is a slow (~tens of min), RAM-heavy (~8GB Docker)
# cargo compile. On a shared daemon (e.g. the canary host, 7.75GiB) an
# unattended build risks OOM-starving a live stack. So the demo REUSES a local
# image and otherwise STOPS and reports — it does not build behind your back.
# Building is opt-in (DEMO_ALLOW_BUILD=1) for a machine you own / a clean box.
step "Resolving container images"
IMAGE_BUILT=false
IMG_START=$(date +%s)
if PATH="$DOCKER_PATH" docker image inspect "$SIDECAR_IMAGE_NAME" >/dev/null 2>&1 \
   && PATH="$DOCKER_PATH" docker image inspect "$GATEWAY_IMAGE_NAME" >/dev/null 2>&1; then
    ok "reusing existing local images ($SIDECAR_IMAGE_NAME + $GATEWAY_IMAGE_NAME) — no build"
elif [ "${DEMO_ALLOW_BUILD:-0}" = "1" ]; then
    # The sidecar Dockerfile uses BuildKit additional contexts; the classic
    # builder cannot build it. Docker Desktop and docker-ce ship buildx, but
    # Debian/Ubuntu `docker.io` installs need the `docker-buildx` package.
    PATH="$DOCKER_PATH" docker buildx version >/dev/null 2>&1 \
        || die "building requires BuildKit (docker buildx). Install it first — Debian/Ubuntu: 'apt install docker-buildx'; other platforms ship it with Docker — then re-run."
    warn "image(s) missing — building (DEMO_ALLOW_BUILD=1; slow + RAM-heavy, wants ~8GB Docker)"
    BUILD_CONTEXT=""
    BUILD_CONTEXT_COMPOSE=""
    if [ -f "$REPO_ROOT/.git" ] && grep -q '^gitdir:' "$REPO_ROOT/.git" 2>/dev/null; then
        BUILD_CONTEXT="$(stage_worktree_docker_context "$REPO_ROOT")" \
            || die "failed to stage linked worktree as a provenance-correct Docker context."
        BUILD_CONTEXT_COMPOSE="$(mktemp -t irin-demo-compose-ctx.XXXXXX.yml)"
        cat > "$BUILD_CONTEXT_COMPOSE" <<EOF
services:
  sidecar:
    build:
      context: ${BUILD_CONTEXT}
EOF
        ok "linked worktree staged with a real Git directory for exact-source provenance"
    fi
    BUILD_RC=0
    if [ -n "$BUILD_CONTEXT_COMPOSE" ]; then
        DEMO_GW_PORT="$DEMO_GW_PORT" DEMO_LEDGER_KEY="$LEDGER_KEY" DEMO_ATTEST_KEYS="$ATTEST_KEYS" \
        DEMO_SIDECAR_IMAGE="$SIDECAR_IMAGE_NAME" DEMO_GATEWAY_IMAGE="$GATEWAY_IMAGE_NAME" \
            PATH="$DOCKER_PATH" docker compose -p "$PROJECT" "${COMPOSE_FILES[@]}" -f "$BUILD_CONTEXT_COMPOSE" build \
            --build-arg CARGO_PROFILE_RELEASE_LTO=false \
            --build-arg CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            || BUILD_RC=$?
    else
        DEMO_GW_PORT="$DEMO_GW_PORT" DEMO_LEDGER_KEY="$LEDGER_KEY" DEMO_ATTEST_KEYS="$ATTEST_KEYS" \
        DEMO_SIDECAR_IMAGE="$SIDECAR_IMAGE_NAME" DEMO_GATEWAY_IMAGE="$GATEWAY_IMAGE_NAME" \
            dc build \
            --build-arg CARGO_PROFILE_RELEASE_LTO=false \
            --build-arg CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            || BUILD_RC=$?
    fi
    if [ -n "$BUILD_CONTEXT" ]; then rm -rf "$BUILD_CONTEXT"; fi
    if [ -n "$BUILD_CONTEXT_COMPOSE" ]; then rm -f "$BUILD_CONTEXT_COMPOSE"; fi
    BUILD_CONTEXT=""
    BUILD_CONTEXT_COMPOSE=""
    [ "$BUILD_RC" -eq 0 ] || die "image build failed."
    IMAGE_BUILT=true
    ok "images built"
elif [ "${DEMO_PULL:-0}" = "1" ]; then
    warn "image(s) missing — DEMO_PULL=1: pulling published images ($PULL_SIDECAR_IMAGE + $PULL_GATEWAY_IMAGE)"
    warn "Hub tags may lag this git tip — pulled binaries are NOT guaranteed to match source. Prefer DEMO_ALLOW_BUILD=1 for exact-source proof."
    if PATH="$DOCKER_PATH" docker pull "$PULL_SIDECAR_IMAGE" >/dev/null 2>&1 \
       && PATH="$DOCKER_PATH" docker pull "$PULL_GATEWAY_IMAGE" >/dev/null 2>&1; then
        PATH="$DOCKER_PATH" docker tag "$PULL_SIDECAR_IMAGE" "$SIDECAR_IMAGE_NAME"
        PATH="$DOCKER_PATH" docker tag "$PULL_GATEWAY_IMAGE" "$GATEWAY_IMAGE_NAME"
        ok "published images pulled and tagged for this run (provenance may lag tip)"
    else
        die "required image(s) missing and the published-image pull failed ($PULL_SIDECAR_IMAGE / $PULL_GATEWAY_IMAGE — offline, or the tag is not published for this platform). Alternatives: build from this checkout with DEMO_ALLOW_BUILD=1 make verify (slow, RAM-heavy cargo compile, ~8GB Docker), or reuse a stack that already built $SIDECAR_IMAGE_NAME / $GATEWAY_IMAGE_NAME from this tip."
    fi
else
    die "required image(s) missing ($SIDECAR_IMAGE_NAME / $GATEWAY_IMAGE_NAME). Default verify does not pull Hub images (DEMO_PULL=0) so a clean machine cannot silently run binaries that lag this source tip. Build from this checkout: DEMO_ALLOW_BUILD=1 make verify (slow, ~8GB Docker, exact source). Or reuse local images already built from this tip. Opt-in Hub pull (may lag tip): DEMO_PULL=1 make verify."
fi
IMG_ELAPSED=$(( $(date +%s) - IMG_START ))

# ======================= <120s CLOCK STARTS HERE ==========================
# Per the Phase E acceptance the demo clock excludes the one-time image build;
# it measures the operator experience once images are available.
CLOCK_START=$(date +%s)

# Pre-pull the UDS curl helper so the arm stage call is not on the clock's
# critical path waiting on a registry pull mid-ceremony.
PATH="$DOCKER_PATH" docker pull "$CURL_IMG" >/dev/null 2>&1 || true

# --- write .env.demo (all knobs the isolated stack needs) -----------------
# Boot with the dispatcher DISABLED so provisioning is quiet; the second
# recreate flips it on with the provisioned key + arm principals.
write_env() {
    local dispatcher_enabled="$1" dispatcher_key="$2"
    cat > "$ENV_FILE" <<ENV
# AUTO-GENERATED by test/demo.sh — dev-safe throwaway secrets. Not .env.
AUTH_PEPPER=$AUTH_PEPPER
BOOTSTRAP_TOKEN=$BOOTSTRAP_TOKEN
GATEWAY_AUTH_FAIL_CLOSED=true
SIDECAR_SOCKET_MODE=0660
SIDECAR_SOCKET_GID=9999
DAILY_SPEND_CAP_USD=25
# Durable state ON so the per-key budget enforcer has a SQLite store. With
# GATEWAY_DURABLE=0 and no Redis it has NO store and fail-closes every budget
# check (429 budget_exceeded), which silently keeps the dispatcher inactive.
GATEWAY_DURABLE=1

# demo image pins + isolation paths (consumed by docker-compose.demo.yml)
DEMO_GW_PORT=$DEMO_GW_PORT
DEMO_SIDECAR_IMAGE=$SIDECAR_IMAGE_NAME
DEMO_GATEWAY_IMAGE=$GATEWAY_IMAGE_NAME
DEMO_LEDGER_KEY=$LEDGER_KEY
DEMO_ATTEST_KEYS=$ATTEST_KEYS

# council-triage routing: gateway -> local no-spend stub
GW_ENABLE_COUNCIL_ENDPOINT=1
COUNCIL_GATEWAY_TOKEN=$COUNCIL_GATEWAY_TOKEN
COUNCIL_BASE_URL=http://council-stub:8765

# watch plane: single demo tenant, dispatcher on the second boot
WATCH_CANARY_TENANT=$DEMO_TENANT
WATCH_DISPATCHER_ENABLED=$dispatcher_enabled
WATCH_PRODUCER_ENABLED=false
WATCH_DISPATCHER_TICK_INTERVAL_MS=500
WATCH_DISPATCHER_MAX_CLAIMS_PER_TICK=2
WATCH_DISPATCHER_GATEWAY_KEY=$dispatcher_key

# software (non-Touch-ID) arm principals + synthetic attest registry
GW_ARM_PRINCIPALS=demo-a:tok-demo-a,demo-b:tok-demo-b
GW_ARM_ATTEST_KEYS_PATH=/var/lib/sidecar/attest_keys.json
ENV
}
write_env "false" ""

# --- boot the isolated stack (bootstrap pass, dispatcher off) -------------
step "Bringing up the isolated stack ($GW_URL, project '$PROJECT')"
# Guard: `docker compose config` must show ONLY the demo port, never 18080.
if dc config 2>/dev/null | grep -qE '18080'; then
    die "SAFETY: verify compose still publishes 18080 (would collide with a live stack). Aborting before up."
fi
dc up -d --no-build gateway sidecar
for _ in $(seq 1 60); do curl -fsS "$GW_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$GW_URL/health" >/dev/null 2>&1 || die "gateway did not become healthy at $GW_URL/health"
ok "gateway healthy at $GW_URL"

# --- provision the dispatcher gateway key (bootstrap -> admin/keys) --------
# The gateway healthcheck can pass before the sidecar's admin surface is up
# (the sidecar binds its UDS a beat later), so /admin/* 503s transiently. Retry
# until the sidecar answers; the provision POST creates a key only on the 200.
step "Provisioning the dispatcher gateway key"
PCODE=""; PBODY=""
for _ in $(seq 1 30); do
    POUT=$(curl -s -w '\n%{http_code}' -X POST "$GW_URL/admin/keys" -H "Content-Type: application/json" \
        -d "{\"budget_key\":\"$DEMO_TENANT\",\"tier\":\"default\",\"rpm\":1000,\"admin_key\":\"$BOOTSTRAP_TOKEN\"}" 2>/dev/null || true)
    PCODE=$(printf '%s' "$POUT" | tail -1)
    PBODY=$(printf '%s' "$POUT" | sed '$d')
    [ "$PCODE" = "200" ] && break
    sleep 1
done
[ "$PCODE" = "200" ] || die "provision-key failed (HTTP ${PCODE:-none}) at $GW_URL/admin/keys: $PBODY"
DISPATCHER_KEY=$(printf '%s' "$PBODY" | jq -r '.raw_key // .key // empty')
[ -n "$DISPATCHER_KEY" ] || die "provision-key response had no raw_key: $PBODY"
ok "dispatcher key provisioned (budget=$DEMO_TENANT)"

# --- recreate with dispatcher ON + the provisioned key --------------------
step "Recreating sidecar with the live dispatcher armed to consult the local endpoint"
write_env "true" "$DISPATCHER_KEY"
dc up -d --no-build --force-recreate gateway sidecar
for _ in $(seq 1 60); do curl -fsS "$GW_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$GW_URL/health" >/dev/null 2>&1 || die "gateway did not become healthy after recreate"
ok "dispatcher stack live"

# Fail before arming or seeding if the Gateway container cannot resolve and
# reach the sibling stub over Compose DNS.
dc exec -T gateway wget -q -O /dev/null http://council-stub:8765/api/health \
    || die "gateway container cannot reach council-stub over Compose DNS"
ok "gateway -> council-stub Compose-network probe healthy (\$0)"

# --- software arm ceremony (stage -> sign challenge -> confirm) ------------
# The attested confirm is what starts the CDC producer (the boot env-arm stays
# off), so this is a required leg, not decoration.
step "Performing the software arm ceremony (stage/sign/confirm; no Touch-ID)"
ARM_BEARER="demo-a:tok-demo-a"
STAGE_OUT=""; STAGE_CODE="000"
for _ in $(seq 1 30); do
    STAGE_OUT=$(curl_uds -w '\n%{http_code}' -X POST http://localhost/watch/admin/producer/arm/stage \
        -H "Authorization: Bearer ${ARM_BEARER}" -H "Content-Type: application/json" -d '{}' 2>/dev/null) || true
    STAGE_CODE=$(printf '%s' "$STAGE_OUT" | tail -1)
    [ -n "$STAGE_CODE" ] && [ "$STAGE_CODE" != "000" ] && break
    sleep 1
done
STAGE_BODY=$(printf '%s' "$STAGE_OUT" | sed '$d')
[ "$STAGE_CODE" = "200" ] || die "arm STAGE failed (HTTP $STAGE_CODE): $STAGE_BODY"
REHEARSAL=$(printf '%s' "$STAGE_BODY" | jq -r '.rehearsal | tostring')
if [ "$REHEARSAL" != "false" ]; then
    die "arm STAGE returned rehearsal=$REHEARSAL — the reused sidecar image is a DIRTY build and may not real-arm.
Rebuild a clean image:  docker image rm $SIDECAR_IMAGE_NAME && DEMO_ALLOW_BUILD=1 make verify   (or run from a clean checkout)."
fi
STAGE_ID=$(printf '%s' "$STAGE_BODY" | jq -r '.stage_id // empty')
CHALLENGE_B64=$(printf '%s' "$STAGE_BODY" | jq -r '.challenge // empty')
[ -n "$STAGE_ID" ] && [ -n "$CHALLENGE_B64" ] || die "arm STAGE missing stage_id/challenge: $STAGE_BODY"
CHAL_BIN="$STATE_DIR/challenge.bin"
printf '%s' "$CHALLENGE_B64" | base64 -d > "$CHAL_BIN" 2>/dev/null || die "arm challenge not valid base64"
SIG_B64=$(openssl dgst -sha256 -sign "$ARM_KEY" -binary "$CHAL_BIN" | base64 | tr -d '\n') || die "openssl sign failed"
CONF_OUT=$(curl_uds -w '\n%{http_code}' -X POST http://localhost/watch/admin/producer/arm/confirm \
    -H "Authorization: Bearer ${ARM_BEARER}" -H "Content-Type: application/json" \
    -d "{\"stage_id\":\"$STAGE_ID\",\"credential_id\":\"demo-arm-cred\",\"credential_type\":\"se-p256\",\"signature\":\"$SIG_B64\"}") || true
CONF_CODE=$(printf '%s' "$CONF_OUT" | tail -1)
[ "$CONF_CODE" = "200" ] || die "arm CONFIRM failed (HTTP $CONF_CODE): $(printf '%s' "$CONF_OUT" | sed '$d')"
rm -f "$CHAL_BIN"
ok "armed (software P-256, rehearsal=false); the attested confirm started the CDC producer"

# --- seed one escalation (watch_fires) so the producer + dispatcher run ----
step "Seeding one escalation into the watch plane"
SIDE=sidecar
DB=/var/lib/sidecar/watch.db
RUN_ID="demo-$(date +%s)-${RANDOM}"   # macOS date has no %N; fresh volume per run avoids collisions
NOW_MS=$(date +%s000)
STATE_JSON="{\"payload\":{\"run_id\":\"$RUN_ID\",\"observed\":{\"cpu\":95},\"reason\":\"demo escalation\",\"urgency\":\"high\"},\"observed_at\":$NOW_MS}"
FIRE_REASON="demo escalation run_id=$RUN_ID"
ENVELOPE_JSON="{\"contract\":\"irin.comms.v0.1\",\"kind\":\"Escalation\",\"id\":\"$RUN_ID\",\"tenant\":\"$DEMO_TENANT\",\"payload\":{\"sentinel\":\"$DEMO_SENTINEL\",\"tier\":\"fast\",\"observed\":{\"cpu\":95},\"reason\":\"demo escalation\",\"urgency\":\"high\",\"proposed_action\":\"ConsultCouncil\",\"run_id\":\"$RUN_ID\"}}"
# watch_fires is a per-tenant SHA-256 hash chain; compute prev/next exactly as
# the sidecar does (length-prefixed preimage).
PREV_HASH=$(dc exec -T "$SIDE" sqlite3 "$DB" \
    "SELECT COALESCE((SELECT hash FROM watch_fires WHERE tenant='$DEMO_TENANT' ORDER BY id DESC LIMIT 1),'');" | tr -d '\r')
if [ -z "$PREV_HASH" ]; then
    PREV_HASH=0ffed28740318eb8e9fa37cc3d034394c1eea87a273ecdbfcdcb937af502acba
fi
FIRE_HASH=$(python3 - "$DEMO_TENANT" "$DEMO_SENTINEL" "$NOW_MS" "$STATE_JSON" "$FIRE_REASON" "$PREV_HASH" <<'PY'
import hashlib, sys
tenant, sentinel, fired_at, state_json, reason, prev = sys.argv[1:]
pre = f"{len(tenant)}:{tenant}|{len(sentinel)}:{sentinel}|{len(fired_at)}:{fired_at}|{len(state_json)}:{state_json}|{len(reason)}:{reason}|{len(prev)}:{prev}"
print(hashlib.sha256(pre.encode()).hexdigest())
PY
)
dc exec -T "$SIDE" sqlite3 "$DB" <<SQL
INSERT INTO watch_fires
  (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash, envelope_json, envelope_schema_version)
VALUES
  ('$DEMO_TENANT','$DEMO_SENTINEL',$NOW_MS,'$STATE_JSON','$FIRE_REASON','$PREV_HASH','$FIRE_HASH','$ENVELOPE_JSON',1);
SQL
ok "escalation seeded (run_id=$RUN_ID)"

# --- wait for producer -> pending, then dispatcher -> outbox ---------------
step "Waiting for the closed loop (producer -> pending -> council -> signed outbox)"
PENDING_ID=""
for _ in $(seq 1 "$POLL_TIMEOUT"); do
    PENDING_ID=$(dc exec -T "$SIDE" sqlite3 "$DB" \
        "SELECT id FROM pending_escalations WHERE tenant='$DEMO_TENANT' AND envelope_json LIKE '%$RUN_ID%' ORDER BY created_at_ms DESC LIMIT 1;" | tr -d '\r')
    [ -n "$PENDING_ID" ] && break
    sleep 1
done
[ -n "$PENDING_ID" ] || die "CDC producer did not enqueue a pending_escalation (run_id=$RUN_ID). See: make verify logs"

STATUS=""
for _ in $(seq 1 "$POLL_TIMEOUT"); do
    STATUS=$(dc exec -T "$SIDE" sqlite3 "$DB" \
        "SELECT status FROM pending_escalations WHERE tenant='$DEMO_TENANT' AND id='$PENDING_ID';" | tr -d '\r')
    case "$STATUS" in
        outbox_written|dismissed) break ;;
        dead_lettered)
            ERR=$(dc exec -T "$SIDE" sqlite3 "$DB" "SELECT COALESCE(last_error,'') FROM pending_escalations WHERE tenant='$DEMO_TENANT' AND id='$PENDING_ID';" | tr -d '\r')
            die "escalation dead_lettered: $ERR" ;;
    esac
    sleep 1
done
[ "$STATUS" = "outbox_written" ] || die "loop did not reach outbox_written (last status: ${STATUS:-<none>})"

# --- read + verify the signed directive_outbox row ------------------------
DIRECTIVE_ID=$(dc exec -T "$SIDE" sqlite3 "$DB" "SELECT id FROM directive_outbox WHERE tenant='$DEMO_TENANT' AND in_response_to='$PENDING_ID';" | tr -d '\r')
SIG_B64_ROW=$(dc exec -T "$SIDE" sqlite3 "$DB" "SELECT signature_b64 FROM directive_outbox WHERE tenant='$DEMO_TENANT' AND in_response_to='$PENDING_ID';" | tr -d '\r')
[ -n "$DIRECTIVE_ID" ] && [ -n "$SIG_B64_ROW" ] || die "directive_outbox row missing id/signature"

# Independent Ed25519 verification through the gateway's public outbox surface
# (admin-gated read + published pubkey), same proof the smoke asserts.
VERIFY_MSG="(gateway outbox surface verification skipped — python cryptography not importable)"
if python3 -c "import cryptography" >/dev/null 2>&1; then
    if python3 - "$GW_URL" "$DEMO_TENANT" "$DIRECTIVE_ID" "$SIG_B64_ROW" "$BOOTSTRAP_TOKEN" <<'PY'
import base64, json, sys, urllib.request
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
base, tenant, did, expect_sig, auth = sys.argv[1:6]
base = base.rstrip("/")
def fetch(p):
    r = urllib.request.Request(base + p)
    if auth: r.add_header("Authorization", f"Bearer {auth}")
    with urllib.request.urlopen(r, timeout=10) as resp: return resp.status, json.loads(resp.read().decode())
st, pk = fetch("/watch/outbox/pubkey")
assert st == 200 and pk.get("alg") == "Ed25519", f"pubkey surface bad: {st} {pk}"
st, row = fetch(f"/watch/outbox/{tenant}/{did}")
assert st == 200 and row.get("id") == did, f"row surface bad: {st}"
sig = row.get("signature") or {}
assert sig.get("value") == expect_sig, "signature.value != DB signature_b64"
assert sig.get("kid") == pk.get("kid"), "kid mismatch"
Ed25519PublicKey.from_public_bytes(base64.b64decode(pk["pubkey_b64"])).verify(
    base64.b64decode(sig["value"]), row["envelope_json_canonical"].encode())
print("verified")
PY
    then VERIFY_MSG="Ed25519 signature VERIFIED against the gateway's published outbox pubkey"
    else VERIFY_MSG="(gateway outbox verification returned non-zero — inspect: make verify logs)"; fi
fi

ELAPSED=$(( $(date +%s) - CLOCK_START ))

# --- pretty-print the signed directive ------------------------------------
ROW_JSON=$(dc exec -T "$SIDE" sqlite3 -json "$DB" \
    "SELECT id, tenant, in_response_to, status, verdict, authority, signing_kid, council_cost_usd, substr(signature_b64,1,24)||'…' AS signature_b64_preview FROM directive_outbox WHERE tenant='$DEMO_TENANT' AND in_response_to='$PENDING_ID';" 2>/dev/null | tr -d '\r' || echo '[]')

trap - EXIT
echo ""
echo "======================================================================"
echo -e "${G}${B}  SIGNED DIRECTIVE PRODUCED — closed loop proven (\$0, no live model calls)${N}"
echo "======================================================================"
echo "  escalation run_id : $RUN_ID"
echo "  pending id        : $PENDING_ID  (status: $STATUS)"
echo "  directive id      : $DIRECTIVE_ID"
echo "  signing kid       : $(printf '%s' "$ROW_JSON" | jq -r '.[0].signing_kid // "?"' 2>/dev/null)"
echo "  signature (b64)   : $(printf '%s' "$SIG_B64_ROW" | cut -c1-40)…"
echo "  verification      : $VERIFY_MSG"
echo "----------------------------------------------------------------------"
echo "  directive_outbox row:"
printf '%s\n' "$ROW_JSON" | jq . 2>/dev/null || printf '%s\n' "$ROW_JSON"
echo "----------------------------------------------------------------------"
if [ "$IMAGE_BUILT" = true ]; then
    echo -e "  ${B}elapsed: ${ELAPSED}s${N}  (+ ${IMG_ELAPSED}s one-time image build)"
else
    echo -e "  ${B}elapsed: ${ELAPSED}s${N}  (images reused; build excluded — one-time build ~ tens of minutes on a cold machine)"
fi
if [ "$ELAPSED" -le 120 ]; then
    echo -e "  ${G}PASS: under the 120s Phase E acceptance budget${N}"
else
    echo -e "  ${R}OVER 120s${N} — see timing above"
fi
echo "======================================================================"
echo "  Stack still up on $GW_URL. Tear down with:  make verify-down"
echo "======================================================================"
