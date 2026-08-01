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
# Staging isolation: fail-closed exclusive lock covering staging → bundle
# completion (mkdir lock + held ownership file). Concurrent/interrupted
# consumers cannot interleave shared generated inputs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

TAURI_DIR="$IRIN_SRC/council-rs/warroom-tauri"
WEB_DIR="$IRIN_SRC/council-rs/warroom/web"
STAGE_SCRIPT="$TAURI_DIR/scripts/stage-bundle-inputs.sh"
LOCK_DIR="$ROOT/packaging/build/app-bundle.lock.d"
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

lock_held=0
release_build_lock() {
  if [[ "$lock_held" == "1" && -d "$LOCK_DIR" ]]; then
    rm -f "$LOCK_DIR/owner" 2>/dev/null || true
    rmdir "$LOCK_DIR" 2>/dev/null || true
    lock_held=0
  fi
}

# If smoke-inert staging was interrupted, never leave that marker in the
# shared production staging tree after the lock is released.
scrub_smoke_inert_staging() {
  if [[ "$MODE" != "smoke-inert" ]]; then
    return 0
  fi
  if [[ -f "$GATEWAY_DEST/STAGED_MODE.txt" ]] \
    && grep -q 'mode=smoke-inert' "$GATEWAY_DEST/STAGED_MODE.txt" 2>/dev/null; then
    rm -rf "$GATEWAY_DEST"
  elif [[ -f "$GATEWAY_DEST/SMOKE_INERT" ]]; then
    rm -rf "$GATEWAY_DEST"
  fi
}

on_exit() {
  status=$?
  scrub_smoke_inert_staging || true
  release_build_lock || true
  exit "$status"
}
trap on_exit EXIT INT TERM

acquire_build_lock() {
  mkdir -p "$(dirname "$LOCK_DIR")"
  local i
  for i in $(seq 1 600); do
    if mkdir "$LOCK_DIR" 2>/dev/null; then
      printf 'pid=%s\nmode=%s\nstarted_at=%s\n' \
        "$$" "$MODE" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$LOCK_DIR/owner"
      lock_held=1
      return 0
    fi
    # Stale lock: owner pid gone.
    if [[ -f "$LOCK_DIR/owner" ]]; then
      local owner_pid
      owner_pid="$(sed -n 's/^pid=//p' "$LOCK_DIR/owner" | head -n 1)"
      if [[ -n "$owner_pid" ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
        rm -f "$LOCK_DIR/owner" 2>/dev/null || true
        rmdir "$LOCK_DIR" 2>/dev/null || true
        continue
      fi
    fi
    sleep 0.5
  done
  die "timed out waiting for exclusive app-bundle build lock ($LOCK_DIR)"
}

acquire_build_lock
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
