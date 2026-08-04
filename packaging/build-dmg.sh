#!/usr/bin/env bash
# Build a self-contained aarch64 IRIN .app + .dmg into the durable candidate store.
#
# Output layout (IRIN_CANDIDATE_ROOT, default ~/.local/state/irin/candidates):
#   .staging/<attempt-id>/          build workspace (moved on success/fail)
#   .attempts/<attempt-id>.json     attempt metadata (outside immutable payload)
#   <version>/<source-sha>/<candidate-id>/
#     candidate.json   IRIN.app   IRIN_<ver>_aarch64.dmg
#     HASHES.txt       bundle-manifest.txt
#     proofs/  smoke/  install/  logs/
#
# packaging/artifacts/ is never identity.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

[[ "$(uname -s)" == "Darwin" ]] || { echo "ERROR: macOS only" >&2; exit 1; }
[[ "$(uname -m)" == "arm64" ]] || { echo "ERROR: aarch64/Apple silicon only" >&2; exit 1; }

TAURI_DIR="$IRIN_SRC/council-rs/warroom-tauri"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

sha256_file() { irin_sha256_file "$1"; }

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
    die "unexpected Mach-O inventory; update the explicit signing order before shipping"
  }
}

verify_developer_id_signature() {
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
    die "$label contains entitlements, but IRIN 0.1.2 declares none; review and document before shipping"
  fi
}

# Packaging mode:
#   local-dev  — ad-hoc sign; non-publishable (Phase A)
#   signed-rc  — Developer ID + hardened runtime; no GHCR/notary/staple (Phase B)
#   production — Developer ID + notarize + staple; publishable (Phase C)
PACK_MODE="${IRIN_DMG_PACK_MODE:-local-dev}"
case "$PACK_MODE" in
  local-dev|signed-rc|production) ;;
  *) die "IRIN_DMG_PACK_MODE must be local-dev, signed-rc, or production (got $PACK_MODE)" ;;
esac

# Gateway Pack staging modes only understand local-dev | production | smoke-inert.
# signed-rc binds local staged Gateway/sidecar inputs to the source SHA without
# claiming registry provenance.
case "$PACK_MODE" in
  production) GATEWAY_PACK_MODE="production" ;;
  *) GATEWAY_PACK_MODE="local-dev" ;;
esac
export IRIN_GATEWAY_PACK_MODE="$GATEWAY_PACK_MODE"

# Release version names the DMG artifact and candidate store path. Defaults from
# tauri.conf.json for local-dev and signed-rc. Production must set
# IRIN_RELEASE_VERSION explicitly (the release transaction exports it from the
# tag). Every mode must equal the Tauri bundle version so candidate identity
# matches the app About/version surface.
TAURI_CONF="$TAURI_DIR/src-tauri/tauri.conf.json"
TAURI_BUNDLE_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$TAURI_CONF")" \
  || die "could not read version from $TAURI_CONF (python3 required)"
if [[ "$PACK_MODE" == "production" && -z "${IRIN_RELEASE_VERSION:-}" ]]; then
  die "production DMG requires IRIN_RELEASE_VERSION set explicitly (the release transaction exports it from the tag)"
fi
IRIN_RELEASE_VERSION="${IRIN_RELEASE_VERSION:-$TAURI_BUNDLE_VERSION}"
export IRIN_RELEASE_VERSION
if [[ "$IRIN_RELEASE_VERSION" != "$TAURI_BUNDLE_VERSION" ]]; then
  die "IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION != tauri.conf.json version=$TAURI_BUNDLE_VERSION; bump the version in council-rs/warroom-tauri/src-tauri/tauri.conf.json in its own commit before building a candidate"
fi

REQUIRE_CLEAN="${IRIN_DMG_REQUIRE_CLEAN:-1}"
if [[ "$REQUIRE_CLEAN" == "1" ]]; then
  if [[ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null || true)" ]]; then
    die "working tree is dirty; commit first so host+council embed a clean SHA (IRIN_DMG_REQUIRE_CLEAN=0 to override)"
  fi
  export IRIN_TAURI_BUILD_DIRTY=false
  export COUNCIL_BUILD_DIRTY=false
  IRIN_TAURI_BUILD_GIT_SHA="$(git -C "$ROOT" rev-parse HEAD)"
  export IRIN_TAURI_BUILD_GIT_SHA
  export COUNCIL_BUILD_GIT_SHA="$IRIN_TAURI_BUILD_GIT_SHA"
fi

SOURCE_SHA="${IRIN_TAURI_BUILD_GIT_SHA:-}"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] \
  || die "full 40-char source SHA required (IRIN_TAURI_BUILD_GIT_SHA); got ${SOURCE_SHA:-empty}"

if [[ "$PACK_MODE" == "production" ]]; then
  [[ -n "${IRIN_GATEWAY_PACK_PROD_MANIFEST:-}" ]] || die \
    "production DMG requires IRIN_GATEWAY_PACK_PROD_MANIFEST (explicit production image manifest)"
  [[ -f "$IRIN_GATEWAY_PACK_PROD_MANIFEST" ]] || die \
    "production manifest missing: $IRIN_GATEWAY_PACK_PROD_MANIFEST"
  if grep -q '"mode"[[:space:]]*:[[:space:]]*"local-dev"' "$IRIN_GATEWAY_PACK_PROD_MANIFEST"; then
    die "production DMG refuses a local-dev Gateway Pack manifest"
  fi
  LOCAL_LEFTOVER="$ROOT/packaging/build/gateway-pack/image-manifest.local.json"
  if [[ -f "$LOCAL_LEFTOVER" ]] && [[ "$(cd "$(dirname "$IRIN_GATEWAY_PACK_PROD_MANIFEST")" && pwd)/$(basename "$IRIN_GATEWAY_PACK_PROD_MANIFEST")" == "$(cd "$(dirname "$LOCAL_LEFTOVER")" && pwd)/$(basename "$LOCAL_LEFTOVER")" ]]; then
    die "production DMG refuses packaging/build/gateway-pack/image-manifest.local.json"
  fi
  if [[ "$REQUIRE_CLEAN" != "1" ]]; then
    die "production DMG requires a clean tree (IRIN_DMG_REQUIRE_CLEAN=1)"
  fi
  echo "=== verify production image provenance before build ==="
  bash "$ROOT/scripts/verify-production-image-provenance.sh" \
    "$IRIN_GATEWAY_PACK_PROD_MANIFEST" "$IRIN_TAURI_BUILD_GIT_SHA" "$IRIN_RELEASE_VERSION" \
    || die "production Gateway Pack manifest failed immutable registry provenance verification"
fi

if [[ "$PACK_MODE" == "signed-rc" || "$PACK_MODE" == "production" ]]; then
  : "${APPLE_SIGNING_IDENTITY:?$PACK_MODE mode requires APPLE_SIGNING_IDENTITY}"
fi
if [[ "$PACK_MODE" == "production" ]]; then
  : "${APPLE_NOTARY_PROFILE:?production mode requires APPLE_NOTARY_PROFILE}"
fi

# --- Candidate staging -------------------------------------------------------

irin_resolve_candidate_root
ATTEMPT_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
STAGING_ROOT="$IRIN_CANDIDATE_ROOT/.staging"
ATTEMPTS_ROOT="$IRIN_CANDIDATE_ROOT/.attempts"
STAGING="$STAGING_ROOT/$ATTEMPT_ID"
mkdir -p "$STAGING" "$ATTEMPTS_ROOT" \
  "$STAGING/proofs" "$STAGING/smoke" "$STAGING/install" "$STAGING/logs"

ATTEMPT_META="$ATTEMPTS_ROOT/${ATTEMPT_ID}.json"
ATTEMPT_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CANDIDATE_FINALIZED=0
CANDIDATE_PATH=""

write_attempt_meta() {
  local result="$1" finished candidate_id candidate_path
  finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  candidate_id="${2:-}"
  candidate_path="${3:-}"
  # Paths/IDs via env — never interpolate into an unquoted Python string
  # (IRIN_CANDIDATE_ROOT may contain quotes/backslashes).
  ATTEMPT_ID="$ATTEMPT_ID" \
  ATTEMPT_STARTED_AT="$ATTEMPT_STARTED_AT" \
  FINISHED="$finished" \
  SOURCE_SHA="$SOURCE_SHA" \
  IRIN_RELEASE_VERSION="$IRIN_RELEASE_VERSION" \
  PACK_MODE="$PACK_MODE" \
  RESULT="$result" \
  CANDIDATE_ID="$candidate_id" \
  CANDIDATE_PATH="$candidate_path" \
  python3 - "$ATTEMPT_META" <<'PY'
import json, os, sys
path = sys.argv[1]
doc = {
  "attempt_id": os.environ["ATTEMPT_ID"],
  "started_at": os.environ["ATTEMPT_STARTED_AT"],
  "finished_at": os.environ["FINISHED"],
  "source_sha": os.environ["SOURCE_SHA"],
  "semver": os.environ["IRIN_RELEASE_VERSION"],
  "pack_mode": os.environ["PACK_MODE"],
  "result": os.environ["RESULT"],
}
if os.environ.get("CANDIDATE_ID"):
    doc["candidate_id"] = os.environ["CANDIDATE_ID"]
if os.environ.get("CANDIDATE_PATH"):
    doc["candidate_path"] = os.environ["CANDIDATE_PATH"]
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY
}

on_fail() {
  local status=$?
  if [[ "$CANDIDATE_FINALIZED" == "1" ]]; then
    exit "$status"
  fi
  # Move failed staging under version/sha/failed/<attempt-id>/; never promote later.
  if [[ -d "$STAGING" ]]; then
    local fail_parent fail_dest
    fail_parent="$IRIN_CANDIDATE_ROOT/$IRIN_RELEASE_VERSION/$SOURCE_SHA/failed"
    fail_dest="$fail_parent/$ATTEMPT_ID"
    mkdir -p "$fail_parent"
    if [[ ! -e "$fail_dest" ]]; then
      mv "$STAGING" "$fail_dest" 2>/dev/null || true
    fi
  fi
  write_attempt_meta "failed" || true
  exit "$status"
}
trap on_fail EXIT

echo "=== IRIN DMG build ==="
echo "ROOT=$ROOT"
echo "PACK_MODE=$PACK_MODE"
echo "GATEWAY_PACK_MODE=$GATEWAY_PACK_MODE"
echo "RELEASE_VERSION=$IRIN_RELEASE_VERSION"
echo "BUILD_SHA=$SOURCE_SHA"
echo "BUILD_DIRTY=${IRIN_TAURI_BUILD_DIRTY:-unknown}"
echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
echo "IRIN_CANDIDATE_ROOT=$IRIN_CANDIDATE_ROOT"
echo "ATTEMPT_ID=$ATTEMPT_ID"
echo "STAGING=$STAGING"

# Shared app factory: Council build → stage → Gateway → web export → Tauri app.
export IRIN_GATEWAY_PACK_MODE="$GATEWAY_PACK_MODE"
export IRIN_APP_TARGET_DIR="$CARGO_TARGET_DIR"
export IRIN_TAURI_BUNDLES="${IRIN_TAURI_BUNDLES:-app}"
bash "$ROOT/packaging/build-app-bundle.sh"

# Resolve the app strictly from this build's pinned target dir (env.sh).
APP="$CARGO_TARGET_DIR/release/bundle/macos/IRIN.app"
[[ -d "$APP" ]] || die "app bundle not found at $APP (app-bundle primitive did not produce it)"
if grep -Rql 'smoke-inert' "$APP" 2>/dev/null; then
  die "finished app contains smoke-inert marker; refusing to package"
fi

echo "=== ad-hoc codesign (build artifact only; never use production credentials) ==="
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"
codesign -dv --verbose=2 "$APP" 2>&1 | head -20 || true

SIDECAR="$APP/Contents/MacOS/council"
[[ -x "$SIDECAR" ]] || die "bundled council missing or not executable: $SIDECAR"
if [[ -d "$APP/Contents/Resources/council-base/cabinets" ]]; then
  BUNDLED_BASE="$APP/Contents/Resources/council-base"
else
  FOUND_CABINETS="$(find "$APP/Contents/Resources" -type d -name cabinets 2>/dev/null | head -1 || true)"
  [[ -n "$FOUND_CABINETS" ]] || die "bundled council-base/cabinets missing under Resources"
  BUNDLED_BASE="$(dirname "$FOUND_CABINETS")"
  echo "NOTE: cabinets at $FOUND_CABINETS"
fi
HERMES_ADAPTER="$BUNDLED_BASE/scripts/hermes-seat-adapter.sh"
[[ -x "$HERMES_ADAPTER" ]] \
  || die "bundled hermes seat adapter missing or not executable under base-dir: $HERMES_ADAPTER"
echo "bundled hermes adapter: $HERMES_ADAPTER"

WARROOM_WEB="$APP/Contents/Resources/warroom-web"
[[ -f "$WARROOM_WEB/index.html" ]] \
  || die "bundled War Room export missing: $WARROOM_WEB/index.html"
echo "bundled War Room export: $WARROOM_WEB"

TOUCH_ID_HELPER="$APP/Contents/Helpers/arm-attest"
[[ -x "$TOUCH_ID_HELPER" && -s "$TOUCH_ID_HELPER" ]] \
  || die "bundled Touch ID helper missing, empty, or not executable: $TOUCH_ID_HELPER"
echo "bundled Touch ID helper: $TOUCH_ID_HELPER"

# Stage the shippable copy under the candidate attempt (not packaging/artifacts).
DEST_APP="$STAGING/IRIN.app"
DEST_DMG="$STAGING/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg"
rm -rf "$DEST_APP"
ditto "$APP" "$DEST_APP"

if [[ "$PACK_MODE" == "signed-rc" || "$PACK_MODE" == "production" ]]; then
  assert_expected_macho_inventory "$DEST_APP"
  echo "=== Developer ID signing (inside-out, hardened runtime; mode=$PACK_MODE) ==="
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" \
    "$DEST_APP/Contents/Helpers/arm-attest"
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" \
    "$DEST_APP/Contents/MacOS/council"
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" \
    "$DEST_APP/Contents/MacOS/council-warroom-tauri"
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" \
    "$DEST_APP"
  codesign --verify --deep --strict "$DEST_APP"
  OUTER_DETAILS="$(codesign -dv --verbose=4 "$DEST_APP" 2>&1)" \
    || die "could not inspect outer app signature"
  OUTER_TEAM="$(awk -F= '$1 == "TeamIdentifier" { print $2; exit }' <<<"$OUTER_DETAILS")"
  [[ -n "$OUTER_TEAM" ]] || die "outer app signature has no TeamIdentifier"
  verify_developer_id_signature "$DEST_APP" "$OUTER_TEAM" "outer app"
  verify_developer_id_signature "$DEST_APP/Contents/Helpers/arm-attest" "$OUTER_TEAM" "Touch ID helper"
  verify_developer_id_signature "$DEST_APP/Contents/MacOS/council" "$OUTER_TEAM" "Council sidecar"
  verify_developer_id_signature "$DEST_APP/Contents/MacOS/council-warroom-tauri" "$OUTER_TEAM" "Tauri host"
  assert_expected_macho_inventory "$DEST_APP"
else
  codesign --force --deep --sign - "$DEST_APP"
  codesign --verify --deep --strict "$DEST_APP"
fi

echo "=== hdiutil DMG ($PACK_MODE) ==="
DMG_STAGE="$STAGING/.dmg-stage"
rm -rf "$DMG_STAGE"
mkdir -p "$DMG_STAGE"
ditto "$DEST_APP" "$DMG_STAGE/IRIN.app"
ln -sf /Applications "$DMG_STAGE/Applications"
rm -f "$DEST_DMG"
hdiutil create -volname "IRIN" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DEST_DMG"
rm -rf "$DMG_STAGE"

STAPLED=false
NOTARY_SUBMISSION_ID=""
NOTARY_SUBMIT_JSON=""
NOTARY_LOG_JSON=""
if [[ "$PACK_MODE" == "production" ]]; then
  echo "=== sign DMG, notarize, retain log, staple ==="
  codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$DEST_DMG"
  NOTARY_SUBMIT_JSON="$STAGING/logs/NOTARY-SUBMIT-${IRIN_RELEASE_VERSION}.json"
  NOTARY_LOG_JSON="$STAGING/logs/NOTARY-LOG-${IRIN_RELEASE_VERSION}.json"
  xcrun notarytool submit --keychain-profile "$APPLE_NOTARY_PROFILE" \
    --wait --output-format json "$DEST_DMG" >"$NOTARY_SUBMIT_JSON"
  NOTARY_SUBMISSION_ID="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d.get("status") == "Accepted" or sys.exit("notary status is not Accepted"); i=d.get("id", ""); i or sys.exit("notary response has no submission id"); print(i)' "$NOTARY_SUBMIT_JSON")" \
    || die "Apple notarization was not accepted; inspect $NOTARY_SUBMIT_JSON"
  xcrun notarytool log --keychain-profile "$APPLE_NOTARY_PROFILE" \
    "$NOTARY_SUBMISSION_ID" "$NOTARY_LOG_JSON"
  python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); issues=d.get("issues", []); issues and sys.exit("notary log contains issues"); print("notary log: Accepted with zero issues")' "$NOTARY_LOG_JSON" \
    || die "Apple notarization log contains issues; inspect $NOTARY_LOG_JSON"
  xcrun stapler staple "$DEST_DMG"
  xcrun stapler validate "$DEST_DMG"
  STAPLED=true
elif [[ "$PACK_MODE" == "signed-rc" ]]; then
  echo "=== signed-rc: Developer ID app only; DMG unsigned; no notarize/staple ==="
  # Phase B is non-publishable. Keep the DMG un-notarized/un-stapled.
  STAPLED=false
fi

# Post-staple (or post-create) DMG SHA-256 is publishing identity for production.
APP_SHA256="$(sha256_file "$DEST_APP/Contents/MacOS/council-warroom-tauri")"
COUNCIL_SHA256="$(sha256_file "$DEST_APP/Contents/MacOS/council")"
ARM_ATTEST_SHA256="$(sha256_file "$DEST_APP/Contents/Helpers/arm-attest")"
GATEWAY_PACK_COMPOSE_SHA256="$(sha256_file "$DEST_APP/Contents/Resources/gateway-pack/docker-compose.yml")"
GATEWAY_PACK_MANIFEST_SHA256="$(sha256_file "$DEST_APP/Contents/Resources/gateway-pack/image-manifest.json")"
WARROOM_WEB_INDEX_SHA256="$(sha256_file "$DEST_APP/Contents/Resources/warroom-web/index.html")"
DMG_SHA256="$(sha256_file "$DEST_DMG")"

IMAGE_MANIFEST="$DEST_APP/Contents/Resources/gateway-pack/image-manifest.json"
IMAGE_DIGESTS="$(irin_image_digests_from_manifest "$IMAGE_MANIFEST")" \
  || die "could not extract gateway/sidecar digests from $IMAGE_MANIFEST"
GATEWAY_DIGEST="$(printf '%s\n' "$IMAGE_DIGESTS" | sed -n '1p')"
SIDECAR_DIGEST="$(printf '%s\n' "$IMAGE_DIGESTS" | sed -n '2p')"
[[ -n "$GATEWAY_DIGEST" && -n "$SIDECAR_DIGEST" ]] \
  || die "could not extract gateway/sidecar digests from $IMAGE_MANIFEST"

echo "=== gateway source binding ==="
irin_assert_gateway_source_binding "$IMAGE_MANIFEST" "$SOURCE_SHA" "$PACK_MODE"

echo "=== write bundle-manifest.txt ==="
BUNDLE_MANIFEST="$STAGING/bundle-manifest.txt"
irin_write_bundle_manifest "$DEST_APP" "$BUNDLE_MANIFEST"
BUNDLE_MANIFEST_DIGEST="$(sha256_file "$BUNDLE_MANIFEST")"

# HASHES.txt is part of the immutable payload tree used for exact-retry.
# It must be byte-deterministic for identical artifact bytes: no timestamps,
# no attempt-specific absolute staging paths, no notary submission IDs.
# Operator diagnostics (built_at, notary ids, notes) go to logs/build-meta.txt.
DMG_BASENAME="IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg"
HASHES_PATH="$STAGING/HASHES.txt"
RELEASABLE=false
[[ "$PACK_MODE" == "production" && "$STAPLED" == "true" ]] && RELEASABLE=true
{
  echo "pack_mode=$PACK_MODE"
  echo "release_version=$IRIN_RELEASE_VERSION"
  echo "releasable=$RELEASABLE"
  echo "stapled=$STAPLED"
  echo "source_sha=$SOURCE_SHA"
  echo "build_dirty=${IRIN_TAURI_BUILD_DIRTY:-unknown}"
  echo "arch=aarch64-apple-darwin"
  echo "app=IRIN.app"
  echo "dmg=$DMG_BASENAME"
  echo "app_sha256=$APP_SHA256"
  echo "council_sha256=$COUNCIL_SHA256"
  echo "arm_attest_sha256=$ARM_ATTEST_SHA256"
  echo "gateway_pack_compose_sha256=$GATEWAY_PACK_COMPOSE_SHA256"
  echo "gateway_pack_manifest_sha256=$GATEWAY_PACK_MANIFEST_SHA256"
  echo "gateway_digest=$GATEWAY_DIGEST"
  echo "sidecar_digest=$SIDECAR_DIGEST"
  echo "warroom_web_index_sha256=$WARROOM_WEB_INDEX_SHA256"
  echo "bundle_manifest_digest=$BUNDLE_MANIFEST_DIGEST"
  echo "dmg_sha256=$DMG_SHA256"
} | tee "$HASHES_PATH"

{
  echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "attempt_id=$ATTEMPT_ID"
  echo "pack_mode=$PACK_MODE"
  echo "source_sha=$SOURCE_SHA"
  echo "release_version=$IRIN_RELEASE_VERSION"
  if [[ "$PACK_MODE" == "production" ]]; then
    echo "notary_submission_id=$NOTARY_SUBMISSION_ID"
    echo "notary_submit_receipt=$(basename "${NOTARY_SUBMIT_JSON:-}")"
    echo "notary_log_receipt=$(basename "${NOTARY_LOG_JSON:-}")"
  fi
  case "$PACK_MODE" in
    local-dev)
      echo "note=local-dev candidate; not for notarization or production promotion"
      ;;
    signed-rc)
      echo "note=signed-rc candidate; Developer ID only; non-publishable; T1 biometry/visual only"
      ;;
  esac
} >"$STAGING/logs/build-meta.txt"

# Immutable identity document only — no ID, timestamp, attempt, or status.
IDENTITY_JSON="$(python3 - <<PY
import json
print(json.dumps({
  "schema_version": 1,
  "source_sha": "$SOURCE_SHA",
  "semver": "$IRIN_RELEASE_VERSION",
  "pack_mode": "$PACK_MODE",
  "bundle_manifest_digest": "$BUNDLE_MANIFEST_DIGEST",
  "dmg_sha256": "$DMG_SHA256",
  "stapled": True if "$STAPLED" == "true" else False,
  "gateway_digest": "$GATEWAY_DIGEST",
  "sidecar_digest": "$SIDECAR_DIGEST",
}))
PY
)"
# Write canonical identity directly to disk — never capture via $(...) which
# strips the required trailing LF and would desync candidate-id from the file.
printf '%s' "$IDENTITY_JSON" | irin_canonical_identity_json >"$STAGING/candidate.json"
CANDIDATE_ID="$(irin_sha256_file "$STAGING/candidate.json")"
[[ "$CANDIDATE_ID" =~ ^[0-9a-f]{64}$ ]] || die "invalid candidate-id"

# Production/publishable candidates require stapled=true; signed-rc requires false.
case "$PACK_MODE" in
  production)
    [[ "$STAPLED" == "true" ]] || die "production candidate requires stapled=true"
    ;;
  signed-rc)
    [[ "$STAPLED" == "false" ]] || die "signed-rc candidate requires stapled=false"
    ;;
  local-dev)
    [[ "$STAPLED" == "false" ]] || die "local-dev candidate requires stapled=false"
    ;;
esac

# Payload hash excludes proofs/smoke/install/logs mutability by design
# (only candidate.json + HASHES + bundle-manifest + DMG + IRIN.app leaves).
# logs/build-meta.txt is diagnostic and is NOT in the immutable payload set.
PAYLOAD_HASH="$(irin_payload_tree_hash "$STAGING")"
DEST_PARENT="$IRIN_CANDIDATE_ROOT/$IRIN_RELEASE_VERSION/$SOURCE_SHA"
DEST="$DEST_PARENT/$CANDIDATE_ID"

echo "=== promote staging → candidate store (exclusive claim) ==="
PROMOTE_RESULT="$(irin_promote_candidate_from_staging "$STAGING" "$DEST")" \
  || die "candidate promote failed"
CANDIDATE_PATH="$DEST"
if [[ "$PROMOTE_RESULT" == "idempotent" ]]; then
  echo "idempotent hit: payload tree identical; discarding staging"
  rm -rf "$STAGING"
  write_attempt_meta "success_idempotent" "$CANDIDATE_ID" "$DEST"
  CANDIDATE_FINALIZED=1
  trap - EXIT
  echo "=== build complete (idempotent) ==="
  echo "candidate_id=$CANDIDATE_ID"
  echo "candidate_path=$DEST"
  echo "dmg_sha256=$DMG_SHA256"
  echo "stapled=$STAPLED"
  echo "payload_tree_hash=$PAYLOAD_HASH"
  exit 0
fi
[[ "$PROMOTE_RESULT" == "created" ]] || die "unexpected promote result: $PROMOTE_RESULT"
write_attempt_meta "success" "$CANDIDATE_ID" "$DEST"
CANDIDATE_FINALIZED=1
trap - EXIT

echo "=== build complete ==="
echo "candidate_id=$CANDIDATE_ID"
echo "candidate_path=$CANDIDATE_PATH"
echo "dmg_sha256=$DMG_SHA256"
echo "stapled=$STAPLED"
echo "payload_tree_hash=$PAYLOAD_HASH"
ls -lah "$CANDIDATE_PATH/IRIN.app" "$CANDIDATE_PATH/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg"
du -sh "$CANDIDATE_PATH/IRIN.app" "$CANDIDATE_PATH/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg"
