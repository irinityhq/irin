#!/usr/bin/env bash
# ci-build-adhoc-candidate.sh — CI helper for W4 candidate isolation / exact-merge.
#
# Builds a local-dev (ad-hoc signed) candidate for the current HEAD SHA into
# IRIN_CANDIDATE_ROOT, verifies it, optionally runs install-verify, and exports
# a deterministic archive for GitHub artifact upload.
#
# No Apple signing material. Never uploads a local ship-check receipt.
# Prints verification PASS only — never "Candidate verified" (tier is owned by
# candidate-status after import + green CI required aggregate).
#
# Usage:
#   IRIN_CANDIDATE_ROOT=... scripts/ci-build-adhoc-candidate.sh \
#       [--install] [--export-dir DIR] [--source-sha SHA]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

DO_INSTALL=0
EXPORT_DIR=""
SOURCE_SHA=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --install) DO_INSTALL=1; shift ;;
    --export-dir) EXPORT_DIR="${2:-}"; shift 2 ;;
    --source-sha) SOURCE_SHA="${2:-}"; shift 2 ;;
    -h|--help)
      cat <<'EOF'
Usage: ci-build-adhoc-candidate.sh [--install] [--export-dir DIR] [--source-sha SHA]

Build local-dev candidate for HEAD (or --source-sha), verify, optionally
install-verify, export archive. Requires macOS arm64. Writes a CI local-dev
Gateway Pack manifest (no Docker daemon required for packaging).
EOF
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ "$(uname -m)" == "arm64" ]] || die "arm64 only"

if [[ -z "$SOURCE_SHA" ]]; then
  SOURCE_SHA="$(git rev-parse HEAD)"
fi
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "invalid source sha: $SOURCE_SHA"
# Refuse dirty tree so the embedded SHA matches the candidate identity.
if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  die "working tree is dirty; CI ad-hoc candidate requires a clean SHA"
fi
[[ "$(git rev-parse HEAD)" == "$SOURCE_SHA" ]] \
  || die "HEAD $(git rev-parse HEAD) != requested source sha $SOURCE_SHA"

# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"
irin_resolve_candidate_root

note "write CI local-dev Gateway Pack manifest (no Docker build)"
MANIFEST_DIR="$ROOT/packaging/build/gateway-pack"
mkdir -p "$MANIFEST_DIR"
MANIFEST="$MANIFEST_DIR/image-manifest.local.json"
# Deterministic digests derived from source SHA so re-runs are stable.
GW_DIG="$(printf 'ci-gateway:%s' "$SOURCE_SHA" | irin_sha256_bytes)"
SC_DIG="$(printf 'ci-sidecar:%s' "$SOURCE_SHA" | irin_sha256_bytes)"
python3 - "$MANIFEST" "$SOURCE_SHA" "$GW_DIG" "$SC_DIG" <<'PY'
import json, sys
path, source_sha, gw, sc = sys.argv[1:]
doc = {
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "ci-adhoc",
  "source_sha": source_sha,
  "notes": "CI ad-hoc candidate inputs; digests are identity-bound, not registry-published images.",
  "images": {
    "gateway": f"irin-desktop/gateway:ci@sha256:{gw}",
    "sidecar": f"irin-desktop/sidecar:ci@sha256:{sc}",
  },
  "watch_invariants": {
    "WATCH_PRODUCER_ENABLED": False,
    "WATCH_DISPATCHER_ENABLED": False,
  },
}
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY

note "build local-dev candidate (ad-hoc sign; no Apple material)"
export IRIN_DMG_PACK_MODE=local-dev
export IRIN_DMG_REQUIRE_CLEAN=1
export IRIN_GATEWAY_PACK_LOCAL_MANIFEST="$MANIFEST"
export IRIN_GATEWAY_PACK_MODE=local-dev
# Capture candidate_path= from build-dmg.
BUILD_LOG="$IRIN_CANDIDATE_ROOT/.ci-build-${SOURCE_SHA}.log"
set +e
bash "$ROOT/packaging/build-dmg.sh" 2>&1 | tee "$BUILD_LOG"
build_ec=${PIPESTATUS[0]}
set -e
[[ $build_ec -eq 0 ]] || die "build-dmg failed (exit $build_ec)"

CANDIDATE_PATH="$(sed -n 's/^candidate_path=//p' "$BUILD_LOG" | tail -n 1)"
if [[ -z "$CANDIDATE_PATH" ]]; then
  # Fallback: locate by source sha under the store.
  CANDIDATE_PATH="$(find "$IRIN_CANDIDATE_ROOT" -mindepth 3 -maxdepth 3 -type d \
    -path "*/${SOURCE_SHA}/*" ! -path '*/failed/*' 2>/dev/null | head -1 || true)"
fi
[[ -n "$CANDIDATE_PATH" && -d "$CANDIDATE_PATH" ]] \
  || die "could not resolve candidate path after build"
export IRIN_CANDIDATE_PATH="$CANDIDATE_PATH"

note "verify candidate (writes proofs/verify.json on PASS)"
bash "$ROOT/packaging/verify-dmg.sh"
[[ -f "$CANDIDATE_PATH/proofs/verify.json" ]] \
  || die "verify.json missing after verify-dmg"

if [[ "$DO_INSTALL" == "1" ]]; then
  note "install-verify (fresh DMG extract + proofs/install.json)"
  bash "$ROOT/scripts/install-verify-candidate.sh" --candidate "$CANDIDATE_PATH"
  [[ -f "$CANDIDATE_PATH/proofs/install.json" ]] \
    || die "install.json missing after install-verify"
fi

if [[ -z "$EXPORT_DIR" ]]; then
  EXPORT_DIR="$IRIN_CANDIDATE_ROOT/.exports/${SOURCE_SHA}"
fi
note "export deterministic candidate archive → $EXPORT_DIR"
EXPORT_OUT="$(bash "$ROOT/scripts/export-candidate.sh" \
  --candidate "$CANDIDATE_PATH" --output "$EXPORT_DIR")"
printf '%s\n' "$EXPORT_OUT"

CANDIDATE_ID="$(basename "$CANDIDATE_PATH")"
ARCHIVE_PATH="$(sed -n 's/^archive_path=//p' <<<"$EXPORT_OUT")"
ARCHIVE_SHA="$(sed -n 's/^archive_sha256=//p' <<<"$EXPORT_OUT")"
PAYLOAD_HASH="$(sed -n 's/^payload_tree_hash=//p' <<<"$EXPORT_OUT")"

# Job-facing summary keys (never print Candidate verified).
printf 'verification=PASS\n'
printf 'candidate_path=%s\n' "$CANDIDATE_PATH"
printf 'candidate_id=%s\n' "$CANDIDATE_ID"
printf 'source_sha=%s\n' "$SOURCE_SHA"
printf 'archive_path=%s\n' "$ARCHIVE_PATH"
printf 'archive_sha256=%s\n' "$ARCHIVE_SHA"
printf 'payload_tree_hash=%s\n' "$PAYLOAD_HASH"
printf 'shipping_tier_claim=none\n'
note "CI ad-hoc candidate complete (verification PASS only; not a shipping tier)"
