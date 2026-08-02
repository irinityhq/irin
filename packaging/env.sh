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

# Freeze immutable payload bytes after promote. proofs/ smoke/ install/ logs/
# remain writable for later tier evidence.
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
    # Prior success may have left a claim after a crash post-rename.
    rm -rf "$claim_path" 2>/dev/null || true
    printf '%s' "idempotent"
    return 0
  fi
  irin_env_die "candidate-id collision with different payload tree (corruption): $d"
}

# Promote staging → dest with plan atomicity:
#   exclusive sibling claim  +  one same-filesystem rename of the whole
#   staging directory onto the still-absent final path.
#
# Never mkdir the final path and fill it child-by-child (observers would see a
# half-built candidate; a crash would strand a partial final directory).
# Never `mv staging existing-dir` on Darwin (that nests staging inside dest).
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

  # We hold the claim. Re-check dest is still absent, then one atomic rename.
  if [[ -e "$dest" ]]; then
    # Lost the race after claim; release claim and treat as existing.
    rmdir "$claim" 2>/dev/null || rm -rf "$claim"
    irin_promote_handle_existing_dest "$dest" "$payload_hash" "$claim"
    return 0
  fi

  # Same-filesystem directory rename: staging becomes dest in one step.
  # Dest must not exist (guaranteed above under claim). If rename fails, release
  # claim and refuse — never fall back to child-by-child copy into dest.
  if ! mv "$staging" "$dest"; then
    rmdir "$claim" 2>/dev/null || rm -rf "$claim"
    irin_env_die \
      "promote: atomic rename failed (cross-device or IO): $staging -> $dest"
  fi

  # Final path is the full candidate tree. Freeze immutable payload, drop claim.
  if ! irin_freeze_immutable_payload "$dest"; then
    # Dest is complete on disk; leave it for diagnosis. Drop claim so retries
    # can take the existing-dest path rather than block forever.
    rmdir "$claim" 2>/dev/null || rm -rf "$claim"
    irin_env_die "promote: freeze failed after atomic rename: $dest"
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
