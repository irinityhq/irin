#!/usr/bin/env bash
# export-candidate.sh — emit a deterministic candidate archive + SHA-256 sidecar
# + trusted export manifest.
#
# Usage:
#   scripts/export-candidate.sh --candidate ABSOLUTE_STORE_PATH [--output DIR]
#
# Archive contains:
#   - immutable payload (candidate.json, HASHES.txt, bundle-manifest.txt, DMG, IRIN.app)
#   - proofs/
#   - install/ witnesses when present (IRIN.app + bundle-manifest.txt; not dmg-mount)
#   - export-binding.json (trusted identity + payload_tree_hash bindings)
#
# Excludes disposable smoke/, logs/, verify/, install/dmg-mount.
#
# Outputs (printed as key=value):
#   archive_path=...
#   archive_sha256=...
#   export_manifest=...
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

Emit a deterministic tar.gz of the candidate immutable payload + proofs/ +
install witnesses (when present), a SHA-256 sidecar, and a trusted export
manifest that import-candidate.sh requires.
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

# Refuse to export a payload that already disagrees with identity.
irin_assert_candidate_payload_matches_identity "$CANDIDATE" >/dev/null \
  || die "candidate payload does not match identity; refusing export"

CANDIDATE_ID="$(basename "$CANDIDATE")"
[[ "$CANDIDATE_ID" =~ ^[0-9a-f]{64}$ ]] \
  || die "candidate path basename is not a candidate-id: $CANDIDATE"

SOURCE_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' \
  "$CANDIDATE/candidate.json")" \
  || die "could not read source_sha from candidate.json"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "invalid source_sha in candidate.json"

DMG_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["dmg_sha256"])' \
  "$CANDIDATE/candidate.json")"
BM_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bundle_manifest_digest"])' \
  "$CANDIDATE/candidate.json")"

PAYLOAD_HASH="$(irin_payload_tree_hash "$CANDIDATE")" \
  || die "could not compute payload tree hash"

# If install proof exists, install witnesses must exist (W2 Installed path).
if [[ -f "$CANDIDATE/proofs/install.json" ]]; then
  [[ -d "$CANDIDATE/install/IRIN.app" ]] \
    || die "proofs/install.json present but install/IRIN.app missing"
  [[ -f "$CANDIDATE/install/bundle-manifest.txt" ]] \
    || die "proofs/install.json present but install/bundle-manifest.txt missing"
fi

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="$(pwd)/candidate-export"
fi
[[ "$OUTPUT_DIR" == /* ]] || OUTPUT_DIR="$(cd "$(dirname "$OUTPUT_DIR")" && pwd)/$(basename "$OUTPUT_DIR")"
mkdir -p "$OUTPUT_DIR" || die "could not create output dir: $OUTPUT_DIR"

ARCHIVE="$OUTPUT_DIR/irin-candidate-${CANDIDATE_ID}.tar.gz"
SIDECAR="${ARCHIVE}.sha256"
MANIFEST="$OUTPUT_DIR/irin-candidate-${CANDIDATE_ID}.export.json"
BINDING="$OUTPUT_DIR/.export-binding-${CANDIDATE_ID}.json"

# Trusted binding written into the archive and mirrored in the sidecar manifest.
python3 - "$BINDING" "$CANDIDATE_ID" "$SOURCE_SHA" "$PAYLOAD_HASH" "$DMG_SHA" "$BM_SHA" <<'PY'
import json, sys
out, cid, sha, payload, dmg, bm = sys.argv[1:]
doc = {
  "schema_version": 1,
  "kind": "irin-candidate-export-binding",
  "candidate_id": cid,
  "source_sha": sha,
  "payload_tree_hash": payload,
  "dmg_sha256": dmg,
  "bundle_manifest_digest": bm,
}
with open(out, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, separators=(",", ":"))
    fh.write("\n")
PY

# Deterministic gzip tar via Python (works on macOS bsdtar and Linux GNU tar hosts).
python3 - "$CANDIDATE" "$ARCHIVE" "$BINDING" <<'PY'
import gzip
import os
import stat
import sys
import tarfile

src = os.path.abspath(sys.argv[1])
out = sys.argv[2]
binding_src = sys.argv[3]

# Immutable payload + proofs + install witnesses. Exclude disposable trees.
INCLUDE_TOP = {
    "candidate.json",
    "HASHES.txt",
    "bundle-manifest.txt",
    "IRIN.app",
    "proofs",
    "install",
}
# Directory basenames never walked into / never archived as content trees.
EXCLUDE_DIR_NAMES = {
    "smoke",
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
    if top == "export-binding.json" and len(parts) == 1:
        return True
    if top not in INCLUDE_TOP:
        return False
    for p in parts:
        if p in EXCLUDE_DIR_NAMES:
            return False
        if p == ".DS_Store" or p.startswith("._"):
            return False
    # install/: only IRIN.app tree + bundle-manifest.txt (not mount scratch).
    if top == "install":
        if len(parts) == 1:
            return True
        if parts[1] == "bundle-manifest.txt" and len(parts) == 2:
            return True
        if parts[1] == "IRIN.app":
            return True
        return False
    return True

entries = []  # list of (kind, rel, full) kind in {dir, file, symlink}

def add_entry(full: str, rel: str) -> None:
    rel = rel.replace(os.sep, "/")
    if not want(rel):
        return
    if os.path.islink(full):
        # Directory symlinks (framework-style) must round-trip as symlinks.
        entries.append(("symlink", rel, full))
    elif os.path.isdir(full):
        entries.append(("dir", rel, full))
    elif os.path.isfile(full):
        entries.append(("file", rel, full))
    else:
        raise SystemExit(f"unsupported path type in candidate: {rel}")

for dirpath, dirnames, filenames in os.walk(src, followlinks=False):
    rel_dir = os.path.relpath(dirpath, src)
    if rel_dir == ".":
        rel_dir = ""

    # Classify children before pruning: symlinks-to-dirs must be recorded, not walked.
    child_dirs = sorted(dirnames)
    dirnames[:] = []
    for name in child_dirs:
        if name in EXCLUDE_DIR_NAMES or name.startswith("._"):
            continue
        full = os.path.join(dirpath, name)
        rel = name if not rel_dir else f"{rel_dir.replace(os.sep, '/')}/{name}"
        if os.path.islink(full):
            add_entry(full, rel)
            # do not walk through the symlink
            continue
        if os.path.isdir(full):
            if want(rel):
                dirnames.append(name)
                add_entry(full, rel)
            # else pruned

    filenames = sorted(f for f in filenames if f != ".DS_Store" and not f.startswith("._"))
    for name in filenames:
        full = os.path.join(dirpath, name)
        rel = name if not rel_dir else f"{rel_dir.replace(os.sep, '/')}/{name}"
        add_entry(full, rel)

# Inject trusted export-binding.json at archive root (os.walk already
# includes the top-level DMG via filenames; do not add it a second time).
entries.append(("file", "export-binding.json", binding_src))

# Stable order by relative path; refuse accidental duplicate members.
entries.sort(key=lambda e: e[1])
seen_rel = set()
for _, rel, _ in entries:
    if rel in seen_rel:
        raise SystemExit(f"duplicate archive member: {rel}")
    seen_rel.add(rel)

FIXED_MTIME = 0
# Stream tar -> gzip -> disk (no full-archive BytesIO; production DMGs are large).
with open(out, "wb") as fh:
    with gzip.GzipFile(filename="", mode="wb", fileobj=fh, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|") as tar:
            for kind, rel, full in entries:
                info = tarfile.TarInfo(name=rel)
                info.mtime = FIXED_MTIME
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                if kind == "symlink":
                    info.type = tarfile.SYMTYPE
                    info.linkname = os.readlink(full)
                    info.mode = 0o777
                    info.size = 0
                    tar.addfile(info)
                elif kind == "dir":
                    info.type = tarfile.DIRTYPE
                    info.mode = 0o755
                    info.size = 0
                    tar.addfile(info)
                elif kind == "file":
                    st = os.lstat(full)
                    info.type = tarfile.REGTYPE
                    mode = 0o755 if (st.st_mode & stat.S_IXUSR) else 0o644
                    info.mode = mode
                    info.size = st.st_size
                    with open(full, "rb") as payload:
                        tar.addfile(info, payload)
                else:
                    raise SystemExit(f"unknown entry kind: {kind}")
PY

ARCHIVE_SHA="$(irin_sha256_file "$ARCHIVE")"
printf '%s  %s\n' "$ARCHIVE_SHA" "$(basename "$ARCHIVE")" >"$SIDECAR"

python3 - "$MANIFEST" "$CANDIDATE_ID" "$SOURCE_SHA" "$PAYLOAD_HASH" "$ARCHIVE_SHA" \
  "$(basename "$ARCHIVE")" "$DMG_SHA" "$BM_SHA" <<'PY'
import json, sys
out, cid, sha, payload, archive_sha, archive_name, dmg, bm = sys.argv[1:]
doc = {
  "schema_version": 1,
  "kind": "irin-candidate-export",
  "candidate_id": cid,
  "source_sha": sha,
  "payload_tree_hash": payload,
  "dmg_sha256": dmg,
  "bundle_manifest_digest": bm,
  "archive_name": archive_name,
  "archive_sha256": archive_sha,
}
with open(out, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY

rm -f "$BINDING"

printf 'archive_path=%s\n' "$ARCHIVE"
printf 'archive_sha256=%s\n' "$ARCHIVE_SHA"
printf 'sidecar_path=%s\n' "$SIDECAR"
printf 'export_manifest=%s\n' "$MANIFEST"
printf 'candidate_id=%s\n' "$CANDIDATE_ID"
printf 'source_sha=%s\n' "$SOURCE_SHA"
printf 'payload_tree_hash=%s\n' "$PAYLOAD_HASH"
