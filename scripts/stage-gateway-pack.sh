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
if grep -E '^[[:space:]]*build:' "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not contain build: directives"
fi
if grep -E '^[[:space:]]*-[[:space:]]*.*(\$\{HOME\}|~/|\$HOME)' "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not mount host-home paths"
fi
if grep -E '^[[:space:]]*-[[:space:]]*.*(gcloud|\.config/gcloud)' "$SRC_PACK/docker-compose.yml" >/dev/null; then
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
# Watch/Outbox admin reads are armed via the validated native spawn env: the
# compose must interpolate exactly this form (ambient host values are scrubbed
# by the native spawn layer; the Keychain-held value never touches the public
# env file). COUNCIL_GATEWAY_TOKEN and WATCH_DISPATCHER_GATEWAY_KEY stay empty
# literals that accept no env input.
if ! grep -qF -- '- WATCH_ADMIN_TOKEN=${WATCH_ADMIN_TOKEN:-}' "$SRC_PACK/docker-compose.yml"; then
  die "gateway pack compose must interpolate WATCH_ADMIN_TOKEN from the native spawn env"
fi
if grep -E '^[[:space:]]*-[[:space:]]*(COUNCIL_GATEWAY_TOKEN|WATCH_DISPATCHER_GATEWAY_KEY)=.*\$\{' \
    "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack compose must not interpolate council/watch-dispatcher token surfaces"
fi
if grep -E '^[[:space:]]*-[[:space:]]*(GW_ARM_DEVIATION_FLAG|GW_ARM_PRINCIPAL_DOMAINS|ARM_NOTIFY_URL|ARM_STAGE_TTL_MS)=.*\$\{' \
    "$SRC_PACK/docker-compose.yml" >/dev/null; then
  die "gateway pack admits only GW_ARM_PRINCIPALS and GW_ARM_ATTEST_KEYS_PATH on the arm surface"
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
    intended_sha="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)"
    intended_sha="$(printf '%s' "$intended_sha" | tr -d '[:space:]')"
    bash "$ROOT/scripts/verify-production-image-provenance.sh" \
      "$PROD_MANIFEST_SRC" "$intended_sha" "${IRIN_RELEASE_VERSION:-}" >&2 \
      || die "production manifest failed immutable registry provenance verification"
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
# Non-secret desktop feature marker. OpenResty workers can read this without
# weakening the root-owned 0600 attestation registry mounted only to sidecar.
printf '' >"$DEST/arm-bridge-enabled"
chmod 0644 "$DEST/arm-bridge-enabled"
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
