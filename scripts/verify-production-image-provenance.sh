#!/usr/bin/env bash
# Verify a production Gateway Pack manifest against immutable registry evidence.
# Read-only: resolves only digest-bound refs and inspects OCI annotations.
set -euo pipefail

MANIFEST="${1:-}"
INTENDED_SHA="${2:-}"
EXPECTED_PACK_VERSION="${3:-}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

[[ -n "$MANIFEST" && -f "$MANIFEST" ]] \
  || die "usage: $0 <production-manifest.json> <intended-40-char-sha> [expected-pack-version]"
[[ "$INTENDED_SHA" =~ ^[0-9a-f]{40}$ ]] \
  || die "intended release commit must be a 40-char lowercase git SHA"
command -v docker >/dev/null || die "docker CLI not found for production image provenance verification"

parsed="$(python3 - "$MANIFEST" "$INTENDED_SHA" "$EXPECTED_PACK_VERSION" <<'PY'
import json
import re
import sys

path, intended_sha, expected_pack_version = sys.argv[1:]

def require(condition, message):
    if not condition:
        raise SystemExit(message)

try:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
except (OSError, json.JSONDecodeError) as exc:
    raise SystemExit(f"invalid production manifest: {exc}")

require(data.get("schema_version") == 1, "schema_version must be 1")
require(data.get("mode") == "production", "manifest mode must be production")
require(data.get("source_sha") == intended_sha, "manifest source_sha does not match intended release commit")
require(data.get("source_dirty") is False, "production manifest source_dirty must be false")
require(data.get("platform") == "linux/arm64", "production Gateway Pack platform must be linux/arm64")
watch = data.get("watch_invariants", {})
require(watch.get("WATCH_PRODUCER_ENABLED") is False, "watch producer must ship disabled")
require(watch.get("WATCH_DISPATCHER_ENABLED") is False, "watch dispatcher must ship disabled")
if expected_pack_version:
    allowed = {expected_pack_version, f"rc-{intended_sha[:12]}"}
    require(data.get("pack_version") in allowed, "production pack_version does not identify this release or RC")

patterns = {
    "gateway": r"ghcr\.io/irinityhq/irin-gateway@sha256:[0-9a-f]{64}",
    "sidecar": r"ghcr\.io/irinityhq/irin-sidecar@sha256:[0-9a-f]{64}",
}
images = data.get("images", {})
for name, pattern in patterns.items():
    ref = images.get(name, "")
    require(re.fullmatch(pattern, ref) is not None, f"{name} is not the canonical immutable GHCR digest")
    print(ref)
PY
)" || die "production manifest structure/provenance claim is invalid: $MANIFEST"

refs=()
while IFS= read -r ref; do
  [[ -n "$ref" ]] && refs[${#refs[@]}]="$ref"
done <<<"$parsed"
[[ "${#refs[@]}" -eq 2 ]] || die "production manifest did not yield exactly two canonical image refs"
GW_REF="${refs[0]}"
SC_REF="${refs[1]}"

registry_digest() {
  local digest_ref="$1" actual expected
  expected="${digest_ref##*@}"
  actual="$(docker buildx imagetools inspect "$digest_ref" --format '{{.Manifest.Digest}}' 2>/dev/null)" \
    || die "cannot resolve immutable registry ref: $digest_ref"
  actual="$(printf '%s' "$actual" | tr -d '[:space:]')"
  [[ "$actual" == "$expected" ]] \
    || die "registry digest mismatch for $digest_ref (resolved ${actual:-<missing>})"
}

image_annotation() {
  local digest_ref="$1" key="$2" value
  value="$(docker buildx imagetools inspect "$digest_ref" --format "{{index .Manifest.Annotations \"$key\"}}" 2>/dev/null)" \
    || die "cannot inspect $key on immutable registry ref: $digest_ref"
  printf '%s' "$value" | tr -d '[:space:]'
}

require_revision() {
  local digest_ref="$1" label="$2" actual
  actual="$(image_annotation "$digest_ref" "org.opencontainers.image.revision")"
  [[ "$actual" == "$INTENDED_SHA" ]] \
    || die "$label revision mismatch: $digest_ref has ${actual:-<missing>}, expected $INTENDED_SHA"
}

registry_digest "$GW_REF"
registry_digest "$SC_REF"
require_revision "$GW_REF" "gateway"
require_revision "$SC_REF" "sidecar"
sidecar_eligible="$(image_annotation "$SC_REF" "io.irinity.irin.sidecar.release-eligible")"
[[ "$sidecar_eligible" == "true" ]] \
  || die "sidecar is not release-eligible: $SC_REF has ${sidecar_eligible:-<missing>}"

printf 'production image provenance verified: source_sha=%s\n' "$INTENDED_SHA"
printf 'gateway=%s\nsidecar=%s\n' "$GW_REF" "$SC_REF"
