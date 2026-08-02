#!/usr/bin/env bash
# install-verify-candidate.sh — fresh-extract the named candidate DMG into
# candidate/install/ and write proofs/install.json when digests match.
#
# Digests only — not Arm/Watch product behavior.
#
# Usage:
#   scripts/install-verify-candidate.sh --candidate ABSOLUTE_STORE_PATH
#
# Refuses:
#   - path outside IRIN_CANDIDATE_ROOT
#   - missing DMG / candidate.json
#   - installed vs candidate bundle-manifest digest divergence
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

CANDIDATE_ARG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate)
      CANDIDATE_ARG="${2:-}"
      shift 2
      ;;
    -h|--help)
      cat <<'EOF'
Usage: install-verify-candidate.sh --candidate ABSOLUTE_STORE_PATH

Fresh-mounts the candidate DMG into candidate/install/ (never copies the
sibling stored IRIN.app). Compares candidate vs installed bundle-manifest
digests and writes proofs/install.json on match.
EOF
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$CANDIDATE_ARG" ]] || die "usage: $0 --candidate ABSOLUTE_STORE_PATH"
export IRIN_CANDIDATE_PATH="$CANDIDATE_ARG"
irin_require_candidate_path
CANDIDATE="$IRIN_CANDIDATE_PATH"

APP_NAME="IRIN.app"
INSTALL_ROOT="$CANDIDATE/install"
MOUNT="$INSTALL_ROOT/dmg-mount"
DEST_APP="$INSTALL_ROOT/$APP_NAME"
DMG="$(find "$CANDIDATE" -maxdepth 1 -type f -name '*.dmg' | head -1 || true)"
[[ -n "$DMG" && -f "$DMG" ]] || die "candidate DMG missing under $CANDIDATE"

IDENTITY="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' \
  "$CANDIDATE/candidate.json")" \
  || die "could not read candidate.json source_sha"
CANDIDATE_ID="$(basename "$CANDIDATE")"
[[ "$CANDIDATE_ID" =~ ^[0-9a-f]{64}$ ]] || die "candidate path basename is not a candidate-id: $CANDIDATE_ID"

CAND_BM="$CANDIDATE/bundle-manifest.txt"
[[ -f "$CAND_BM" ]] || die "bundle-manifest.txt missing: $CAND_BM"
CAND_BM_DIGEST="$(irin_sha256_file "$CAND_BM")"
IDENTITY_BM="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bundle_manifest_digest"])' \
  "$CANDIDATE/candidate.json")"
[[ "$CAND_BM_DIGEST" == "$IDENTITY_BM" ]] \
  || die "candidate bundle-manifest digest does not match identity"

note "fresh-extract DMG into install/ (not the stored app, not /Applications)"
if mount | grep -q "$MOUNT"; then
  hdiutil detach "$MOUNT" -force 2>/dev/null || true
fi
# Keep proofs/ untouched; wipe only install disposable root contents.
rm -rf "$DEST_APP" "$MOUNT" "$INSTALL_ROOT/bundle-manifest.txt"
mkdir -p "$INSTALL_ROOT" "$MOUNT"
hdiutil attach "$DMG" -mountpoint "$MOUNT" -readonly -nobrowse
trap 'hdiutil detach "$MOUNT" -force 2>/dev/null || true' EXIT
SRC_APP="$(find "$MOUNT" -maxdepth 2 -name "$APP_NAME" -type d | head -1 || true)"
[[ -d "$SRC_APP" ]] || die "app not found in DMG"
ditto "$SRC_APP" "$DEST_APP"
hdiutil detach "$MOUNT" -force 2>/dev/null || true
trap - EXIT
rm -rf "$MOUNT"
[[ -d "$DEST_APP" ]] || die "missing app after extract: $DEST_APP"

note "recompute installed bundle-manifest"
irin_write_bundle_manifest "$DEST_APP" "$INSTALL_ROOT/bundle-manifest.txt"
INST_BM_DIGEST="$(irin_sha256_file "$INSTALL_ROOT/bundle-manifest.txt")"

[[ "$INST_BM_DIGEST" == "$CAND_BM_DIGEST" ]] \
  || die "installed bundle-manifest digest diverges from candidate (installed=$INST_BM_DIGEST candidate=$CAND_BM_DIGEST)"

# Content-identity equality (path/kind/payload + freeze-normalized mode) is
# enforced by candidate-status; we still refuse obvious path/kind/payload diffs.
python3 - "$CAND_BM" "$INSTALL_ROOT/bundle-manifest.txt" <<'PY' || die "install vs candidate bundle-manifest content identity diverges"
import sys
cand = open(sys.argv[1], encoding="utf-8").read().splitlines()
inst = open(sys.argv[2], encoding="utf-8").read().splitlines()

def content_rows(lines):
    out = {}
    for line in lines:
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) < 4:
            raise SystemExit(f"bad manifest line: {line!r}")
        path, kind, mode, payload = parts[0], parts[1], parts[2], parts[3]
        # Freeze-norm: clear write bits for comparison.
        try:
            m = int(mode, 8) & ~0o222
            mode_n = format(m, "04o")
        except ValueError:
            mode_n = mode
        out[path] = (kind, mode_n, payload)
    return out

if content_rows(cand) != content_rows(inst):
    raise SystemExit(1)
PY

# Paths/digests via env — never interpolate into an unquoted Python string.
EXTRA="$(
  CAND_BM_DIGEST="$CAND_BM_DIGEST" \
  INST_BM_DIGEST="$INST_BM_DIGEST" \
  DEST_APP="$DEST_APP" \
  DMG="$DMG" \
  python3 - <<'PY'
import json, os
print(json.dumps({
  "candidate_bundle_manifest_digest": os.environ["CAND_BM_DIGEST"],
  "installed_bundle_manifest_digest": os.environ["INST_BM_DIGEST"],
  "installed_app_path": os.environ["DEST_APP"],
  "dmg_path": os.environ["DMG"],
}))
PY
)"

irin_write_proof_envelope \
  "$CANDIDATE/proofs/install.json" \
  "install" \
  "$CANDIDATE_ID" \
  "$IDENTITY" \
  "PASS" \
  "$EXTRA"

note "install proof written"
echo "candidate_path=$CANDIDATE"
echo "install_app=$DEST_APP"
echo "candidate_bundle_manifest_digest=$CAND_BM_DIGEST"
echo "installed_bundle_manifest_digest=$INST_BM_DIGEST"
echo "proof=$CANDIDATE/proofs/install.json"
