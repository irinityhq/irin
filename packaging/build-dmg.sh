#!/usr/bin/env bash
# Build a self-contained aarch64 IRIN .app + .dmg from this monorepo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

[[ "$(uname -s)" == "Darwin" ]] || { echo "ERROR: macOS only" >&2; exit 1; }
[[ "$(uname -m)" == "arm64" ]] || { echo "ERROR: aarch64/Apple silicon only" >&2; exit 1; }

TAURI_DIR="$IRIN_SRC/council-rs/warroom-tauri"
WEB_DIR="$IRIN_SRC/council-rs/warroom/web"
STAGE_SCRIPT="$TAURI_DIR/scripts/stage-bundle-inputs.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

sha256_file() {
  local path="$1" value
  value="$(shasum -a 256 "$path" | awk '{print $1}')" \
    || die "could not hash artifact: $path"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || die "invalid SHA-256 result for artifact: $path"
  printf '%s' "$value"
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
    die "unexpected Mach-O inventory; update the explicit signing order before shipping"
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
    die "$label contains entitlements, but IRIN 0.1.2 declares none; review and document before shipping"
  fi
}

# Packaging mode: local-dev (default, non-releasable) or production (strict).
# A release build must set IRIN_DMG_PACK_MODE=production and supply a real
# production Gateway Pack manifest. local-dev cannot be notarized by the
# release target and is visibly labeled in HASHES.txt.
PACK_MODE="${IRIN_DMG_PACK_MODE:-local-dev}"
case "$PACK_MODE" in
  local-dev|production) ;;
  *) die "IRIN_DMG_PACK_MODE must be local-dev or production (got $PACK_MODE)" ;;
esac
export IRIN_GATEWAY_PACK_MODE="$PACK_MODE"

# Release version names the DMG artifact. local-dev defaults to 0.1.2 for
# backward compatibility; production must set IRIN_RELEASE_VERSION explicitly
# (the release transaction exports it from the tag) and it must equal the
# Tauri bundle version, or the DMG would mislabel the app it ships.
if [[ "$PACK_MODE" == "production" && -z "${IRIN_RELEASE_VERSION:-}" ]]; then
  die "production DMG requires IRIN_RELEASE_VERSION set explicitly (the release transaction exports it from the tag)"
fi
IRIN_RELEASE_VERSION="${IRIN_RELEASE_VERSION:-0.1.2}"
export IRIN_RELEASE_VERSION
TAURI_CONF="$TAURI_DIR/src-tauri/tauri.conf.json"
TAURI_BUNDLE_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$TAURI_CONF")" \
  || die "could not read version from $TAURI_CONF (python3 required)"
if [[ "$PACK_MODE" == "production" && "$IRIN_RELEASE_VERSION" != "$TAURI_BUNDLE_VERSION" ]]; then
  die "IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION != tauri.conf.json version=$TAURI_BUNDLE_VERSION; bump the version in council-rs/warroom-tauri/src-tauri/tauri.conf.json in its own commit before running the release transaction"
fi

REQUIRE_CLEAN="${IRIN_DMG_REQUIRE_CLEAN:-1}"
if [[ "$REQUIRE_CLEAN" == "1" ]]; then
  if [[ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null || true)" ]]; then
    die "working tree is dirty; commit first so host+council embed a clean SHA (IRIN_DMG_REQUIRE_CLEAN=0 to override)"
  fi
  export IRIN_TAURI_BUILD_DIRTY=false
  export COUNCIL_BUILD_DIRTY=false
  export IRIN_TAURI_BUILD_GIT_SHA
  IRIN_TAURI_BUILD_GIT_SHA="$(git -C "$ROOT" rev-parse HEAD)"
  export IRIN_TAURI_BUILD_GIT_SHA
  export COUNCIL_BUILD_GIT_SHA="$IRIN_TAURI_BUILD_GIT_SHA"
fi

if [[ "$PACK_MODE" == "production" ]]; then
  [[ -n "${IRIN_GATEWAY_PACK_PROD_MANIFEST:-}" ]] || die \
    "production DMG requires IRIN_GATEWAY_PACK_PROD_MANIFEST (explicit production image manifest)"
  [[ -f "$IRIN_GATEWAY_PACK_PROD_MANIFEST" ]] || die \
    "production manifest missing: $IRIN_GATEWAY_PACK_PROD_MANIFEST"
  if grep -q '"mode"[[:space:]]*:[[:space:]]*"local-dev"' "$IRIN_GATEWAY_PACK_PROD_MANIFEST"; then
    die "production DMG refuses a local-dev Gateway Pack manifest"
  fi
  # Refuse leftover local-dev build output as production input.
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

echo "=== IRIN DMG build ==="
echo "ROOT=$ROOT"
echo "PACK_MODE=$PACK_MODE"
echo "RELEASE_VERSION=$IRIN_RELEASE_VERSION"
echo "BUILD_SHA=${IRIN_TAURI_BUILD_GIT_SHA:-unknown}"
echo "BUILD_DIRTY=${IRIN_TAURI_BUILD_DIRTY:-unknown}"
echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"

echo "=== cargo build council (release, aarch64) ==="
(
  cd "$IRIN_SRC"
  cargo build --release -p council-rs --bin council
)

echo "=== stage bundled council + base-dir resources ==="
bash "$STAGE_SCRIPT"

echo "=== stage Gateway Pack runtime assets (mode=$PACK_MODE) ==="
bash "$ROOT/scripts/stage-gateway-pack.sh"

echo "=== npm ci warroom web + tauri ==="
(
  cd "$WEB_DIR"
  if [[ -f package-lock.json ]]; then
    npm ci --prefer-offline --no-audit --progress=false
  else
    npm install --no-audit --progress=false
  fi
)
(
  cd "$TAURI_DIR"
  if [[ -f package-lock.json ]]; then
    npm ci --prefer-offline --no-audit --progress=false
  else
    npm install --no-audit --progress=false
  fi
)

echo "=== tauri build (app + dmg) ==="
(
  cd "$TAURI_DIR"
  # Keep host provenance aligned with env (packaging isolation may use separate target dir).
  export IRIN_TAURI_BUILD_GIT_SHA COUNCIL_BUILD_GIT_SHA
  export IRIN_TAURI_BUILD_DIRTY COUNCIL_BUILD_DIRTY
  npm run tauri build -- --bundles app,dmg
)

# Resolve the app strictly from this build's pinned target dir (env.sh).
# Never scavenge other target dirs: a stale foreign build (e.g. a port-isolated
# smoke app with a different baked-in Council port) would be packaged silently.
APP="$CARGO_TARGET_DIR/release/bundle/macos/IRIN.app"
[[ -d "$APP" ]] || die "app bundle not found at $APP (tauri build did not produce it)"

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
# Fail closed: packaged app must ship executable hermes seat adapter under base-dir.
HERMES_ADAPTER="$BUNDLED_BASE/scripts/hermes-seat-adapter.sh"
[[ -x "$HERMES_ADAPTER" ]] \
  || die "bundled hermes seat adapter missing or not executable under base-dir: $HERMES_ADAPTER"
echo "bundled hermes adapter: $HERMES_ADAPTER"

# Council serves this same static export on :8765 for private tailnet access.
# The desktop webview still consumes Tauri's frontendDist from the bundle.
WARROOM_WEB="$APP/Contents/Resources/warroom-web"
[[ -f "$WARROOM_WEB/index.html" ]] \
  || die "bundled War Room export missing: $WARROOM_WEB/index.html"
echo "bundled War Room export: $WARROOM_WEB"

TOUCH_ID_HELPER="$APP/Contents/Helpers/arm-attest"
[[ -x "$TOUCH_ID_HELPER" && -s "$TOUCH_ID_HELPER" ]] \
  || die "bundled Touch ID helper missing, empty, or not executable: $TOUCH_ID_HELPER"
echo "bundled Touch ID helper: $TOUCH_ID_HELPER"

mkdir -p "$ROOT/packaging/artifacts"
DEST_APP="$ROOT/packaging/artifacts/IRIN.app"
DEST_DMG="$ROOT/packaging/artifacts/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg"
rm -rf "$DEST_APP"
ditto "$APP" "$DEST_APP"

if [[ "$PACK_MODE" == "production" ]]; then
  : "${APPLE_SIGNING_IDENTITY:?production mode requires APPLE_SIGNING_IDENTITY}"
  : "${APPLE_NOTARY_PROFILE:?production mode requires APPLE_NOTARY_PROFILE}"
  assert_expected_macho_inventory "$DEST_APP"
  echo "=== Developer ID signing (inside-out, hardened runtime) ==="
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
  verify_production_signature "$DEST_APP" "$OUTER_TEAM" "outer app"
  verify_production_signature "$DEST_APP/Contents/Helpers/arm-attest" "$OUTER_TEAM" "Touch ID helper"
  verify_production_signature "$DEST_APP/Contents/MacOS/council" "$OUTER_TEAM" "Council sidecar"
  verify_production_signature "$DEST_APP/Contents/MacOS/council-warroom-tauri" "$OUTER_TEAM" "Tauri host"
  assert_expected_macho_inventory "$DEST_APP"
else
  codesign --force --deep --sign - "$DEST_APP"
  codesign --verify --deep --strict "$DEST_APP"
fi

echo "=== hdiutil DMG ($PACK_MODE) ==="
STAGE="$ROOT/packaging/build/dmg-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
ditto "$DEST_APP" "$STAGE/IRIN.app"
ln -sf /Applications "$STAGE/Applications"
rm -f "$DEST_DMG"
hdiutil create -volname "IRIN" -srcfolder "$STAGE" -ov -format UDZO "$DEST_DMG"

NOTARY_SUBMISSION_ID=""
NOTARY_SUBMIT_JSON=""
NOTARY_LOG_JSON=""
if [[ "$PACK_MODE" == "production" ]]; then
  echo "=== sign DMG, notarize, retain log, staple ==="
  codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$DEST_DMG"
  mkdir -p "$ROOT/packaging/receipts"
  NOTARY_SUBMIT_JSON="$ROOT/packaging/receipts/NOTARY-SUBMIT-${IRIN_RELEASE_VERSION}.json"
  NOTARY_LOG_JSON="$ROOT/packaging/receipts/NOTARY-LOG-${IRIN_RELEASE_VERSION}.json"
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
fi

APP_SHA256="$(sha256_file "$DEST_APP/Contents/MacOS/council-warroom-tauri")"
COUNCIL_SHA256="$(sha256_file "$DEST_APP/Contents/MacOS/council")"
ARM_ATTEST_SHA256="$(sha256_file "$DEST_APP/Contents/Helpers/arm-attest")"
GATEWAY_PACK_COMPOSE_SHA256="$(sha256_file "$DEST_APP/Contents/Resources/gateway-pack/docker-compose.yml")"
GATEWAY_PACK_MANIFEST_SHA256="$(sha256_file "$DEST_APP/Contents/Resources/gateway-pack/image-manifest.json")"
WARROOM_WEB_INDEX_SHA256="$(sha256_file "$DEST_APP/Contents/Resources/warroom-web/index.html")"
DMG_SHA256="$(sha256_file "$DEST_DMG")"

{
  echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "pack_mode=$PACK_MODE"
  echo "release_version=$IRIN_RELEASE_VERSION"
  echo "releasable=$([[ "$PACK_MODE" == "production" ]] && echo true || echo false)"
  echo "source_sha=${IRIN_TAURI_BUILD_GIT_SHA:-unknown}"
  echo "build_dirty=${IRIN_TAURI_BUILD_DIRTY:-unknown}"
  echo "arch=aarch64-apple-darwin"
  echo "app=$DEST_APP"
  echo "dmg=$DEST_DMG"
  echo "app_sha256=$APP_SHA256"
  echo "council_sha256=$COUNCIL_SHA256"
  echo "arm_attest_sha256=$ARM_ATTEST_SHA256"
  echo "gateway_pack_compose_sha256=$GATEWAY_PACK_COMPOSE_SHA256"
  echo "gateway_pack_manifest_sha256=$GATEWAY_PACK_MANIFEST_SHA256"
  echo "warroom_web_index_sha256=$WARROOM_WEB_INDEX_SHA256"
  echo "dmg_sha256=$DMG_SHA256"
  if [[ "$PACK_MODE" == "production" ]]; then
    echo "notary_submission_id=$NOTARY_SUBMISSION_ID"
    echo "notary_submit_receipt=$(basename "$NOTARY_SUBMIT_JSON")"
    echo "notary_log_receipt=$(basename "$NOTARY_LOG_JSON")"
  fi
  if [[ "$PACK_MODE" != "production" ]]; then
    echo "note=local-dev candidate; not for notarization or production promotion"
  fi
} | tee "$ROOT/packaging/artifacts/HASHES.txt"

echo "=== build complete ==="
ls -lah "$DEST_APP" "$DEST_DMG"
du -sh "$DEST_APP" "$DEST_DMG"
