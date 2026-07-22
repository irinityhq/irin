#!/usr/bin/env bash
# Stage runtime-only Gateway Pack assets into the Tauri resources tree (gitignored).
# Copies compose + nginx/conf/lua from packaging/gateway-pack and gateway/.
# Does not build or commit images.
#
# Modes (IRIN_GATEWAY_PACK_MODE):
#   local-dev  (default for regression) — requires a local-dev manifest
#   production — requires an explicitly supplied production manifest path;
#                refuses local-dev manifests and placeholder digests
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_PACK="$ROOT/packaging/gateway-pack"
GATEWAY="$ROOT/gateway"
DEST="${1:-$ROOT/council-rs/warroom-tauri/src-tauri/resources/gateway-pack}"
MODE="${IRIN_GATEWAY_PACK_MODE:-local-dev}"
LOCAL_MANIFEST_SRC="${IRIN_GATEWAY_PACK_LOCAL_MANIFEST:-$ROOT/packaging/build/gateway-pack/image-manifest.local.json}"
PROD_MANIFEST_SRC="${IRIN_GATEWAY_PACK_PROD_MANIFEST:-}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

[[ -f "$SRC_PACK/docker-compose.yml" ]] || die "missing $SRC_PACK/docker-compose.yml"
[[ -d "$GATEWAY/conf" && -d "$GATEWAY/lua" && -f "$GATEWAY/nginx.conf" ]] \
  || die "missing gateway runtime assets under $GATEWAY"

case "$MODE" in
  local-dev|production) ;;
  *) die "IRIN_GATEWAY_PACK_MODE must be local-dev or production (got $MODE)" ;;
esac

# Fail closed: production-shaped compose must never ship build: directives or HOME mounts.
if grep -E '^\s*build:' "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not contain build: directives"
fi
if grep -E '^\s*-\s*.*(\$\{HOME\}|~/|\$HOME)' "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not mount host-home paths"
fi
if grep -E '^\s*-\s*.*(gcloud|\.config/gcloud)' "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not mount gcloud credential dirs"
fi
if grep -E 'canary\.yml|docker-compose\.canary' "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not include canary overlays"
fi
if ! grep -q 'WATCH_PRODUCER_ENABLED=false' "$SRC_PACK/docker-compose.yml"; then
  die "gateway pack compose must hardcode WATCH_PRODUCER_ENABLED=false"
fi
if ! grep -q 'WATCH_DISPATCHER_ENABLED=false' "$SRC_PACK/docker-compose.yml"; then
  die "gateway pack compose must hardcode WATCH_DISPATCHER_ENABLED=false"
fi
if ! grep -q 'irin-desktop-gateway' "$SRC_PACK/docker-compose.yml"; then
  die "gateway pack compose must declare fixed project name irin-desktop-gateway"
fi

pick_manifest() {
  if [[ "$MODE" == "production" ]]; then
    [[ -n "$PROD_MANIFEST_SRC" ]] || die \
      "production mode requires IRIN_GATEWAY_PACK_PROD_MANIFEST pointing at a real production manifest"
    [[ -f "$PROD_MANIFEST_SRC" ]] || die "production manifest missing: $PROD_MANIFEST_SRC"
    # Refuse leftover local-dev outputs even if path points near them.
    if grep -q '"mode"[[:space:]]*:[[:space:]]*"local-dev"' "$PROD_MANIFEST_SRC"; then
      die "production packaging refuses a local-dev manifest: $PROD_MANIFEST_SRC"
    fi
    if ! grep -q '"mode"[[:space:]]*:[[:space:]]*"production"' "$PROD_MANIFEST_SRC"; then
      die "production manifest must set mode=production: $PROD_MANIFEST_SRC"
    fi
    if grep -E '"gateway"|"sidecar"' "$PROD_MANIFEST_SRC" | grep -q 'irin-desktop/'; then
      die "production manifest must not use irin-desktop/* local image names"
    fi
    if grep -qE 'sha256:0{64}' "$PROD_MANIFEST_SRC"; then
      die "production manifest has placeholder zero digests"
    fi
    printf '%s\n' "$PROD_MANIFEST_SRC"
    return
  fi

  # local-dev
  if [[ -n "${IRIN_GATEWAY_PACK_LOCAL_MANIFEST:-}" ]]; then
    [[ -f "$LOCAL_MANIFEST_SRC" ]] || die "local-dev manifest missing: $LOCAL_MANIFEST_SRC"
  fi
  if [[ -f "$LOCAL_MANIFEST_SRC" ]]; then
    if grep -q '"mode"[[:space:]]*:[[:space:]]*"production"' "$LOCAL_MANIFEST_SRC"; then
      die "local-dev packaging refuses a production manifest at $LOCAL_MANIFEST_SRC"
    fi
    if ! grep -q '"mode"[[:space:]]*:[[:space:]]*"local-dev"' "$LOCAL_MANIFEST_SRC"; then
      die "local-dev manifest must set mode=local-dev: $LOCAL_MANIFEST_SRC"
    fi
    printf '%s\n' "$LOCAL_MANIFEST_SRC"
    return
  fi
  die "local-dev mode requires $LOCAL_MANIFEST_SRC (run scripts/build-gateway-pack-dev-images.sh first)"
}

MANIFEST_SRC="$(pick_manifest)"

rm -rf "$DEST"
mkdir -p "$DEST"
cp -f "$SRC_PACK/docker-compose.yml" "$DEST/docker-compose.yml"
cp -f "$SRC_PACK/README.md" "$DEST/README.md"
cp -f "$GATEWAY/nginx.conf" "$DEST/nginx.conf"
rsync -a --delete "$GATEWAY/conf/" "$DEST/conf/"
rsync -a --delete "$GATEWAY/lua/" "$DEST/lua/"
cp -f "$MANIFEST_SRC" "$DEST/image-manifest.json"

# Stamp packaging mode into a non-secret receipt next to the staged tree.
printf 'mode=%s\nmanifest_src=%s\n' "$MODE" "$MANIFEST_SRC" >"$DEST/STAGED_MODE.txt"

grep -q 'WATCH_PRODUCER_ENABLED=false' "$DEST/docker-compose.yml" || die "staged compose lost watch-off"

printf 'staged gateway pack -> %s (mode=%s, manifest=%s)\n' "$DEST" "$MODE" "$MANIFEST_SRC"
find "$DEST" -type f | wc -l | awk '{print "files:", $1}'
