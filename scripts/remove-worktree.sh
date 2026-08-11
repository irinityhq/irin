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
# Also harvest worktree-local ship-check receipts (`.irin-receipts/ship-*.txt`)
# into the invoking checkout's `.irin-receipts/` (operator path: canonical
# checkout) so source-proof history survives teardown:
#   - destination absent → copy exact bytes
#   - destination identical → continue
#   - same name, different content → refuse (no overwrite, no second hierarchy)
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
        # Top-level IRIN.app marker: real dir only for "complete"; still note
        # install/IRIN.app (witness) or a loose app dir for incomplete refuse.
        if os.path.isdir(app_path) and not os.path.islink(app_path):
            if install_belongs_to_parent:
                note(install_parent, "install:IRIN.app")
            else:
                note(dirpath, "IRIN.app")
        elif os.path.islink(app_path) or (
            os.path.lexists(app_path) and not os.path.isdir(app_path)
        ):
            # Symlink / non-dir app is recognized incomplete residue (refuse).
            if install_belongs_to_parent:
                note(install_parent, "install:IRIN.app-symlink")
            else:
                note(dirpath, "IRIN.app-symlink")

def is_complete(path: str) -> bool:
    try:
        names = os.listdir(path)
    except OSError:
        return False
    app = os.path.join(path, "IRIN.app")
    # Complete requires a real non-symlink IRIN.app directory (store law).
    if os.path.islink(app) or not os.path.isdir(app):
        return False
    return (
        os.path.isfile(os.path.join(path, "candidate.json"))
        and os.path.isfile(os.path.join(path, "HASHES.txt"))
        and os.path.isfile(os.path.join(path, "bundle-manifest.txt"))
        and sum(
            1
            for n in names
            if n.endswith(".dmg") and os.path.isfile(os.path.join(path, n))
        )
        == 1
    )

# No blanket collapse of nested incomplete clusters. Install witnesses are
# attached to the parent during scan (install_belongs_to_parent). Any other
# nested incomplete residue (e.g. legacy HASHES under a spill) remains its own
# incomplete cluster and blocks removal.

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

# --- ship-check receipt harvest (source-proof history) -----------------------
# Worktree-local ship-*.txt would vanish with the worktree. Copy into the
# invoking checkout's .irin-receipts/ (expected: canonical operator checkout).
# Publication is exclusive (temp file + hard-link): no TOCTOU overwrite.
# Identical destination continues; different content or exclusive-create race
# that leaves non-identical dest refuses (exit 1). No second hierarchy.
src_receipts="$destination/.irin-receipts"
if [[ -d "$src_receipts" ]]; then
  shopt -s nullglob
  ship_sources=("$src_receipts"/ship-*.txt)
  shopt -u nullglob
  if (( ${#ship_sources[@]} > 0 )); then
    dest_receipts="$SOURCE_ROOT/.irin-receipts"
    # Destination root must be a real directory under the invoking checkout —
    # never a symlink (which could land receipts outside the storage root).
    if [[ -L "$dest_receipts" ]]; then
      printf 'ERROR: refusing symlinked ship receipt root: %s\n' "$dest_receipts" >&2
      exit 1
    fi
    if [[ -e "$dest_receipts" && ! -d "$dest_receipts" ]]; then
      printf 'ERROR: refusing non-directory ship receipt root: %s\n' "$dest_receipts" >&2
      exit 1
    fi
    mkdir -p "$dest_receipts" || {
      printf 'ERROR: failed to create ship receipt root: %s\n' "$dest_receipts" >&2
      exit 1
    }
    if [[ -L "$dest_receipts" || ! -d "$dest_receipts" ]]; then
      printf 'ERROR: ship receipt root must be a real directory: %s\n' "$dest_receipts" >&2
      exit 1
    fi

    # Pin the destination root: enter it once (a held cwd is a kernel handle),
    # verify the physical path, then stage/link with cwd-relative names so a
    # later symlink swap of the path cannot redirect staging or publication.
    source_root_phys="$(cd "$SOURCE_ROOT" && pwd -P)"
    (
      cd "$dest_receipts" || {
        printf 'ERROR: cannot enter ship receipt root: %s\n' "$dest_receipts" >&2
        exit 1
      }
      [[ "$(pwd -P)" == "$source_root_phys/.irin-receipts" ]] || {
        printf 'ERROR: ship receipt root resolved outside the invoking checkout: %s\n' "$(pwd -P)" >&2
        exit 1
      }
      # Physical containment: only harvest regular files under the worktree tree.
      wt_phys="$(cd "$destination" && pwd -P)"
      for src in "${ship_sources[@]}"; do
        [[ -f "$src" && ! -L "$src" ]] || {
          printf 'ERROR: refusing non-regular ship receipt: %s\n' "$src" >&2
          exit 1
        }
        src_phys="$(cd "$(dirname "$src")" && pwd -P)/$(basename "$src")"
        case "$src_phys" in
          "$wt_phys"/*) ;;
          *)
            printf 'ERROR: ship receipt escaped target worktree: %s\n' "$src" >&2
            exit 1
            ;;
        esac
        base="$(basename "$src")"
        # Basename must stay ship-*.txt (no path separators / traversal).
        [[ "$base" == ship-*.txt && "$base" != *'/'* && "$base" != *'\\'* ]] || {
          printf 'ERROR: unexpected ship receipt name: %s\n' "$base" >&2
          exit 1
        }
        dest="./$base"
        shown="$dest_receipts/$base"
        # Fast path: existing identical → continue; different/non-file → refuse.
        if [[ -e "$dest" || -L "$dest" ]]; then
          if [[ -f "$dest" && ! -L "$dest" ]] && cmp -s "$src" "$dest"; then
            printf 'Ship receipt already present (identical): %s\n' "$shown"
            continue
          fi
          printf 'ERROR: refusing ship receipt overwrite (same name, different content):\n' >&2
          printf '  worktree: %s\n' "$src" >&2
          printf '  existing: %s\n' "$shown" >&2
          printf 'Move or reconcile deliberately; remove will not overwrite or create a second hierarchy.\n' >&2
          exit 1
        fi
        # Exclusive publish: stage in the held dest root, hard-link into place
        # (fails if dest appears between check and ln), then drop the temp name.
        stage="$(mktemp "./.ship-harvest.XXXXXX")" || {
          printf 'ERROR: failed to stage ship receipt: %s\n' "$src" >&2
          exit 1
        }
        if ! cp -a "$src" "$stage"; then
          rm -f "$stage"
          printf 'ERROR: failed to harvest ship receipt: %s\n' "$src" >&2
          exit 1
        fi
        # A source swapped to a symlink after its check stages as a symlink;
        # the staged object must still be a regular file.
        [[ -f "$stage" && ! -L "$stage" ]] || {
          rm -f "$stage"
          printf 'ERROR: staged ship receipt is not a regular file: %s\n' "$src" >&2
          exit 1
        }
        if ! cmp -s "$src" "$stage"; then
          rm -f "$stage"
          printf 'ERROR: staged ship receipt bytes mismatch: %s\n' "$src" >&2
          exit 1
        fi
        if ln "$stage" "$dest" 2>/dev/null; then
          rm -f "$stage"
          if [[ ! -f "$dest" || -L "$dest" ]] || ! cmp -s "$src" "$dest"; then
            printf 'ERROR: harvested ship receipt bytes mismatch: %s\n' "$shown" >&2
            exit 1
          fi
          printf 'Harvested ship receipt → %s\n' "$shown"
          continue
        fi
        # Exclusive create failed: dest now exists (or cannot be linked). Accept
        # only when the existing file is a regular file with identical bytes.
        rm -f "$stage"
        if [[ ! -e "$dest" && ! -L "$dest" ]]; then
          # Link failed with no destination present (permissions, hard links
          # unavailable): that is a publication failure, not a collision.
          printf 'ERROR: failed to publish ship receipt exclusively: %s\n' "$shown" >&2
          exit 1
        fi
        if [[ -f "$dest" && ! -L "$dest" ]] && cmp -s "$src" "$dest"; then
          printf 'Ship receipt already present (identical): %s\n' "$shown"
          continue
        fi
        printf 'ERROR: refusing ship receipt overwrite (same name, different content):\n' >&2
        printf '  worktree: %s\n' "$src" >&2
        printf '  existing: %s\n' "$shown" >&2
        printf 'Move or reconcile deliberately; remove will not overwrite or create a second hierarchy.\n' >&2
        exit 1
      done
    )
  fi
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
