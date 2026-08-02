#!/usr/bin/env bash
# export-candidate.sh — emit a deterministic candidate archive + SHA-256 sidecar.
#
# Usage:
#   scripts/export-candidate.sh --candidate ABSOLUTE_STORE_PATH [--output DIR]
#
# Archive contains the immutable payload (candidate.json, HASHES.txt,
# bundle-manifest.txt, DMG, IRIN.app) plus proofs/. Disposable smoke/, install/,
# logs/, and verify/ trees are excluded so the archive is stable and lean.
#
# Outputs (printed as key=value):
#   archive_path=...
#   archive_sha256=...
#   candidate_id=...
#   source_sha=...
#   payload_tree_hash=...
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

CANDIDATE_ARG=""
OUTPUT_DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate)
      CANDIDATE_ARG="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT_DIR="${2:-}"
      shift 2
      ;;
    -h|--help)
      cat <<'EOF'
Usage: export-candidate.sh --candidate ABSOLUTE_STORE_PATH [--output DIR]

Emit a deterministic tar.gz of the candidate immutable payload + proofs/,
plus a SHA-256 sidecar. Archive is suitable for GitHub Actions artifact upload
and later import-candidate.sh.
EOF
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$CANDIDATE_ARG" ]] || die "usage: $0 --candidate ABSOLUTE_STORE_PATH [--output DIR]"
export IRIN_CANDIDATE_PATH="$CANDIDATE_ARG"
irin_require_candidate_path
CANDIDATE="$IRIN_CANDIDATE_PATH"

CANDIDATE_ID="$(basename "$CANDIDATE")"
[[ "$CANDIDATE_ID" =~ ^[0-9a-f]{64}$ ]] \
  || die "candidate path basename is not a candidate-id: $CANDIDATE"

SOURCE_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' \
  "$CANDIDATE/candidate.json")" \
  || die "could not read source_sha from candidate.json"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "invalid source_sha in candidate.json"

PAYLOAD_HASH="$(irin_payload_tree_hash "$CANDIDATE")" \
  || die "could not compute payload tree hash"

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="$(pwd)/candidate-export"
fi
[[ "$OUTPUT_DIR" == /* ]] || OUTPUT_DIR="$(cd "$(dirname "$OUTPUT_DIR")" && pwd)/$(basename "$OUTPUT_DIR")"
mkdir -p "$OUTPUT_DIR" || die "could not create output dir: $OUTPUT_DIR"

ARCHIVE="$OUTPUT_DIR/irin-candidate-${CANDIDATE_ID}.tar.gz"
SIDECAR="${ARCHIVE}.sha256"
MANIFEST="$OUTPUT_DIR/irin-candidate-${CANDIDATE_ID}.export.json"

# Deterministic gzip tar via Python (works on macOS bsdtar and Linux GNU tar hosts).
python3 - "$CANDIDATE" "$ARCHIVE" <<'PY'
import gzip
import hashlib
import io
import os
import stat
import sys
import tarfile

src = os.path.abspath(sys.argv[1])
out = sys.argv[2]

# Immutable payload + proofs only. Exclude disposable/diagnostic trees.
INCLUDE_TOP = {
    "candidate.json",
    "HASHES.txt",
    "bundle-manifest.txt",
    "IRIN.app",
    "proofs",
}
EXCLUDE_DIR_NAMES = {
    "smoke",
    "install",
    "logs",
    "verify",
    "dmg-mount",
    ".DS_Store",
}

def want(rel: str) -> bool:
    if not rel or rel == ".":
        return False
    parts = rel.split("/")
    top = parts[0]
    if top.endswith(".dmg") and len(parts) == 1:
        return True
    if top not in INCLUDE_TOP:
        return False
    for p in parts:
        if p in EXCLUDE_DIR_NAMES:
            return False
        if p.startswith(".") and p not in (".", ".."):
            # keep hidden files under proofs if any; skip apple double
            if p == ".DS_Store" or p.startswith("._"):
                return False
    return True

entries = []
for dirpath, dirnames, filenames in os.walk(src, followlinks=False):
    rel_dir = os.path.relpath(dirpath, src)
    if rel_dir == ".":
        rel_dir = ""
    # Prune excluded directories in-place.
    dirnames[:] = sorted(
        d for d in dirnames
        if d not in EXCLUDE_DIR_NAMES and not d.startswith("._")
    )
    filenames = sorted(f for f in filenames if f != ".DS_Store" and not f.startswith("._"))

    if rel_dir:
        if not want(rel_dir):
            dirnames[:] = []
            continue
        entries.append(("dir", rel_dir.replace(os.sep, "/"), dirpath))

    for name in filenames:
        full = os.path.join(dirpath, name)
        rel = name if not rel_dir else f"{rel_dir.replace(os.sep, '/')}/{name}"
        if want(rel):
            entries.append(("file", rel, full))

# Also catch top-level DMG if walk order missed via filter
for name in sorted(os.listdir(src)):
    full = os.path.join(src, name)
    if name.endswith(".dmg") and os.path.isfile(full):
        if ("file", name, full) not in entries:
            entries.append(("file", name, full))

# Stable order by relative path.
entries.sort(key=lambda e: e[1])

# Fixed mtime for determinism.
FIXED_MTIME = 0

buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w") as tar:
    for kind, rel, full in entries:
        info = tarfile.TarInfo(name=rel)
        info.mtime = FIXED_MTIME
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        if kind == "dir" or os.path.isdir(full) and not os.path.islink(full):
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.size = 0
            tar.addfile(info)
        elif os.path.islink(full):
            info.type = tarfile.SYMTYPE
            info.linkname = os.readlink(full)
            info.mode = 0o777
            info.size = 0
            tar.addfile(info)
        elif os.path.isfile(full):
            st = os.lstat(full)
            info.type = tarfile.REGTYPE
            # Preserve executable bit only; ignore owner write (frozen payload).
            mode = 0o755 if (st.st_mode & stat.S_IXUSR) else 0o644
            info.mode = mode
            info.size = st.st_size
            with open(full, "rb") as fh:
                tar.addfile(info, fh)
        else:
            raise SystemExit(f"unsupported path type in candidate: {rel}")

raw = buf.getvalue()
# Deterministic gzip: mtime=0, no filename.
with open(out, "wb") as fh:
    with gzip.GzipFile(filename="", mode="wb", fileobj=fh, mtime=0) as gz:
        gz.write(raw)
PY

ARCHIVE_SHA="$(irin_sha256_file "$ARCHIVE")"
printf '%s  %s\n' "$ARCHIVE_SHA" "$(basename "$ARCHIVE")" >"$SIDECAR"

python3 - "$MANIFEST" "$CANDIDATE_ID" "$SOURCE_SHA" "$PAYLOAD_HASH" "$ARCHIVE_SHA" \
  "$(basename "$ARCHIVE")" <<'PY'
import json, sys
out, cid, sha, payload, archive_sha, archive_name = sys.argv[1:]
doc = {
  "schema_version": 1,
  "kind": "irin-candidate-export",
  "candidate_id": cid,
  "source_sha": sha,
  "payload_tree_hash": payload,
  "archive_name": archive_name,
  "archive_sha256": archive_sha,
}
with open(out, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY

printf 'archive_path=%s\n' "$ARCHIVE"
printf 'archive_sha256=%s\n' "$ARCHIVE_SHA"
printf 'sidecar_path=%s\n' "$SIDECAR"
printf 'export_manifest=%s\n' "$MANIFEST"
printf 'candidate_id=%s\n' "$CANDIDATE_ID"
printf 'source_sha=%s\n' "$SOURCE_SHA"
printf 'payload_tree_hash=%s\n' "$PAYLOAD_HASH"
