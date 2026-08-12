#!/usr/bin/env bash
# Verify a named candidate DMG layout and codesign without mutating the store app.
# Never re-signs the ditto'd app — promotion requires an untouched DMG copy.
#
# Required:
#   IRIN_CANDIDATE_PATH  absolute path under IRIN_CANDIDATE_ROOT
# Optional:
#   IRIN_DMG_PACK_MODE   must match receipt when set
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

irin_require_candidate_path
CANDIDATE="$IRIN_CANDIDATE_PATH"
APP_NAME="IRIN.app"
# Disposable verify extract lives under the candidate (not packaging/test-apps).
VERIFY_ROOT="$CANDIDATE/verify"
MOUNT="$VERIFY_ROOT/dmg-mount"
DEST_APP="$VERIFY_ROOT/$APP_NAME"
REPORT="$CANDIDATE/logs/VERIFY.txt"
mkdir -p "$CANDIDATE/logs" "$VERIFY_ROOT"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

DMG="$(find "$CANDIDATE" -maxdepth 1 -type f -name '*.dmg' | head -1 || true)"
[[ -n "$DMG" && -f "$DMG" ]] || die "candidate DMG missing under $CANDIDATE"
HASHES_PATH="$CANDIDATE/HASHES.txt"
REQUESTED_PACK_MODE="${IRIN_DMG_PACK_MODE:-}"

# Allow explicit overrides only when they point at the same candidate files.
if [[ -n "${IRIN_DMG_PATH:-}" ]]; then
  [[ "$(cd "$(dirname "$IRIN_DMG_PATH")" && pwd)/$(basename "$IRIN_DMG_PATH")" == "$(cd "$(dirname "$DMG")" && pwd)/$(basename "$DMG")" ]] \
    || die "IRIN_DMG_PATH must be the candidate DMG"
fi
if [[ -n "${IRIN_DMG_HASHES_PATH:-}" ]]; then
  [[ "$(cd "$(dirname "$IRIN_DMG_HASHES_PATH")" && pwd)/$(basename "$IRIN_DMG_HASHES_PATH")" == "$HASHES_PATH" ]] \
    || die "IRIN_DMG_HASHES_PATH must be the candidate HASHES.txt"
fi

IRIN_RELEASE_VERSION="${IRIN_RELEASE_VERSION:-}"

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

sha256_file() { irin_sha256_file "$1"; }

verify_sha256() {
  local key="$1" path="$2" label="$3" expected actual
  expected="$(receipt_sha256 "$key")"
  actual="$(sha256_file "$path")"
  [[ "$actual" == "$expected" ]] || die "$label SHA-256 does not match explicit receipt"
  log "$label SHA-256: receipt match"
}

# Nested DevID / Mach-O inventory (shared with install-verify).
# shellcheck source=/dev/null
source "$ROOT/packaging/codesign-identity.sh"

verify_gateway_manifest() {
  local manifest="$1" mode="$2" version="$3" source_sha="$4"
  # Shared source-binding rules (signed-rc must equal build/receipt SHA).
  irin_assert_gateway_source_binding "$manifest" "$source_sha" "$mode" | tee -a "$REPORT"
  python3 - "$manifest" "$mode" "$version" "$source_sha" <<'PY'
import json
import re
import sys

manifest_path, expected_mode, release_version, source_sha = sys.argv[1:]
with open(manifest_path, encoding="utf-8") as handle:
    data = json.load(handle)
assert data.get("schema_version") == 1, "schema_version must be 1"
watch = data.get("watch_invariants", {})
assert watch.get("WATCH_PRODUCER_ENABLED") is False, "watch producer must ship disabled"
assert watch.get("WATCH_DISPATCHER_ENABLED") is False, "watch dispatcher must ship disabled"
if expected_mode == "production":
    assert re.fullmatch(r"[0-9a-f]{40}", source_sha), "receipt source_sha is invalid"
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
print("Gateway Pack manifest: schema, mode, images, and watch-off invariants verified")
PY
}

# Recompute candidate identity and refuse mismatch.
# Hash the on-disk bytes (do not $(cat) — trailing LF is identity-critical).
verify_candidate_identity() {
  local recomputed_id expected_id tmp_canon
  recomputed_id="$(irin_sha256_file "$CANDIDATE/candidate.json")"
  expected_id="$(basename "$CANDIDATE")"
  [[ "$recomputed_id" == "$expected_id" ]] \
    || die "candidate-id does not recompute from candidate.json (store=$expected_id recomputed=$recomputed_id)"
  tmp_canon="$(mktemp)"
  irin_canonical_identity_json <"$CANDIDATE/candidate.json" >"$tmp_canon"
  if ! cmp -s "$CANDIDATE/candidate.json" "$tmp_canon"; then
    rm -f "$tmp_canon"
    die "candidate.json is not in canonical identity form"
  fi
  rm -f "$tmp_canon"
  log "candidate_id: recomputes from canonical candidate.json"
}

mkdir -p "$CANDIDATE/logs"
: >"$REPORT"
log "=== verify-dmg $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "ROOT=$ROOT"
log "CANDIDATE=$CANDIDATE"
log "DMG=$DMG"

[[ -f "$DMG" ]] || die "missing DMG: $DMG"
[[ -f "$HASHES_PATH" ]] || die "HASHES.txt missing: $HASHES_PATH"
case "$REQUESTED_PACK_MODE" in
  ""|local-dev|signed-rc|production) ;;
  *) die "IRIN_DMG_PACK_MODE must be local-dev, signed-rc, or production (got $REQUESTED_PACK_MODE)" ;;
esac
RECEIPT_PACK_MODE="$(receipt_value pack_mode)"
case "$RECEIPT_PACK_MODE" in
  local-dev|signed-rc|production) ;;
  *) die "receipt pack_mode must be local-dev, signed-rc, or production (got $RECEIPT_PACK_MODE)" ;;
esac
if [[ -n "$REQUESTED_PACK_MODE" && "$REQUESTED_PACK_MODE" != "$RECEIPT_PACK_MODE" ]]; then
  die "requested pack mode $REQUESTED_PACK_MODE does not match receipt pack_mode=$RECEIPT_PACK_MODE"
fi
RECEIPT_RELEASE_VERSION="$(receipt_value release_version)"
if [[ -n "$IRIN_RELEASE_VERSION" && "$IRIN_RELEASE_VERSION" != "$RECEIPT_RELEASE_VERSION" ]]; then
  die "receipt release_version=$RECEIPT_RELEASE_VERSION does not match IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION"
fi
IRIN_RELEASE_VERSION="$RECEIPT_RELEASE_VERSION"
export IRIN_RELEASE_VERSION
log "HASHES=$HASHES_PATH (candidate)"
log "receipt_pack_mode=$RECEIPT_PACK_MODE"
log "IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION"

[[ "$(uname -m)" == "arm64" ]] || die "arm64 only"
verify_sha256 dmg_sha256 "$DMG" "DMG"
verify_candidate_identity

# Optional: re-check bundle-manifest digest against stored app (store copy).
if [[ -f "$CANDIDATE/bundle-manifest.txt" ]]; then
  expected_bm="$(receipt_value bundle_manifest_digest 2>/dev/null || true)"
  if [[ -n "$expected_bm" ]]; then
    actual_bm="$(sha256_file "$CANDIDATE/bundle-manifest.txt")"
    [[ "$actual_bm" == "$expected_bm" ]] \
      || die "bundle-manifest.txt digest does not match HASHES receipt"
    log "bundle-manifest.txt: receipt match"
  fi
fi

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
  [[ "$RECEIPT_PACK_MODE" == "production" || "$RECEIPT_PACK_MODE" == "signed-rc" ]] \
    || die "Developer ID-signed app cannot be verified with a local-dev receipt"
elif [[ "$RECEIPT_PACK_MODE" == "production" || "$RECEIPT_PACK_MODE" == "signed-rc" \
  || "$REQUESTED_PACK_MODE" == "production" || "$REQUESTED_PACK_MODE" == "signed-rc" ]]; then
  die "$RECEIPT_PACK_MODE verification requires a Developer ID Application signature"
fi
log "effective_pack_mode=$VERIFY_PACK_MODE"

if [[ "$VERIFY_PACK_MODE" == "production" || "$VERIFY_PACK_MODE" == "signed-rc" ]]; then
  log "=== Developer ID assertions: every Mach-O, identity, runtime ==="
  irin_assert_nested_developer_id_identity "$DEST_APP"
fi

if [[ "$VERIFY_PACK_MODE" == "production" ]]; then
  log "=== production-only: Gatekeeper + staple ==="
  spctl --assess --type execute -vv "$DEST_APP" 2>&1 | tee -a "$REPORT" \
    || die "Gatekeeper assessment failed on untouched copy"
  xcrun stapler validate "$DMG" 2>&1 | tee -a "$REPORT" \
    || die "DMG is not stapled"
  RECEIPT_STAPLED="$(receipt_value stapled 2>/dev/null || echo true)"
  [[ "$RECEIPT_STAPLED" == "true" ]] || die "production receipt must record stapled=true"
fi

if [[ "$VERIFY_PACK_MODE" == "signed-rc" ]]; then
  RECEIPT_STAPLED="$(receipt_value stapled 2>/dev/null || echo false)"
  [[ "$RECEIPT_STAPLED" == "false" ]] || die "signed-rc receipt must record stapled=false"
  if xcrun stapler validate "$DMG" >/dev/null 2>&1; then
    die "signed-rc DMG must not be stapled"
  fi
  log "signed-rc: confirmed unstapled (non-publishable)"
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

# Tier-bearing proof envelope (atomic). Not part of the immutable payload tree.
CANDIDATE_ID="$(basename "$CANDIDATE")"
DMG_SHA256_VALUE="$(receipt_sha256 dmg_sha256)"
BUNDLE_DIGEST_VALUE="$(receipt_value bundle_manifest_digest 2>/dev/null || true)"
if [[ -z "$BUNDLE_DIGEST_VALUE" && -f "$CANDIDATE/bundle-manifest.txt" ]]; then
  BUNDLE_DIGEST_VALUE="$(sha256_file "$CANDIDATE/bundle-manifest.txt")"
fi
EXTRA_PROOF="$(python3 - <<PY
import json
print(json.dumps({
  "dmg_sha256": "$DMG_SHA256_VALUE",
  "bundle_manifest_digest": "$BUNDLE_DIGEST_VALUE",
  "pack_mode": "$VERIFY_PACK_MODE",
  "release_version": "$IRIN_RELEASE_VERSION",
  "tool": "packaging/verify-dmg.sh",
}))
PY
)"
irin_write_proof_envelope \
  "$CANDIDATE/proofs/verify.json" \
  "verify" \
  "$CANDIDATE_ID" \
  "$RECEIPT_SOURCE_SHA" \
  "PASS" \
  "$EXTRA_PROOF"
log "proofs/verify.json: written (result=PASS)"

log "=== verify-dmg PASS ==="
log "candidate=$CANDIDATE"
log "dest_app=$DEST_APP"
