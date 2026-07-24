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

echo "=== immutability check: $TAG ==="
# Release tags are immutable: once a *complete* pair (gateway + sidecar) is
# published under a v* tag, neither image is replaced. A partial prior push
# (one image present, the other missing) must not strand retries — the missing
# half may still be published, but the existing half is never overwritten.
# rc-* tags are operator iteration and may be rebuilt. Absence proof is
# fail-closed: only an explicit not-found counts as unpublished; any
# transient registry, network, or auth failure aborts the publish.
tag_is_unpublished() {
  local ref="$1" out
  if out="$(docker buildx imagetools inspect "$ref" 2>&1)"; then
    return 1
  fi
  # Match only explicit registry not-found shapes. Do not match bare "unknown"
  # (e.g. "certificate signed by unknown authority" must fail closed).
  case "$out" in
    *"not found"*|*"Not Found"*|*"NOT_FOUND"*| \
    *"MANIFEST_UNKNOWN"*|*"manifest unknown"*| \
    *"NAME_UNKNOWN"*|*"Name Unknown"*) return 0 ;;
    *) die "cannot prove $ref is unpublished (registry inspection failed): $out" ;;
  esac
}

# PUSH_GW / PUSH_SC: 1 = build+push this image; 0 = already published, skip.
# RECEIPT_ONLY: both tags already exist — never overwrite; only re-inspect digests
# and write the receipt (recovers from a post-push inspect/receipt failure).
# Provenance SHA comes from image annotations / IRIN_PACK_IMAGES_SOURCE_SHA —
# never from a possibly different current HEAD (moved-tag safety).
PUSH_GW=1
PUSH_SC=1
RECEIPT_ONLY=0
case "$TAG" in
  rc-*)
    ;;
  *)
    gw_unpub=0
    sc_unpub=0
    tag_is_unpublished "$GW_IMAGE:$TAG" && gw_unpub=1
    tag_is_unpublished "$SC_IMAGE:$TAG" && sc_unpub=1
    if [[ "$gw_unpub" -eq 0 && "$sc_unpub" -eq 0 ]]; then
      # Complete pair already published: immutability forbids rebuild, but a
      # prior run may have failed after push while writing the digest receipt.
      # Re-inspect and rewrite the receipt only — never strand a complete pair.
      PUSH_GW=0
      PUSH_SC=0
      RECEIPT_ONLY=1
      echo "both $GW_IMAGE:$TAG and $SC_IMAGE:$TAG already published — receipt-only (no overwrite)"
    else
      if [[ "$gw_unpub" -eq 0 ]]; then
        PUSH_GW=0
        echo "gateway $GW_IMAGE:$TAG already published — will not overwrite; pushing sidecar only if needed"
      fi
      if [[ "$sc_unpub" -eq 0 ]]; then
        PUSH_SC=0
        echo "sidecar $SC_IMAGE:$TAG already published — will not overwrite; pushing gateway only if needed"
      fi
    fi
    ;;
esac

if [[ "$RECEIPT_ONLY" -eq 1 ]]; then
  echo "=== skip build/push (receipt-only recovery for complete published pair) ==="
  CTX=""
else
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

# Provenance on every publish: index annotations + image labels so receipt-only
# recovery can re-read the commit that actually built these digests.
OCI_REV_ANNOT=(
  --annotation "index:org.opencontainers.image.revision=${SHA}"
  --annotation "manifest:org.opencontainers.image.revision=${SHA}"
  --label "org.opencontainers.image.revision=${SHA}"
)

if [[ "$PUSH_GW" -eq 1 ]]; then
  echo "=== build+push gateway image (linux/arm64) $GW_IMAGE:$TAG ==="
  docker buildx build --platform linux/arm64 \
    -f "$ROOT/gateway/Dockerfile.gateway" \
    "${OCI_REV_ANNOT[@]}" \
    -t "$GW_IMAGE:$TAG" --push "$ROOT/gateway"
else
  echo "=== skip gateway push (already published, immutable) $GW_IMAGE:$TAG ==="
fi

if [[ "$PUSH_SC" -eq 1 ]]; then
  echo "=== build+push sidecar image (linux/arm64) $SC_IMAGE:$TAG ==="
  docker buildx build --platform linux/arm64 \
    -f "$ROOT/gateway/sidecar-rs/Dockerfile" \
    "${OCI_REV_ANNOT[@]}" \
    -t "$SC_IMAGE:$TAG" --push "$SIDECAR_CONTEXT"
else
  echo "=== skip sidecar push (already published, immutable) $SC_IMAGE:$TAG ==="
fi

cleanup_ctx
trap - EXIT
CTX=""
fi

echo "=== resolve published digests ==="
gw_digest="$(docker buildx imagetools inspect "$GW_IMAGE:$TAG" --format '{{.Manifest.Digest}}')"
sc_digest="$(docker buildx imagetools inspect "$SC_IMAGE:$TAG" --format '{{.Manifest.Digest}}')"
[[ "$gw_digest" == sha256:* && "${#gw_digest}" -eq 71 ]] || die "bad gateway digest: $gw_digest"
[[ "$sc_digest" == sha256:* && "${#sc_digest}" -eq 71 ]] || die "bad sidecar digest: $sc_digest"

# Provenance for the receipt must name the commit that *built* these digests.
# On a normal publish that is HEAD. On receipt-only recovery we never stamp
# the current checkout onto foreign digests (e.g. a moved v* tag pointing at
# a newer main commit while the registry still holds the earlier build).
image_revision_annotation() {
  local ref="$1" rev
  rev="$(docker buildx imagetools inspect "$ref" --format '{{index .Manifest.Annotations "org.opencontainers.image.revision"}}' 2>/dev/null || true)"
  rev="$(printf '%s' "$rev" | tr -d '[:space:]')"
  [[ "$rev" =~ ^[0-9a-f]{40}$ ]] || return 1
  printf '%s' "$rev"
}

if [[ "$RECEIPT_ONLY" -eq 1 ]]; then
  SOURCE_SHA=""
  # Explicit operator override wins (documented recovery path).
  if [[ -n "${IRIN_PACK_IMAGES_SOURCE_SHA:-}" ]]; then
    SOURCE_SHA="$(printf '%s' "$IRIN_PACK_IMAGES_SOURCE_SHA" | tr -d '[:space:]')"
    [[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]       || die "IRIN_PACK_IMAGES_SOURCE_SHA must be a 40-char lowercase git SHA (got ${IRIN_PACK_IMAGES_SOURCE_SHA})"
  else
    gw_rev="$(image_revision_annotation "$GW_IMAGE:$TAG" || true)"
    sc_rev="$(image_revision_annotation "$SC_IMAGE:$TAG" || true)"
    if [[ -n "$gw_rev" && "$gw_rev" == "$sc_rev" ]]; then
      SOURCE_SHA="$gw_rev"
    fi
  fi
  [[ -n "$SOURCE_SHA" ]] || die "receipt-only recovery: refuse to stamp current checkout ($SHA) onto already-published digests without verified provenance. Re-run with IRIN_PACK_IMAGES_SOURCE_SHA=<40-char commit that built these images>, or ensure both images were published with matching org.opencontainers.image.revision annotations."
  if [[ "$SOURCE_SHA" != "$SHA" ]]; then
    echo "NOTE: receipt SOURCE_SHA=$SOURCE_SHA differs from current HEAD=$SHA (provenance from published images / override; digests not rebuilt)"
  fi
else
  SOURCE_SHA="$SHA"
fi

mkdir -p "$OUT_DIR"
RECEIPT="$OUT_DIR/pushed-images-$TAG.env"
cat >"$RECEIPT" <<EOF
IRIN_PACK_IMAGES_TAG=$TAG
IRIN_PACK_IMAGES_SOURCE_SHA=$SOURCE_SHA
IRIN_GATEWAY_IMAGE=$GW_IMAGE@$gw_digest
IRIN_SIDECAR_IMAGE=$SC_IMAGE@$sc_digest
EOF
chmod 644 "$RECEIPT"

if [[ "$RECEIPT_ONLY" -eq 1 ]]; then
  echo "=== production image receipt recovered (no push) ==="
else
  echo "=== production images published ==="
fi
echo "gateway=$GW_IMAGE@$gw_digest"
echo "sidecar=$SC_IMAGE@$sc_digest"
echo "receipt=$RECEIPT"
echo "NOTE: first push creates the packages PRIVATE; flip both to public before release."
