#!/usr/bin/env bash
# Stage Tauri externalBin + bundled council-base resources for a self-contained app.
# Generated under src-tauri/binaries and src-tauri/resources (gitignored).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TAURI_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COUNCIL_RS="$(cd "$TAURI_ROOT/.." && pwd)"
REPO_ROOT="$(cd "$COUNCIL_RS/.." && pwd)"
SRC_TAURI="$TAURI_ROOT/src-tauri"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

TRIPLE="${IRIN_BUNDLE_TARGET_TRIPLE:-}"
if [[ -z "$TRIPLE" ]]; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) TRIPLE="aarch64-apple-darwin" ;;
    Darwin-x86_64) TRIPLE="x86_64-apple-darwin" ;;
    *) die "unsupported host for bundle staging: $(uname -s)-$(uname -m)" ;;
  esac
fi

# Prefer workspace target, then council-rs local target, then CARGO_TARGET_DIR.
CANDIDATES=(
  "${CARGO_TARGET_DIR:-}/release/council"
  "$REPO_ROOT/target/release/council"
  "$COUNCIL_RS/target/release/council"
)
COUNCIL_BIN=""
for c in "${CANDIDATES[@]}"; do
  [[ -n "$c" && -x "$c" ]] || continue
  COUNCIL_BIN="$c"
  break
done

if [[ -z "$COUNCIL_BIN" ]]; then
  echo "=== building release council for bundle stage ==="
  (
    cd "$REPO_ROOT"
    cargo build --release -p council-rs --bin council
  )
  for c in "${CANDIDATES[@]}"; do
    [[ -n "$c" && -x "$c" ]] || continue
    COUNCIL_BIN="$c"
    break
  done
fi
[[ -x "$COUNCIL_BIN" ]] || die "council binary missing after build"

BIN_STAGE="$SRC_TAURI/binaries"
RES_STAGE="$SRC_TAURI/resources/council-base"
mkdir -p "$BIN_STAGE"
cp -f "$COUNCIL_BIN" "$BIN_STAGE/council-${TRIPLE}"
chmod +x "$BIN_STAGE/council-${TRIPLE}"

rm -rf "$RES_STAGE"
mkdir -p "$RES_STAGE"
rsync -a "$COUNCIL_RS/cabinets/" "$RES_STAGE/cabinets/"
rsync -a "$COUNCIL_RS/prompts/" "$RES_STAGE/prompts/"
for f in models.yaml roles.yaml \
  agy_routing.yaml claude_routing.yaml gemini_routing.yaml grok_routing.yaml; do
  [[ -f "$COUNCIL_RS/$f" ]] && cp -f "$COUNCIL_RS/$f" "$RES_STAGE/"
done
if [[ -d "$COUNCIL_RS/schemas" ]]; then
  rsync -a "$COUNCIL_RS/schemas/" "$RES_STAGE/schemas/"
fi
# Packaged base-dir must ship the Hermes seat adapter at the same relative path
# as the source tree (grok_routing.yaml → hermes.default_adapter). Without this,
# source-tree discovery supports grok_hermes but the signed installed app does not.
ADAPTER_SRC="$COUNCIL_RS/scripts/hermes-seat-adapter.sh"
ADAPTER_DST="$RES_STAGE/scripts/hermes-seat-adapter.sh"
[[ -f "$ADAPTER_SRC" ]] || die "hermes seat adapter missing: $ADAPTER_SRC"
mkdir -p "$RES_STAGE/scripts"
cp -f "$ADAPTER_SRC" "$ADAPTER_DST"
chmod +x "$ADAPTER_DST"
[[ -x "$ADAPTER_DST" ]] || die "staged hermes seat adapter not executable: $ADAPTER_DST"
[[ -d "$RES_STAGE/cabinets" ]] || die "staged cabinets missing"

# Touch ID signing helper. Built from canonical source into the Tauri staging
# tree; tauri.conf.json places the Mach-O under Contents/Helpers, Apple's
# standard nested-code location. Production signs it inside-out with Developer
# ID, Hardened Runtime, and a secure timestamp. The native host pins its SHA-256
# at enrollment, so a changed helper forces explicit re-enrollment.
# macOS-only: the helper needs Secure Enclave + LocalAuthentication.
HELPER_SRC="$REPO_ROOT/gateway/bin/arm-attest.swift"
HELPER_DST="$SRC_TAURI/resources/arm-attest"
if [[ "$(uname -s)" == "Darwin" ]]; then
  [[ -f "$HELPER_SRC" ]] || die "Touch ID helper source missing: $HELPER_SRC"
  command -v swiftc >/dev/null 2>&1 || die "swiftc is required to stage the Touch ID helper"
  mkdir -p "$(dirname "$HELPER_DST")"
  swiftc -O -o "$HELPER_DST" "$HELPER_SRC"
  chmod +x "$HELPER_DST"
  [[ -x "$HELPER_DST" ]] || die "staged Touch ID helper not executable: $HELPER_DST"
  echo "staged touch-id helper: $HELPER_DST"
fi

echo "staged binary: $BIN_STAGE/council-${TRIPLE}"
echo "staged base-dir: $RES_STAGE"
echo "staged hermes adapter: $ADAPTER_DST"
echo "source council: $COUNCIL_BIN"
