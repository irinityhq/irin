#!/bin/bash
set -euo pipefail

# =============================================================================
# build-warroom-assets.sh
# Purpose: Build the Next.js War Room UI and prepare static assets for Tauri.
#
# Lives in council-rs/warroom-tauri/ (Phase 2 hybrid, in-tree). Invoked from
# src-tauri via `bash ../scripts/build-warroom-assets.sh` (Tauri cwd is src-tauri).
# Static export writes to warroom/web/.next-tauri/ (gitignored) and is copied
# into warroom-tauri/warroom-web-dist/.
#
# Usage (from warroom-tauri/):
#   bash scripts/build-warroom-assets.sh
#
# Environment variables you can set:
#   WARROOM_WEB_DIR   - Path to the Next.js source (default: sibling council-rs/warroom/web)
#   WARROOM_DIST_DIR  - Where to place the ready-to-serve assets for Tauri
#                       (default: ./warroom-web-dist relative to spike root)
#   NEXT_BUILD_MODE   - "export" only for production Tauri bundles.
#                       "default" is intentionally rejected because .next/static
#                       alone is not a valid frontendDist.
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WARROOM_TAURI_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COUNCIL_RS_ROOT="$(cd "$WARROOM_TAURI_ROOT/.." && pwd)"

WARROOM_WEB_DIR="${WARROOM_WEB_DIR:-$COUNCIL_RS_ROOT/warroom/web}"
WARROOM_DIST_DIR="${WARROOM_DIST_DIR:-$WARROOM_TAURI_ROOT/warroom-web-dist}"
NEXT_BUILD_MODE="${NEXT_BUILD_MODE:-export}"

echo "=== War Room Hybrid Asset Builder (Tauri) ==="
echo "warroom-tauri root:  $WARROOM_TAURI_ROOT"
echo "council-rs root:     $COUNCIL_RS_ROOT"
echo "War Room web source: $WARROOM_WEB_DIR"
echo "Target dist for Tauri: $WARROOM_DIST_DIR"
echo "Next build mode:     $NEXT_BUILD_MODE"
echo

if [ ! -d "$WARROOM_WEB_DIR" ]; then
  echo "ERROR: WARROOM_WEB_DIR does not exist: $WARROOM_WEB_DIR"
  echo "Make sure you are running this from the correct sibling layout."
  exit 1
fi

# Clean previous output in the spike's local copy
rm -rf "$WARROOM_DIST_DIR"
mkdir -p "$WARROOM_DIST_DIR"

pushd "$WARROOM_WEB_DIR" >/dev/null

echo "Installing dependencies (if needed)..."
npm ci --prefer-offline --no-audit --progress=false || npm install

echo "Building Next.js War Room..."
if [ "$NEXT_BUILD_MODE" = "export" ]; then
  # Requires next.config.ts to honor WARROOM_TAURI_EXPORT=1 by setting
  # output: "export" and distDir: ".next-tauri". Keeping this separate from
  # .next-hosted prevents a Tauri export from invalidating the live web UI.
  npm run build:tauri
  if [ ! -f ".next-tauri/index.html" ]; then
    echo "ERROR: static export did not produce .next-tauri/index.html"
    echo "Do not copy .next/static by itself; it is not a valid Tauri frontendDist."
    echo "Check the Tauri output/distDir gates in next.config.ts, then rerun this script."
    exit 1
  fi
  rsync -a --delete .next-tauri/. "$WARROOM_DIST_DIR"/
else
  echo "ERROR: NEXT_BUILD_MODE=$NEXT_BUILD_MODE is not supported for Tauri bundling."
  echo "Use devUrl for a running Next dev/server process, or use NEXT_BUILD_MODE=export."
  exit 1
fi

popd >/dev/null

echo
echo "=== Assets prepared ==="
echo "You can now point tauri.conf.json 'frontendDist' at: $WARROOM_DIST_DIR"
echo "Example (for production bundle):"
echo '  "frontendDist": "../warroom-web-dist"'
echo
echo "For development you will usually prefer:"
echo '  "devUrl": "http://127.0.0.1:3010"   (tauri dev runs `npm run dev:local` in warroom/web)'
echo
echo "Done."
