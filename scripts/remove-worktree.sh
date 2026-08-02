#!/usr/bin/env bash
# Recoverably tear down a clean IRIN linked worktree; retain its branch.
#
# Before removal, scan *ignored* paths only for recognized candidate
# artifacts/proofs left by legacy or failed commands. W1 writes durable
# candidates directly to IRIN_CANDIDATE_ROOT, so there is normally nothing
# to harvest. If recognized ignored evidence is found:
#   - complete payload → validate identity + payload bytes, import atomically
#   - incomplete / legacy residue → refuse removal (do not silently destroy)
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
destination_phys="$(cd "$destination" && pwd -P)"
registered=""
while IFS= read -r line; do
  [[ "$line" == worktree\ * ]] || continue
  wt="${line#worktree }"
  [[ -d "$wt" ]] || continue
  if [[ "$(cd "$wt" && pwd -P)" == "$destination_phys" ]]; then
    registered="$wt"
    break
  fi
done < <(git -C "$SOURCE_ROOT" worktree list --porcelain 2>/dev/null || true)
if [[ -n "$registered" ]]; then
  destination="$registered"
else
  destination="$destination_phys"
fi
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

# --- recognized ignored candidate evidence ---------------------------------
# shellcheck source=/dev/null
source "$SOURCE_ROOT/packaging/env.sh"
irin_resolve_candidate_root

# Scan ignored packaging / receipt surfaces. Markers are recognized independently
# of candidate.json so legacy HASHES/DMG/app/proof residue cannot be destroyed
# silently. Extra roots (tests) must stay physically under the target worktree.
scan_report="$(
  IRIN_WORKTREE_EVIDENCE_SCAN_ROOTS="${IRIN_WORKTREE_EVIDENCE_SCAN_ROOTS:-}" \
  python3 - "$destination" <<'PY'
import os
import sys

root = os.path.abspath(sys.argv[1])
root_real = os.path.realpath(root)

def under_root(path: str) -> bool:
    real = os.path.realpath(path)
    return real == root_real or real.startswith(root_real + os.sep)

scan_roots = [
    os.path.join(root, "packaging", "artifacts"),
    os.path.join(root, "packaging", "receipts"),
    os.path.join(root, "packaging", "test-apps"),
    os.path.join(root, "packaging", "build"),
    os.path.join(root, ".irin-receipts"),
]

extra = os.environ.get("IRIN_WORKTREE_EVIDENCE_SCAN_ROOTS", "")
for part in extra.split(os.pathsep):
    part = part.strip()
    if not part:
        continue
    # Relative paths join under the worktree. Absolute paths must still resolve
    # physically under the target worktree (no escape to /tmp or home).
    candidate = part if os.path.isabs(part) else os.path.join(root, part)
    if not under_root(candidate):
        raise SystemExit(
            f"refusing scan root outside target worktree: {part!r} -> {candidate}"
        )
    scan_roots.append(candidate)

# Marker basenames / patterns that recognize candidate evidence without
# requiring candidate.json first.
MARKER_FILES = {
    "candidate.json",
    "HASHES.txt",
    "bundle-manifest.txt",
}
# Never walk into these for clustering (IRIN.app internals are not clusters;
# install/ is a witness tree under a candidate, not a separate candidate).
SKIP_DIR_NAMES = {
    "cargo-home",
    "cargo-target",
    "npm-cache",
    "tmp",
    "node_modules",
    "target",
    "dmg-mount",
    "IRIN.app",
}

# dirpath -> set of marker descriptions found
clusters = {}

def note(dirpath: str, marker: str) -> None:
    ap = os.path.abspath(dirpath)
    if not under_root(ap):
        raise SystemExit(f"scanner escaped worktree at {ap}")
    clusters.setdefault(ap, set()).add(marker)

for scan in scan_roots:
    if not os.path.isdir(scan):
        continue
    if not under_root(scan):
        raise SystemExit(f"scan root escaped worktree: {scan}")
    for dirpath, dirnames, filenames in os.walk(scan, followlinks=False):
        base = os.path.basename(dirpath)
        if base in SKIP_DIR_NAMES:
            dirnames[:] = []
            continue
        # Do not follow symlink children; do not descend into IRIN.app.
        keep = []
        for d in sorted(dirnames):
            full_d = os.path.join(dirpath, d)
            if d in SKIP_DIR_NAMES or os.path.islink(full_d):
                continue
            keep.append(d)
        dirnames[:] = keep

        # install/ under a candidate parent: attach witnesses to the parent so
        # install/bundle-manifest.txt + install/IRIN.app are not a separate cluster.
        under_install = os.path.basename(dirpath) == "install"
        install_parent = os.path.dirname(dirpath) if under_install else ""
        install_belongs_to_parent = under_install and (
            os.path.isfile(os.path.join(install_parent, "candidate.json"))
            or os.path.isfile(os.path.join(install_parent, "HASHES.txt"))
        )

        for name in filenames:
            full = os.path.join(dirpath, name)
            if name in MARKER_FILES:
                if install_belongs_to_parent and name in (
                    "bundle-manifest.txt",
                    "HASHES.txt",
                    "candidate.json",
                ):
                    # Only install/bundle-manifest is expected; still pin to parent.
                    note(install_parent, f"install:{name}")
                else:
                    note(dirpath, name)
            elif name.endswith(".dmg") and os.path.isfile(full):
                note(dirpath, f"dmg:{name}")
            elif name.endswith(".json") and os.path.basename(dirpath) == "proofs":
                parent = os.path.dirname(dirpath)
                note(parent, f"proof:{name}")

        app_path = os.path.join(dirpath, "IRIN.app")
        if os.path.isdir(app_path) or os.path.islink(app_path):
            if install_belongs_to_parent:
                note(install_parent, "install:IRIN.app")
            else:
                note(dirpath, "IRIN.app")

def is_complete(path: str) -> bool:
    try:
        names = os.listdir(path)
    except OSError:
        return False
    return (
        os.path.isfile(os.path.join(path, "candidate.json"))
        and os.path.isfile(os.path.join(path, "HASHES.txt"))
        and os.path.isfile(os.path.join(path, "bundle-manifest.txt"))
        and (
            os.path.isdir(os.path.join(path, "IRIN.app"))
            or os.path.islink(os.path.join(path, "IRIN.app"))
        )
        and sum(
            1
            for n in names
            if n.endswith(".dmg") and os.path.isfile(os.path.join(path, n))
        )
        == 1
    )

# Collapse: any incomplete cluster nested under a complete candidate root is
# install/proof residue of that candidate, not a separate incomplete spill.
complete_roots = [p for p in clusters if is_complete(p)]
for path in list(clusters):
    if is_complete(path):
        continue
    for croot in complete_roots:
        if path == croot or path.startswith(croot + os.sep):
            # Absorb nested markers into the complete parent.
            clusters[croot].update(clusters[path])
            del clusters[path]
            break

# Emit machine-readable lines: STATUS\tPATH\tmarkers...
for path in sorted(clusters):
    markers = ",".join(sorted(clusters[path]))
    status = "complete" if is_complete(path) else "incomplete"
    print(f"{status}\t{path}\t{markers}")
PY
)" || {
  printf 'ERROR: evidence scan failed\n%s\n' "$scan_report" >&2
  exit 1
}

complete_paths=()
incomplete_paths=()
while IFS= read -r line; do
  [[ -n "$line" ]] || continue
  status="${line%%$'\t'*}"
  rest="${line#*$'\t'}"
  path="${rest%%$'\t'*}"
  markers="${rest#*$'\t'}"
  if [[ "$status" == "complete" ]]; then
    complete_paths+=("$path")
  else
    incomplete_paths+=("$path|$markers")
  fi
done <<<"$scan_report"

if (( ${#incomplete_paths[@]} > 0 )); then
  printf 'ERROR: recognized incomplete/legacy candidate evidence under worktree (refusing removal):\n' >&2
  for item in "${incomplete_paths[@]}"; do
    path="${item%%|*}"
    markers="${item#*|}"
    printf '  %s  markers=[%s]\n' "$path" "$markers" >&2
  done
  printf 'Move or delete deliberately, or complete the payload (candidate.json + HASHES + bundle-manifest + IRIN.app + one DMG) before removal.\n' >&2
  exit 1
fi

if (( ${#complete_paths[@]} > 0 )); then
  printf 'Found complete ignored candidate evidence under worktree:\n' >&2
  for p in "${complete_paths[@]}"; do
    printf '  %s\n' "$p" >&2
  done

  for evidence in "${complete_paths[@]}"; do
    # Physical containment re-check before any delete.
    evidence_phys="$(cd "$evidence" && pwd -P)"
    dest_phys="$(cd "$destination" && pwd -P)"
    case "$evidence_phys" in
      "$dest_phys"|"$dest_phys"/*) ;;
      *)
        printf 'ERROR: evidence path escaped target worktree: %s\n' "$evidence" >&2
        exit 1
        ;;
    esac

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
    [[ "$src_sha" =~ ^[0-9a-f]{40}$ ]] || {
      printf 'ERROR: bad source_sha from evidence: %s\n' "$evidence" >&2
      exit 1
    }

    # Payload bytes must match identity (same gate as import-candidate / W2).
    if ! irin_assert_candidate_payload_matches_identity "$evidence" >/dev/null; then
      printf 'ERROR: ignored candidate evidence payload does not match identity: %s\n' \
        "$evidence" >&2
      exit 1
    fi
    if ! irin_payload_tree_hash "$evidence" >/dev/null; then
      printf 'ERROR: ignored candidate evidence failed payload tree hash: %s\n' \
        "$evidence" >&2
      exit 1
    fi

    stage="$IRIN_CANDIDATE_ROOT/.staging/worktree-harvest-$(python3 -c 'import uuid; print(uuid.uuid4())')"
    mkdir -p "$(dirname "$stage")"
    rm -rf "$stage"
    mkdir -p "$stage"
    for name in candidate.json HASHES.txt bundle-manifest.txt IRIN.app proofs install; do
      if [[ -e "$evidence/$name" ]]; then
        cp -a "$evidence/$name" "$stage/$name"
      fi
    done
    for dmg in "$evidence"/*.dmg; do
      [[ -f "$dmg" ]] || continue
      cp -a "$dmg" "$stage/$(basename "$dmg")"
    done
    mkdir -p "$stage/proofs" "$stage/smoke" "$stage/install" "$stage/logs"

    # Re-assert after copy (bit-rot / partial copy refuse).
    irin_assert_candidate_payload_matches_identity "$stage" >/dev/null || {
      printf 'ERROR: harvested staging payload identity check failed: %s\n' "$evidence" >&2
      rm -rf "$stage"
      exit 1
    }

    dest="$(irin_assert_safe_candidate_dest \
      "$IRIN_CANDIDATE_ROOT" "$semver" "$src_sha" "$cid")" || {
      printf 'ERROR: unsafe candidate destination for evidence: %s\n' "$evidence" >&2
      rm -rf "$stage"
      exit 1
    }
    result="$(irin_promote_candidate_from_staging "$stage" "$dest")" || {
      printf 'ERROR: failed to import ignored candidate evidence into store: %s\n' \
        "$evidence" >&2
      exit 1
    }
    printf 'Harvested ignored candidate evidence → %s (%s)\n' "$dest" "$result"
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
