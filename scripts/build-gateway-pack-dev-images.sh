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
DIRTY_COUNT="$(git -C "$ROOT" status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
if [[ "$DIRTY_COUNT" != "0" ]]; then
  TAG_SUFFIX="dirty-${SHORT}"
  SOURCE_DIRTY=true
else
  TAG_SUFFIX="clean-${SHORT}"
  SOURCE_DIRTY=false
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

echo "=== prepare sidecar docker context ==="
# Sidecar Dockerfile needs monorepo root + a real .git (not a worktree gitfile).
CTX=""
cleanup_ctx() {
  if [[ -n "${CTX:-}" && -d "${CTX:-}" ]]; then
    rm -rf "$CTX"
  fi
}
trap cleanup_ctx EXIT

if [[ -f "$ROOT/.git" ]]; then
  CTX="$(mktemp -d "${TMPDIR:-/tmp}/irin-gw-pack-ctx.XXXXXX")"
  echo "worktree detected; materializing self-contained context at $CTX"
  COMMON="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir)"
  # Full HEAD tree via checkout-index — not `git archive`, which applies
  # export-ignore and drops tracked paths (examples/, etc.). Those missing
  # paths made `git status` non-empty after read-tree, so build.rs baked
  # GW_BUILD_DIRTY=true on a clean monorepo (false rehearsal-only arm).
  git -C "$ROOT" checkout-index --all --prefix="$CTX/"
  mkdir -p "$CTX/.git/objects" "$CTX/.git/refs/heads" "$CTX/.git/info"
  rsync -a "$COMMON/objects/" "$CTX/.git/objects/"
  if [[ -f "$COMMON/packed-refs" ]]; then
    cp -f "$COMMON/packed-refs" "$CTX/.git/packed-refs"
  fi
  if [[ -d "$COMMON/refs" ]]; then
    rsync -a "$COMMON/refs/" "$CTX/.git/refs/"
  fi
  printf 'ref: refs/heads/pack-build\n' >"$CTX/.git/HEAD"
  echo "$SHA" >"$CTX/.git/refs/heads/pack-build"
  cat >"$CTX/.git/config" <<'EOF'
[core]
	repositoryformatversion = 0
	filemode = true
	bare = false
	logallrefupdates = true
EOF
  git -C "$CTX" read-tree HEAD

  # Dirty local-dev builds must compile the actual candidate, not silently
  # fall back to HEAD. Overlay tracked changes as a binary-safe patch and
  # copy only Git-visible untracked source. The temporary repository then
  # truthfully reports dirty provenance to build.rs inside Docker.
  if [[ "$DIRTY_COUNT" != "0" ]]; then
    if ! git -C "$ROOT" diff --quiet HEAD --; then
      git -C "$ROOT" diff --binary HEAD -- |
        git -C "$CTX" apply --whitespace=nowarn -
    fi
    while IFS= read -r -d '' rel; do
      mkdir -p "$CTX/$(dirname "$rel")"
      cp -p "$ROOT/$rel" "$CTX/$rel"
    done < <(git -C "$ROOT" ls-files --others --exclude-standard -z)
    [[ -n "$(git -C "$CTX" status --porcelain)" ]] ||
      die "dirty worktree overlay produced a clean sidecar context"
  else
    # Fail closed: a clean monorepo must materialize a clean sidecar context
    # so GW_BUILD_DIRTY=false is honest (not false-dirty from export-ignore).
    [[ -z "$(git -C "$CTX" status --porcelain 2>/dev/null || true)" ]] ||
      die "clean monorepo materialized a dirty sidecar context (checkout-index/read-tree bug)"
  fi

  git -C "$CTX" rev-parse HEAD >/dev/null \
    || die "materialized context is not a git repository"
  SIDECAR_CONTEXT="$CTX"
else
  SIDECAR_CONTEXT="$ROOT"
fi

echo "=== build sidecar image (arm64) $SC_TAG ==="
docker build \
  --platform linux/arm64 \
  -f "$ROOT/gateway/sidecar-rs/Dockerfile" \
  -t "$SC_TAG" \
  "$SIDECAR_CONTEXT"

cleanup_ctx
trap - EXIT
CTX=""

gw_id="$(docker image inspect --format '{{.Id}}' "$GW_TAG")"
sc_id="$(docker image inspect --format '{{.Id}}' "$SC_TAG")"
[[ "$gw_id" == sha256:* ]] || die "unexpected gateway image id: $gw_id"
[[ "$sc_id" == sha256:* ]] || die "unexpected sidecar image id: $sc_id"

gw_digest="${gw_id#sha256:}"
sc_digest="${sc_id#sha256:}"
[[ "${#gw_digest}" -eq 64 ]] || die "gateway digest length"
[[ "${#sc_digest}" -eq 64 ]] || die "sidecar digest length"

GW_REF="irin-desktop/gateway@sha256:${gw_digest}"
SC_REF="irin-desktop/sidecar@sha256:${sc_digest}"

docker tag "$gw_id" "irin-desktop/gateway:sha-${gw_digest:0:12}" || true
docker tag "$sc_id" "irin-desktop/sidecar:sha-${sc_digest:0:12}" || true

MANIFEST="$OUT_DIR/image-manifest.local.json"
cat >"$MANIFEST" <<EOF
{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "0.1.0-local-${TAG_SUFFIX}",
  "source_sha": "$SHA",
  "source_dirty": $SOURCE_DIRTY,
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
