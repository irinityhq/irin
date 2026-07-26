#!/usr/bin/env bash
# Verify a candidate DMG layout and codesign without mutating the test copy.
# Never re-signs the ditto'd app — promotion requires an untouched DMG copy.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

TEST_APPS="$ROOT/packaging/test-apps"
MOUNT="$ROOT/packaging/build/dmg-mount"
IRIN_RELEASE_VERSION="${IRIN_RELEASE_VERSION:-0.1.2}"
DMG="${IRIN_DMG_PATH:-$ROOT/packaging/artifacts/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg}"
APP_NAME="IRIN.app"
DEST_APP="$TEST_APPS/$APP_NAME"
REPORT="$ROOT/packaging/receipts/VERIFY.txt"
HASHES_PATH="${IRIN_DMG_HASHES_PATH:-}"
REQUESTED_PACK_MODE="${IRIN_DMG_PACK_MODE:-}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

receipt_value() {
  local key="$1" count value
  count="$(awk -F= -v key="$key" '$1 == key { count++ } END { print count + 0 }' "$HASHES_PATH")" \
    || die "could not parse receipt key $key: $HASHES_PATH"
  [[ "$count" == "1" ]] \
    || die "receipt must contain exactly one $key entry (found $count): $HASHES_PATH"
  value="$(awk -v prefix="$key=" 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }' "$HASHES_PATH")" \
    || die "could not read receipt key $key: $HASHES_PATH"
  [[ -n "$value" ]] || die "receipt key $key is empty: $HASHES_PATH"
  printf '%s' "$value"
}

receipt_sha256() {
  local key="$1" value
  value="$(receipt_value "$key")"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] \
    || die "receipt key $key must be one lowercase SHA-256 value: $HASHES_PATH"
  printf '%s' "$value"
}

sha256_file() {
  local path="$1" value
  value="$(shasum -a 256 "$path" | awk '{print $1}')" \
    || die "could not hash artifact: $path"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || die "invalid SHA-256 result for artifact: $path"
  printf '%s' "$value"
}

verify_sha256() {
  local key="$1" path="$2" label="$3" expected actual
  expected="$(receipt_sha256 "$key")"
  actual="$(sha256_file "$path")"
  [[ "$actual" == "$expected" ]] || die "$label SHA-256 does not match explicit receipt"
  log "$label SHA-256: receipt match"
}

macho_inventory() {
  local app="$1" candidate
  while IFS= read -r -d '' candidate; do
    if file -b "$candidate" 2>/dev/null | grep -q '^Mach-O'; then
      printf '%s\n' "${candidate#"$app"/}"
    fi
  done < <(find "$app/Contents" -type f -print0)
}

assert_expected_macho_inventory() {
  local app="$1" actual expected
  actual="$(macho_inventory "$app" | LC_ALL=C sort)"
  expected="$(printf '%s\n' \
    'Contents/Helpers/arm-attest' \
    'Contents/MacOS/council' \
    'Contents/MacOS/council-warroom-tauri' | LC_ALL=C sort)"
  [[ "$actual" == "$expected" ]] || {
    printf 'Expected Mach-O inventory:\n%s\nActual Mach-O inventory:\n%s\n' "$expected" "$actual" >&2
    die "unexpected Mach-O inventory in the untouched DMG copy"
  }
}

verify_production_signature() {
  local artifact="$1" expected_team="$2" label="$3" details entitlements team
  codesign --verify --strict "$artifact" \
    || die "$label failed strict signature verification"
  details="$(codesign -dv --verbose=4 "$artifact" 2>&1)" \
    || die "could not inspect $label signature"
  [[ "$details" == *"Authority=Developer ID Application"* ]] \
    || die "$label is not signed with Developer ID Application"
  grep -Eq '^CodeDirectory .*flags=.*runtime' <<<"$details" \
    || die "$label is missing the Hardened Runtime signature flag"
  grep -q '^Timestamp=' <<<"$details" \
    || die "$label is missing a trusted signing timestamp"
  ! grep -q '^Timestamp=none$' <<<"$details" \
    || die "$label has no trusted signing timestamp"
  team="$(awk -F= '$1 == "TeamIdentifier" { print $2; exit }' <<<"$details")"
  [[ -n "$team" && "$team" == "$expected_team" ]] \
    || die "$label TeamIdentifier does not match the outer app"
  entitlements="$(codesign -d --entitlements :- "$artifact" 2>/dev/null || true)"
  if grep -q '<key>' <<<"$entitlements"; then
    die "$label contains entitlements, but IRIN 0.1.2 declares none"
  fi
  log "$label signature: Developer ID, runtime, timestamp, TeamIdentifier, no entitlements"
}

verify_gateway_manifest() {
  local manifest="$1" mode="$2" version="$3" source_sha="$4"
  python3 - "$manifest" "$mode" "$version" "$source_sha" <<'PY'
import json
import re
import sys

manifest_path, expected_mode, release_version, source_sha = sys.argv[1:]
with open(manifest_path, encoding="utf-8") as handle:
    data = json.load(handle)
assert data.get("schema_version") == 1, "schema_version must be 1"
assert data.get("mode") == expected_mode, "manifest mode does not match receipt"
watch = data.get("watch_invariants", {})
assert watch.get("WATCH_PRODUCER_ENABLED") is False, "watch producer must ship disabled"
assert watch.get("WATCH_DISPATCHER_ENABLED") is False, "watch dispatcher must ship disabled"
if expected_mode == "production":
    assert re.fullmatch(r"[0-9a-f]{40}", source_sha), "receipt source_sha is invalid"
    assert data.get("source_sha") == source_sha, "manifest source_sha does not match receipt"
    assert data.get("source_dirty") is False, "production manifest source_dirty must be false"
    allowed_versions = {release_version, f"rc-{source_sha[:12]}"}
    assert data.get("pack_version") in allowed_versions, "production pack_version does not identify this release or RC"
    assert data.get("platform") == "linux/arm64", "production Gateway Pack platform must be linux/arm64"
    expected_images = {
        "gateway": r"ghcr\.io/irinityhq/irin-gateway@sha256:[0-9a-f]{64}",
        "sidecar": r"ghcr\.io/irinityhq/irin-sidecar@sha256:[0-9a-f]{64}",
    }
    images = data.get("images", {})
    for name, pattern in expected_images.items():
        assert re.fullmatch(pattern, images.get(name, "")), f"{name} is not the canonical immutable GHCR digest"
print("Gateway Pack manifest: schema, mode, source, immutable images, and watch-off invariants verified")
PY
}

mkdir -p "$ROOT/packaging/receipts" "$TEST_APPS" "$MOUNT"
: >"$REPORT"
log "=== verify-dmg $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "ROOT=$ROOT"
log "DMG=$DMG"
log "IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION"

[[ -n "$HASHES_PATH" ]] \
  || die "IRIN_DMG_HASHES_PATH is required; pass the receipt produced with this exact DMG"
[[ -f "$HASHES_PATH" ]] || die "explicit HASHES receipt missing: $HASHES_PATH"
case "$REQUESTED_PACK_MODE" in
  ""|local-dev|production) ;;
  *) die "IRIN_DMG_PACK_MODE must be local-dev or production (got $REQUESTED_PACK_MODE)" ;;
esac
RECEIPT_PACK_MODE="$(receipt_value pack_mode)"
case "$RECEIPT_PACK_MODE" in
  local-dev|production) ;;
  *) die "receipt pack_mode must be local-dev or production (got $RECEIPT_PACK_MODE)" ;;
esac
if [[ -n "$REQUESTED_PACK_MODE" && "$REQUESTED_PACK_MODE" != "$RECEIPT_PACK_MODE" ]]; then
  die "requested pack mode $REQUESTED_PACK_MODE does not match receipt pack_mode=$RECEIPT_PACK_MODE"
fi
RECEIPT_RELEASE_VERSION="$(receipt_value release_version)"
[[ "$RECEIPT_RELEASE_VERSION" == "$IRIN_RELEASE_VERSION" ]] \
  || die "receipt release_version=$RECEIPT_RELEASE_VERSION does not match IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION"
log "HASHES=$HASHES_PATH (explicit)"
log "receipt_pack_mode=$RECEIPT_PACK_MODE"

[[ -f "$DMG" ]] || die "missing DMG: $DMG"
[[ "$(uname -m)" == "arm64" ]] || die "arm64 only"
verify_sha256 dmg_sha256 "$DMG" "DMG"

if mount | grep -q "$MOUNT"; then
  hdiutil detach "$MOUNT" -force 2>/dev/null || true
fi
rm -rf "$MOUNT" "$DEST_APP"
mkdir -p "$MOUNT"

log "=== mount DMG (read-only) ==="
hdiutil attach "$DMG" -mountpoint "$MOUNT" -readonly -nobrowse
trap 'hdiutil detach "$MOUNT" -force 2>/dev/null || true' EXIT

SRC_APP="$(find "$MOUNT" -maxdepth 2 -name "$APP_NAME" -type d | head -1 || true)"
[[ -d "$SRC_APP" ]] || die "app not found inside DMG"
log "DMG app: $SRC_APP"

log "=== ditto untouched copy (no re-sign) ==="
ditto "$SRC_APP" "$DEST_APP"
[[ -d "$DEST_APP" ]] || die "copy failed"

log "=== codesign verify (must pass as shipped; never re-sign test copy) ==="
if ! codesign --verify --deep --strict "$DEST_APP"; then
  die "codesign verification failed on untouched DMG copy — do not re-sign; fix the build"
fi
codesign -dv "$DEST_APP" 2>&1 | tee -a "$REPORT" || true
SIGNATURE_DETAILS="$(codesign -dv --verbose=4 "$DEST_APP" 2>&1)" \
  || die "could not inspect untouched app signature"
IS_DEVELOPER_ID=0
if [[ "$SIGNATURE_DETAILS" == *"Authority=Developer ID Application"* ]]; then
  IS_DEVELOPER_ID=1
fi

VERIFY_PACK_MODE="$RECEIPT_PACK_MODE"
if [[ "$IS_DEVELOPER_ID" == "1" ]]; then
  VERIFY_PACK_MODE="production"
  [[ "$RECEIPT_PACK_MODE" == "production" ]] \
    || die "Developer ID-signed app cannot be verified with a local-dev receipt"
elif [[ "$RECEIPT_PACK_MODE" == "production" || "$REQUESTED_PACK_MODE" == "production" ]]; then
  die "production verification requires a Developer ID Application signature"
fi
log "effective_pack_mode=$VERIFY_PACK_MODE"

if [[ "$VERIFY_PACK_MODE" == "production" ]]; then
  log "=== production assertions: every Mach-O, identity, runtime, Gatekeeper, staple ==="
  assert_expected_macho_inventory "$DEST_APP"
  OUTER_TEAM="$(awk -F= '$1 == "TeamIdentifier" { print $2; exit }' <<<"$SIGNATURE_DETAILS")"
  [[ -n "$OUTER_TEAM" ]] || die "outer app signature has no TeamIdentifier"
  verify_production_signature "$DEST_APP" "$OUTER_TEAM" "outer app"
  verify_production_signature "$DEST_APP/Contents/Helpers/arm-attest" "$OUTER_TEAM" "Touch ID helper"
  verify_production_signature "$DEST_APP/Contents/MacOS/council" "$OUTER_TEAM" "Council sidecar"
  verify_production_signature "$DEST_APP/Contents/MacOS/council-warroom-tauri" "$OUTER_TEAM" "Tauri host"
  spctl --assess --type execute -vv "$DEST_APP" 2>&1 | tee -a "$REPORT" \
    || die "Gatekeeper assessment failed on untouched copy"
  xcrun stapler validate "$DMG" 2>&1 | tee -a "$REPORT" \
    || die "DMG is not stapled"
fi

HOST="$DEST_APP/Contents/MacOS/council-warroom-tauri"
SIDECAR="$DEST_APP/Contents/MacOS/council"
[[ -x "$HOST" ]] || die "host binary missing"
[[ -x "$SIDECAR" ]] || die "council sidecar missing"
file "$HOST" | tee -a "$REPORT"
file "$SIDECAR" | tee -a "$REPORT"
file "$SIDECAR" | grep -q arm64 || die "sidecar not arm64"

CABINETS="$(find "$DEST_APP/Contents/Resources" -type d -name cabinets | head -1 || true)"
[[ -n "$CABINETS" ]] || die "cabinets not in Resources"
BASE_DIR="$(dirname "$CABINETS")"
log "base-dir: $BASE_DIR"
log "cabinets: $(ls "$CABINETS" | wc -l | tr -d ' ') files"
HERMES_ADAPTER="$BASE_DIR/scripts/hermes-seat-adapter.sh"
[[ -x "$HERMES_ADAPTER" ]] || die "hermes seat adapter missing or not executable: $HERMES_ADAPTER"
log "hermes adapter: $HERMES_ADAPTER (executable)"

TOUCH_ID_HELPER="$DEST_APP/Contents/Helpers/arm-attest"
[[ -x "$TOUCH_ID_HELPER" && -s "$TOUCH_ID_HELPER" ]] \
  || die "Touch ID helper missing, empty, or not executable: $TOUCH_ID_HELPER"
log "Touch ID helper: $TOUCH_ID_HELPER (non-empty, executable)"

GATEWAY_PACK="$DEST_APP/Contents/Resources/gateway-pack"
for required_file in docker-compose.yml image-manifest.json arm-bridge-enabled nginx.conf; do
  [[ -f "$GATEWAY_PACK/$required_file" ]] \
    || die "Gateway Pack asset missing: $GATEWAY_PACK/$required_file"
done
for required_dir in conf lua; do
  [[ -d "$GATEWAY_PACK/$required_dir" ]] \
    || die "Gateway Pack directory missing: $GATEWAY_PACK/$required_dir"
done
log "Gateway Pack assets: required files and directories present"
WARROOM_WEB="$DEST_APP/Contents/Resources/warroom-web"
[[ -f "$WARROOM_WEB/index.html" ]] || die "bundled War Room export missing: $WARROOM_WEB/index.html"
log "War Room static export: index present"

verify_sha256 app_sha256 "$HOST" "host binary"
verify_sha256 council_sha256 "$SIDECAR" "Council sidecar"
verify_sha256 arm_attest_sha256 "$TOUCH_ID_HELPER" "Touch ID helper"
verify_sha256 gateway_pack_compose_sha256 "$GATEWAY_PACK/docker-compose.yml" "Gateway Pack compose"
verify_sha256 gateway_pack_manifest_sha256 "$GATEWAY_PACK/image-manifest.json" "Gateway Pack manifest"
verify_sha256 warroom_web_index_sha256 "$WARROOM_WEB/index.html" "War Room index"
RECEIPT_SOURCE_SHA="$(receipt_value source_sha)"
verify_gateway_manifest "$GATEWAY_PACK/image-manifest.json" "$VERIFY_PACK_MODE" \
  "$IRIN_RELEASE_VERSION" "$RECEIPT_SOURCE_SHA" | tee -a "$REPORT"
if [[ "$VERIFY_PACK_MODE" == "production" ]]; then
  log "=== replay immutable registry provenance for bundled production manifest ==="
  bash "$ROOT/scripts/verify-production-image-provenance.sh" \
    "$GATEWAY_PACK/image-manifest.json" "$RECEIPT_SOURCE_SHA" "$IRIN_RELEASE_VERSION" \
    | tee -a "$REPORT" \
    || die "bundled production manifest failed immutable registry provenance verification"
fi

GUIDANCE_OK=0
if strings "$HOST" 2>/dev/null | grep -Fq 'Gateway is optional'; then
  log "gateway optional guidance: present in host binary strings"
  GUIDANCE_OK=1
fi
if [[ "$GUIDANCE_OK" != 1 ]]; then
  if grep -R -F -l 'Docker Desktop' "$DEST_APP/Contents" 2>/dev/null | head -1 | grep -q .; then
    log "gateway/Docker guidance: present in frontend assets"
    GUIDANCE_OK=1
  fi
fi
[[ "$GUIDANCE_OK" == 1 ]] || die "Gateway/Docker guidance text missing from bundle"

log "=== verify-dmg PASS ==="
log "dest_app=$DEST_APP"
