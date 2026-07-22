#!/usr/bin/env bash
# Stage runtime-only Gateway Pack assets into the Tauri resources tree (gitignored).
# Copies compose + nginx/conf/lua from packaging/gateway-pack and gateway/.
# Does not build or commit images. Optional local image manifest is copied when present.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_PACK="$ROOT/packaging/gateway-pack"
GATEWAY="$ROOT/gateway"
DEST="${1:-$ROOT/council-rs/warroom-tauri/src-tauri/resources/gateway-pack}"
LOCAL_MANIFEST_SRC="${IRIN_GATEWAY_PACK_LOCAL_MANIFEST:-$ROOT/packaging/build/gateway-pack/image-manifest.local.json}"
PROD_MANIFEST_SRC="${IRIN_GATEWAY_PACK_PROD_MANIFEST:-$SRC_PACK/image-manifest.production.example.json}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

[[ -f "$SRC_PACK/docker-compose.yml" ]] || die "missing $SRC_PACK/docker-compose.yml"
[[ -d "$GATEWAY/conf" && -d "$GATEWAY/lua" && -f "$GATEWAY/nginx.conf" ]] \
  || die "missing gateway runtime assets under $GATEWAY"

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

rm -rf "$DEST"
mkdir -p "$DEST"
cp -f "$SRC_PACK/docker-compose.yml" "$DEST/docker-compose.yml"
cp -f "$SRC_PACK/README.md" "$DEST/README.md"
cp -f "$GATEWAY/nginx.conf" "$DEST/nginx.conf"
rsync -a --delete "$GATEWAY/conf/" "$DEST/conf/"
rsync -a --delete "$GATEWAY/lua/" "$DEST/lua/"

# Prefer a local immutable test manifest when the dev image builder has produced one.
if [[ -f "$LOCAL_MANIFEST_SRC" ]]; then
  cp -f "$LOCAL_MANIFEST_SRC" "$DEST/image-manifest.json"
  printf 'staged local image manifest from %s\n' "$LOCAL_MANIFEST_SRC"
elif [[ -f "$PROD_MANIFEST_SRC" ]]; then
  # Production example is not loadable (placeholder digests); still stage for shape checks.
  cp -f "$PROD_MANIFEST_SRC" "$DEST/image-manifest.json"
  printf 'staged production-example image manifest (not for live start without real digests)\n'
else
  die "no image manifest available to stage"
fi

# Narrow runtime conf: drop grafana/alert noise is optional; keep full conf for gateway correctness.
# Ensure Watch-off markers remain visible in staged compose.
grep -q 'WATCH_PRODUCER_ENABLED=false' "$DEST/docker-compose.yml" || die "staged compose lost watch-off"

printf 'staged gateway pack -> %s\n' "$DEST"
find "$DEST" -type f | wc -l | awk '{print "files:", $1}'
