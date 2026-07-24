#!/usr/bin/env bash
# Isolation env for IRIN DMG packaging — generated state under packaging/.
# shellcheck shell=bash
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export IRIN_DMG_ROOT="$ROOT"
export IRIN_SRC="$ROOT"
export TMPDIR="${IRIN_DMG_TMPDIR:-$ROOT/packaging/build/tmp}"
export CARGO_HOME="${CARGO_HOME:-$ROOT/packaging/build/cargo-home}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/packaging/build/cargo-target}"
export npm_config_cache="${npm_config_cache:-$ROOT/packaging/build/npm-cache}"
export npm_config_prefer_offline=true
# Never force color into logs/receipts when selection or capture becomes data.
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"
export NO_COLOR="${NO_COLOR:-1}"

# Matching provenance for host + council. Prefer an already-committed clean SHA.
if [[ -z "${IRIN_TAURI_BUILD_GIT_SHA:-}" ]]; then
  if SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null)"; then
    export IRIN_TAURI_BUILD_GIT_SHA="$SHA"
  fi
fi
if [[ -n "${IRIN_TAURI_BUILD_GIT_SHA:-}" ]]; then
  export COUNCIL_BUILD_GIT_SHA="${COUNCIL_BUILD_GIT_SHA:-$IRIN_TAURI_BUILD_GIT_SHA}"
fi
if [[ -z "${IRIN_TAURI_BUILD_DIRTY:-}" ]]; then
  if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null || true)" ]]; then
    export IRIN_TAURI_BUILD_DIRTY=true
  else
    export IRIN_TAURI_BUILD_DIRTY=false
  fi
fi
export COUNCIL_BUILD_DIRTY="${COUNCIL_BUILD_DIRTY:-$IRIN_TAURI_BUILD_DIRTY}"

mkdir -p "$TMPDIR" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$npm_config_cache" \
  "$ROOT/packaging/artifacts" "$ROOT/packaging/test-home" "$ROOT/packaging/test-apps" \
  "$ROOT/packaging/build/dmg-mount" "$ROOT/packaging/receipts"
