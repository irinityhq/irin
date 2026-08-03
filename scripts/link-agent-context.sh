#!/usr/bin/env bash
# link-agent-context.sh — attach private IRIN agent doctrine into a worktree.
#
# SSOT lives only on the canonical operator checkout as real files. Each linked
# worktree gets exact-name symlinks (never copies, never a second ledger):
#   AGENTS.md
#   CLAUDE.md
#   RTK.md
#
# ProjectMem stays entirely on the canonical checkout and is reached only via
# MCP (--root at that checkout). This script never links or creates .projectmem.
#
# Usage:
#   scripts/link-agent-context.sh                    # link into this checkout
#   scripts/link-agent-context.sh --status
#   scripts/link-agent-context.sh --worktree PATH
#   scripts/link-agent-context.sh --from CANONICAL --worktree PATH
#   scripts/link-agent-context.sh --all-worktrees
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DISCOVER_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DOCTRINE_NAMES=(AGENTS.md CLAUDE.md RTK.md)

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*"; }

MODE="link"
FROM_ROOT=""
TARGET_ROOT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --status) MODE="status"; shift ;;
    --all-worktrees) MODE="all"; shift ;;
    --worktree)
      TARGET_ROOT="${2:-}"
      [[ -n "$TARGET_ROOT" && -d "$TARGET_ROOT" ]] || die "--worktree requires an existing directory"
      TARGET_ROOT="$(cd "$TARGET_ROOT" && pwd)"
      shift 2
      ;;
    --from)
      FROM_ROOT="${2:-}"
      [[ -n "$FROM_ROOT" && -d "$FROM_ROOT" ]] || die "--from requires an existing directory"
      FROM_ROOT="$(cd "$FROM_ROOT" && pwd)"
      shift 2
      ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

is_regular_file() {
  [[ -f "$1" && ! -L "$1" ]]
}

is_exact_symlink_to() {
  local link="$1" expected="$2" current
  [[ -L "$link" ]] || return 1
  current="$(readlink "$link")"
  # Prefer absolute comparison via resolved paths when both resolve.
  if [[ "$current" == "$expected" ]]; then
    return 0
  fi
  if [[ -e "$link" && -e "$expected" ]]; then
    [[ "$(cd "$(dirname "$link")" && cd "$(dirname "$current")" 2>/dev/null && pwd)/$(basename "$current")" == "$expected" ]] \
      && return 0
    [[ "$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$link")" == \
       "$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$expected")" ]] \
      && return 0
  fi
  return 1
}

list_worktree_paths() {
  local root="$1" line
  while IFS= read -r line; do
    case "$line" in
      worktree\ *)
        printf '%s\n' "${line#worktree }"
        ;;
    esac
  done < <(git -C "$root" worktree list --porcelain 2>/dev/null || true)
}

resolve_source_root() {
  if [[ -n "$FROM_ROOT" ]]; then
    printf '%s\n' "$FROM_ROOT"
    return 0
  fi

  local candidate name ok
  # Prefer a worktree that holds real doctrine files (not symlinks).
  while IFS= read -r candidate; do
    [[ -d "$candidate" ]] || continue
    ok=1
    for name in "${DOCTRINE_NAMES[@]}"; do
      if ! is_regular_file "$candidate/$name"; then
        ok=0
        break
      fi
    done
    if [[ "$ok" == 1 ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done < <(list_worktree_paths "$DISCOVER_ROOT")

  # Fallback: script-owning checkout if it holds real doctrine.
  ok=1
  for name in "${DOCTRINE_NAMES[@]}"; do
    if ! is_regular_file "$DISCOVER_ROOT/$name"; then
      ok=0
      break
    fi
  done
  if [[ "$ok" == 1 ]]; then
    printf '%s\n' "$DISCOVER_ROOT"
    return 0
  fi

  return 1
}

require_source() {
  local source="$1" name
  [[ -d "$source" ]] || die "canonical source missing: $source"
  for name in "${DOCTRINE_NAMES[@]}"; do
    is_regular_file "$source/$name" || die "canonical doctrine missing or not a regular file: $source/$name"
  done
  # Health check only — never link the ledger into a worktree.
  [[ -d "$source/.projectmem" && ! -L "$source/.projectmem" ]] \
    || die "canonical ProjectMem ledger missing at $source/.projectmem (initialize only on the canonical checkout)"
  [[ -f "$source/.projectmem/summary.md" ]] \
    || die "canonical ProjectMem ledger is not initialized (missing summary.md)"
}

require_ignored() {
  local dest="$1" name
  for name in "${DOCTRINE_NAMES[@]}"; do
    if ! git -C "$dest" check-ignore -q "$name"; then
      die "private path is not ignored in $dest: $name (keep doctrine out of the public tree via shared exclude)"
    fi
  done
}

# Destination must be an IRIN worktree root of the same monorepo as source.
# Rejects arbitrary Git dirs, subdirectories, and foreign checkouts.
require_same_repo_worktree_root() {
  local dest="$1" source="$2" dest_top source_common dest_common dest_phys top_phys
  git -C "$dest" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    || die "destination is not a Git worktree: $dest"
  dest_top="$(git -C "$dest" rev-parse --show-toplevel)"
  dest_phys="$(cd "$dest" && pwd -P)"
  top_phys="$(cd "$dest_top" && pwd -P)"
  [[ "$dest_phys" == "$top_phys" ]] \
    || die "destination must be a worktree root, not a subdirectory: $dest (toplevel is $dest_top)"
  source_common="$(git -C "$source" rev-parse --path-format=absolute --git-common-dir)"
  dest_common="$(git -C "$dest" rev-parse --path-format=absolute --git-common-dir)"
  [[ "$source_common" == "$dest_common" ]] \
    || die "destination is not a worktree of the same repository as the canonical source"
}

require_no_worktree_projectmem() {
  local dest="$1"
  if [[ -e "$dest/.projectmem" || -L "$dest/.projectmem" ]]; then
    die "refusing to attach while $dest/.projectmem exists (ProjectMem stays canonical-only; remove the worktree-local path)"
  fi
}

status_one() {
  local dest="$1" source="$2" name link
  note "target=$dest"
  note "source=$source"
  for name in "${DOCTRINE_NAMES[@]}"; do
    link="$dest/$name"
    if [[ -L "$link" ]]; then
      note "$name: symlink → $(readlink "$link")"
    elif is_regular_file "$link"; then
      if [[ "$dest" == "$source" ]]; then
        note "$name: canonical regular file"
      else
        note "$name: ERROR real file (refusing to overwrite)"
      fi
    elif [[ -e "$link" ]]; then
      note "$name: ERROR unexpected path type"
    else
      note "$name: ABSENT"
    fi
  done
  if [[ "$dest" == "$source" ]]; then
    note ".projectmem: canonical ledger present (not linked into worktrees)"
  elif [[ -e "$dest/.projectmem" || -L "$dest/.projectmem" ]]; then
    note "WARNING: .projectmem present in target (must remain canonical-only; do not link)"
  else
    note ".projectmem: absent in target (correct)"
  fi
}

link_one() {
  local dest="$1" source="$2" name link target
  local -a pending=()
  [[ -d "$dest" ]] || die "destination missing: $dest"
  dest="$(cd "$dest" && pwd)"
  source="$(cd "$source" && pwd)"

  if [[ "$dest" == "$source" ]]; then
    note "skip link into canonical source: $dest"
    return 0
  fi

  require_source "$source"
  require_same_repo_worktree_root "$dest" "$source"
  require_no_worktree_projectmem "$dest"
  require_ignored "$dest"

  # Phase 1: validate every destination in this shell (not a subshell — die must
  # abort the whole attach). Collect names that still need ln -s. No mutation yet.
  for name in "${DOCTRINE_NAMES[@]}"; do
    link="$dest/$name"
    target="$source/$name"
    if [[ -L "$link" ]]; then
      if is_exact_symlink_to "$link" "$target"; then
        continue
      fi
      die "unexpected symlink at $link → $(readlink "$link") (expected $target)"
    fi
    if [[ -e "$link" ]]; then
      die "refusing to overwrite real path at $link"
    fi
    pending+=("$name")
  done

  if [[ "${#pending[@]}" -eq 0 ]]; then
    for name in "${DOCTRINE_NAMES[@]}"; do
      note "already linked: $dest/$name → $source/$name"
    done
    require_ignored "$dest"
    return 0
  fi

  # Phase 2: create only after every path is known-safe.
  for name in "${pending[@]}"; do
    link="$dest/$name"
    target="$source/$name"
    ln -s "$target" "$link"
    note "linked $link → $target"
  done

  # Fail closed if anything private became trackable dirt.
  require_ignored "$dest"
  # Named private paths must not appear as untracked/tracked dirt.
  local dirty
  dirty="$(git -C "$dest" status --porcelain --untracked-files=normal -- AGENTS.md CLAUDE.md RTK.md .projectmem 2>/dev/null || true)"
  if [[ -n "$dirty" ]]; then
    printf 'ERROR: private doctrine paths dirty after link:\n%s\n' "$dirty" >&2
    exit 1
  fi
  return 0
}

# Run one attach in an isolated shell so die/exit and set -e failures stay local.
# Parent captures status without disabling errexit for the function body (if ! fn).
try_link_one() {
  local dest="$1" source="$2"
  (
    set -euo pipefail
    link_one "$dest" "$source"
  )
}

SOURCE="$(resolve_source_root)" || die "could not resolve canonical doctrine source (pass --from)"
require_source "$SOURCE"

if [[ "$MODE" == "status" ]]; then
  if [[ -n "$TARGET_ROOT" ]]; then
    status_one "$TARGET_ROOT" "$SOURCE"
  else
    status_one "$(cd "$DISCOVER_ROOT" && pwd)" "$SOURCE"
  fi
  exit 0
fi

if [[ "$MODE" == "all" ]]; then
  failures=0
  while IFS= read -r wt; do
    [[ -d "$wt" ]] || continue
    note "--- $wt ---"
    # Subshell isolate: one bad worktree must not abort remaining attaches.
    if try_link_one "$wt" "$SOURCE"; then
      :
    else
      note "FAILED $wt"
      failures=$((failures + 1))
    fi
  done < <(list_worktree_paths "$DISCOVER_ROOT")
  [[ "$failures" -eq 0 ]] || die "link-agent-context failed for $failures worktree(s)"
  exit 0
fi

if [[ -n "$TARGET_ROOT" ]]; then
  link_one "$TARGET_ROOT" "$SOURCE"
else
  link_one "$(cd "$DISCOVER_ROOT" && pwd)" "$SOURCE"
fi
