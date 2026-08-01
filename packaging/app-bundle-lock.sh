#!/usr/bin/env bash
# Shared exclusive lock for IRIN.app staging inputs under
# council-rs/warroom-tauri/src-tauri/{binaries,resources} and warroom-web-dist.
#
# Source this file (do not exec). Callers that already hold the lock must export
# IRIN_APP_BUNDLE_LOCK_HELD=1 before invoking nested writers (e.g.
# stage-gateway-pack.sh from build-app-bundle.sh).
#
# shellcheck shell=bash

# Expect ROOT to be set by the sourcing script to the IRIN repo root.
: "${ROOT:?ROOT must be set before sourcing app-bundle-lock.sh}"

IRIN_APP_BUNDLE_LOCK_DIR="${IRIN_APP_BUNDLE_LOCK_DIR:-$ROOT/packaging/build/app-bundle.lock.d}"
IRIN_APP_BUNDLE_GATEWAY_DEST="${IRIN_APP_BUNDLE_GATEWAY_DEST:-$ROOT/council-rs/warroom-tauri/src-tauri/resources/gateway-pack}"

_irin_app_bundle_lock_held_local=0

irin_app_bundle_lock_die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

irin_app_bundle_lock_release() {
  if [[ "${_irin_app_bundle_lock_held_local}" == "1" && -d "$IRIN_APP_BUNDLE_LOCK_DIR" ]]; then
    rm -f "$IRIN_APP_BUNDLE_LOCK_DIR/owner" 2>/dev/null || true
    rmdir "$IRIN_APP_BUNDLE_LOCK_DIR" 2>/dev/null || true
    _irin_app_bundle_lock_held_local=0
    unset IRIN_APP_BUNDLE_LOCK_HELD
  fi
}

# Remove a smoke-inert staged tree from the shared Gateway Pack destination.
# Safe to call when the tree is absent or is a real local-dev/production stage.
irin_scrub_smoke_inert_gateway_pack() {
  local dest="${1:-$IRIN_APP_BUNDLE_GATEWAY_DEST}"
  if [[ -f "$dest/SMOKE_INERT" ]] \
    || { [[ -f "$dest/STAGED_MODE.txt" ]] \
      && grep -q 'mode=smoke-inert' "$dest/STAGED_MODE.txt" 2>/dev/null; }; then
    rm -rf "$dest"
  fi
}

irin_app_bundle_lock_acquire() {
  local label="${1:-unspecified}"
  if [[ "${IRIN_APP_BUNDLE_LOCK_HELD:-0}" == "1" ]]; then
    # Nested writer under an outer holder (same process tree).
    return 0
  fi
  mkdir -p "$(dirname "$IRIN_APP_BUNDLE_LOCK_DIR")"
  local i owner_pid
  for i in $(seq 1 600); do
    if mkdir "$IRIN_APP_BUNDLE_LOCK_DIR" 2>/dev/null; then
      printf 'pid=%s\nlabel=%s\nstarted_at=%s\n' \
        "$$" "$label" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        >"$IRIN_APP_BUNDLE_LOCK_DIR/owner"
      _irin_app_bundle_lock_held_local=1
      export IRIN_APP_BUNDLE_LOCK_HELD=1
      return 0
    fi
    if [[ -f "$IRIN_APP_BUNDLE_LOCK_DIR/owner" ]]; then
      owner_pid="$(sed -n 's/^pid=//p' "$IRIN_APP_BUNDLE_LOCK_DIR/owner" | head -n 1)"
      if [[ -n "$owner_pid" ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
        rm -f "$IRIN_APP_BUNDLE_LOCK_DIR/owner" 2>/dev/null || true
        rmdir "$IRIN_APP_BUNDLE_LOCK_DIR" 2>/dev/null || true
        continue
      fi
    fi
    sleep 0.5
  done
  irin_app_bundle_lock_die \
    "timed out waiting for exclusive app-bundle staging lock ($IRIN_APP_BUNDLE_LOCK_DIR)"
}

# Acquire only when writing the canonical shared Gateway Pack destination.
irin_app_bundle_lock_acquire_for_gateway_dest() {
  local dest="$1"
  local label="${2:-stage-gateway-pack}"
  local canonical="$IRIN_APP_BUNDLE_GATEWAY_DEST"
  local resolved="$dest"
  # Prefer realpath when parents exist; fall back to string equality so a
  # first-time stage (resources/ missing) still locks the shared tree.
  if [[ -d "$(dirname "$dest")" && -d "$(dirname "$canonical")" ]]; then
    resolved="$(cd "$(dirname "$dest")" && printf '%s/%s' "$(pwd -P)" "$(basename "$dest")")"
    canonical="$(cd "$(dirname "$canonical")" && printf '%s/%s' "$(pwd -P)" "$(basename "$canonical")")"
  fi
  if [[ "$resolved" == "$canonical" || "$dest" == "$IRIN_APP_BUNDLE_GATEWAY_DEST" ]]; then
    irin_app_bundle_lock_acquire "$label"
  fi
}
