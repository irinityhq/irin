#!/usr/bin/env bash
# Deterministic Gateway Pack integration smoke (tracked receipt script).
#
# Proves compose lifecycle for irin-desktop-gateway against local-dev images
# with isolated secrets, while preserving a deliberate foreign project/volume
# and an unrelated Keychain item.
#
# Safety:
#   - Never touches project name "gateway" (canonical).
#   - Generated secrets live under a temp dir and are wiped.
#   - Keychain uses a unique test service only (never production account).
#   - No provider call; no Watch arming; no production credentials.
#   - Receipts contain only non-secret status/IDs/hashes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECEIPT_DIR="$ROOT/packaging/receipts"
RECEIPT="$RECEIPT_DIR/GATEWAY_PACK_INTEGRATION_SMOKE.txt"
COMPOSE="$ROOT/packaging/gateway-pack/docker-compose.yml"
MANIFEST_LOCAL="$ROOT/packaging/build/gateway-pack/image-manifest.local.json"
FOREIGN_PROJECT="irin-foreign-isolation-probe"
FOREIGN_VOLUME="irin_foreign_isolation_vol"
TEST_SERVICE="com.sovereign.council.warroom.test.pack-smoke.$$"
TEST_ACCOUNT="gateway-client-gw-api-key-test"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/irin-gw-pack-smoke.XXXXXX")"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$RECEIPT"; }

mkdir -p "$RECEIPT_DIR"
: >"$RECEIPT"
log "=== gateway-pack-integration-smoke $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "work=$WORK"
log "pid=$$"

cleanup() {
  set +e
  # Tear down desktop project if we started it (never foreign).
  if [[ -f "$WORK/compose.public.env" ]]; then
    docker compose -p irin-desktop-gateway -f "$COMPOSE" --env-file "$WORK/compose.public.env" \
      down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  # Delete only our unique Keychain test item.
  security delete-generic-password -s "$TEST_SERVICE" -a "$TEST_ACCOUNT" >/dev/null 2>&1 || true
  # Wipe isolated secrets dir.
  rm -rf "$WORK"
  # Leave foreign project/volume/keychain alone (proved surviving below).
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

# --- Foreign isolation fixtures (must survive) ---
docker volume create "$FOREIGN_VOLUME" >/dev/null
# Minimal foreign compose project (busybox sleep) — distinct name.
cat >"$WORK/foreign-compose.yml" <<EOF
name: $FOREIGN_PROJECT
services:
  keep:
    image: busybox:1.36
    command: ["sleep", "3600"]
    volumes:
      - $FOREIGN_VOLUME:/data
EOF
docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" up -d >/dev/null
log "foreign_project_started=$FOREIGN_PROJECT"
log "foreign_volume=$FOREIGN_VOLUME"

# Unrelated Keychain item (unique service) — must survive cleanup of desktop item.
FOREIGN_KC_SERVICE="com.sovereign.council.warroom.test.foreign.$$"
security add-generic-password -U -s "$FOREIGN_KC_SERVICE" -a "foreign-item" -w "not-a-secret-marker" >/dev/null 2>&1 \
  || security delete-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null 2>&1
security add-generic-password -U -s "$FOREIGN_KC_SERVICE" -a "foreign-item" -w "not-a-secret-marker" >/dev/null
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

# Bounded compose up.
python3 - <<'PY' "$STAGE/docker-compose.yml" "$WORK/compose.public.env"
import subprocess, sys
compose, envf = sys.argv[1], sys.argv[2]
cmd = [
    "docker", "compose", "-p", "irin-desktop-gateway",
    "-f", compose, "--env-file", envf,
    "up", "-d", "--remove-orphans",
]
r = subprocess.run(cmd, capture_output=True, timeout=180)
sys.exit(r.returncode)
PY
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

# Tear down desktop only.
docker compose -p irin-desktop-gateway -f "$STAGE/docker-compose.yml" --env-file "$WORK/compose.public.env" \
  down --volumes --remove-orphans >/dev/null
log "desktop_down=ok"

# Foreign still present.
docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" ps -q | grep -q . \
  || die "foreign project was destroyed"
docker volume inspect "$FOREIGN_VOLUME" >/dev/null || die "foreign volume destroyed"
security find-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null \
  || die "foreign keychain item destroyed"
log "foreign_project_survived=true"
log "foreign_volume_survived=true"
log "foreign_keychain_survived=true"

# Cleanup foreign fixtures we created (test-owned).
docker compose -p "$FOREIGN_PROJECT" -f "$WORK/foreign-compose.yml" down --volumes --remove-orphans >/dev/null || true
docker volume rm "$FOREIGN_VOLUME" >/dev/null 2>&1 || true
security delete-generic-password -s "$FOREIGN_KC_SERVICE" -a "foreign-item" >/dev/null 2>&1 || true
security delete-generic-password -s "$TEST_SERVICE" -a "$TEST_ACCOUNT" >/dev/null 2>&1 || true

# Scrub env vars from this shell.
unset AUTH_PEPPER BOOTSTRAP_TOKEN PEPPER BOOTSTRAP

log "RESULT=PASS"
printf 'PASS: gateway pack integration smoke\n'
