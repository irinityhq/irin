#!/usr/bin/env bash
# import-candidate.sh — verify and atomically import an exported candidate archive.
#
# Usage:
#   scripts/import-candidate.sh --archive PATH
#       [--export-manifest PATH]
#       [--expected-source-sha SHA]
#       [--expected-candidate-id ID]
#       [--expected-archive-sha256 HEX]
#
# Verifies before any store promote:
#   - archive SHA-256 (sidecar, --expected-archive-sha256, or export manifest)
#   - required trusted export manifest (sibling irin-candidate-*.export.json by
#     default, or --export-manifest) binding candidate_id, source_sha,
#     payload_tree_hash, dmg_sha256, bundle_manifest_digest
#   - embedded export-binding.json agrees with the sidecar manifest
#   - candidate.json canonical form and recomputed candidate-id
#   - on-disk DMG/app/HASHES/bundle-manifest match identity (payload assert)
#   - recomputed payload_tree_hash matches the trusted export binding
# then promotes into IRIN_CANDIDATE_ROOT via the same atomic rename path as builds.
#
# Candidate verified is NOT printed here. After import, candidate-status combines
# stored proof with the green exact-SHA CI aggregate.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '=== %s ===\n' "$*"; }

ARCHIVE=""
EXPORT_MANIFEST=""
EXPECTED_SOURCE_SHA=""
EXPECTED_CANDIDATE_ID=""
EXPECTED_ARCHIVE_SHA=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --archive)
      ARCHIVE="${2:-}"
      shift 2
      ;;
    --export-manifest)
      EXPORT_MANIFEST="${2:-}"
      shift 2
      ;;
    --expected-source-sha)
      EXPECTED_SOURCE_SHA="${2:-}"
      shift 2
      ;;
    --expected-candidate-id)
      EXPECTED_CANDIDATE_ID="${2:-}"
      shift 2
      ;;
    --expected-archive-sha256)
      EXPECTED_ARCHIVE_SHA="${2:-}"
      shift 2
      ;;
    -h|--help)
      cat <<'EOF'
Usage: import-candidate.sh --archive PATH
    [--export-manifest PATH]
    [--expected-source-sha SHA]
    [--expected-candidate-id ID]
    [--expected-archive-sha256 HEX]

Verify a deterministic export archive against its trusted export manifest and
on-disk identity bindings, then atomically move it into IRIN_CANDIDATE_ROOT.
Refuses tampered archives, payload/identity mismatches, and missing manifests.
EOF
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$ARCHIVE" ]] || die "usage: $0 --archive PATH"
[[ -f "$ARCHIVE" ]] || die "archive not found: $ARCHIVE"
[[ "$ARCHIVE" == /* ]] || ARCHIVE="$(cd "$(dirname "$ARCHIVE")" && pwd)/$(basename "$ARCHIVE")"

# Default trusted export manifest: sibling of the archive.
if [[ -z "$EXPORT_MANIFEST" ]]; then
  if [[ "$ARCHIVE" == *.tar.gz ]]; then
    EXPORT_MANIFEST="${ARCHIVE%.tar.gz}.export.json"
  else
    EXPORT_MANIFEST="${ARCHIVE}.export.json"
  fi
fi
[[ -f "$EXPORT_MANIFEST" ]] \
  || die "trusted export manifest required (missing: $EXPORT_MANIFEST); pass --export-manifest"
[[ "$EXPORT_MANIFEST" == /* ]] \
  || EXPORT_MANIFEST="$(cd "$(dirname "$EXPORT_MANIFEST")" && pwd)/$(basename "$EXPORT_MANIFEST")"

irin_resolve_candidate_root

# --- load trusted export manifest --------------------------------------------
MANIFEST_JSON="$(python3 - "$EXPORT_MANIFEST" <<'PY'
import json, sys
path = sys.argv[1]
with open(path, encoding="utf-8") as fh:
    doc = json.load(fh)
if not isinstance(doc, dict):
    raise SystemExit("export manifest must be a JSON object")
if doc.get("kind") not in ("irin-candidate-export", "irin-candidate-export-binding"):
    raise SystemExit(f"export manifest kind not recognized: {doc.get('kind')!r}")
for key in (
    "candidate_id",
    "source_sha",
    "payload_tree_hash",
    "dmg_sha256",
    "bundle_manifest_digest",
):
    if not doc.get(key):
        raise SystemExit(f"export manifest missing required field: {key}")
# Print stable key=value for the shell.
for key in (
    "candidate_id",
    "source_sha",
    "payload_tree_hash",
    "dmg_sha256",
    "bundle_manifest_digest",
    "archive_sha256",
    "archive_name",
):
    val = doc.get(key) or ""
    print(f"{key}={val}")
PY
)" || die "export manifest invalid: $EXPORT_MANIFEST"

manifest_get() { sed -n "s/^${1}=//p" <<<"$MANIFEST_JSON" | head -n 1; }
MANIFEST_CID="$(manifest_get candidate_id)"
MANIFEST_SOURCE="$(manifest_get source_sha)"
MANIFEST_PAYLOAD="$(manifest_get payload_tree_hash)"
MANIFEST_DMG="$(manifest_get dmg_sha256)"
MANIFEST_BM="$(manifest_get bundle_manifest_digest)"
MANIFEST_ARCHIVE_SHA="$(manifest_get archive_sha256)"

[[ "$MANIFEST_CID" =~ ^[0-9a-f]{64}$ ]] || die "export manifest candidate_id invalid"
[[ "$MANIFEST_SOURCE" =~ ^[0-9a-f]{40}$ ]] || die "export manifest source_sha invalid"
[[ "$MANIFEST_PAYLOAD" =~ ^[0-9a-f]{64}$ ]] || die "export manifest payload_tree_hash invalid"
[[ "$MANIFEST_DMG" =~ ^[0-9a-f]{64}$ ]] || die "export manifest dmg_sha256 invalid"
[[ "$MANIFEST_BM" =~ ^[0-9a-f]{64}$ ]] || die "export manifest bundle_manifest_digest invalid"

# --- archive integrity -------------------------------------------------------
ACTUAL_ARCHIVE_SHA="$(irin_sha256_file "$ARCHIVE")"
SIDECAR="${ARCHIVE}.sha256"
if [[ -z "$EXPECTED_ARCHIVE_SHA" && -f "$SIDECAR" ]]; then
  EXPECTED_ARCHIVE_SHA="$(awk '{print $1; exit}' "$SIDECAR")"
fi
# Prefer explicit expected, then sidecar, then manifest binding.
if [[ -z "$EXPECTED_ARCHIVE_SHA" && -n "$MANIFEST_ARCHIVE_SHA" ]]; then
  EXPECTED_ARCHIVE_SHA="$MANIFEST_ARCHIVE_SHA"
fi
if [[ -n "$EXPECTED_ARCHIVE_SHA" ]]; then
  [[ "$EXPECTED_ARCHIVE_SHA" =~ ^[0-9a-f]{64}$ ]] \
    || die "invalid expected archive sha256: $EXPECTED_ARCHIVE_SHA"
  [[ "$ACTUAL_ARCHIVE_SHA" == "$EXPECTED_ARCHIVE_SHA" ]] \
    || die "archive SHA-256 mismatch (got $ACTUAL_ARCHIVE_SHA, expected $EXPECTED_ARCHIVE_SHA)"
  note "archive SHA-256 matches trusted binding"
else
  die "no trusted archive SHA-256 (provide sidecar, --expected-archive-sha256, or manifest archive_sha256)"
fi
if [[ -n "$MANIFEST_ARCHIVE_SHA" && "$MANIFEST_ARCHIVE_SHA" != "$ACTUAL_ARCHIVE_SHA" ]]; then
  die "export manifest archive_sha256 mismatch (got $ACTUAL_ARCHIVE_SHA, manifest $MANIFEST_ARCHIVE_SHA)"
fi

# --- extract to exclusive staging under the durable store --------------------
IMPORT_STAGING_ROOT="$IRIN_CANDIDATE_ROOT/.import-staging"
IMPORT_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
STAGING="$IMPORT_STAGING_ROOT/$IMPORT_ID"
mkdir -p "$IMPORT_STAGING_ROOT"
mkdir "$STAGING" || die "could not create exclusive import staging: $STAGING"

cleanup() {
  status=$?
  if [[ -n "${STAGING:-}" && -d "$STAGING" ]]; then
    chmod -R u+w "$STAGING" 2>/dev/null || true
    rm -rf "$STAGING"
  fi
  exit "$status"
}
trap cleanup EXIT

note "extract archive into import staging"
python3 - "$ARCHIVE" "$STAGING" <<'PY'
import gzip
import io
import os
import sys
import tarfile

archive, dest = sys.argv[1], sys.argv[2]
with gzip.open(archive, "rb") as gz:
    data = gz.read()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tar:
    for member in tar.getmembers():
        name = member.name
        if name.startswith("/") or name.startswith("\\") or ".." in name.split("/"):
            raise SystemExit(f"refusing unsafe archive member path: {name!r}")
        if member.issym() or member.islnk():
            link = member.linkname or ""
            if link.startswith("/") or ".." in link.split("/"):
                raise SystemExit(f"refusing unsafe link target: {name!r} -> {link!r}")
        try:
            tar.extract(member, path=dest, filter="data")
        except TypeError:
            tar.extract(member, path=dest)
PY

# --- structural presence -----------------------------------------------------
[[ -f "$STAGING/candidate.json" ]] || die "archive missing candidate.json"
[[ -f "$STAGING/HASHES.txt" ]] || die "archive missing HASHES.txt"
[[ -f "$STAGING/bundle-manifest.txt" ]] || die "archive missing bundle-manifest.txt"
[[ -d "$STAGING/IRIN.app" || -L "$STAGING/IRIN.app" ]] || die "archive missing IRIN.app"
[[ -f "$STAGING/export-binding.json" ]] || die "archive missing export-binding.json (trusted binding)"
dmg_count="$(find "$STAGING" -maxdepth 1 -type f -name '*.dmg' | wc -l | tr -d ' ')"
[[ "$dmg_count" == "1" ]] || die "archive must contain exactly one DMG (found $dmg_count)"

# --- embedded binding must agree with sidecar manifest -----------------------
python3 - "$STAGING/export-binding.json" "$MANIFEST_CID" "$MANIFEST_SOURCE" \
  "$MANIFEST_PAYLOAD" "$MANIFEST_DMG" "$MANIFEST_BM" <<'PY' \
  || die "embedded export-binding.json disagrees with trusted export manifest"
import json, sys
path, cid, sha, payload, dmg, bm = sys.argv[1:]
with open(path, encoding="utf-8") as fh:
    doc = json.load(fh)
if doc.get("kind") != "irin-candidate-export-binding":
    raise SystemExit(f"bad embedded binding kind: {doc.get('kind')!r}")
for key, expected in (
    ("candidate_id", cid),
    ("source_sha", sha),
    ("payload_tree_hash", payload),
    ("dmg_sha256", dmg),
    ("bundle_manifest_digest", bm),
):
    got = doc.get(key)
    if got != expected:
        raise SystemExit(f"embedded binding {key} mismatch ({got!r} vs {expected!r})")
print("embedded binding matches export manifest")
PY

# --- canonical identity + recomputed candidate-id ----------------------------
RECOMPUTED_ID="$(python3 - "$STAGING/candidate.json" <<'PY'
import hashlib, json, sys
path = sys.argv[1]
raw = open(path, "rb").read()
data = json.loads(raw.decode("utf-8"))
if not isinstance(data, dict):
    raise SystemExit("candidate.json must be an object")
canon = json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
if raw.decode("utf-8") != canon:
    raise SystemExit("candidate.json is not in canonical identity form")
print(hashlib.sha256(canon.encode("utf-8")).hexdigest())
PY
)" || die "candidate.json failed canonical identity check"

SOURCE_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["source_sha"])' \
  "$STAGING/candidate.json")"
SEMVER="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["semver"])' \
  "$STAGING/candidate.json")"
ID_DMG="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["dmg_sha256"])' \
  "$STAGING/candidate.json")"
ID_BM="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bundle_manifest_digest"])' \
  "$STAGING/candidate.json")"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "invalid source_sha in imported candidate.json"
[[ -n "$SEMVER" ]] || die "semver missing from candidate.json"

[[ "$RECOMPUTED_ID" == "$MANIFEST_CID" ]] \
  || die "candidate-id mismatch vs export manifest (got $RECOMPUTED_ID, manifest $MANIFEST_CID)"
[[ "$SOURCE_SHA" == "$MANIFEST_SOURCE" ]] \
  || die "source_sha mismatch vs export manifest (got $SOURCE_SHA, manifest $MANIFEST_SOURCE)"
[[ "$ID_DMG" == "$MANIFEST_DMG" ]] \
  || die "identity dmg_sha256 mismatch vs export manifest"
[[ "$ID_BM" == "$MANIFEST_BM" ]] \
  || die "identity bundle_manifest_digest mismatch vs export manifest"

if [[ -n "$EXPECTED_CANDIDATE_ID" ]]; then
  [[ "$EXPECTED_CANDIDATE_ID" == "$RECOMPUTED_ID" ]] \
    || die "candidate-id mismatch (got $RECOMPUTED_ID, expected $EXPECTED_CANDIDATE_ID)"
fi
if [[ -n "$EXPECTED_SOURCE_SHA" ]]; then
  [[ "$EXPECTED_SOURCE_SHA" == "$SOURCE_SHA" ]] \
    || die "source_sha mismatch (got $SOURCE_SHA, expected $EXPECTED_SOURCE_SHA)"
fi

# --- on-disk payload must match identity (catches DMG/app mutation) ----------
note "assert payload bytes match candidate.json identity"
irin_assert_candidate_payload_matches_identity "$STAGING" \
  || die "imported payload does not match candidate.json identity"

# --- payload tree hash must match trusted export binding ---------------------
PAYLOAD_HASH="$(irin_payload_tree_hash "$STAGING")" \
  || die "imported payload tree hash failed (corrupt or incomplete archive)"
[[ "$PAYLOAD_HASH" == "$MANIFEST_PAYLOAD" ]] \
  || die "payload_tree_hash mismatch vs export manifest (got $PAYLOAD_HASH, expected $MANIFEST_PAYLOAD)"
note "payload_tree_hash matches trusted export binding"

# Binding file is not part of the store payload; drop before promote.
rm -f "$STAGING/export-binding.json"

# Ensure optional dirs exist for later tier evidence.
mkdir -p "$STAGING/proofs" "$STAGING/smoke" "$STAGING/install" "$STAGING/logs"

# Safe path components + physical containment under IRIN_CANDIDATE_ROOT.
DEST="$(irin_assert_safe_candidate_dest \
  "$IRIN_CANDIDATE_ROOT" "$SEMVER" "$SOURCE_SHA" "$RECOMPUTED_ID")" \
  || die "refusing unsafe candidate destination path"
note "promote import staging → $DEST"
PROMOTE_RESULT="$(irin_promote_candidate_from_staging "$STAGING" "$DEST")" \
  || die "promote failed"
if [[ ! -d "$STAGING" ]]; then
  STAGING=""
fi
trap - EXIT
if [[ -n "${STAGING:-}" && -d "$STAGING" ]]; then
  chmod -R u+w "$STAGING" 2>/dev/null || true
  rm -rf "$STAGING"
fi

# Post-promote re-check (store path must still match identity).
irin_assert_candidate_payload_matches_identity "$DEST" >/dev/null \
  || die "post-promote payload identity check failed"

printf 'candidate_path=%s\n' "$DEST"
printf 'candidate_id=%s\n' "$RECOMPUTED_ID"
printf 'source_sha=%s\n' "$SOURCE_SHA"
printf 'payload_tree_hash=%s\n' "$PAYLOAD_HASH"
printf 'promote_result=%s\n' "$PROMOTE_RESULT"
printf 'archive_sha256=%s\n' "$ACTUAL_ARCHIVE_SHA"
note "import complete (payload identity verified; not a shipping tier claim)"
