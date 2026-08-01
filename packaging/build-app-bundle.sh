#!/usr/bin/env bash
# Internal IRIN.app factory primitive (not a root Makefile target).
#
# Locked stage order:
#   Council build
#   → Council/base/helper staging
#   → selected Gateway staging (real or inert)
#   → web export (via tauri beforeBuildCommand)
#   → Tauri app build
#
# Consumer-specific signing / DMG / HASHES stay outside this script.
#
# Env knobs:
#   IRIN_APP_TARGET_DIR       → CARGO_TARGET_DIR for this build
#   IRIN_TAURI_CONFIG_OVERLAY → optional Tauri --config overlay JSON
#   IRIN_GATEWAY_PACK_MODE    → local-dev | production | smoke-inert
#   IRIN_TAURI_BUNDLES        → tauri --bundles value (default: app)
#
# Staging isolation: fail-closed exclusive lock (packaging/app-bundle-lock.sh)
# covering staging → bundle completion. Nested writers honor
# IRIN_APP_BUNDLE_LOCK_HELD=1.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"
# shellcheck source=/dev/null
source "$ROOT/packaging/app-bundle-lock.sh"

TAURI_DIR="$IRIN_SRC/council-rs/warroom-tauri"
WEB_DIR="$IRIN_SRC/council-rs/warroom/web"
STAGE_SCRIPT="$TAURI_DIR/scripts/stage-bundle-inputs.sh"
GATEWAY_DEST="$TAURI_DIR/src-tauri/resources/gateway-pack"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

MODE="${IRIN_GATEWAY_PACK_MODE:-local-dev}"
case "$MODE" in
  local-dev|production|smoke-inert) ;;
  *) die "IRIN_GATEWAY_PACK_MODE must be local-dev, production, or smoke-inert (got $MODE)" ;;
esac
export IRIN_GATEWAY_PACK_MODE="$MODE"

BUNDLES="${IRIN_TAURI_BUNDLES:-app}"
[[ -n "$BUNDLES" ]] || die "IRIN_TAURI_BUNDLES must be non-empty"

if [[ -n "${IRIN_APP_TARGET_DIR:-}" ]]; then
  export CARGO_TARGET_DIR="$IRIN_APP_TARGET_DIR"
fi
mkdir -p "$CARGO_TARGET_DIR"

on_exit() {
  status=$?
  if [[ "$MODE" == "smoke-inert" ]]; then
    irin_scrub_smoke_inert_gateway_pack "$GATEWAY_DEST" || true
  fi
  irin_app_bundle_lock_release || true
  exit "$status"
}
trap on_exit EXIT INT TERM

irin_app_bundle_lock_acquire "build-app-bundle:$MODE"
echo "=== IRIN app-bundle primitive ==="
echo "ROOT=$ROOT"
echo "IRIN_GATEWAY_PACK_MODE=$MODE"
echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
echo "IRIN_TAURI_BUNDLES=$BUNDLES"
echo "IRIN_TAURI_CONFIG_OVERLAY=${IRIN_TAURI_CONFIG_OVERLAY:-}"

echo "=== cargo build council (release) ==="
(
  cd "$IRIN_SRC"
  cargo build --release -p council-rs --bin council
)

echo "=== stage bundled council + base-dir + helper ==="
bash "$STAGE_SCRIPT"

echo "=== stage Gateway Pack (mode=$MODE) ==="
# Outer lock held; nested stage-gateway-pack must not re-acquire.
export IRIN_APP_BUNDLE_LOCK_HELD=1
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

echo "=== tauri build (bundles=$BUNDLES) ==="
(
  cd "$TAURI_DIR"
  export IRIN_TAURI_BUILD_GIT_SHA COUNCIL_BUILD_GIT_SHA
  export IRIN_TAURI_BUILD_DIRTY COUNCIL_BUILD_DIRTY
  export CARGO_TARGET_DIR
  if [[ -n "${IRIN_TAURI_CONFIG_OVERLAY:-}" ]]; then
    [[ -f "$IRIN_TAURI_CONFIG_OVERLAY" ]] \
      || die "IRIN_TAURI_CONFIG_OVERLAY missing: $IRIN_TAURI_CONFIG_OVERLAY"
    npm run tauri build -- --bundles "$BUNDLES" --config "$IRIN_TAURI_CONFIG_OVERLAY"
  else
    npm run tauri build -- --bundles "$BUNDLES"
  fi
)

APP="$CARGO_TARGET_DIR/release/bundle/macos/IRIN.app"
[[ -d "$APP" ]] || die "app bundle not found at $APP (tauri build did not produce it)"
echo "=== app-bundle primitive complete: $APP ==="
