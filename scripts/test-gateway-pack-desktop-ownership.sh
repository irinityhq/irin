#!/usr/bin/env bash
# Deterministic survivor test: pre-existing irin-desktop-gateway must cause
# smoke to refuse before Enable/up, and must not be torn down with --volumes.
#
# Creates a minimal busybox project under the fixed desktop name, invokes the
# integration smoke (which must refuse), then proves project + volume survived.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECEIPT_DIR="$ROOT/packaging/receipts"
RECEIPT="$RECEIPT_DIR/GATEWAY_PACK_DESKTOP_OWNERSHIP.txt"
MARKER_VOLUME="irin_desktop_preexist_survivor_$$"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/irin-desktop-own.XXXXXX")"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$RECEIPT"; }

mkdir -p "$RECEIPT_DIR"
: >"$RECEIPT"
log "=== gateway-pack-desktop-ownership $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="

command -v docker >/dev/null || die "docker CLI required"
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
  die "Docker daemon not ready"
fi

cleanup() {
  set +e
  # Always remove the survivor fixture we created (this script owns it).
  docker compose -p irin-desktop-gateway -f "$WORK/preexist-compose.yml" \
    down --volumes --remove-orphans >/dev/null 2>&1 || true
  docker volume rm "$MARKER_VOLUME" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# If a real desktop pack is already running, do not clobber it for this test.
if docker compose -p irin-desktop-gateway ps -a -q 2>/dev/null | grep -q . \
  || docker compose ls -a 2>/dev/null | grep -q 'irin-desktop-gateway'; then
  log "desktop_already_present=true"
  die "refuse: irin-desktop-gateway already present; free it before ownership survivor test"
fi

docker volume create "$MARKER_VOLUME" >/dev/null
cat >"$WORK/preexist-compose.yml" <<EOF
name: irin-desktop-gateway
services:
  keep:
    image: busybox:1.36
    command: ["sleep", "3600"]
    volumes:
      - survivor:/data
volumes:
  survivor:
    name: $MARKER_VOLUME
    external: true
EOF
docker compose -p irin-desktop-gateway -f "$WORK/preexist-compose.yml" up -d >/dev/null
log "preexist_project_started=true"
log "survivor_volume=$MARKER_VOLUME"

# Integration smoke must refuse with desktop_preexisting and leave us intact.
set +e
SMOKE_OUT="$(
  bash "$ROOT/scripts/test-gateway-pack-integration-smoke.sh" 2>&1
)"
SMOKE_RC=$?
set -e
log "integration_smoke_rc=$SMOKE_RC"
# Receipt may not exist if script died early before tee; parse stdout.
if printf '%s\n' "$SMOKE_OUT" | grep -q 'desktop_preexisting=true'; then
  log "desktop_preexisting=true"
else
  log "smoke_output_tail=$(printf '%s\n' "$SMOKE_OUT" | tail -n 8 | tr '\n' ' | ')"
  die "integration smoke did not report desktop_preexisting=true"
fi
[[ "$SMOKE_RC" -ne 0 ]] || die "integration smoke must refuse (non-zero) when desktop preexists"

# Prove survivor: project still present, volume intact.
docker compose -p irin-desktop-gateway -f "$WORK/preexist-compose.yml" ps -q | grep -q . \
  || die "pre-existing desktop project was destroyed by refused smoke"
docker volume inspect "$MARKER_VOLUME" >/dev/null \
  || die "survivor volume destroyed by refused smoke"
log "desktop_project_survived=true"
log "survivor_volume_survived=true"
log "owned_desktop_teardown=false"

log "RESULT=PASS"
printf 'PASS: gateway pack desktop ownership survivor\n'
