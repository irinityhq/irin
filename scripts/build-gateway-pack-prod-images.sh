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

REVISION_ANNOTATION="org.opencontainers.image.revision"
SIDECAR_ELIGIBILITY_ANNOTATION="io.irinity.irin.sidecar.release-eligible"

resolve_digest() {
  local ref="$1" digest
  digest="$(docker buildx imagetools inspect "$ref" --format '{{.Manifest.Digest}}' 2>/dev/null)" \
    || die "cannot resolve published digest for $ref"
  [[ "$digest" == sha256:* && "${#digest}" -eq 71 ]] || die "bad digest for $ref: $digest"
  printf '%s' "$digest"
}

image_annotation() {
  local digest_ref="$1" key="$2" value
  [[ "$digest_ref" == *@sha256:* ]] || die "annotation inspection requires a digest-bound ref (got $digest_ref)"
  value="$(docker buildx imagetools inspect "$digest_ref" --format "{{index .Manifest.Annotations \"$key\"}}" 2>/dev/null || true)"
  printf '%s' "$value" | tr -d '[:space:]'
}

require_revision() {
  local digest_ref="$1" intended_sha="$2" label="$3" actual
  actual="$(image_annotation "$digest_ref" "$REVISION_ANNOTATION")"
  [[ "$actual" == "$intended_sha" ]] \
    || die "$label provenance mismatch: $digest_ref has ${REVISION_ANNOTATION}=${actual:-<missing>}, expected $intended_sha"
}

require_sidecar_eligibility() {
  local digest_ref="$1" actual
  actual="$(image_annotation "$digest_ref" "$SIDECAR_ELIGIBILITY_ANNOTATION")"
  [[ "$actual" == "true" ]] \
    || die "sidecar is not release-eligible: $digest_ref has ${SIDECAR_ELIGIBILITY_ANNOTATION}=${actual:-<missing>} (expected true)"
}

# PUSH_GW / PUSH_SC: 1 = build+push this image; 0 = already published, skip.
# RECEIPT_ONLY: both tags already exist — never overwrite; only re-inspect digests
# and write the receipt (recovers from a post-push inspect/receipt failure).
# Provenance is accepted only when digest-bound annotations match this exact
# checkout; a moved tag cannot rewrite or recover a receipt for another commit.
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

# A release retry may publish only the missing half of an immutable pair. Before
# any build or push, prove every existing half is the intended release commit.
# The sidecar additionally needs the immutable release-eligibility annotation;
# the compile-time build arg alone is not registry evidence.
if [[ "$PUSH_GW" -eq 0 ]]; then
  existing_gw_digest="$(resolve_digest "$GW_IMAGE:$TAG")"
  require_revision "$GW_IMAGE@$existing_gw_digest" "$SHA" "existing gateway"
fi
if [[ "$PUSH_SC" -eq 0 ]]; then
  existing_sc_digest="$(resolve_digest "$SC_IMAGE:$TAG")"
  require_revision "$SC_IMAGE@$existing_sc_digest" "$SHA" "existing sidecar"
  require_sidecar_eligibility "$SC_IMAGE@$existing_sc_digest"
fi

if [[ "$RECEIPT_ONLY" -eq 1 ]]; then
  echo "=== skip build/push (receipt-only recovery for complete published pair) ==="
  CTX=""
else
echo "=== prepare sidecar docker context (worktree-safe) ==="
CTX=""
cleanup_ctx() {
  if [[ -n "${CTX:-}" && -d "$CTX" ]]; then
    rm -rf "$CTX"
  fi
}
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
  --annotation "index:${REVISION_ANNOTATION}=${SHA}"
  --annotation "manifest:${REVISION_ANNOTATION}=${SHA}"
  --label "${REVISION_ANNOTATION}=${SHA}"
)
SIDECAR_ELIGIBILITY_ANNOT=(
  --annotation "index:${SIDECAR_ELIGIBILITY_ANNOTATION}=true"
  --annotation "manifest:${SIDECAR_ELIGIBILITY_ANNOTATION}=true"
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
    --build-arg GW_RELEASE_ELIGIBLE=true \
    "${OCI_REV_ANNOT[@]}" \
    "${SIDECAR_ELIGIBILITY_ANNOT[@]}" \
    -t "$SC_IMAGE:$TAG" --push "$SIDECAR_CONTEXT"
else
  echo "=== skip sidecar push (already published, immutable) $SC_IMAGE:$TAG ==="
fi

cleanup_ctx
trap - EXIT
CTX=""
fi

echo "=== resolve and verify published digests ==="
gw_digest="$(resolve_digest "$GW_IMAGE:$TAG")"
sc_digest="$(resolve_digest "$SC_IMAGE:$TAG")"
GW_DIGEST_REF="$GW_IMAGE@$gw_digest"
SC_DIGEST_REF="$SC_IMAGE@$sc_digest"

# Receipt generation is a release gate, not a place to infer provenance. Both
# digest-bound images must attest to this exact checkout, and the sidecar must
# carry the publisher-only eligibility annotation. Ordinary/dev images lack it.
require_revision "$GW_DIGEST_REF" "$SHA" "gateway"
require_revision "$SC_DIGEST_REF" "$SHA" "sidecar"
require_sidecar_eligibility "$SC_DIGEST_REF"
SOURCE_SHA="$SHA"

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
