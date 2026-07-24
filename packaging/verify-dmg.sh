#!/usr/bin/env bash
# Verify a candidate DMG layout and codesign without mutating the test copy.
# Never re-signs the ditto'd app — promotion requires an untouched DMG copy.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

TEST_APPS="$ROOT/packaging/test-apps"
MOUNT="$ROOT/packaging/build/dmg-mount"
IRIN_RELEASE_VERSION="${IRIN_RELEASE_VERSION:-0.1.0}"
DMG="${IRIN_DMG_PATH:-$ROOT/packaging/artifacts/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg}"
APP_NAME="IRIN.app"
DEST_APP="$TEST_APPS/$APP_NAME"
REPORT="$ROOT/packaging/receipts/VERIFY.txt"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

mkdir -p "$ROOT/packaging/receipts" "$TEST_APPS" "$MOUNT"
: >"$REPORT"
log "=== verify-dmg $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "ROOT=$ROOT"
log "DMG=$DMG"
log "IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION"

[[ -f "$DMG" ]] || die "missing DMG: $DMG"
[[ "$(uname -m)" == "arm64" ]] || die "arm64 only"

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

if [[ "${IRIN_DMG_PACK_MODE:-local-dev}" == "production" ]]; then
  log "=== production assertions: identity, Gatekeeper, staple ==="
  AUTH="$(codesign -dv --verbose=4 "$DEST_APP" 2>&1 | grep '^Authority=' | head -1 || true)"
  [[ "$AUTH" == *"Developer ID Application"* ]] \
    || die "production app is not Developer ID signed (got: ${AUTH:-none})"
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
