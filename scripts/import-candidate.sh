#!/usr/bin/env bash
# import-candidate.sh — verify and atomically import an exported candidate archive.
#
# Usage:
#   scripts/import-candidate.sh --archive PATH
#       [--expected-source-sha SHA]
#       [--expected-candidate-id ID]
#       [--expected-archive-sha256 HEX]
#
# Verifies:
#   - optional archive SHA-256 (sidecar or --expected-archive-sha256)
#   - candidate.json canonical form and recomputed candidate-id
#   - payload tree integrity
#   - optional source SHA / candidate ID bindings
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
EXPECTED_SOURCE_SHA=""
EXPECTED_CANDIDATE_ID=""
EXPECTED_ARCHIVE_SHA=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --archive)
      ARCHIVE="${2:-}"
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
    [--expected-source-sha SHA]
    [--expected-candidate-id ID]
    [--expected-archive-sha256 HEX]

Verify a deterministic export archive and atomically move it into
IRIN_CANDIDATE_ROOT. Refuses tampered archives, payload mismatches, and
source/candidate-id disagreements.
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

irin_resolve_candidate_root

# --- archive integrity -------------------------------------------------------
ACTUAL_ARCHIVE_SHA="$(irin_sha256_file "$ARCHIVE")"
SIDECAR="${ARCHIVE}.sha256"
if [[ -z "$EXPECTED_ARCHIVE_SHA" && -f "$SIDECAR" ]]; then
  EXPECTED_ARCHIVE_SHA="$(awk '{print $1; exit}' "$SIDECAR")"
fi
if [[ -n "$EXPECTED_ARCHIVE_SHA" ]]; then
  [[ "$EXPECTED_ARCHIVE_SHA" =~ ^[0-9a-f]{64}$ ]] \
    || die "invalid expected archive sha256: $EXPECTED_ARCHIVE_SHA"
  [[ "$ACTUAL_ARCHIVE_SHA" == "$EXPECTED_ARCHIVE_SHA" ]] \
    || die "archive SHA-256 mismatch (got $ACTUAL_ARCHIVE_SHA, expected $EXPECTED_ARCHIVE_SHA)"
  note "archive SHA-256 matches"
fi

# --- extract to exclusive staging under the durable store --------------------
IMPORT_STAGING_ROOT="$IRIN_CANDIDATE_ROOT/.import-staging"
IMPORT_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
STAGING="$IMPORT_STAGING_ROOT/$IMPORT_ID"
mkdir -p "$IMPORT_STAGING_ROOT"
# Fail closed if staging path already exists.
mkdir "$STAGING" || die "could not create exclusive import staging: $STAGING"

cleanup() {
  status=$?
  if [[ -d "$STAGING" ]]; then
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
# Reject path traversal and absolute names.
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
        # filter='data' is the Python 3.12+ safe default; fall back on older runtimes.
        try:
            tar.extract(member, path=dest, filter="data")
        except TypeError:
            tar.extract(member, path=dest)
PY

# --- validate candidate identity --------------------------------------------
[[ -f "$STAGING/candidate.json" ]] || die "archive missing candidate.json"
[[ -f "$STAGING/HASHES.txt" ]] || die "archive missing HASHES.txt"
[[ -f "$STAGING/bundle-manifest.txt" ]] || die "archive missing bundle-manifest.txt"
[[ -d "$STAGING/IRIN.app" ]] || die "archive missing IRIN.app"
dmg_count="$(find "$STAGING" -maxdepth 1 -type f -name '*.dmg' | wc -l | tr -d ' ')"
[[ "$dmg_count" == "1" ]] || die "archive must contain exactly one DMG (found $dmg_count)"

# Canonical form + recomputed candidate-id.
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
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "invalid source_sha in imported candidate.json"
[[ -n "$SEMVER" ]] || die "semver missing from candidate.json"

if [[ -n "$EXPECTED_CANDIDATE_ID" ]]; then
  [[ "$EXPECTED_CANDIDATE_ID" == "$RECOMPUTED_ID" ]] \
    || die "candidate-id mismatch (got $RECOMPUTED_ID, expected $EXPECTED_CANDIDATE_ID)"
fi
if [[ -n "$EXPECTED_SOURCE_SHA" ]]; then
  [[ "$EXPECTED_SOURCE_SHA" == "$SOURCE_SHA" ]] \
    || die "source_sha mismatch (got $SOURCE_SHA, expected $EXPECTED_SOURCE_SHA)"
fi

PAYLOAD_HASH="$(irin_payload_tree_hash "$STAGING")" \
  || die "imported payload tree hash failed (corrupt or incomplete archive)"

# Ensure proofs/ exists for later tier evidence (may be empty in minimal exports).
mkdir -p "$STAGING/proofs" "$STAGING/smoke" "$STAGING/install" "$STAGING/logs"

DEST="$IRIN_CANDIDATE_ROOT/$SEMVER/$SOURCE_SHA/$RECOMPUTED_ID"
note "promote import staging → $DEST"
PROMOTE_RESULT="$(irin_promote_candidate_from_staging "$STAGING" "$DEST")" \
  || die "promote failed"
# Staging was renamed into dest (or discarded on idempotent match).
if [[ "$PROMOTE_RESULT" == "created" ]]; then
  # prevent cleanup from wiping the promoted path (rename leaves STAGING gone)
  :
fi
# Mark staging consumed so EXIT trap does not rm a path we no longer own.
if [[ ! -d "$STAGING" ]]; then
  STAGING=""
fi
trap - EXIT
# If promote was idempotent, staging may still exist under import-staging.
if [[ -n "$STAGING" && -d "$STAGING" ]]; then
  chmod -R u+w "$STAGING" 2>/dev/null || true
  rm -rf "$STAGING"
fi

printf 'candidate_path=%s\n' "$DEST"
printf 'candidate_id=%s\n' "$RECOMPUTED_ID"
printf 'source_sha=%s\n' "$SOURCE_SHA"
printf 'payload_tree_hash=%s\n' "$PAYLOAD_HASH"
printf 'promote_result=%s\n' "$PROMOTE_RESULT"
printf 'archive_sha256=%s\n' "$ACTUAL_ARCHIVE_SHA"
note "import complete (verification PASS on bytes; not a shipping tier claim)"
