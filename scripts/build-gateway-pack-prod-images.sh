#!/usr/bin/env bash
# Build and PUSH production Gateway Pack images to GHCR with immutable digest refs.
#
# Runs on GitHub Actions (tag lane / rc dispatch) and locally for operator re-runs.
# The unit is shared so CI and the operator produce identical images from the
# exact same source.
#
# Required env:
#   IRIN_PACK_IMAGES_TAG       image tag, e.g. v0.1.0 or rc-<sha12>
# Auth (one of):
#   GITHUB_TOKEN + github.actor on Actions (packages:write), or
#   GHCR_USERNAME + GHCR_TOKEN locally (PAT with write:packages)
# Optional:
#   IRIN_GATEWAY_PACK_REQUIRE_CLEAN=1 (default; ignored on Actions checkout)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${IRIN_GATEWAY_PACK_BUILD_DIR:-$ROOT/packaging/build/gateway-pack}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null || die "docker CLI not found"
docker info >/dev/null 2>&1 || die "Docker daemon is not running"

TAG="${IRIN_PACK_IMAGES_TAG:-}"
[[ -n "$TAG" ]] || die "IRIN_PACK_IMAGES_TAG is required (v<semver> or rc-<sha12>)"
case "$TAG" in
  v[0-9]*.[0-9]*.[0-9]*|rc-????????????) ;;
  *) die "IRIN_PACK_IMAGES_TAG must be v<semver> or rc-<sha12> (got $TAG)" ;;
esac

REGISTRY="ghcr.io"
OWNER="irinityhq"
GW_IMAGE="$REGISTRY/$OWNER/irin-gateway"
SC_IMAGE="$REGISTRY/$OWNER/irin-sidecar"

SHA="$(git -C "$ROOT" rev-parse HEAD)"
if [[ -z "${GITHUB_ACTIONS:-}" && "${IRIN_GATEWAY_PACK_REQUIRE_CLEAN:-1}" == "1" ]]; then
  [[ -z "$(git -C "$ROOT" status --porcelain 2>/dev/null || true)" ]] \
    || die "working tree is dirty; commit first (IRIN_GATEWAY_PACK_REQUIRE_CLEAN=0 to override)"
fi

echo "=== GHCR login ($REGISTRY) ==="
if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
  echo "${GITHUB_TOKEN}" | docker login "$REGISTRY" -u "${GITHUB_ACTOR:-github-actions}" --password-stdin
else
  # Default to the operator's gh login (needs write:packages scope:
  # gh auth refresh -h github.com -s write:packages). Explicit
  # GHCR_USERNAME/GHCR_TOKEN still wins when set.
  if [[ -z "${GHCR_TOKEN:-}" ]] && command -v gh >/dev/null; then
    GHCR_TOKEN="$(gh auth token 2>/dev/null || true)"
    GHCR_USERNAME="${GHCR_USERNAME:-$(gh api user --jq .login 2>/dev/null || true)}"
  fi
  [[ -n "${GHCR_USERNAME:-}" && -n "${GHCR_TOKEN:-}" ]] \
    || die "local run requires gh auth (write:packages) or GHCR_USERNAME + GHCR_TOKEN"
  echo "${GHCR_TOKEN}" | docker login "$REGISTRY" -u "${GHCR_USERNAME}" --password-stdin
fi

echo "=== prepare sidecar docker context (worktree-safe) ==="
CTX=""
cleanup_ctx() { [[ -n "${CTX:-}" && -d "${CTX:-}" ]] && rm -rf "$CTX"; }
trap cleanup_ctx EXIT
if [[ -f "$ROOT/.git" ]]; then
  CTX="$(mktemp -d "${TMPDIR:-/tmp}/irin-gw-pack-ctx.XXXXXX")"
  COMMON="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir)"
  git -C "$ROOT" archive HEAD | tar -x -C "$CTX"
  mkdir -p "$CTX/.git/objects" "$CTX/.git/refs/heads" "$CTX/.git/info"
  rsync -a "$COMMON/objects/" "$CTX/.git/objects/"
  [[ -f "$COMMON/packed-refs" ]] && cp -f "$COMMON/packed-refs" "$CTX/.git/packed-refs"
  [[ -d "$COMMON/refs" ]] && rsync -a "$COMMON/refs/" "$CTX/.git/refs/"
  printf 'ref: refs/heads/pack-build\n' >"$CTX/.git/HEAD"
  echo "$SHA" >"$CTX/.git/refs/heads/pack-build"
  printf '[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n' >"$CTX/.git/config"
  git -C "$CTX" read-tree HEAD
  while IFS= read -r rel; do
    [[ -n "$rel" ]] || continue
    [[ -e "$CTX/$rel" ]] || git -C "$CTX" update-index --force-remove -- "$rel" 2>/dev/null || true
  done < <(git -C "$CTX" ls-files)
  SIDECAR_CONTEXT="$CTX"
else
  SIDECAR_CONTEXT="$ROOT"
fi

docker buildx inspect irin-pack-builder >/dev/null 2>&1 \
  || docker buildx create --name irin-pack-builder --use >/dev/null
docker buildx use irin-pack-builder

echo "=== build+push gateway image (linux/arm64) $GW_IMAGE:$TAG ==="
docker buildx build --platform linux/arm64 \
  -f "$ROOT/gateway/Dockerfile.gateway" \
  -t "$GW_IMAGE:$TAG" --push "$ROOT/gateway"

echo "=== build+push sidecar image (linux/arm64) $SC_IMAGE:$TAG ==="
docker buildx build --platform linux/arm64 \
  -f "$ROOT/gateway/sidecar-rs/Dockerfile" \
  -t "$SC_IMAGE:$TAG" --push "$SIDECAR_CONTEXT"

cleanup_ctx
trap - EXIT
CTX=""

echo "=== resolve published digests ==="
gw_digest="$(docker buildx imagetools inspect "$GW_IMAGE:$TAG" --format '{{.Manifest.Digest}}')"
sc_digest="$(docker buildx imagetools inspect "$SC_IMAGE:$TAG" --format '{{.Manifest.Digest}}')"
[[ "$gw_digest" == sha256:* && "${#gw_digest}" -eq 71 ]] || die "bad gateway digest: $gw_digest"
[[ "$sc_digest" == sha256:* && "${#sc_digest}" -eq 71 ]] || die "bad sidecar digest: $sc_digest"

mkdir -p "$OUT_DIR"
RECEIPT="$OUT_DIR/pushed-images-$TAG.env"
cat >"$RECEIPT" <<EOF
IRIN_PACK_IMAGES_TAG=$TAG
IRIN_PACK_IMAGES_SOURCE_SHA=$SHA
IRIN_GATEWAY_IMAGE=$GW_IMAGE@$gw_digest
IRIN_SIDECAR_IMAGE=$SC_IMAGE@$sc_digest
EOF
chmod 644 "$RECEIPT"

echo "=== production images published ==="
echo "gateway=$GW_IMAGE@$gw_digest"
echo "sidecar=$SC_IMAGE@$sc_digest"
echo "receipt=$RECEIPT"
echo "NOTE: first push creates the packages PRIVATE; flip both to public before release."
