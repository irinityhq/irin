#!/usr/bin/env bash
#
# prove_provenance_legs.sh — two-leg provenance gate.
#
# Falsification gate for the build-provenance -> attestation -> signed-arm path.
# It proves, by reading the RUNNING binary (never a shell $EXPECT var), that the
# unified cargo-chef build bakes GW_BUILD_DIRTY correctly for BOTH tree states:
#
#   DIRTY leg (run FIRST):  a deliberately-dirtied tree -> the built sidecar reports
#                           build_id "<sha>-dirty" AND arm/stage is forced to
#                           rehearsal=true (B6: a dirty/unidentifiable build may
#                           NEVER arm the real producer).
#   CLEAN leg:              `git status --porcelain` empty is a HARD precondition
#                           (FAIL, never skip) -> the built sidecar reports build_id
#                           "<sha>" (no -dirty) AND arm/stage returns rehearsal=false
#                           (real arm allowed). The clean directive OUTBOX is proven
#                           separately by the full Phase 3 smoke on this same image.
#
# Cache-provenance NON-CROSSING (the invariant this gate enforces mechanically):
# each leg is a fresh `docker compose build sidecar` whose in-container build.rs ran
# `git status` over THAT leg's actual tree (clean vs staged marker). cargo-chef caches
# DEPS only; the leaf crate (gateway-sidecar, carrying attest.rs) is `cargo clean -p`'d
# every build, so no cache can serve one leg an artifact baked under the other's state.
#
# Oracle: the arm/stage response carries `rehearsal` (bool) and `challenge` (base64
# of the JCS bytes, which embed `build_id`). We decode the challenge and assert the
# embedded build_id + the rehearsal verdict — i.e. we read the artifact, not the wire.
#
# CACHE-CROSSING DEFENSE (hardening): enforcement is the RUNTIME
# ORACLE, not the layer-cache keying. Even if some cache served a leg a leaf baked
# under the OTHER tree state, the running binary would report the wrong (rehearsal,
# build_id) and the per-leg assertion below would FAIL. So the gate holds regardless
# of how any layer cache is keyed (Council invariant: "...gating merge regardless of
# cache state"). Defence in depth: `.git` is inside the content-addressed `COPY` so
# each leg already gets a distinct layer (the .git/index-keyed bust), plus the
# Dockerfile `cargo clean -p gateway-sidecar`. Do NOT add a cross-run layer cache
# keyed on lockfile/Dockerfile/args ALONE — only one that includes git tree state.
#
# Dirtying mechanism (P0-1): a TRACKED staged marker (`echo > .smoke_marker;
# git add`). A bare `touch` is unsound — build.rs's fingerprint watches only
# .git/{HEAD,index,refs/heads,packed-refs}, so the dirty must mutate the index.
#
set -euo pipefail

# P1-5 — FORBID any leg-selection build input on the provenance compile. Leg state
# comes ONLY from the real .git tree, never an env channel an attacker could set.
if [ -n "${GW_SMOKE_LEG:-}" ]; then
    echo "FAIL: GW_SMOKE_LEG is forbidden (P1-5). Leg state must come from the git tree, not an env var." >&2
    exit 1
fi

cd "$(dirname "$0")/.."   # gateway subtree root (this script lives in test/)
REPO_ROOT="$PWD"
GIT_WORKTREE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
pass() { echo -e "${GREEN}PASS${NC}: $1"; }
fail() { echo -e "${RED}FAIL${NC}: $1" >&2; exit 1; }
info() { echo -e "INFO: $1"; }
warn() { echo -e "${YELLOW}WARN${NC}: $1"; }

# Isolated compose project — NEVER the production "gateway" project. This script
# seeds a throwaway attest fixture into ${PROJECT}_sidecar_data and `down -v`s the
# whole project on exit; pointed at production it would destroy the live sidecar
# volume (attest keyset, signed ledger, provisioned keys). Fail closed when the
# production project name is selected.
PROJECT="${COMPOSE_PROJECT_NAME:-provtest}"
if [ "$PROJECT" = "gateway" ]; then
    fail "refusing to run against compose project 'gateway' (production volumes; cleanup is down -v). Use the provtest default or another isolated COMPOSE_PROJECT_NAME."
fi
export COMPOSE_PROJECT_NAME="$PROJECT"
MARKER="$REPO_ROOT/.smoke_marker"
# Light profile for the gate build (provenance is independent of LTO; speed isn't).
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-false}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}"
# Dev-safe defaults so `docker compose up sidecar` boots without the full smoke env.
export AUTH_PEPPER="${AUTH_PEPPER:-provtest-pepper}"
export GW_ARM_PRINCIPALS="ci-a:tok-ci-a,ci-b:tok-ci-b"
export GW_ARM_ATTEST_KEYS_PATH="/var/lib/sidecar/attest_keys.json"
ARM_BEARER="ci-a:tok-ci-a"

ARM_KEY_PEM=""; ATT_KEYS_JSON=""

cleanup() {
    set +e
    docker compose down -v --remove-orphans >/dev/null 2>&1 || true
    # ALWAYS restore a clean tree — never leave the staged marker behind.
    git rm -f --cached "$MARKER" >/dev/null 2>&1 || true
    rm -f "$MARKER"
    [ -n "$ARM_KEY_PEM" ] && rm -f "$ARM_KEY_PEM"
    [ -n "$ATT_KEYS_JSON" ] && rm -f "$ATT_KEYS_JSON"
}
trap cleanup EXIT INT TERM

prepare_arm_artifacts() {
    ARM_KEY_PEM="$(mktemp -t provtest-arm-key.XXXXXX.pem)"
    ATT_KEYS_JSON="$(mktemp -t provtest-attest.XXXXXX.json)"
    openssl ecparam -name prime256v1 -genkey -noout -out "$ARM_KEY_PEM" >/dev/null 2>&1 \
        || fail "openssl ecparam failed (openssl required)"
    local pub_b64
    pub_b64=$(openssl ec -in "$ARM_KEY_PEM" -pubout -conv_form compressed -outform DER 2>/dev/null | tail -c 33 | base64 | tr -d '\n')
    [ -n "$pub_b64" ] || fail "failed to extract compressed P-256 public key"
    cat > "$ATT_KEYS_JSON" <<EOF
[{"credential_id":"ci-arm-cred","credential_type":"se-p256","public_key":"$pub_b64","label":"provtest","enrolled_at":"2026-01-01T00:00:00Z"}]
EOF
    chmod 600 "$ARM_KEY_PEM" "$ATT_KEYS_JSON"
}

# Seed the boot-only attest registry onto the named sidecar_data volume BEFORE the
# sidecar starts (it reads GW_ARM_ATTEST_KEYS_PATH at boot; fail-closed if absent).
seed_registry() {
    docker volume create "${PROJECT}_sidecar_data" >/dev/null 2>&1 || true
    docker run --rm -v "${PROJECT}_sidecar_data:/d" -v "$ATT_KEYS_JSON:/k.json:ro" \
        busybox:1.37 sh -c 'cp /k.json /d/attest_keys.json && chmod 600 /d/attest_keys.json' \
        || fail "failed to seed attest registry onto ${PROJECT}_sidecar_data"
}

build_leg() {  # $1 = clean|dirty
    local leg="$1"
    if [ "$leg" = "dirty" ]; then
        echo "provenance smoke leg=dirty (Council P1-6)" > "$MARKER"
        git add "$MARKER"   # busts .git/index -> in-container `git status` is non-empty
        info "dirty leg: staged tracked marker -> porcelain: $(git status --porcelain | tr '\n' ';')"
    else
        # HARD precondition — a clean leg built on a dirty checkout proves nothing.
        [ -z "$(git status --porcelain)" ] || fail "clean leg precondition: working tree is DIRTY (must be empty). Refusing to proceed."
        info "clean leg: precondition OK (working tree clean)"
    fi
    info "building sidecar image ($leg leg) via docker compose..."
    docker compose build sidecar >/dev/null 2>&1 || fail "docker compose build sidecar failed ($leg leg)"
    if [ "$leg" = "dirty" ]; then
        git rm -f --cached "$MARKER" >/dev/null 2>&1 || true
        rm -f "$MARKER"
    fi
}

curl_uds() {
    docker run --rm -i --user 0 -v "${PROJECT}_sidecar_sock:/run/sidecar" \
        curlimages/curl:8.12.1 -sS --unix-socket /run/sidecar/sidecar.sock "$@"
}

probe_leg() {  # $1 = clean|dirty ; asserts the oracle from the RUNNING binary
    local leg="$1"
    docker compose up -d --no-deps sidecar >/dev/null 2>&1 || fail "docker compose up sidecar failed ($leg leg)"
    # Wait for the management UDS to exist + answer.
    local up=""
    for _ in $(seq 1 60); do
        if docker compose exec -T sidecar sh -c 'test -S /run/sidecar/sidecar.sock' >/dev/null 2>&1; then up=1; break; fi
        sleep 1
    done
    [ -n "$up" ] || { docker compose logs sidecar 2>&1 | tail -30 >&2; fail "sidecar socket never came up ($leg leg)"; }

    docker pull curlimages/curl:8.12.1 >/dev/null 2>&1 || true
    local out code body rehearsal challenge build_id
    # `test -S` only proves the socket FILE exists — the management listener may not
    # be accepting yet (bind→accept window), so RETRY the stage call until we get a
    # real HTTP code (not 000/connection-refused). Re-staging is idempotent (the
    # stage_id nonce just changes). This is the readiness gate the gateway-less boot
    # lost vs the full smoke's health wait.
    code="000"
    for _ in $(seq 1 30); do
        out=$(curl_uds -w '\n%{http_code}' -X POST http://localhost/watch/admin/producer/arm/stage \
            -H "Authorization: Bearer ${ARM_BEARER}" -H "Content-Type: application/json" -d '{}' 2>/dev/null) || true
        code=$(printf '%s' "$out" | tail -1)
        [ "$code" != "000" ] && [ -n "$code" ] && break
        sleep 1
    done
    body=$(printf '%s' "$out" | sed '$d')
    [ "$code" = "200" ] || { echo "stage body: $body" >&2; docker compose logs --no-color sidecar 2>&1 | tail -30 >&2; fail "arm/stage HTTP $code ($leg leg) — fail closed"; }

    # tostring (NOT `// "true"`): jq's // treats boolean false as empty.
    rehearsal=$(printf '%s' "$body" | jq -r '.rehearsal | tostring')
    challenge=$(printf '%s' "$body" | jq -r '.challenge // empty')
    # NEGATIVE CONTROL: read the EMBEDDED build_id straight out of the signed
    # challenge bytes the running binary produced — the artifact, not the wire.
    build_id=$(printf '%s' "$challenge" | base64 -d 2>/dev/null | jq -r '.build_id // "PARSE_FAIL"')
    info "$leg leg oracle: rehearsal=$rehearsal build_id=$build_id"

    case "$leg" in
        dirty)
            [ "$rehearsal" = "true" ] || fail "DIRTY leg: expected rehearsal=true (B6 must refuse real arm), got '$rehearsal'"
            case "$build_id" in
                *-dirty) : ;;
                *) fail "DIRTY leg: expected build_id '<sha>-dirty', got '$build_id'" ;;
            esac
            pass "DIRTY leg refuses real arm (rehearsal=true, build_id=$build_id)"
            ;;
        clean)
            [ "$rehearsal" = "false" ] || fail "CLEAN leg: expected rehearsal=false (real arm allowed), got '$rehearsal' — DARK build leaked into the clean leg"
            case "$build_id" in
                *-dirty|PARSE_FAIL|unknown*) fail "CLEAN leg: expected clean build_id '<sha>', got '$build_id'" ;;
                *) : ;;
            esac
            pass "CLEAN leg allows real arm (rehearsal=false, build_id=$build_id)"
            ;;
    esac
    # Best-effort disarm + teardown between legs. `-v` wipes the named volumes
    # (sidecar_sock + sidecar_data) so NO stale socket file or arm state carries into
    # the next leg — a lingering dead socket would make the next leg's `test -S` pass
    # against a non-listening socket (the observed CI flake). The ledger key is a HOST
    # bind mount (survives -v); seed_registry re-creates the attest registry per leg.
    curl_uds -X POST http://localhost/watch/admin/producer/disarm \
        -H "Authorization: Bearer ${ARM_BEARER}" -H "Content-Type: application/json" -d '{}' >/dev/null 2>&1 || true
    docker compose down -v --remove-orphans >/dev/null 2>&1 || true
}

# --- run: DIRTY first (falsification), then CLEAN -------------------------------
info "Council P1-6 two-leg provenance gate (project=$PROJECT, lto=$CARGO_PROFILE_RELEASE_LTO)"
[ -n "$GIT_WORKTREE_ROOT" ] || fail "could not resolve the enclosing Git worktree root. Run this gate from a normal checkout."
[ -d "$GIT_WORKTREE_ROOT/.git" ] || fail ".git is not a real directory at the worktree root (a linked-worktree .git is a pointer file -> always-DARK). Run this gate on a normal checkout."
prepare_arm_artifacts

build_leg dirty
seed_registry
probe_leg dirty

build_leg clean
seed_registry
probe_leg clean

DIRTY_SHA_OK=1
pass "two-leg provenance gate GREEN — dirty refuses, clean arms; same SHA, suffix differs by tree state only"
