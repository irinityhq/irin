#!/usr/bin/env bash
# Recoverably tear down a clean IRIN linked worktree; retain its branch.
#
# Before removal, scan *ignored* paths only for recognized candidate
# artifacts/proofs left by legacy or failed commands. W1 writes durable
# candidates directly to IRIN_CANDIDATE_ROOT, so there is normally nothing
# to harvest. If recognized ignored evidence is found, validate identity and
# import atomically into the store, or refuse removal.
#
# Keeps dirty-worktree refusal and branch retention. Does not add an
# unmerged-branch acknowledgement gate: removing a clean worktree does not
# delete its branch.
set -euo pipefail

SOURCE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run from an IRIN checkout\n' >&2
  exit 1
}
destination="${1:-}"
[[ -n "$destination" ]] || { printf 'usage: %s /absolute/worktree/path\n' "$0" >&2; exit 2; }
[[ "$destination" == /* ]] || { printf 'ERROR: worktree path must be absolute\n' >&2; exit 1; }
[[ -d "$destination" ]] || { printf 'ERROR: worktree does not exist: %s\n' "$destination" >&2; exit 1; }
# Prefer the path Git registered for this worktree (avoids macOS /tmp -> /private/tmp
# mismatch). Fall back to the physical path when the argument is already resolved.
destination_arg="$destination"
destination="$(cd "$destination" && pwd -P)"
registered=""
while IFS= read -r line; do
  [[ "$line" == worktree\ * ]] || continue
  wt="${line#worktree }"
  [[ -d "$wt" ]] || continue
  if [[ "$(cd "$wt" && pwd -P)" == "$destination" ]]; then
    registered="$wt"
    break
  fi
done < <(git -C "$SOURCE_ROOT" worktree list --porcelain 2>/dev/null || true)
if [[ -n "$registered" ]]; then
  destination="$registered"
fi
[[ "$destination" != "$(cd "$SOURCE_ROOT" && pwd -P)" ]] || {
  printf 'ERROR: refusing to remove the checkout running this command\n' >&2
  exit 1
}
# Also refuse when the physical path is the command's own checkout.
[[ "$(cd "$destination" && pwd -P)" != "$(cd "$SOURCE_ROOT" && pwd -P)" ]] || {
  printf 'ERROR: refusing to remove the checkout running this command\n' >&2
  exit 1
}

git -C "$destination" rev-parse --is-inside-work-tree >/dev/null
branch="$(git -C "$destination" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ -n "$branch" && "$branch" != "main" && "$branch" != "master" ]] || {
  printf 'ERROR: refusing to remove detached or main worktree: %s\n' "$destination" >&2
  exit 1
}
if [[ -n "$(git -C "$destination" status --porcelain --untracked-files=normal)" ]]; then
  printf 'ERROR: worktree has uncommitted files; preserve or clean them first\n' >&2
  git -C "$destination" status --short >&2
  exit 1
fi
unset destination_arg

# --- recognized ignored candidate evidence ---------------------------------
# shellcheck source=/dev/null
source "$SOURCE_ROOT/packaging/env.sh"
irin_resolve_candidate_root

# Collect directories that look like a candidate payload tree under ignored paths.
# Recognized markers: candidate.json + (HASHES.txt | *.dmg | proofs/*.json).
mapfile_candidates() {
  # Prints absolute paths of candidate-like directories (one per line).
  python3 - "$destination" <<'PY'
import os
import sys

root = os.path.abspath(sys.argv[1])
# Only scan known ignored packaging / receipt surfaces (not the whole tree).
scan_roots = [
    os.path.join(root, "packaging", "artifacts"),
    os.path.join(root, "packaging", "receipts"),
    os.path.join(root, "packaging", "test-apps"),
    os.path.join(root, "packaging", "build"),
    os.path.join(root, ".irin-receipts"),
]
# Also honor any extra ignored candidate spill dirs via env (tests).
extra = os.environ.get("IRIN_WORKTREE_EVIDENCE_SCAN_ROOTS", "")
for part in extra.split(os.pathsep):
    part = part.strip()
    if part:
        scan_roots.append(part if os.path.isabs(part) else os.path.join(root, part))

found = []
seen = set()

def is_candidate_dir(path: str) -> bool:
    cj = os.path.join(path, "candidate.json")
    if not os.path.isfile(cj):
        return False
    has_hashes = os.path.isfile(os.path.join(path, "HASHES.txt"))
    has_dmg = any(
        n.endswith(".dmg") and os.path.isfile(os.path.join(path, n))
        for n in os.listdir(path)
    )
    proofs = os.path.join(path, "proofs")
    has_proof = os.path.isdir(proofs) and any(
        n.endswith(".json") and os.path.isfile(os.path.join(proofs, n))
        for n in os.listdir(proofs)
    )
    return has_hashes or has_dmg or has_proof

for scan in scan_roots:
    if not os.path.isdir(scan):
        continue
    for dirpath, dirnames, filenames in os.walk(scan, followlinks=False):
        # Skip huge cargo/npm caches quickly.
        base = os.path.basename(dirpath)
        if base in {"cargo-home", "cargo-target", "npm-cache", "tmp", "node_modules", "target"}:
            dirnames[:] = []
            continue
        if "candidate.json" in filenames and is_candidate_dir(dirpath):
            ap = os.path.abspath(dirpath)
            if ap not in seen:
                seen.add(ap)
                found.append(ap)
            dirnames[:] = []  # do not walk into a candidate tree

for path in found:
    print(path)
PY
}

evidence_paths=()
while IFS= read -r line; do
  [[ -n "$line" ]] && evidence_paths+=("$line")
done < <(mapfile_candidates)

if (( ${#evidence_paths[@]} > 0 )); then
  printf 'Found recognized ignored candidate evidence under worktree:\n' >&2
  for p in "${evidence_paths[@]}"; do
    printf '  %s\n' "$p" >&2
  done

  # Harvest promotes via packaging/env.sh helpers (same atomic path as import).
  for evidence in "${evidence_paths[@]}"; do
    # Evidence outside IRIN_CANDIDATE_ROOT cannot use irin_require_candidate_path;
    # validate payload identity then promote a copy into the durable store.
    if [[ ! -f "$evidence/candidate.json" || ! -f "$evidence/HASHES.txt" \
      || ! -f "$evidence/bundle-manifest.txt" || ! -d "$evidence/IRIN.app" ]]; then
      printf 'ERROR: incomplete ignored candidate evidence (cannot validate/import): %s\n' \
        "$evidence" >&2
      printf 'Move or delete it deliberately, or complete the payload before removal.\n' >&2
      exit 1
    fi
    dmg_count="$(find "$evidence" -maxdepth 1 -type f -name '*.dmg' | wc -l | tr -d ' ')"
    if [[ "$dmg_count" != "1" ]]; then
      printf 'ERROR: ignored candidate evidence must contain exactly one DMG: %s\n' \
        "$evidence" >&2
      exit 1
    fi

    # Validate canonical identity and capture bindings before any store write.
    id_lines="$(python3 - "$evidence" <<'PY'
import hashlib, json, os, sys
root = sys.argv[1]
path = os.path.join(root, "candidate.json")
raw = open(path, "rb").read()
data = json.loads(raw.decode("utf-8"))
if not isinstance(data, dict):
    raise SystemExit("candidate.json must be an object")
canon = json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
if raw.decode("utf-8") != canon:
    raise SystemExit("candidate.json not canonical")
sha = data.get("source_sha") or ""
if not isinstance(sha, str) or len(sha) != 40:
    raise SystemExit("invalid source_sha")
semver = data.get("semver") or ""
if not semver:
    raise SystemExit("semver missing")
print(hashlib.sha256(canon.encode()).hexdigest())
print(sha)
print(semver)
PY
)" || {
      printf 'ERROR: ignored candidate evidence failed identity validation: %s\n' \
        "$evidence" >&2
      exit 1
    }
    cid="$(sed -n '1p' <<<"$id_lines")"
    src_sha="$(sed -n '2p' <<<"$id_lines")"
    semver="$(sed -n '3p' <<<"$id_lines")"
    [[ "$cid" =~ ^[0-9a-f]{64}$ ]] || {
      printf 'ERROR: bad candidate-id from evidence: %s\n' "$evidence" >&2
      exit 1
    }

    # Payload tree must hash cleanly (refuses incomplete / corrupt trees).
    if ! irin_payload_tree_hash "$evidence" >/dev/null; then
      printf 'ERROR: ignored candidate evidence failed payload tree hash: %s\n' \
        "$evidence" >&2
      exit 1
    fi

    stage="$IRIN_CANDIDATE_ROOT/.staging/worktree-harvest-$(python3 -c 'import uuid; print(uuid.uuid4())')"
    mkdir -p "$(dirname "$stage")"
    # Copy then promote (evidence stays in worktree until promote succeeds).
    rm -rf "$stage"
    mkdir -p "$stage"
    # Copy payload + proofs only.
    for name in candidate.json HASHES.txt bundle-manifest.txt IRIN.app proofs; do
      if [[ -e "$evidence/$name" ]]; then
        cp -a "$evidence/$name" "$stage/$name"
      fi
    done
    for dmg in "$evidence"/*.dmg; do
      [[ -f "$dmg" ]] || continue
      cp -a "$dmg" "$stage/$(basename "$dmg")"
    done
    mkdir -p "$stage/proofs" "$stage/smoke" "$stage/install" "$stage/logs"
    dest="$IRIN_CANDIDATE_ROOT/$semver/$src_sha/$cid"
    result="$(irin_promote_candidate_from_staging "$stage" "$dest")" || {
      printf 'ERROR: failed to import ignored candidate evidence into store: %s\n' \
        "$evidence" >&2
      exit 1
    }
    printf 'Harvested ignored candidate evidence → %s (%s)\n' "$dest" "$result"
    # Drop the worktree-local copy only after durable promote.
    chmod -R u+w "$evidence" 2>/dev/null || true
    rm -rf "$evidence"
  done
fi

runtime_state_dir=""
if [[ -f "$destination/.irin-worktree.env" ]]; then
  runtime_state_dir="$(sed -n 's/^IRIN_RUNTIME_STATE_DIR=//p' "$destination/.irin-worktree.env" | head -n 1)"
fi
if [[ -n "$runtime_state_dir" ]]; then
  allowed_root="$(cd "${HOME}/.local/state/irin/worktrees" 2>/dev/null && pwd -P)" || {
    printf 'ERROR: refusing unresolved runtime state root\n' >&2
    exit 1
  }
  if [[ -d "$runtime_state_dir" ]]; then
    resolved_runtime_state_dir="$(cd "$runtime_state_dir" 2>/dev/null && pwd -P)" || {
      printf 'ERROR: refusing unresolved runtime state path: %s\n' "$runtime_state_dir" >&2
      exit 1
    }
  else
    runtime_leaf="$(basename "$runtime_state_dir")"
    runtime_parent="$(cd "$(dirname "$runtime_state_dir")" 2>/dev/null && pwd -P)" || {
      printf 'ERROR: refusing unresolved runtime state path: %s\n' "$runtime_state_dir" >&2
      exit 1
    }
    [[ "$runtime_leaf" != "." && "$runtime_leaf" != ".." ]] || {
      printf 'ERROR: refusing unexpected runtime state path: %s\n' "$runtime_state_dir" >&2
      exit 1
    }
    resolved_runtime_state_dir="${runtime_parent}/${runtime_leaf}"
  fi
  [[ "$(dirname "$resolved_runtime_state_dir")" == "$allowed_root" ]] || {
    printf 'ERROR: refusing unexpected runtime state path: %s\n' "$runtime_state_dir" >&2
    exit 1
  }
  runtime_state_dir="$resolved_runtime_state_dir"
fi

git -C "$SOURCE_ROOT" worktree remove "$destination"
if [[ -n "$runtime_state_dir" && -d "$runtime_state_dir" ]]; then
  rm -rf -- "$runtime_state_dir"
  printf 'Removed generated runtime state: %s\n' "$runtime_state_dir"
fi
printf 'Removed worktree: %s\n' "$destination"
printf 'Retained branch: %s\n' "$branch"
