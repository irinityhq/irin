#!/usr/bin/env bash
# Static hygiene for the Gateway Pack source assets (no Docker required).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/packaging/gateway-pack/docker-compose.yml"
README="$ROOT/packaging/gateway-pack/README.md"
EXAMPLE="$ROOT/packaging/gateway-pack/image-manifest.production.example.json"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'PASS: %s\n' "$*"; }

[[ -f "$COMPOSE" ]] || die "compose missing"
[[ -f "$README" ]] || die "README missing"
[[ -f "$EXAMPLE" ]] || die "production example manifest missing"

grep -q 'name: irin-desktop-gateway' "$COMPOSE" || die "fixed project name"
grep -q 'WATCH_PRODUCER_ENABLED=false' "$COMPOSE" || die "producer false"
grep -q 'WATCH_DISPATCHER_ENABLED=false' "$COMPOSE" || die "dispatcher false"
! grep -E '^[[:space:]]*build:' "$COMPOSE" >/dev/null || die "build: forbidden"
! grep -E '^[[:space:]]*-\s*.*(\$\{HOME\}|~/|\$HOME)' "$COMPOSE" >/dev/null || die "HOME mounts forbidden"
! grep -E '^[[:space:]]*-\s*.*(gcloud|\.config/gcloud)' "$COMPOSE" >/dev/null || die "gcloud mounts forbidden"
grep -q '127.0.0.1:18080:8080' "$COMPOSE" || die "fixed loopback port"
grep -q 'IRIN_DESKTOP_LEDGER_KEY' "$COMPOSE" || die "app-owned ledger mount"
grep -q 'IRIN_DESKTOP_PACK_ROOT' "$COMPOSE" || die "pack root mount"
ARM_MOUNT='${IRIN_DESKTOP_ARM_KEYS}:/run/secrets/arm_attest_keys.json:ro'
[[ "$(grep -Fc "$ARM_MOUNT" "$COMPOSE")" -eq 1 ]] ||
  die "Touch ID public registry must be read-only and sidecar-only"
ARM_MARKER_MOUNT='${IRIN_DESKTOP_PACK_ROOT}/arm-bridge-enabled:/run/irin-features/arm-bridge-enabled:ro'
[[ "$(grep -Fc "$ARM_MARKER_MOUNT" "$COMPOSE")" -eq 1 ]] ||
  die "Touch ID edge must receive exactly one read-only desktop marker"
grep -q 'ARM_BRIDGE_MARKER_PATH = "/run/irin-features/arm-bridge-enabled"' "$ROOT/gateway/lua/sidecar.lua" ||
  die "Touch ID bridge must check the desktop marker"
grep -Fq 'local body_file = ngx.req.get_body_file()' "$ROOT/gateway/lua/sidecar.lua" ||
  die "Touch ID bridge must preserve Nginx file-buffered confirm bodies"
grep -Fq 'ARM_BRIDGE_MAX_BODY_BYTES = 65536' "$ROOT/gateway/lua/sidecar.lua" ||
  die "Touch ID bridge must bound file-buffered confirm bodies"
grep -Fq 'return ""' "$ROOT/gateway/lua/sidecar.lua" ||
  die "Touch ID bridge must normalize a truly empty POST for lua-resty-http"
grep -Fq 'if body and #body > 0 then' "$ROOT/gateway/lua/sidecar.lua" ||
  die "Touch ID bridge must not label an empty POST body as JSON"
! grep -q '/run/secrets/arm_attest_keys.json' "$ROOT/gateway/lua/sidecar.lua" ||
  die "Touch ID edge Lua must never read the root-owned registry"
grep -q 'Vertex' "$README" || die "support matrix must mention Vertex"
grep -q 'Not supported' "$README" || die "support matrix negatives"
grep -q '"mode": "production"' "$EXAMPLE" || die "example mode"
grep -q '@sha256:' "$EXAMPLE" || die "example digests"
# Real arming requires an explicit production/test lane compile claim. The
# Dockerfile and ordinary local-dev image builder must remain rehearsal-only.
grep -q '^ARG GW_RELEASE_ELIGIBLE=false$' "$ROOT/gateway/sidecar-rs/Dockerfile" ||
  die "sidecar Dockerfile must default release eligibility false"
! grep -q 'GW_RELEASE_ELIGIBLE=true' "$ROOT/scripts/build-gateway-pack-dev-images.sh" ||
  die "local-dev image builder must remain rehearsal-only"
grep -q -- '--build-arg GW_RELEASE_ELIGIBLE=true' "$ROOT/scripts/build-gateway-pack-prod-images.sh" ||
  die "production image publisher must opt into release eligibility"

# Digest-shaped refs in example (even placeholders).
python3 - <<'PY' "$EXAMPLE" || die "example manifest shape"
import json, re, sys
p = sys.argv[1]
m = json.load(open(p))
assert m["schema_version"] == 1
assert m["mode"] == "production"
pat = re.compile(r"^[^@\s]+@sha256:[0-9a-f]{64}$")
for k, v in m["images"].items():
    assert pat.match(v), k
for k, v in m["third_party_pins"].items():
    assert pat.match(v), k
assert m["watch_invariants"]["WATCH_PRODUCER_ENABLED"] is False
assert m["watch_invariants"]["WATCH_DISPATCHER_ENABLED"] is False
print("ok")
PY

# Stage into a temp dir and re-check.
TMP="$(mktemp -d "${TMPDIR:-/tmp}/gw-pack-stage.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
# local-dev stage requires a local-dev manifest (generate a minimal one for the static gate).
LOCAL_MANI="$TMP/image-manifest.local.json"
HEX="$(printf 'a%.0s' {1..64})"
cat >"$LOCAL_MANI" <<EOF
{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "0.1.0-static-test",
  "source_sha": "test",
  "source_dirty": false,
  "images": {
    "gateway": "irin-desktop/gateway@sha256:$HEX",
    "sidecar": "irin-desktop/sidecar@sha256:$HEX"
  },
  "watch_invariants": {
    "WATCH_PRODUCER_ENABLED": false,
    "WATCH_DISPATCHER_ENABLED": false
  }
}
EOF
IRIN_GATEWAY_PACK_MODE=local-dev \
  IRIN_GATEWAY_PACK_LOCAL_MANIFEST="$LOCAL_MANI" \
  bash "$ROOT/scripts/stage-gateway-pack.sh" "$TMP/gateway-pack"
[[ -f "$TMP/gateway-pack/docker-compose.yml" ]] || die "stage compose"
[[ -f "$TMP/gateway-pack/nginx.conf" ]] || die "stage nginx"
[[ -f "$TMP/gateway-pack/arm-bridge-enabled" ]] || die "stage arm bridge marker"
python3 -c 'import os,sys; assert (os.stat(sys.argv[1]).st_mode & 0o777) == 0o644' \
  "$TMP/gateway-pack/arm-bridge-enabled" || die "arm bridge marker must be mode 0644"
[[ -d "$TMP/gateway-pack/conf" ]] || die "stage conf"
[[ -d "$TMP/gateway-pack/lua" ]] || die "stage lua"
[[ -f "$TMP/gateway-pack/image-manifest.json" ]] || die "stage manifest"
grep -q 'mode=local-dev' "$TMP/gateway-pack/STAGED_MODE.txt" || die "staged mode stamp"

# Production stage must refuse local-dev manifest.
if IRIN_GATEWAY_PACK_MODE=production \
  IRIN_GATEWAY_PACK_PROD_MANIFEST="$LOCAL_MANI" \
  bash "$ROOT/scripts/stage-gateway-pack.sh" "$TMP/gateway-pack-prod" 2>/dev/null; then
  die "production stage must refuse local-dev manifest"
fi

bash "$ROOT/scripts/test-production-image-provenance.sh" ||
  die "production image provenance contracts"

pass "gateway pack static assets"
