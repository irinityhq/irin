#!/usr/bin/env bash
# Build a self-contained aarch64 Council War Room .app + .dmg from this monorepo.
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
fi

echo "=== IRIN DMG build ==="
echo "ROOT=$ROOT"
echo "PACK_MODE=$PACK_MODE"
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
APP="$CARGO_TARGET_DIR/release/bundle/macos/Council War Room.app"
[[ -d "$APP" ]] || die "app bundle not found at $APP (tauri build did not produce it)"

echo "=== ad-hoc codesign (build artifact only; never use production credentials) ==="
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"
codesign -dv --verbose=2 "$APP" 2>&1 | head -20 || true

SIDECAR="$APP/Contents/MacOS/council"
[[ -x "$SIDECAR" ]] || die "bundled council missing or not executable: $SIDECAR"
if [[ ! -d "$APP/Contents/Resources/council-base/cabinets" ]]; then
  FOUND_BASE="$(find "$APP/Contents/Resources" -type d -name cabinets 2>/dev/null | head -1 || true)"
  [[ -n "$FOUND_BASE" ]] || die "bundled council-base/cabinets missing under Resources"
  echo "NOTE: cabinets at $FOUND_BASE"
fi

mkdir -p "$ROOT/packaging/artifacts"
DEST_APP="$ROOT/packaging/artifacts/Council War Room.app"
DEST_DMG="$ROOT/packaging/artifacts/Council War Room_0.1.0_aarch64.dmg"
rm -rf "$DEST_APP"
ditto "$APP" "$DEST_APP"
codesign --force --deep --sign - "$DEST_APP"
codesign --verify --deep --strict "$DEST_APP"

echo "=== hdiutil DMG from ad-hoc signed app ==="
STAGE="$ROOT/packaging/build/dmg-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
ditto "$DEST_APP" "$STAGE/Council War Room.app"
ln -sf /Applications "$STAGE/Applications"
rm -f "$DEST_DMG"
hdiutil create -volname "Council War Room" -srcfolder "$STAGE" -ov -format UDZO "$DEST_DMG"

{
  echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "pack_mode=$PACK_MODE"
  echo "releasable=$([[ "$PACK_MODE" == "production" ]] && echo true || echo false)"
  echo "source_sha=${IRIN_TAURI_BUILD_GIT_SHA:-unknown}"
  echo "build_dirty=${IRIN_TAURI_BUILD_DIRTY:-unknown}"
  echo "arch=aarch64-apple-darwin"
  echo "app=$DEST_APP"
  echo "dmg=$DEST_DMG"
  echo "app_sha256=$(shasum -a 256 "$DEST_APP/Contents/MacOS/council-warroom-tauri" | awk '{print $1}')"
  echo "council_sha256=$(shasum -a 256 "$DEST_APP/Contents/MacOS/council" | awk '{print $1}')"
  echo "dmg_sha256=$(shasum -a 256 "$DEST_DMG" | awk '{print $1}')"
  if [[ "$PACK_MODE" != "production" ]]; then
    echo "note=local-dev candidate; not for notarization or production promotion"
  fi
} | tee "$ROOT/packaging/artifacts/HASHES.txt"

echo "=== build complete ==="
ls -lah "$DEST_APP" "$DEST_DMG"
du -sh "$DEST_APP" "$DEST_DMG"
