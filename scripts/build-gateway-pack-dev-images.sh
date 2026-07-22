#!/usr/bin/env bash
# Development-only builder: create branch-clean arm64 Gateway + sidecar images and
# a test-only immutable local manifest. Does NOT publish. Does NOT weaken the
# production digest requirement (production still requires published name@sha256).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${IRIN_GATEWAY_PACK_BUILD_DIR:-$ROOT/packaging/build/gateway-pack}"
REQUIRE_CLEAN="${IRIN_GATEWAY_PACK_REQUIRE_CLEAN:-1}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ "$(uname -m)" == "arm64" ]] || die "Apple silicon (arm64) only"
command -v docker >/dev/null || die "docker CLI not found"
docker info >/dev/null 2>&1 || die "Docker daemon is not running"

if [[ "$REQUIRE_CLEAN" == "1" ]]; then
  if [[ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null || true)" ]]; then
    die "working tree is dirty; commit first so images embed a clean SHA (IRIN_GATEWAY_PACK_REQUIRE_CLEAN=0 to override)"
  fi
fi

SHA="$(git -C "$ROOT" rev-parse HEAD)"
SHORT="$(git -C "$ROOT" rev-parse --short=12 HEAD)"
DIRTY="$(git -C "$ROOT" status --porcelain 2>/dev/null | head -1 | wc -l | tr -d ' ')"
if [[ "$DIRTY" != "0" ]]; then
  TAG_SUFFIX="dirty-${SHORT}"
else
  TAG_SUFFIX="clean-${SHORT}"
fi

GW_TAG="irin-desktop/gateway:local-${TAG_SUFFIX}"
SC_TAG="irin-desktop/sidecar:local-${TAG_SUFFIX}"

mkdir -p "$OUT_DIR"

echo "=== build gateway image (arm64) $GW_TAG ==="
docker build \
  --platform linux/arm64 \
  -f "$ROOT/gateway/Dockerfile.gateway" \
  -t "$GW_TAG" \
  "$ROOT/gateway"

echo "=== build sidecar image (arm64) $SC_TAG ==="
# Sidecar Dockerfile expects monorepo root context (for .git provenance + path deps).
docker build \
  --platform linux/arm64 \
  -f "$ROOT/gateway/sidecar-rs/Dockerfile" \
  -t "$SC_TAG" \
  "$ROOT"

gw_id="$(docker image inspect --format '{{.Id}}' "$GW_TAG")"
sc_id="$(docker image inspect --format '{{.Id}}' "$SC_TAG")"
[[ "$gw_id" == sha256:* ]] || die "unexpected gateway image id: $gw_id"
[[ "$sc_id" == sha256:* ]] || die "unexpected sidecar image id: $sc_id"

gw_digest="${gw_id#sha256:}"
sc_digest="${sc_id#sha256:}"
[[ "${#gw_digest}" -eq 64 ]] || die "gateway digest length"
[[ "${#sc_digest}" -eq 64 ]] || die "sidecar digest length"

# Record as name@sha256:content-id (local proof). Production uses published repo digests.
GW_REF="irin-desktop/gateway@sha256:${gw_digest}"
SC_REF="irin-desktop/sidecar@sha256:${sc_digest}"

# Ensure Docker can resolve the digest form against the local image id.
docker tag "$gw_id" "irin-desktop/gateway:sha-${gw_digest:0:12}" || true
docker tag "$sc_id" "irin-desktop/sidecar:sha-${sc_digest:0:12}" || true

MANIFEST="$OUT_DIR/image-manifest.local.json"
cat >"$MANIFEST" <<EOF
{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "0.1.0-local-${TAG_SUFFIX}",
  "source_sha": "$SHA",
  "source_dirty": $([ "$DIRTY" = "0" ] && echo false || echo true),
  "built_at_unix": $(date +%s),
  "platform": "linux/arm64",
  "notes": "Test-only local manifest. Not a releasable GHCR ref. Production requires published name@sha256 digests.",
  "images": {
    "gateway": "$GW_REF",
    "sidecar": "$SC_REF"
  },
  "local_tags": {
    "gateway": "$GW_TAG",
    "sidecar": "$SC_TAG"
  },
  "image_ids": {
    "gateway": "$gw_id",
    "sidecar": "$sc_id"
  },
  "third_party_pins": {
    "openresty_base": "openresty/openresty:1.31.1.1-alpine-slim@sha256:bde121249048b8c7f73500287e90d32aa65edc9672d4f5ab454c1cf7e029c6ec",
    "rust_builder": "rust:alpine@sha256:f87aa870663e2b57ec8c69de82c7eedf7383bee987eef7612c0359635eaadb41"
  },
  "watch_invariants": {
    "WATCH_PRODUCER_ENABLED": false,
    "WATCH_DISPATCHER_ENABLED": false
  }
}
EOF
chmod 644 "$MANIFEST"

# Also write a resolved env snippet for manual compose smoke (not used by the app).
cat >"$OUT_DIR/compose.images.env" <<EOF
IRIN_GATEWAY_IMAGE=$GW_REF
IRIN_SIDECAR_IMAGE=$SC_REF
EOF
chmod 644 "$OUT_DIR/compose.images.env"

echo "=== local gateway pack images ready ==="
echo "manifest=$MANIFEST"
echo "gateway=$GW_REF"
echo "sidecar=$SC_REF"
echo "NOTE: local-dev only; production still requires published immutable digests."
