#!/usr/bin/env bash
# Deterministic production-image provenance contracts. Uses a fake Docker CLI;
# no daemon, registry access, image build, or push occurs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/irin-prod-provenance.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
FAKE_BIN="$TMP/bin"
FAKE_LOG="$TMP/docker.log"
FAKE_STATE="$TMP/state"
mkdir -p "$FAKE_BIN" "$FAKE_STATE"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

cat >"$FAKE_BIN/docker" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_DOCKER_LOG"

case "${1:-}" in
  info) exit 0 ;;
  login) cat >/dev/null; exit 0 ;;
esac

if [[ "${1:-}" == "buildx" && "${2:-}" == "imagetools" && "${3:-}" == "inspect" ]]; then
  ref="${4:-}"
  if [[ "$ref" == *irin-gateway* ]]; then
    kind="GW"
    digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  elif [[ "$ref" == *irin-sidecar* ]]; then
    kind="SC"
    digest="sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  else
    printf 'unknown fake ref: %s\n' "$ref" >&2
    exit 2
  fi

  exists_var="FAKE_${kind}_EXISTS"
  if [[ "$ref" != *@sha256:* && "${!exists_var:-1}" != "1" ]]; then
    printf 'manifest unknown: %s\n' "$ref" >&2
    exit 1
  fi

  format="${6:-}"
  case "$format" in
    *Manifest.Digest*) printf '%s\n' "$digest" ;;
    *org.opencontainers.image.revision*)
      rev_var="FAKE_${kind}_REV"
      printf '%s\n' "${!rev_var:-}"
      ;;
    *io.irinity.irin.sidecar.release-eligible*)
      printf '%s\n' "${FAKE_SC_ELIGIBLE:-}"
      ;;
    *) printf 'fake image %s\n' "$ref" ;;
  esac
  exit 0
fi

if [[ "${1:-}" == "buildx" ]]; then
  exit 0
fi
printf 'unsupported fake docker invocation: %s\n' "$*" >&2
exit 2
FAKE
chmod +x "$FAKE_BIN/docker"

export PATH="$FAKE_BIN:$PATH"
export FAKE_DOCKER_LOG="$FAKE_LOG"
SHA="$(git -C "$ROOT" rev-parse HEAD)"
BAD_SHA="0000000000000000000000000000000000000000"
TAG="v9.9.9"

run_manifest() {
  IRIN_PACK_IMAGES_TAG="$TAG" \
  IRIN_PACK_IMAGES_SOURCE_SHA="$SHA" \
  IRIN_PACK_MANIFEST_OUT="$TMP/manifest.json" \
  FAKE_GW_EXISTS=1 FAKE_SC_EXISTS=1 \
  FAKE_GW_REV="${FAKE_GW_REV:-$SHA}" \
  FAKE_SC_REV="${FAKE_SC_REV:-$SHA}" \
  FAKE_SC_ELIGIBLE="${FAKE_SC_ELIGIBLE-true}" \
    bash "$ROOT/scripts/generate-production-manifest.sh"
}

: >"$FAKE_LOG"
run_manifest >/dev/null
python3 - "$TMP/manifest.json" "$SHA" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
assert manifest["source_sha"] == sys.argv[2]
assert manifest["images"]["gateway"].startswith("ghcr.io/irinityhq/irin-gateway@sha256:")
assert manifest["images"]["sidecar"].startswith("ghcr.io/irinityhq/irin-sidecar@sha256:")
PY
pass "manifest accepts matching digest-bound provenance and eligible sidecar"

if FAKE_GW_REV="$BAD_SHA" run_manifest >/dev/null 2>&1; then
  fail "manifest accepted mismatched gateway revision"
fi
if FAKE_SC_REV="$BAD_SHA" run_manifest >/dev/null 2>&1; then
  fail "manifest accepted mismatched sidecar revision"
fi
if FAKE_SC_ELIGIBLE="" run_manifest >/dev/null 2>&1; then
  fail "manifest accepted ordinary/dev sidecar without eligibility annotation"
fi
pass "manifest rejects mismatched revisions and non-release sidecars"

# A hand-authored manifest cannot substitute an arbitrary digest: the shared
# verifier resolves the exact digest ref and compares registry evidence.
FORGED="$TMP/manifest-forged.json"
python3 - "$TMP/manifest.json" "$FORGED" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
data["images"]["gateway"] = "ghcr.io/irinityhq/irin-gateway@sha256:" + ("c" * 64)
with open(sys.argv[2], "w", encoding="utf-8") as handle:
    json.dump(data, handle)
PY
if FAKE_GW_EXISTS=1 FAKE_SC_EXISTS=1 FAKE_GW_REV="$SHA" FAKE_SC_REV="$SHA" FAKE_SC_ELIGIBLE=true \
  bash "$ROOT/scripts/verify-production-image-provenance.sh" \
    "$FORGED" "$SHA" "${TAG#v}" >/dev/null 2>&1; then
  fail "shared verifier accepted a forged hand-authored digest"
fi
pass "shared verifier rejects a forged hand-authored manifest"

FORGED_STAGE="$TMP/forged-stage"
if IRIN_GATEWAY_PACK_MODE=production IRIN_GATEWAY_PACK_PROD_MANIFEST="$FORGED" \
  IRIN_RELEASE_VERSION="${TAG#v}" \
  FAKE_GW_EXISTS=1 FAKE_SC_EXISTS=1 FAKE_GW_REV="$SHA" FAKE_SC_REV="$SHA" FAKE_SC_ELIGIBLE=true \
    bash "$ROOT/scripts/stage-gateway-pack.sh" "$FORGED_STAGE" >/dev/null 2>&1; then
  fail "production staging accepted a forged hand-authored manifest"
fi
[[ ! -e "$FORGED_STAGE/image-manifest.json" ]] \
  || fail "production staging copied a forged manifest before provenance verification"
pass "production staging rejects forged manifests before copying assets"

for consumer in \
  "$ROOT/scripts/generate-production-manifest.sh" \
  "$ROOT/scripts/stage-gateway-pack.sh" \
  "$ROOT/packaging/build-dmg.sh" \
  "$ROOT/packaging/verify-dmg.sh"; do
  grep -Fq 'verify-production-image-provenance.sh' "$consumer" \
    || fail "production provenance verifier is not wired into $consumer"
done
pass "generation, staging, DMG build, and untouched-copy verification share one gate"

# Partial immutable release retry: a pre-existing half must be verified before
# the missing half reaches buildx build. First exercise a foreign gateway.
: >"$FAKE_LOG"
if GITHUB_ACTIONS=1 GITHUB_TOKEN=fake-token GITHUB_ACTOR=fake-actor \
  IRIN_PACK_IMAGES_TAG="$TAG" IRIN_GATEWAY_PACK_BUILD_DIR="$TMP/out-gw" \
  FAKE_GW_EXISTS=1 FAKE_SC_EXISTS=0 FAKE_GW_REV="$BAD_SHA" \
  FAKE_SC_REV="$SHA" FAKE_SC_ELIGIBLE=true \
    bash "$ROOT/scripts/build-gateway-pack-prod-images.sh" >/dev/null 2>&1; then
  fail "partial publish accepted a foreign existing gateway"
fi
! grep -Fq 'buildx build' "$FAKE_LOG" || fail "partial publish built before verifying existing gateway"

# Then exercise an existing ordinary/dev sidecar: matching revision alone is
# insufficient without the publisher-only eligibility annotation.
: >"$FAKE_LOG"
if GITHUB_ACTIONS=1 GITHUB_TOKEN=fake-token GITHUB_ACTOR=fake-actor \
  IRIN_PACK_IMAGES_TAG="$TAG" IRIN_GATEWAY_PACK_BUILD_DIR="$TMP/out-sc" \
  FAKE_GW_EXISTS=0 FAKE_SC_EXISTS=1 FAKE_GW_REV="$SHA" \
  FAKE_SC_REV="$SHA" FAKE_SC_ELIGIBLE="" \
    bash "$ROOT/scripts/build-gateway-pack-prod-images.sh" >/dev/null 2>&1; then
  fail "partial publish accepted an ineligible existing sidecar"
fi
! grep -Fq 'buildx build' "$FAKE_LOG" || fail "partial publish built before verifying existing sidecar"
pass "partial publish fails closed before build"

grep -Fq "index:\${SIDECAR_ELIGIBILITY_ANNOTATION}=true" "$ROOT/scripts/build-gateway-pack-prod-images.sh" \
  || fail "publisher missing index eligibility annotation"
grep -Fq "manifest:\${SIDECAR_ELIGIBILITY_ANNOTATION}=true" "$ROOT/scripts/build-gateway-pack-prod-images.sh" \
  || fail "publisher missing manifest eligibility annotation"
pass "publisher emits immutable sidecar eligibility annotations"
