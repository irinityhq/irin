#!/usr/bin/env bash
# IRIN release transaction: the single fail-closed ladder from source to an
# accepted, notarized IRIN.dmg. Nothing here publishes a GitHub Release in
# --dry-run-rc mode; in release mode assets attach to the DRAFT release that
# .github/workflows/release.yml creates on the tag.
#
# Usage:
#   scripts/release-transaction.sh --tag v0.1.0          # release mode (post-merge, tag pushed)
#   scripts/release-transaction.sh --dry-run-rc          # pre-merge proof on this branch head
#
# Required env (both modes):
#   APPLE_SIGNING_IDENTITY   e.g. "Developer ID Application: Name (TEAMID)"
#   APPLE_NOTARY_PROFILE     notarytool keychain profile (xcrun notarytool store-credentials)
# Release mode additionally expects the release-images workflow to have
# published v<semver> images; dry-run dispatches rc-<sha12> images itself.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

MODE=""
TAG=""
case "${1:-}" in
  --tag) MODE="release"; TAG="${2:-}" ;;
  --dry-run-rc) MODE="rc" ;;
  *) die "usage: $0 --tag v<semver> | --dry-run-rc" ;;
esac

note "preflight: machine and credentials"
[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ "$(uname -m)" == "arm64" ]] || die "Apple silicon only"
command -v docker >/dev/null && docker info >/dev/null 2>&1 || die "Docker required for the promotion smoke"
command -v gh >/dev/null || die "gh CLI required"
[[ -n "${APPLE_SIGNING_IDENTITY:-}" ]] || die "APPLE_SIGNING_IDENTITY is required"
[[ -n "${APPLE_NOTARY_PROFILE:-}" ]] || die "APPLE_NOTARY_PROFILE is required"
security find-identity -v -p codesigning | grep -F "$APPLE_SIGNING_IDENTITY" >/dev/null \
  || die "signing identity not in keychain: $APPLE_SIGNING_IDENTITY"
xcrun notarytool history --keychain-profile "$APPLE_NOTARY_PROFILE" --output-format json >/dev/null 2>&1 \
  || die "notary profile unusable: $APPLE_NOTARY_PROFILE"

note "preflight: source tree"
[[ -z "$(git status --porcelain 2>/dev/null || true)" ]] || die "working tree is dirty"
SHA="$(git rev-parse HEAD)"
[[ -z "${IRIN_SMOKE_APP:-}" ]] || die "IRIN_SMOKE_APP substitution is forbidden in the release transaction"
[[ -z "${IRIN_APP_SUPPORT_ROOT:-}" ]] || die "IRIN_APP_SUPPORT_ROOT isolation is forbidden in the release transaction"
[[ "$HOME" == "/Users/"* && ! -L "$HOME" ]] || die "remapped HOME is forbidden in the release transaction"

case "$MODE" in
  release)
    [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "bad tag: $TAG"
    [[ "$(git rev-parse "$TAG^{commit}" 2>/dev/null || true)" == "$SHA" ]] \
      || die "HEAD ($SHA) is not the tagged commit ($TAG)"
    IMAGES_TAG="$TAG"
    # The production DMG gate requires this to equal the tauri.conf.json
    # version; a mismatch dies there with the version-bump explanation.
    IRIN_RELEASE_VERSION="${TAG#v}"
    ;;
  rc)
    IMAGES_TAG="rc-$(git rev-parse --short=12 HEAD)"
    # No tag in the rc lane: derive the version from the Tauri bundle config
    # so the production DMG gate passes by construction.
    IRIN_RELEASE_VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' \
      "$ROOT/council-rs/warroom-tauri/src-tauri/tauri.conf.json")" \
      || die "could not read version from tauri.conf.json (python3 required)"
    ;;
esac
export IRIN_RELEASE_VERSION
echo "source_sha=$SHA images_tag=$IMAGES_TAG mode=$MODE release_version=$IRIN_RELEASE_VERSION"

note "images: ensure published digests exist"
if [[ "$MODE" == "rc" ]]; then
  # The rc lane cannot dispatch release-images.yml: workflow_dispatch only
  # works for workflows already on the default branch, and this workflow
  # arrives with the product PR. Build and push rc images locally with
  # operator GHCR credentials instead (GHCR_USERNAME + GHCR_TOKEN, PAT with
  # write:packages — revocable once the tag lane runs on merged source).
  command -v rsync >/dev/null || die "rsync required for the image context"
  IRIN_PACK_IMAGES_TAG="$IMAGES_TAG" bash scripts/build-gateway-pack-prod-images.sh
fi

note "manifest: generate from the live registry"
IRIN_PACK_IMAGES_TAG="$IMAGES_TAG" bash scripts/generate-production-manifest.sh
MANIFEST="$ROOT/packaging/build/gateway-pack/image-manifest.production.json"
grep -q '"mode"[[:space:]]*:[[:space:]]*"production"' "$MANIFEST" || die "manifest is not production"
grep -E '"gateway"|"sidecar"' "$MANIFEST" | grep -q 'ghcr.io/irinityhq/.*@sha256:' || die "manifest images are not pinned ghcr digests"
grep -E '"gateway"|"sidecar"' "$MANIFEST" | grep -qE 'example|irin-desktop/' && die "manifest contains placeholder/local refs"

note "dmg: production build (sign + notarize + staple)"
IRIN_DMG_PACK_MODE=production \
IRIN_GATEWAY_PACK_PROD_MANIFEST="$MANIFEST" \
IRIN_DMG_REQUIRE_CLEAN=1 \
make dmg-build

note "dmg: verify untouched artifact"
IRIN_DMG_PACK_MODE=production make dmg-verify

note "promotion smoke on the untouched DMG"
PROMOTION=1 bash packaging/smoke-full-app.sh

note "checksums and receipt"
HASHES="$ROOT/packaging/artifacts/HASHES.txt"
[[ -f "$HASHES" ]] || die "HASHES.txt missing"
DMG="$ROOT/packaging/artifacts/IRIN_${IRIN_RELEASE_VERSION}_aarch64.dmg"
[[ -f "$DMG" ]] || die "DMG artifact missing: $DMG"
shasum -a 256 "$DMG"

RECEIPT="$ROOT/packaging/receipts/release-transaction-$(date +%Y%m%dT%H%M%S).txt"
mkdir -p "$ROOT/packaging/receipts"
{
  echo "mode=$MODE"
  echo "source_sha=$SHA"
  echo "images_tag=$IMAGES_TAG"
  grep -E '"gateway"|"sidecar"' "$MANIFEST"
  echo "dmg=$DMG"
  shasum -a 256 "$DMG"
  echo "signing_identity=$APPLE_SIGNING_IDENTITY"
  echo "notarized=true stapled=true verified=true promotion=FULL_PASS"
} >"$RECEIPT"
echo "receipt=$RECEIPT"

if [[ "$MODE" == "release" ]]; then
  note "attach accepted bytes to the DRAFT release"
  gh release view "$TAG" --json isDraft --jq '.isDraft' | grep -q true \
    || die "release $TAG is not a draft — refusing to mutate a public release"
  gh release upload "$TAG" "$DMG" "$HASHES" --clobber
  echo "NEXT: re-download the asset from the draft, checksum-compare, install, launch — then publish."
else
  note "dry-run complete: production path proven on rc images; no release mutated"
fi
