#!/usr/bin/env bash
# Generate the production Gateway Pack image manifest from PUBLISHED GHCR refs.
# Resolves digests from the registry (never trusts local state), asserts the
# canonical ghcr.io/irinityhq registry, and writes the manifest consumed by
# IRIN_GATEWAY_PACK_PROD_MANIFEST in production packaging.
#
# Required env:
#   IRIN_PACK_IMAGES_TAG   image tag to pin (v<semver> or rc-<sha12>)
# Optional env:
#   IRIN_PACK_MANIFEST_OUT  output path (default: packaging/build/gateway-pack/image-manifest.production.json)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

TAG="${IRIN_PACK_IMAGES_TAG:-}"
[[ -n "$TAG" ]] || die "IRIN_PACK_IMAGES_TAG is required"
OUT="${IRIN_PACK_MANIFEST_OUT:-$ROOT/packaging/build/gateway-pack/image-manifest.production.json}"

GW_IMAGE="ghcr.io/irinityhq/irin-gateway"
SC_IMAGE="ghcr.io/irinityhq/irin-sidecar"

command -v docker >/dev/null || die "docker CLI not found"

resolve() {
  local ref="$1"
  # Anonymous read: works for public packages with no registry auth.
  docker buildx imagetools inspect "$ref" --format '{{.Manifest.Digest}}' 2>/dev/null \
    || die "cannot resolve $ref — published? package public?"
}

echo "=== resolve published digests (registry is the source of truth) ==="
gw_digest="$(resolve "$GW_IMAGE:$TAG")"
sc_digest="$(resolve "$SC_IMAGE:$TAG")"

for pair in "gateway:$gw_digest" "sidecar:$sc_digest"; do
  name="${pair%%:*}"; digest="${pair#*:}"
  [[ "$digest" == sha256:* && "${#digest}" -eq 71 ]] || die "bad $name digest: $digest"
done

GW_REF="$GW_IMAGE@$gw_digest"
SC_REF="$SC_IMAGE@$sc_digest"

# Hard assertions: canonical registry only, never placeholder/local names.
case "$GW_REF" in ghcr.io/irinityhq/*) ;; *) die "registry assertion failed: $GW_REF" ;; esac
case "$SC_REF" in ghcr.io/irinityhq/*) ;; *) die "registry assertion failed: $SC_REF" ;; esac
[[ "$GW_REF" != *example* && "$SC_REF" != *example* ]] || die "placeholder registry leaked into manifest"
[[ "$GW_REF" != *irin-desktop/* && "$SC_REF" != *irin-desktop/* ]] || die "local image name leaked into manifest"

SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"

mkdir -p "$(dirname "$OUT")"
cat >"$OUT" <<EOF
{
  "schema_version": 1,
  "mode": "production",
  "pack_version": "${TAG#v}",
  "source_sha": "$SHA",
  "source_dirty": false,
  "built_at_unix": $(date +%s),
  "platform": "linux/arm64",
  "notes": "Pinned from the live registry at generation time; refs are name@sha256 only.",
  "images": {
    "gateway": "$GW_REF",
    "sidecar": "$SC_REF"
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
chmod 644 "$OUT"

echo "=== production manifest written ==="
echo "manifest=$OUT"
echo "gateway=$GW_REF"
echo "sidecar=$SC_REF"
