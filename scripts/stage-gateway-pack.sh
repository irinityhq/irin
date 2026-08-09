#!/usr/bin/env bash
# Stage runtime-only Gateway Pack assets into the Tauri resources tree (gitignored).
# Copies compose + nginx/conf/lua from packaging/gateway-pack and gateway/.
# Does not build or commit images.
#
# Modes (IRIN_GATEWAY_PACK_MODE):
#   local-dev  (default for regression) — requires a local-dev manifest
#   production — requires an explicitly supplied production manifest path;
#                refuses local-dev manifests and placeholder digests
#   smoke-inert — tracked nginx/conf/lua + generated minimal disarmed
#                manifest; stamped smoke-inert; no Docker images
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/app-bundle-lock.sh"
SRC_PACK="$ROOT/packaging/gateway-pack"
GATEWAY="$ROOT/gateway"
DEST="${1:-$ROOT/council-rs/warroom-tauri/src-tauri/resources/gateway-pack}"
MODE="${IRIN_GATEWAY_PACK_MODE:-local-dev}"
LOCAL_MANIFEST_SRC="${IRIN_GATEWAY_PACK_LOCAL_MANIFEST:-$ROOT/packaging/build/gateway-pack/image-manifest.local.json}"
PROD_MANIFEST_SRC="${IRIN_GATEWAY_PACK_PROD_MANIFEST:-}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# Exclusive lock when writing the shared Tauri resources tree. Nested callers
# under build-app-bundle.sh export IRIN_APP_BUNDLE_LOCK_HELD=1 and skip.
# Temp destinations (asset tests) do not take the lock.
_stage_lock_release() {
  irin_app_bundle_lock_release || true
}
trap _stage_lock_release EXIT INT TERM
irin_app_bundle_lock_acquire_for_gateway_dest "$DEST" "stage-gateway-pack:$MODE"

[[ -f "$SRC_PACK/docker-compose.yml" ]] || die "missing $SRC_PACK/docker-compose.yml"
[[ -d "$GATEWAY/conf" && -d "$GATEWAY/lua" && -f "$GATEWAY/nginx.conf" ]] \
  || die "missing gateway runtime assets under $GATEWAY"

case "$MODE" in
  local-dev|production|smoke-inert) ;;
  *) die "IRIN_GATEWAY_PACK_MODE must be local-dev, production, or smoke-inert (got $MODE)" ;;
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
# Pack-native watch profile/inbox: exact interpolation + mount forms only.
# Profile mount is read-only; inbox is the sole writable operator mount.
# Producer/dispatcher stay hardcoded false (guards above never relaxed).
if ! grep -qF -- '- SENTINELS_CONFIG_PATH=${IRIN_WATCH_PROFILE_PATH:-}' \
    "$SRC_PACK/docker-compose.yml"; then
  die "gateway pack compose must interpolate SENTINELS_CONFIG_PATH from IRIN_WATCH_PROFILE_PATH"
fi
SENTINELS_MOUNT='${IRIN_DESKTOP_SENTINELS_DIR}:/var/lib/gateway/sentinels:ro'
INBOX_MOUNT='${IRIN_DESKTOP_WATCH_INBOX_DIR}:/var/lib/gateway/inbox'
if [[ "$(grep -Fc "$SENTINELS_MOUNT" "$SRC_PACK/docker-compose.yml")" -ne 1 ]]; then
  die "gateway pack compose must mount IRIN_DESKTOP_SENTINELS_DIR read-only at /var/lib/gateway/sentinels"
fi
if [[ "$(grep -Fc "$INBOX_MOUNT" "$SRC_PACK/docker-compose.yml")" -ne 1 ]]; then
  die "gateway pack compose must mount IRIN_DESKTOP_WATCH_INBOX_DIR at /var/lib/gateway/inbox"
fi
# The bind-source variables may appear only as those exact mounts — a second
# mount (e.g. a writable duplicate) would satisfy the counts above and bypass
# the :ro policy below.
if [[ "$(grep -Fc 'IRIN_DESKTOP_SENTINELS_DIR' "$SRC_PACK/docker-compose.yml")" -ne 1 ]]; then
  die "IRIN_DESKTOP_SENTINELS_DIR must appear exactly once in compose (the read-only profile mount)"
fi
if [[ "$(grep -Fc 'IRIN_DESKTOP_WATCH_INBOX_DIR' "$SRC_PACK/docker-compose.yml")" -ne 1 ]]; then
  die "IRIN_DESKTOP_WATCH_INBOX_DIR must appear exactly once in compose (the inbox mount)"
fi
# Refuse writable profile mounts (must stay :ro).
if grep -E 'IRIN_DESKTOP_SENTINELS_DIR.*sentinels[^:]*$' "$SRC_PACK/docker-compose.yml" \
    | grep -v ':ro' >/dev/null; then
  die "gateway pack sentinels profile mount must be read-only"
fi
[[ -f "$SRC_PACK/default-sentinels.yaml" ]] \
  || die "missing bundled default watch profile template: $SRC_PACK/default-sentinels.yaml"
if ! grep -qE '^[[:space:]]*tenant:[[:space:]]*canary[[:space:]]*$' \
    "$SRC_PACK/default-sentinels.yaml"; then
  die "bundled default watch profile tenant must be canary (PACK_WATCH_CANARY_TENANT)"
fi
if ! grep -q 'file-inbox-watch' "$SRC_PACK/default-sentinels.yaml"; then
  die "bundled default watch profile must declare file-inbox-watch"
fi

stage_smoke_inert() {
  rm -rf "$DEST"
  mkdir -p "$DEST"
  # Write the isolation marker BEFORE any copy so an interrupted stage is
  # still recognizable by EXIT scrubs (never leave a partial unmarked tree).
  printf 'smoke-inert\n' >"$DEST/SMOKE_INERT"
  printf 'mode=smoke-inert\nmanifest_src=generated-smoke-inert\npartial=1\n' \
    >"$DEST/STAGED_MODE.txt"
  printf '' >"$DEST/arm-bridge-enabled"
  chmod 0644 "$DEST/arm-bridge-enabled"
  cp -f "$SRC_PACK/docker-compose.yml" "$DEST/docker-compose.yml"
  cp -f "$SRC_PACK/README.md" "$DEST/README.md"
  cp -f "$SRC_PACK/default-sentinels.yaml" "$DEST/default-sentinels.yaml"
  cp -f "$GATEWAY/nginx.conf" "$DEST/nginx.conf"
  rsync -a --delete "$GATEWAY/conf/" "$DEST/conf/"
  rsync -a --delete "$GATEWAY/lua/" "$DEST/lua/"
  # Minimal disarmed manifest — no Docker images, clearly non-promotable.
  cat >"$DEST/image-manifest.json" <<'EOF'
{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "smoke-inert",
  "source_sha": "smoke-inert",
  "source_dirty": true,
  "images": {
    "gateway": "irin-desktop/gateway@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    "sidecar": "irin-desktop/sidecar@sha256:0000000000000000000000000000000000000000000000000000000000000000"
  },
  "watch_invariants": {
    "WATCH_PRODUCER_ENABLED": false,
    "WATCH_DISPATCHER_ENABLED": false
  }
}
EOF
  printf 'mode=smoke-inert\nmanifest_src=generated-smoke-inert\n' >"$DEST/STAGED_MODE.txt"
  grep -q 'WATCH_PRODUCER_ENABLED=false' "$DEST/docker-compose.yml" \
    || die "staged compose lost watch-off"
  printf 'staged gateway pack -> %s (mode=smoke-inert, no Docker images)\n' "$DEST"
  find "$DEST" -type f | wc -l | awk '{print "files:", $1}'
}

if [[ "$MODE" == "smoke-inert" ]]; then
  stage_smoke_inert
  exit 0
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
cp -f "$SRC_PACK/default-sentinels.yaml" "$DEST/default-sentinels.yaml"
cp -f "$GATEWAY/nginx.conf" "$DEST/nginx.conf"
rsync -a --delete "$GATEWAY/conf/" "$DEST/conf/"
rsync -a --delete "$GATEWAY/lua/" "$DEST/lua/"
cp -f "$MANIFEST_SRC" "$DEST/image-manifest.json"

# Stamp packaging mode into a non-secret receipt next to the staged tree.
printf 'mode=%s\nmanifest_src=%s\n' "$MODE" "$MANIFEST_SRC" >"$DEST/STAGED_MODE.txt"

grep -q 'WATCH_PRODUCER_ENABLED=false' "$DEST/docker-compose.yml" || die "staged compose lost watch-off"
grep -q 'WATCH_DISPATCHER_ENABLED=false' "$DEST/docker-compose.yml" \
  || die "staged compose lost dispatcher-off"
grep -qF -- '- SENTINELS_CONFIG_PATH=${IRIN_WATCH_PROFILE_PATH:-}' "$DEST/docker-compose.yml" \
  || die "staged compose lost SENTINELS_CONFIG_PATH interpolation"
[[ -f "$DEST/default-sentinels.yaml" ]] || die "staged pack lost default-sentinels.yaml"

printf 'staged gateway pack -> %s (mode=%s, manifest=%s)\n' "$DEST" "$MODE" "$MANIFEST_SRC"
find "$DEST" -type f | wc -l | awk '{print "files:", $1}'
