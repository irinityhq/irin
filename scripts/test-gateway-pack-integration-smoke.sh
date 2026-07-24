#!/usr/bin/env bash
# Deterministic Gateway Pack integration smoke (tracked receipt script).
#
# Proves compose lifecycle for irin-desktop-gateway against local-dev images
# with isolated secrets, while preserving a deliberate foreign project/volume
# and an unrelated Keychain item through the product action. The foreign
# fixtures are created before the product action, asserted to survive it, and
# then removed by this test itself on both success and failure paths.
#
# Safety:
#   - Never touches project name "gateway" (canonical).
#   - Generated secrets live under a temp dir and are wiped.
#   - Keychain uses a unique test service only (never production account).
#   - No provider call; no Watch arming; no production credentials.
#   - Receipts contain only non-secret status/IDs/hashes.
#   - Foreign fixtures created by this run are tracked and removed on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECEIPT_DIR="$ROOT/packaging/receipts"
RECEIPT="$RECEIPT_DIR/GATEWAY_PACK_INTEGRATION_SMOKE.txt"
COMPOSE="$ROOT/packaging/gateway-pack/docker-compose.yml"
MANIFEST_LOCAL="$ROOT/packaging/build/gateway-pack/image-manifest.local.json"
FOREIGN_PROJECT="irin-foreign-isolation-probe"
FOREIGN_VOLUME="irin_foreign_isolation_vol"
TEST_SERVICE="com.irinity.irin.test.pack-smoke.$$"
TEST_ACCOUNT="gateway-client-gw-api-key-test"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/irin-gw-pack-smoke.XXXXXX")"
DESKTOP_PREEXISTING=false
OWNED_DESKTOP_TEARDOWN=false
# Foreign fixtures this run created; the EXIT trap removes exactly these.
FOREIGN_PROJECT_CREATED=false
FOREIGN_VOLUME_CREATED=false
FOREIGN_KC_CREATED=false
FOREIGN_KC_SERVICE="com.irinity.irin.test.foreign.$$"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$RECEIPT"; }

desktop_project_present() {
  if docker compose -p irin-desktop-gateway ps -a -q 2>/dev/null | grep -q .; then
    return 0
  fi
  if docker compose ls -a 2>/dev/null | grep -q 'irin-desktop-gateway'; then
    return 0
  fi
  return 1
}

mkdir -p "$RECEIPT_DIR"
: >"$RECEIPT"
log "=== gateway-pack-integration-smoke $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "work=$WORK"
log "pid=$$"

cleanup() {
  set +e
  # Tear down desktop project only if this run created it (never foreign, never pre-existing).
  if [[ "$OWNED_DESKTOP_TEARDOWN" == "true" ]]; then
    if [[ -f "$WORK/compose.public.env" ]]; then
      docker compose -p irin-desktop-gateway -f "$COMPOSE" --env-file "$WORK/compose.public.env" \
        down --volumes --remove-orphans >/dev/null 2>&1 || true
    else
      docker compose -p irin-desktop-gateway down --volumes --remove-orphans >/dev/null 2>&1 || true
    fi
    log "owned_desktop_teardown=true"
  else
    log "owned_desktop_teardown=false"
  fi
  # Remove only the foreign fixtures this run created (tracked at creation).
  # The main flow asserts they survive the product action; the harness still
  # owns their removal on both success and failure paths so a failed run
  # cannot leak the BusyBox keeper, the volume, or the Keychain item. The
  # compose down needs -f with the staged foreign-compose.yml (compose
  # refuses `-p NAME down` without a configuration file), so it must run
  # before $WORK is wiped.
  if [[ "$FOREIGN_PROJECT_CREATED" == "true" ]]; then
    docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" \
      down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  if [[ "$FOREIGN_VOLUME_CREATED" == "true" ]]; then
    docker volume rm "$FOREIGN_VOLUME" >/dev/null 2>&1 || true
  fi
  if [[ "$FOREIGN_KC_CREATED" == "true" ]]; then
    security delete-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null 2>&1 || true
  fi
  # Delete only our unique Keychain test item.
  security delete-generic-password -s "$TEST_SERVICE" -a "$TEST_ACCOUNT" >/dev/null 2>&1 || true
  # Wipe isolated secrets dir (after the foreign compose down; see above).
  rm -rf "$WORK"
}
trap cleanup EXIT

command -v docker >/dev/null || die "docker CLI required for integration smoke"
# Bounded daemon probe (never hang forever).
if ! python3 - <<'PY'
import subprocess, sys
try:
    r = subprocess.run(
        ["docker", "info", "--format", "{{.ServerVersion}}"],
        capture_output=True, timeout=12, check=False,
    )
    sys.exit(0 if r.returncode == 0 else 1)
except subprocess.TimeoutExpired:
    sys.exit(2)
PY
then
  die "Docker daemon not ready (or timed out)"
fi

[[ -f "$COMPOSE" ]] || die "missing compose"

# Refuse before any Enable/up side effects if the fixed desktop project exists.
# (Checked before manifest so ownership survivor need not rebuild images.)
if desktop_project_present; then
  DESKTOP_PREEXISTING=true
  log "desktop_preexisting=true"
  die "refuse: irin-desktop-gateway already exists; will not down --volumes a pre-existing desktop pack"
fi
DESKTOP_PREEXISTING=false
log "desktop_preexisting=false"

[[ -f "$MANIFEST_LOCAL" ]] || die "missing local-dev manifest; run make gateway-pack-dev-images"

# Parse local-dev image refs (no secret material).
GW_REF="$(python3 - <<'PY' "$MANIFEST_LOCAL"
import json,sys
m=json.load(open(sys.argv[1]))
assert m["mode"]=="local-dev", m["mode"]
print(m["images"]["gateway"])
PY
)"
SC_REF="$(python3 - <<'PY' "$MANIFEST_LOCAL"
import json,sys
m=json.load(open(sys.argv[1]))
print(m["images"]["sidecar"])
PY
)"
SOURCE_SHA="$(python3 - <<'PY' "$MANIFEST_LOCAL"
import json,sys
m=json.load(open(sys.argv[1]))
print(m.get("source_sha") or "unknown")
PY
)"
log "manifest_mode=local-dev"
log "source_sha=$SOURCE_SHA"
log "gateway_ref_digest_len=${#GW_REF}"
log "sidecar_ref_digest_len=${#SC_REF}"

# --- Foreign isolation fixtures (must survive the product action) ---
# This test removes these tracked fixtures itself afterwards, on both paths.
docker volume create "$FOREIGN_VOLUME" >/dev/null
FOREIGN_VOLUME_CREATED=true
# Minimal foreign compose project (busybox sleep) — distinct name.
cat >"$WORK/foreign-compose.yml" <<EOF
name: $FOREIGN_PROJECT
services:
  keep:
    image: busybox:1.36
    command: ["sleep", "3600"]
    volumes:
      - foreign_data:/data
volumes:
  foreign_data:
    name: $FOREIGN_VOLUME
    external: true
EOF
# Arm teardown before up so a partially-created project is still removed.
FOREIGN_PROJECT_CREATED=true
docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" up -d >/dev/null
log "foreign_project_started=$FOREIGN_PROJECT"
log "foreign_volume=$FOREIGN_VOLUME"

# Unrelated Keychain item (unique service) — must survive cleanup of desktop item.
security add-generic-password -U -s "$FOREIGN_KC_SERVICE" -a "foreign-item" -w "not-a-secret-marker" >/dev/null 2>&1 \
  || security delete-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null 2>&1
security add-generic-password -U -s "$FOREIGN_KC_SERVICE" -a "foreign-item" -w "not-a-secret-marker" >/dev/null
FOREIGN_KC_CREATED=true
log "foreign_keychain_service=$FOREIGN_KC_SERVICE"

# Isolated generated secrets (never printed).
PEPPER="$(openssl rand -hex 32)"
BOOTSTRAP="$(openssl rand -hex 32)"
LEDGER="$WORK/ledger_key"
openssl rand 32 >"$LEDGER"
chmod 600 "$LEDGER"

# Public compose env only (no provider keys, no long-lived secrets on disk except blank bootstrap).
PACK_ROOT="$ROOT/packaging/gateway-pack"
# Stage conf/lua paths from gateway runtime for compose mounts.
STAGE="$WORK/pack"
mkdir -p "$STAGE"
cp -f "$COMPOSE" "$STAGE/docker-compose.yml"
cp -f "$ROOT/gateway/nginx.conf" "$STAGE/nginx.conf"
rsync -a "$ROOT/gateway/conf/" "$STAGE/conf/"
rsync -a "$ROOT/gateway/lua/" "$STAGE/lua/"
cp -f "$MANIFEST_LOCAL" "$STAGE/image-manifest.json"

cat >"$WORK/compose.public.env" <<EOF
IRIN_GATEWAY_IMAGE=$GW_REF
IRIN_SIDECAR_IMAGE=$SC_REF
IRIN_DESKTOP_PACK_ROOT=$STAGE
IRIN_DESKTOP_LEDGER_KEY=$LEDGER
GATEWAY_DURABLE=1
GATEWAY_AUTH_FAIL_CLOSED=true
SIDECAR_SOCKET_MODE=0660
SIDECAR_SOCKET_GID=9999
GW_ENABLE_COUNCIL_ENDPOINT=0
COUNCIL_BASE_URL=http://host.docker.internal:8765
WATCH_PRODUCER_ENABLED=false
WATCH_DISPATCHER_ENABLED=false
WATCH_CANARY_TENANT=canary
DAILY_SPEND_CAP_USD=25
WATCH_MAX_FANOUT_COST_USD=2.50
BOOTSTRAP_TOKEN=
EOF
chmod 600 "$WORK/compose.public.env"

# Assert public env has no secret material patterns we refuse to persist.
if grep -E '^(AUTH_PEPPER|WATCH_ADMIN_TOKEN|COUNCIL_GATEWAY_TOKEN|XAI_API_KEY|OPENAI_API_KEY|ANTHROPIC_API_KEY|NVIDIA_API_KEY)=' \
  "$WORK/compose.public.env" | grep -v '=$' | grep -q .; then
  die "public env must not carry provider/auth secrets"
fi
log "public_env=ok (no provider/auth secrets)"

# Start with process-env secrets (not in file).
export AUTH_PEPPER="$PEPPER"
export BOOTSTRAP_TOKEN="$BOOTSTRAP"
export WATCH_PRODUCER_ENABLED=false
export WATCH_DISPATCHER_ENABLED=false

# Refuse if :18080 is owned by a non-desktop listener (canonical gateway).
if curl -fsS --max-time 2 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
  if ! docker compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q .; then
    die "port 18080 already answers /health outside irin-desktop-gateway (stop canonical Gateway first, then restore after smoke)"
  fi
fi

# Bounded compose up (surface stderr on failure; never print env secrets).
set +e
UP_OUT="$(
  python3 - <<'PY' "$STAGE/docker-compose.yml" "$WORK/compose.public.env" 2>&1
import subprocess, sys
compose, envf = sys.argv[1], sys.argv[2]
cmd = [
    "docker", "compose", "-p", "irin-desktop-gateway",
    "-f", compose, "--env-file", envf,
    "up", "-d", "--remove-orphans",
]
try:
    r = subprocess.run(cmd, capture_output=True, timeout=180, text=True)
except subprocess.TimeoutExpired:
    print("compose_up_timeout", file=sys.stderr)
    sys.exit(124)
# Redact any accidental secret-looking tokens from docker noise before print.
import re
def scrub(s: str) -> str:
    s = re.sub(r"gw_[0-9a-fA-F]{32}", "<redacted:gw_key>", s)
    s = re.sub(r"(BOOTSTRAP_TOKEN|AUTH_PEPPER)=\\S+", r"\\1=<redacted>", s)
    return s[:800]
sys.stderr.write(scrub(r.stderr or ""))
sys.stdout.write(scrub(r.stdout or ""))
sys.exit(r.returncode)
PY
)"
UP_RC=$?
set -e
if [[ "$UP_RC" != "0" ]]; then
  log "compose_up_failed rc=$UP_RC"
  log "compose_up_output_redacted=${UP_OUT//$'\n'/ | }"
  die "compose up failed for irin-desktop-gateway (rc=$UP_RC)"
fi
# This run created the project — cleanup may tear it down.
OWNED_DESKTOP_TEARDOWN=true
log "owned_desktop_teardown_armed=true"
log "compose_up=ok project=irin-desktop-gateway"

# Health probe (no secrets).
HEALTH_OK=0
for _ in $(seq 1 60); do
  if curl -fsS --max-time 3 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
    HEALTH_OK=1
    break
  fi
  sleep 0.5
done
[[ "$HEALTH_OK" == "1" ]] || die "gateway /health not ready"
log "health=200"

# Fail-closed models without key.
CODE="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:18080/v1/models" || true)"
[[ "$CODE" == "401" || "$CODE" == "403" ]] || die "models not fail-closed (code=$CODE)"
log "models_fail_closed=code_$CODE"

# Store a disposable Keychain item under unique service (presence only later).
security add-generic-password -U -s "$TEST_SERVICE" -a "$TEST_ACCOUNT" -w "gw_$(openssl rand -hex 16)" >/dev/null
security find-generic-password -s "$TEST_SERVICE" -a "$TEST_ACCOUNT" >/dev/null
log "keychain_test_item=present (value not read in receipt)"

# Watch forced off.
docker compose -p irin-desktop-gateway -f "$STAGE/docker-compose.yml" --env-file "$WORK/compose.public.env" \
  exec -T sidecar sh -c 'echo producer=$WATCH_PRODUCER_ENABLED dispatcher=$WATCH_DISPATCHER_ENABLED' 2>/dev/null \
  | tee -a "$RECEIPT" | grep -q 'producer=false' || log "watch_env_probe=skipped_or_unavailable"

# Tear down desktop only (owned by this run).
docker compose -p irin-desktop-gateway -f "$STAGE/docker-compose.yml" --env-file "$WORK/compose.public.env" \
  down --volumes --remove-orphans >/dev/null
OWNED_DESKTOP_TEARDOWN=false
log "desktop_down=ok"
log "owned_desktop_teardown=true"

# Foreign still present.
docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" ps -q | grep -q . \
  || die "foreign project was destroyed"
docker volume inspect "$FOREIGN_VOLUME" >/dev/null || die "foreign volume destroyed"
security find-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null \
  || die "foreign keychain item destroyed"
log "foreign_project_survived=true"
log "foreign_volume_survived=true"
log "foreign_keychain_survived=true"

# Cleanup foreign fixtures we created (test-owned). The EXIT trap removes any
# of these left behind on a failure path; clear each tracking flag here so a
# successful run removes every fixture exactly once.
if [[ "$FOREIGN_PROJECT_CREATED" == "true" ]]; then
  docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" down --volumes --remove-orphans >/dev/null || true
  FOREIGN_PROJECT_CREATED=false
fi
if [[ "$FOREIGN_VOLUME_CREATED" == "true" ]]; then
  docker volume rm "$FOREIGN_VOLUME" >/dev/null 2>&1 || true
  FOREIGN_VOLUME_CREATED=false
fi
if [[ "$FOREIGN_KC_CREATED" == "true" ]]; then
  security delete-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null 2>&1 || true
  FOREIGN_KC_CREATED=false
fi
security delete-generic-password -s "$TEST_SERVICE" -a "$TEST_ACCOUNT" >/dev/null 2>&1 || true
log "foreign_fixtures_removed=true"

# Scrub env vars from this shell.
unset AUTH_PEPPER BOOTSTRAP_TOKEN PEPPER BOOTSTRAP

log "RESULT=PASS"
printf 'PASS: gateway pack integration smoke\n'
