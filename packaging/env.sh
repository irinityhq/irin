#!/usr/bin/env bash
# Isolation env for IRIN DMG packaging — build caches under packaging/;
# durable candidate identity under IRIN_CANDIDATE_ROOT (never a worktree).
# shellcheck shell=bash
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export IRIN_DMG_ROOT="$ROOT"
export IRIN_SRC="$ROOT"
export TMPDIR="${IRIN_DMG_TMPDIR:-$ROOT/packaging/build/tmp}"
export CARGO_HOME="${CARGO_HOME:-$ROOT/packaging/build/cargo-home}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/packaging/build/cargo-target}"
# Release packaging stays provenance-isolated but never retains incremental state.
export CARGO_INCREMENTAL=0
export npm_config_cache="${npm_config_cache:-$ROOT/packaging/build/npm-cache}"
export npm_config_prefer_offline=true
# Never force color into logs/receipts when selection or capture becomes data.
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"
export NO_COLOR="${NO_COLOR:-1}"

# Matching provenance for host + council. Prefer an already-committed clean SHA.
if [[ -z "${IRIN_TAURI_BUILD_GIT_SHA:-}" ]]; then
  if SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null)"; then
    export IRIN_TAURI_BUILD_GIT_SHA="$SHA"
  fi
fi
if [[ -n "${IRIN_TAURI_BUILD_GIT_SHA:-}" ]]; then
  export COUNCIL_BUILD_GIT_SHA="${COUNCIL_BUILD_GIT_SHA:-$IRIN_TAURI_BUILD_GIT_SHA}"
fi
if [[ -z "${IRIN_TAURI_BUILD_DIRTY:-}" ]]; then
  if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=normal 2>/dev/null || true)" ]]; then
    export IRIN_TAURI_BUILD_DIRTY=true
  else
    export IRIN_TAURI_BUILD_DIRTY=false
  fi
fi
export COUNCIL_BUILD_DIRTY="${COUNCIL_BUILD_DIRTY:-$IRIN_TAURI_BUILD_DIRTY}"

# --- Candidate store (durable identity; never inside a linked worktree) -------

irin_env_die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# Resolve IRIN_CANDIDATE_ROOT to an absolute path. Default:
#   ~/.local/state/irin/candidates
# Refuses any root under this monorepo checkout or any git worktree of it.
irin_resolve_candidate_root() {
  local root monorepo wt line
  root="${IRIN_CANDIDATE_ROOT:-$HOME/.local/state/irin/candidates}"
  if [[ "$root" == "~" || "$root" == "~/"* ]]; then
    root="${HOME}${root#\~}"
  fi
  mkdir -p "$root" || irin_env_die "could not create IRIN_CANDIDATE_ROOT: $root"
  root="$(cd "$root" && pwd)" || irin_env_die "could not resolve IRIN_CANDIDATE_ROOT: $root"
  monorepo="$(cd "$ROOT" && pwd)" || irin_env_die "could not resolve monorepo root"
  case "$root" in
    "$monorepo"|"$monorepo"/*)
      irin_env_die "IRIN_CANDIDATE_ROOT must not be inside the source checkout ($monorepo); got $root"
      ;;
  esac
  while IFS= read -r line; do
    case "$line" in
      worktree\ *)
        wt="${line#worktree }"
        if [[ -d "$wt" ]]; then
          wt="$(cd "$wt" && pwd)" || continue
          case "$root" in
            "$wt"|"$wt"/*)
              irin_env_die "IRIN_CANDIDATE_ROOT must not be inside a git worktree ($wt); got $root"
              ;;
          esac
        fi
        ;;
    esac
  done < <(git -C "$ROOT" worktree list --porcelain 2>/dev/null || true)
  export IRIN_CANDIDATE_ROOT="$root"
}

# Canonical identity JSON: recursively sorted keys, no insignificant whitespace,
# one trailing LF. candidate-id = sha256 of those exact bytes.
irin_canonical_identity_json() {
  # stdin: JSON object → stdout: canonical form
  python3 -c '
import json, sys
data = json.load(sys.stdin)
sys.stdout.write(json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False))
sys.stdout.write("\n")
'
}

irin_sha256_file() {
  local path="$1" value
  value="$(/usr/bin/shasum -a 256 "$path" | /usr/bin/awk '{print $1}')" \
    || irin_env_die "could not hash: $path"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || irin_env_die "invalid SHA-256 for: $path"
  printf '%s' "$value"
}

irin_sha256_bytes() {
  # stdin → 64-char hex on stdout
  local value
  value="$(/usr/bin/shasum -a 256 | /usr/bin/awk '{print $1}')" \
    || irin_env_die "could not hash stdin"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || irin_env_die "invalid SHA-256 for stdin"
  printf '%s' "$value"
}

# Write bundle-manifest.txt for an app bundle:
#   sorted relpath, file type, executable mode (octal), SHA-256 or symlink target
# Volatile metadata (mtime, xattr, owner) excluded.
irin_write_bundle_manifest() {
  local app="$1" out="$2"
  [[ -d "$app" ]] || irin_env_die "bundle-manifest: app missing: $app"
  python3 - "$app" "$out" <<'PY'
import hashlib
import os
import sys

app = os.path.abspath(sys.argv[1])
out = sys.argv[2]
rows = []

def mode_oct(path: str) -> str:
    return format(os.lstat(path).st_mode & 0o777, "04o")

for dirpath, dirnames, filenames in os.walk(app, followlinks=False):
    dirnames.sort()
    filenames.sort()
    for name in dirnames:
        full = os.path.join(dirpath, name)
        if os.path.islink(full):
            rel = os.path.relpath(full, app)
            rows.append((rel.replace(os.sep, "/"), "symlink", mode_oct(full), os.readlink(full)))
        else:
            rel = os.path.relpath(full, app)
            rows.append((rel.replace(os.sep, "/"), "dir", mode_oct(full), "-"))
    for name in filenames:
        full = os.path.join(dirpath, name)
        rel = os.path.relpath(full, app).replace(os.sep, "/")
        if os.path.islink(full):
            rows.append((rel, "symlink", mode_oct(full), os.readlink(full)))
        elif os.path.isfile(full):
            h = hashlib.sha256()
            with open(full, "rb") as fh:
                for chunk in iter(lambda: fh.read(1024 * 1024), b""):
                    h.update(chunk)
            rows.append((rel, "file", mode_oct(full), h.hexdigest()))
        else:
            rows.append((rel, "other", mode_oct(full), "-"))

rows.sort(key=lambda r: r[0])
with open(out, "w", encoding="utf-8", newline="\n") as fh:
    for rel, kind, mode, payload in rows:
        fh.write(f"{rel}\t{kind}\t{mode}\t{payload}\n")
PY
}

# Immutable payload tree hash for exact-retry identity:
# candidate.json + HASHES.txt + bundle-manifest.txt + DMG + every IRIN.app leaf.
irin_payload_tree_hash() {
  local candidate_dir="$1"
  python3 - "$candidate_dir" <<'PY'
import hashlib
import os
import sys

root = os.path.abspath(sys.argv[1])
required = ["candidate.json", "HASHES.txt", "bundle-manifest.txt"]
for name in required:
    path = os.path.join(root, name)
    if not os.path.isfile(path):
        raise SystemExit(f"payload missing required file: {name}")

dmgs = sorted(
    n for n in os.listdir(root)
    if n.endswith(".dmg") and os.path.isfile(os.path.join(root, n))
)
if len(dmgs) != 1:
    raise SystemExit(f"payload must contain exactly one DMG (found {len(dmgs)})")
app = os.path.join(root, "IRIN.app")
if not os.path.isdir(app):
    raise SystemExit("payload missing IRIN.app")

entries = []

def file_sha(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()

for name in required + dmgs:
    rel = name
    entries.append((rel, file_sha(os.path.join(root, name))))

for dirpath, dirnames, filenames in os.walk(app, followlinks=False):
    dirnames.sort()
    filenames.sort()
    for name in dirnames + filenames:
        full = os.path.join(dirpath, name)
        rel = os.path.relpath(full, root).replace(os.sep, "/")
        if os.path.islink(full):
            payload = "symlink:" + os.readlink(full)
            entries.append((rel, hashlib.sha256(payload.encode()).hexdigest()))
        elif os.path.isfile(full):
            entries.append((rel, file_sha(full)))
        elif os.path.isdir(full):
            continue
        else:
            entries.append((rel, hashlib.sha256(b"other").hexdigest()))

entries.sort(key=lambda r: r[0])
h = hashlib.sha256()
for rel, digest in entries:
    h.update(rel.encode("utf-8"))
    h.update(b"\0")
    h.update(digest.encode("ascii"))
    h.update(b"\n")
print(h.hexdigest())
PY
}

# Extract gateway/sidecar identity digests from image-manifest.json.
# Prefer @sha256:<hex>; otherwise use the full image ref string.
irin_image_digests_from_manifest() {
  local manifest="$1"
  python3 - "$manifest" <<'PY'
import json, re, sys
path = sys.argv[1]
with open(path, encoding="utf-8") as fh:
    data = json.load(fh)
images = data.get("images") or {}
for key in ("gateway", "sidecar"):
    ref = images.get(key) or ""
    if not ref:
        raise SystemExit(f"image-manifest missing images.{key}: {path}")
    m = re.search(r"@sha256:([0-9a-f]{64})$", ref)
    print(m.group(1) if m else ref)
PY
}

# Bind Gateway/sidecar inputs to the candidate source SHA.
# production: full provenance is checked elsewhere; still require field match.
# signed-rc: local-dev-mode manifest required; source_sha must equal the build SHA
#   (no registry provenance claim).
# local-dev: when the manifest carries a 40-char source_sha, it must match.
irin_assert_gateway_source_binding() {
  local manifest="$1" expected_sha="$2" pack_mode="$3"
  python3 - "$manifest" "$expected_sha" "$pack_mode" <<'PY'
import json, re, sys
manifest_path, expected_sha, pack_mode = sys.argv[1:]
with open(manifest_path, encoding="utf-8") as fh:
    data = json.load(fh)
mode = data.get("mode")
got = data.get("source_sha")
if pack_mode == "signed-rc":
    if mode != "local-dev":
        raise SystemExit(
            f"signed-rc requires a local-dev Gateway Pack manifest (got mode={mode!r})"
        )
    if not re.fullmatch(r"[0-9a-f]{40}", expected_sha or ""):
        raise SystemExit("signed-rc requires a full 40-char source SHA")
    if got != expected_sha:
        raise SystemExit(
            f"signed-rc Gateway inputs are not source-bound: manifest source_sha={got!r} "
            f"!= build source_sha={expected_sha!r}; rebuild local Gateway images for this SHA"
        )
elif pack_mode == "production":
    if mode != "production":
        raise SystemExit(f"production requires production Gateway Pack manifest (got mode={mode!r})")
    if got != expected_sha:
        raise SystemExit(
            f"production Gateway inputs are not source-bound: manifest source_sha={got!r} "
            f"!= build source_sha={expected_sha!r}"
        )
elif pack_mode == "local-dev":
    if mode != "local-dev":
        raise SystemExit(f"local-dev requires local-dev Gateway Pack manifest (got mode={mode!r})")
    if isinstance(got, str) and re.fullmatch(r"[0-9a-f]{40}", got):
        if got != expected_sha:
            raise SystemExit(
                f"local-dev Gateway inputs are not source-bound: manifest source_sha={got!r} "
                f"!= build source_sha={expected_sha!r}"
            )
else:
    raise SystemExit(f"unknown pack_mode for gateway binding: {pack_mode!r}")
print(f"gateway source binding ok: pack_mode={pack_mode} source_sha={expected_sha}")
PY
}

# Freeze immutable payload bytes (candidate.json, HASHES, bundle-manifest, DMG,
# IRIN.app). proofs/ smoke/ install/ logs/ remain writable for tier evidence.
# Idempotent: safe to re-run on an already-frozen tree (heals crash residue).
irin_freeze_immutable_payload() {
  local dest="$1" dmg
  [[ -d "$dest" ]] || irin_env_die "freeze: not a directory: $dest"
  [[ -f "$dest/candidate.json" ]] || irin_env_die "freeze: candidate.json missing: $dest"
  chmod a-w "$dest/candidate.json" "$dest/HASHES.txt" "$dest/bundle-manifest.txt" \
    || irin_env_die "freeze: could not lock identity receipts under $dest"
  for dmg in "$dest"/*.dmg; do
    [[ -f "$dmg" ]] || continue
    chmod a-w "$dmg" || irin_env_die "freeze: could not lock DMG: $dmg"
  done
  if [[ -d "$dest/IRIN.app" ]]; then
    chmod -R a-w "$dest/IRIN.app" || irin_env_die "freeze: could not lock IRIN.app under $dest"
  fi
}

# Handle an already-present final candidate path during promote.
# Prints "idempotent" on payload match; dies on incomplete or corruption.
# Re-applies freeze so a prior rename-then-crash leaves no writable residue.
# bash 3.2 compatible (no nested functions).
irin_promote_handle_existing_dest() {
  local d="$1" expected_hash="$2" claim_path="$3" got
  if [[ ! -d "$d" ]]; then
    irin_env_die "promote: dest path exists but is not a directory: $d"
  fi
  if [[ ! -f "$d/candidate.json" ]]; then
    irin_env_die \
      "promote: dest exists but is incomplete (crashed or non-atomic promote): $d"
  fi
  got="$(irin_payload_tree_hash "$d")"
  if [[ "$got" == "$expected_hash" ]]; then
    # Heal freeze if a prior crash left complete-but-writable payload bytes.
    irin_freeze_immutable_payload "$d" \
      || irin_env_die "promote: could not re-freeze existing candidate: $d"
    # Prior success may have left a claim after a crash post-rename.
    rm -rf "$claim_path" 2>/dev/null || true
    printf '%s' "idempotent"
    return 0
  fi
  irin_env_die "candidate-id collision with different payload tree (corruption): $d"
}

# Promote staging → dest with plan atomicity:
#   exclusive sibling claim  +  freeze staging payload  +  one same-filesystem
#   rename of the whole staging directory onto the still-absent final path.
#
# Payload is frozen *before* rename so the final path becomes visible already
# immutable (plan: payload bytes immutable after the atomic move). A crash
# between freeze and rename only leaves frozen bytes under staging, never a
# complete-but-writable final candidate. Idempotent existing-dest also freezes.
#
# Never mkdir the final path and fill it child-by-child. Never `mv staging
# existing-dir` on Darwin (that nests staging inside dest).
#
# Prints "created" or "idempotent". Payload mismatch under the same
# candidate-id is hard refuse (corruption).
irin_promote_candidate_from_staging() {
  local staging="$1" dest="$2" dest_parent claim payload_hash
  [[ -d "$staging" ]] || irin_env_die "promote: staging missing: $staging"
  [[ -f "$staging/candidate.json" ]] || irin_env_die "promote: staging incomplete: $staging"
  [[ "$staging" == /* && "$dest" == /* ]] \
    || irin_env_die "promote: staging and dest must be absolute paths"
  dest_parent="$(dirname "$dest")"
  # Sibling claim: <candidate-id>.claim next to the still-absent final path.
  claim="${dest}.claim"
  mkdir -p "$dest_parent" || irin_env_die "promote: could not create parent: $dest_parent"
  # Content hash before freeze (mode bits are not in the payload tree hash).
  payload_hash="$(irin_payload_tree_hash "$staging")"

  # Fast path: final candidate already present (exact retry / concurrent winner).
  if [[ -e "$dest" ]]; then
    irin_promote_handle_existing_dest "$dest" "$payload_hash" "$claim"
    return 0
  fi

  # Exclusive sibling claim. Holds the right to rename into the still-absent dest.
  if ! mkdir "$claim" 2>/dev/null; then
    # Another promote holds the claim, or a prior crash left it.
    if [[ -e "$dest" ]]; then
      irin_promote_handle_existing_dest "$dest" "$payload_hash" "$claim"
      return 0
    fi
    irin_env_die \
      "promote: concurrent or stale claim exists (no final candidate yet): $claim"
  fi

  # We hold the claim. Re-check dest is still absent, then freeze + rename.
  if [[ -e "$dest" ]]; then
    # Lost the race after claim; release claim and treat as existing.
    rmdir "$claim" 2>/dev/null || rm -rf "$claim"
    irin_promote_handle_existing_dest "$dest" "$payload_hash" "$claim"
    return 0
  fi

  # Freeze *before* rename so the final path appears already immutable.
  if ! irin_freeze_immutable_payload "$staging"; then
    rmdir "$claim" 2>/dev/null || rm -rf "$claim"
    irin_env_die "promote: freeze failed on staging before rename: $staging"
  fi

  # Same-filesystem directory rename: staging becomes dest in one step.
  # Dest must not exist (guaranteed above under claim). If rename fails, release
  # claim and refuse — never fall back to child-by-child copy into dest.
  # Staging remains frozen under its original path if rename fails.
  if ! mv "$staging" "$dest"; then
    rmdir "$claim" 2>/dev/null || rm -rf "$claim"
    irin_env_die \
      "promote: atomic rename failed (cross-device or IO): $staging -> $dest"
  fi

  rmdir "$claim" 2>/dev/null || rm -rf "$claim"
  printf '%s' "created"
  return 0
}

# Atomically write a tier-bearing proof envelope under proofs/.
# Extra fields: optional JSON object string merged into the envelope (default {}).
irin_write_proof_envelope() {
  local out_path="$1" proof_kind="$2" candidate_id="$3" source_sha="$4" result="$5"
  # Avoid ${6:-{}} — bash parses the inner } as closing the expansion and
  # appends a literal }, corrupting JSON extras.
  local extra_json="${6-}"
  [[ -n "$extra_json" ]] || extra_json='{}'
  local out_dir tmp run_id ts
  out_dir="$(dirname "$out_path")"
  mkdir -p "$out_dir" || irin_env_die "could not create proof dir: $out_dir"
  run_id="$(python3 -c 'import uuid; print(uuid.uuid4())')"
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  tmp="$(mktemp "$out_dir/.proof.XXXXXX")"
  python3 - "$tmp" "$proof_kind" "$candidate_id" "$source_sha" "$result" \
    "$extra_json" "$run_id" "$ts" <<'PY'
import json, sys
out, kind, cid, sha, result, extra_raw, run_id, ts = sys.argv[1:]
extra = json.loads(extra_raw) if extra_raw else {}
if not isinstance(extra, dict):
    raise SystemExit("proof extra fields must be a JSON object")
doc = {
    "schema_version": 1,
    "proof_kind": kind,
    "candidate_id": cid,
    "source_sha": sha,
    "result": result,
    "tool_version": "irin-packaging/1",
    "run_id": run_id,
    "timestamp": ts,
}
# Extra must not clobber core identity keys.
for key, value in extra.items():
    if key in doc:
        raise SystemExit(f"proof extra field collides with core key: {key}")
    doc[key] = value
with open(out, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, sort_keys=True, indent=2)
    fh.write("\n")
PY
  mv -f "$tmp" "$out_path" || irin_env_die "could not atomically write proof: $out_path"
}

# Assert store path components cannot escape IRIN_CANDIDATE_ROOT.
# semver: single path component (no slashes, no ..); source_sha: 40 lowercase hex;
# candidate_id: 64 lowercase hex. Returns absolute DEST; dies on escape.
#
# Containment is validated on the nearest existing ancestor *before* any mkdir,
# so an intermediate symlink out of the store cannot create directories outside
# IRIN_CANDIDATE_ROOT and then only fail afterwards.
irin_assert_safe_candidate_dest() {
  local root="$1" semver="$2" source_sha="$3" candidate_id="$4"
  local dest root_real cursor rel next_comp resolved_parent
  [[ -n "$root" && "$root" == /* ]] || irin_env_die "candidate root must be absolute"
  # Single-component semver: digits/letters/dot/plus/hyphen only (e.g. 0.1.2, 0.1.2-rc.1).
  # Refuse slash and backslash via character class (no literal \ in the pattern).
  [[ "$semver" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]*$ ]] \
    || irin_env_die "unsafe or invalid semver component: $semver"
  [[ "$semver" != *".."* ]] \
    || irin_env_die "semver must be a single path component: $semver"
  [[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] \
    || irin_env_die "source_sha must be 40 lowercase hex chars: $source_sha"
  [[ "$candidate_id" =~ ^[0-9a-f]{64}$ ]] \
    || irin_env_die "candidate_id must be 64 lowercase hex chars: $candidate_id"

  root_real="$(cd "$root" && pwd -P)" || irin_env_die "could not resolve candidate root: $root"
  dest="${root_real}/${semver}/${source_sha}/${candidate_id}"

  # Walk components from the store root; refuse any existing path component that
  # is a symlink or resolves outside root_real *before* creating missing dirs.
  cursor="$root_real"
  for next_comp in "$semver" "$source_sha"; do
    rel="${cursor}/${next_comp}"
    if [[ -e "$rel" || -L "$rel" ]]; then
      if [[ -L "$rel" ]]; then
        irin_env_die "candidate path component is a symlink (refusing escape): $rel"
      fi
      if [[ ! -d "$rel" ]]; then
        irin_env_die "candidate path component exists but is not a directory: $rel"
      fi
      resolved_parent="$(cd "$rel" && pwd -P)" \
        || irin_env_die "could not resolve path component: $rel"
      case "$resolved_parent" in
        "$root_real"|"$root_real"/*) ;;
        *) irin_env_die "candidate path component escapes store root: $rel -> $resolved_parent" ;;
      esac
      cursor="$resolved_parent"
    else
      # Missing segment: create only after parent is known to be inside the root.
      case "$cursor" in
        "$root_real"|"$root_real"/*) ;;
        *) irin_env_die "candidate path parent escapes store root: $cursor" ;;
      esac
      mkdir -p "$rel" || irin_env_die "could not create candidate path: $rel"
      # Refuse if mkdir followed a race-created symlink out of the root.
      if [[ -L "$rel" ]]; then
        irin_env_die "candidate path became a symlink after create: $rel"
      fi
      resolved_parent="$(cd "$rel" && pwd -P)" \
        || irin_env_die "could not resolve created path: $rel"
      case "$resolved_parent" in
        "$root_real"|"$root_real"/*) ;;
        *) irin_env_die "created candidate path escapes store root: $rel -> $resolved_parent" ;;
      esac
      cursor="$resolved_parent"
    fi
  done

  # Final leaf is candidate_id (must not exist as a symlink out; promote creates it).
  dest="${cursor}/${candidate_id}"
  if [[ -e "$dest" || -L "$dest" ]]; then
    if [[ -L "$dest" ]]; then
      irin_env_die "candidate dest is a symlink (refusing escape): $dest"
    fi
    if [[ ! -d "$dest" ]]; then
      irin_env_die "candidate dest exists but is not a directory: $dest"
    fi
    dest="$(cd "$dest" && pwd -P)" || irin_env_die "could not resolve candidate dest: $dest"
    case "$dest" in
      "$root_real"/*) ;;
      *) irin_env_die "candidate dest escapes IRIN_CANDIDATE_ROOT ($root_real): $dest" ;;
    esac
  else
    case "$cursor" in
      "$root_real"|"$root_real"/*) ;;
      *) irin_env_die "candidate dest parent escapes store root: $cursor" ;;
    esac
    dest="${cursor}/${candidate_id}"
  fi
  printf '%s' "$dest"
}

# Assert a candidate directory's on-disk payload matches candidate.json identity.
# Checks (all must hold):
#   - exactly one DMG; DMG bytes == identity.dmg_sha256
#   - bundle-manifest.txt digest == identity.bundle_manifest_digest
#   - HASHES.txt source_sha/dmg_sha256/bundle_manifest_digest/pack_mode match identity
#   - IRIN.app path/kind/content/freeze-normalized-mode match bundle-manifest.txt
#     (same comparison as scripts/candidate-status.sh / W2)
# Dies on any mismatch. Used by import-candidate and worktree evidence harvest
# so a mutated DMG/app under an unchanged candidate.json cannot be promoted.
irin_assert_candidate_payload_matches_identity() {
  local cand="$1"
  [[ -d "$cand" ]] || irin_env_die "payload assert: not a directory: $cand"
  python3 - "$cand" <<'PY'
import hashlib
import json
import os
import re
import sys

cand = os.path.abspath(sys.argv[1])
cj = os.path.join(cand, "candidate.json")
hashes_path = os.path.join(cand, "HASHES.txt")
bm_path = os.path.join(cand, "bundle-manifest.txt")
errs = []

def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()

def mode_oct(path: str) -> str:
    return format(os.lstat(path).st_mode & 0o777, "04o")

def compute_bundle_manifest_rows(app: str) -> list:
    """Match irin_write_bundle_manifest / candidate-status row shape."""
    app = os.path.abspath(app)
    rows = []
    for dirpath, dirnames, filenames in os.walk(app, followlinks=False):
        dirnames.sort()
        filenames.sort()
        for name in dirnames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, app).replace(os.sep, "/")
            if os.path.islink(full):
                rows.append((rel, "symlink", mode_oct(full), os.readlink(full)))
            else:
                rows.append((rel, "dir", mode_oct(full), "-"))
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, app).replace(os.sep, "/")
            if os.path.islink(full):
                rows.append((rel, "symlink", mode_oct(full), os.readlink(full)))
            elif os.path.isfile(full):
                rows.append((rel, "file", mode_oct(full), sha256_file(full)))
            else:
                rows.append((rel, "other", mode_oct(full), "-"))
    rows.sort(key=lambda r: r[0])
    return rows

def parse_bundle_manifest(text: str) -> dict:
    out = {}
    for line in text.splitlines():
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) != 4:
            raise ValueError(f"malformed bundle-manifest row: {line!r}")
        rel, kind, mode, payload = parts
        if rel in out:
            raise ValueError(f"duplicate bundle-manifest path: {rel}")
        out[rel] = (kind, mode, payload)
    return out

def freeze_normalized_mode(mode_str: str) -> str:
    """Clear write bits only (chmod a-w freeze); preserve r/x. Matches W2."""
    mode = int(mode_str, 8)
    return format(mode & ~0o222, "04o")

def app_symlink_containment_errors(app: str) -> list:
    """Every symlink under IRIN.app must resolve physically inside IRIN.app.

    Safe framework-relative links (e.g. Current -> A) pass. Absolute or
    escaping targets (e.g. ExternalPointer -> /tmp/Evil) fail. Prevents
    export/harvest from blessing a mutable external pointer that import
    would refuse as an unsafe link.
    """
    local_errs = []
    app_real = os.path.realpath(app)
    if not app_real.endswith(os.sep):
        app_prefix = app_real + os.sep
    else:
        app_prefix = app_real
    for dirpath, dirnames, filenames in os.walk(app, followlinks=False):
        # Inspect symlink dirs without descending through them.
        for name in list(dirnames) + list(filenames):
            full = os.path.join(dirpath, name)
            if not os.path.islink(full):
                continue
            rel = os.path.relpath(full, app).replace(os.sep, "/")
            target = os.readlink(full)
            try:
                resolved = os.path.realpath(full)
            except OSError as exc:
                local_errs.append(f"IRIN.app symlink {rel} could not be resolved: {exc}")
                continue
            if resolved != app_real and not resolved.startswith(app_prefix):
                local_errs.append(
                    f"IRIN.app symlink escapes app: {rel} -> {target!r} "
                    f"(resolved {resolved})"
                )
    return local_errs

def app_content_matches_manifest(app: str, stored_manifest_text: str) -> list:
    """Path/kind/payload + freeze-normalized mode + symlink containment.

    Same rules as candidate-status, plus physical containment of symlink targets.
    """
    local_errs = []
    local_errs.extend(app_symlink_containment_errors(app))
    try:
        stored = parse_bundle_manifest(stored_manifest_text)
    except ValueError as exc:
        return local_errs + [str(exc)]
    current_rows = compute_bundle_manifest_rows(app)
    current = {rel: (kind, mode, payload) for rel, kind, mode, payload in current_rows}
    stored_paths = set(stored)
    current_paths = set(current)
    missing = sorted(stored_paths - current_paths)
    extra = sorted(current_paths - stored_paths)
    if missing:
        local_errs.append(f"IRIN.app missing paths from bundle-manifest: {missing[:5]}")
    if extra:
        local_errs.append(f"IRIN.app has paths not in bundle-manifest: {extra[:5]}")
    for rel in sorted(stored_paths & current_paths):
        s_kind, s_mode, s_payload = stored[rel]
        c_kind, c_mode, c_payload = current[rel]
        if s_kind != c_kind:
            local_errs.append(
                f"IRIN.app kind mismatch for {rel}: stored={s_kind} current={c_kind}"
            )
            continue
        if s_payload != c_payload:
            local_errs.append(f"IRIN.app content mismatch for {rel}")
        try:
            s_norm = freeze_normalized_mode(s_mode)
            c_norm = freeze_normalized_mode(c_mode)
        except ValueError as exc:
            local_errs.append(f"IRIN.app mode parse for {rel}: {exc}")
            continue
        if s_norm != c_norm:
            local_errs.append(
                f"IRIN.app mode mismatch for {rel}: "
                f"stored={s_mode}(norm {s_norm}) current={c_mode}(norm {c_norm})"
            )
    return local_errs

if not os.path.isfile(cj):
    raise SystemExit(f"payload assert: candidate.json missing: {cj}")
raw = open(cj, "rb").read()
try:
    identity = json.loads(raw.decode("utf-8"))
except json.JSONDecodeError as exc:
    raise SystemExit(f"payload assert: candidate.json not JSON: {exc}") from exc
if not isinstance(identity, dict):
    raise SystemExit("payload assert: candidate.json must be an object")
canon = json.dumps(identity, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
if raw.decode("utf-8") != canon:
    errs.append("candidate.json is not in canonical identity form")

for key in (
    "source_sha",
    "semver",
    "pack_mode",
    "dmg_sha256",
    "bundle_manifest_digest",
):
    if key not in identity:
        errs.append(f"candidate.json missing {key}")

dmg_sha = identity.get("dmg_sha256")
bm_sha = identity.get("bundle_manifest_digest")
source_sha = identity.get("source_sha")
pack_mode = identity.get("pack_mode")
semver = identity.get("semver")
if not isinstance(dmg_sha, str) or not re.fullmatch(r"[0-9a-f]{64}", dmg_sha or ""):
    errs.append("identity dmg_sha256 invalid")
if not isinstance(bm_sha, str) or not re.fullmatch(r"[0-9a-f]{64}", bm_sha or ""):
    errs.append("identity bundle_manifest_digest invalid")
if not isinstance(source_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", source_sha or ""):
    errs.append("identity source_sha must be 40 lowercase hex")
if not isinstance(semver, str) or not re.fullmatch(r"[0-9A-Za-z][0-9A-Za-z.+-]*", semver or ""):
    errs.append("identity semver is not a safe single path component")
elif ".." in (semver or "") or "/" in (semver or "") or "\\" in str(semver or ""):
    errs.append("identity semver must be a single path component")

dmgs = sorted(
    n for n in os.listdir(cand)
    if n.endswith(".dmg") and os.path.isfile(os.path.join(cand, n))
)
if len(dmgs) != 1:
    errs.append(f"exactly one DMG required (found {len(dmgs)})")
else:
    actual_dmg = sha256_file(os.path.join(cand, dmgs[0]))
    if dmg_sha and actual_dmg != dmg_sha:
        errs.append(
            f"DMG bytes do not match identity dmg_sha256 "
            f"(got {actual_dmg}, identity {dmg_sha})"
        )

if not os.path.isfile(bm_path):
    errs.append("bundle-manifest.txt missing")
else:
    actual_bm = sha256_file(bm_path)
    if bm_sha and actual_bm != bm_sha:
        errs.append(
            f"bundle-manifest.txt does not match identity digest "
            f"(got {actual_bm}, identity {bm_sha})"
        )

if not os.path.isfile(hashes_path):
    errs.append("HASHES.txt missing")
else:
    hashes = {}
    for line in open(hashes_path, encoding="utf-8"):
        line = line.rstrip("\n")
        if not line or "=" not in line:
            continue
        k, v = line.split("=", 1)
        if k in hashes:
            errs.append(f"duplicate HASHES key: {k}")
        hashes[k] = v
    for key, expected in (
        ("source_sha", source_sha),
        ("dmg_sha256", dmg_sha),
        ("bundle_manifest_digest", bm_sha),
        ("pack_mode", pack_mode),
    ):
        got = hashes.get(key)
        if got != expected:
            errs.append(f"HASHES {key} != identity ({got!r} vs {expected!r})")

app = os.path.join(cand, "IRIN.app")
# Top-level IRIN.app must be a real directory physically under the candidate.
# Framework-style symlinks *inside* the app remain valid (manifest-bound).
# A top-level symlink would let harvest promote a mutable external pointer.
if os.path.islink(app):
    errs.append(
        "IRIN.app must not be a symlink (top-level app must be a real directory "
        "physically contained in the candidate; internal framework symlinks are OK)"
    )
elif not os.path.isdir(app):
    errs.append("IRIN.app missing")
else:
    app_real = os.path.realpath(app)
    cand_real = os.path.realpath(cand)
    if app_real != os.path.join(cand_real, "IRIN.app") and not app_real.startswith(
        cand_real + os.sep
    ):
        errs.append(
            f"IRIN.app is not physically contained under the candidate "
            f"(app={app_real}, candidate={cand_real})"
        )
    elif not os.path.isfile(bm_path):
        pass  # already recorded
    else:
        stored_bm_text = open(bm_path, encoding="utf-8").read()
        errs.extend(app_content_matches_manifest(app, stored_bm_text))

if errs:
    raise SystemExit("payload assert failed:\n  - " + "\n  - ".join(errs))
print("payload matches identity")
PY
}

# Require an absolute candidate store path under IRIN_CANDIDATE_ROOT.
# Sets: IRIN_CANDIDATE_PATH (canonical), and derives DMG/HASHES/APP when present.
irin_require_candidate_path() {
  local path canon root
  path="${IRIN_CANDIDATE_PATH:-}"
  [[ -n "$path" ]] || irin_env_die \
    "IRIN_CANDIDATE_PATH is required (absolute path under IRIN_CANDIDATE_ROOT); no packaging/artifacts or /Applications fallback"
  [[ "$path" == /* ]] || irin_env_die "IRIN_CANDIDATE_PATH must be absolute: $path"
  irin_resolve_candidate_root
  root="$IRIN_CANDIDATE_ROOT"
  [[ -d "$path" ]] || irin_env_die "IRIN_CANDIDATE_PATH is not a directory: $path"
  canon="$(cd "$path" && pwd)" || irin_env_die "could not resolve IRIN_CANDIDATE_PATH: $path"
  case "$canon" in
    "$root"/*) ;;
    *) irin_env_die "IRIN_CANDIDATE_PATH must be under IRIN_CANDIDATE_ROOT ($root); got $canon" ;;
  esac
  # failed/ attempts are not valid candidates for verify/smoke/publish
  case "$canon" in
    */failed/*) irin_env_die "refusing failed attempt path as candidate: $canon" ;;
  esac
  [[ -f "$canon/candidate.json" ]] || irin_env_die "candidate.json missing: $canon"
  [[ -f "$canon/HASHES.txt" ]] || irin_env_die "HASHES.txt missing: $canon"
  [[ -f "$canon/bundle-manifest.txt" ]] || irin_env_die "bundle-manifest.txt missing: $canon"
  local dmg_count
  dmg_count="$(find "$canon" -maxdepth 1 -type f -name '*.dmg' | wc -l | tr -d ' ')"
  [[ "$dmg_count" == "1" ]] || irin_env_die "candidate must contain exactly one DMG (found $dmg_count): $canon"
  export IRIN_CANDIDATE_PATH="$canon"
}

# Resolve candidate root early so packaging scripts share one pinned path.
irin_resolve_candidate_root

mkdir -p "$TMPDIR" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$npm_config_cache" \
  "$ROOT/packaging/artifacts" "$ROOT/packaging/test-home" "$ROOT/packaging/test-apps" \
  "$ROOT/packaging/build/dmg-mount" "$ROOT/packaging/receipts" \
  "$IRIN_CANDIDATE_ROOT"
