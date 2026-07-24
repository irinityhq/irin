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
[[ -d "$RES_STAGE/cabinets" ]] || die "staged cabinets missing"

echo "staged binary: $BIN_STAGE/council-${TRIPLE}"
echo "staged base-dir: $RES_STAGE"
echo "source council: $COUNCIL_BIN"
