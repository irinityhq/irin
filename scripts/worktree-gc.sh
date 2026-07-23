#!/usr/bin/env bash
# List or remove clean linked worktrees whose branch is already merged into
# origin/main or whose remote tracking branch is gone. Never touches the
# checkout running this command, main, or dirty trees.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run from an IRIN checkout\n' >&2
  exit 1
}
cd "$ROOT"
METHODOLOGY_ROOT="${IRIN_METHODOLOGY_ROOT:-$ROOT}"

apply=0
if [[ "${1:-}" == "--apply" ]]; then
  apply=1
elif [[ -n "${1:-}" ]]; then
  printf 'usage: %s [--apply]\n' "$0" >&2
  exit 2
fi

git fetch origin main >/dev/null 2>&1 || true
origin_main="$(git rev-parse origin/main 2>/dev/null || true)"
[[ -n "$origin_main" ]] || {
  printf 'ERROR: origin/main is required\n' >&2
  exit 1
}

self="$(cd "$ROOT" && pwd -P)"
candidates=0
removed=0

while IFS= read -r line; do
  [[ "$line" == worktree\ * ]] || continue
  dest="${line#worktree }"
  [[ -d "$dest" ]] || continue
  resolved="$(cd "$dest" && pwd -P)"
  [[ "$resolved" != "$self" ]] || continue

  branch="$(git -C "$dest" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
  [[ -n "$branch" && "$branch" != "main" && "$branch" != "master" ]] || continue

  if [[ -n "$(git -C "$dest" status --porcelain --untracked-files=normal)" ]]; then
    printf 'SKIP dirty: %s (%s)\n' "$resolved" "$branch"
    continue
  fi

  merged=0
  if git merge-base --is-ancestor "$(git -C "$dest" rev-parse HEAD)" origin/main 2>/dev/null; then
    merged=1
  fi
  # Squash merges leave local tips that are not ancestors of origin/main.
  # A deleted origin/<branch> after merge is the usual operator signal.
  remote_gone=0
  if ! git show-ref --verify --quiet "refs/remotes/origin/$branch" 2>/dev/null \
    && ! git ls-remote --exit-code --heads origin "$branch" >/dev/null 2>&1; then
    remote_gone=1
  fi

  if [[ "$merged" != 1 && "$remote_gone" != 1 ]]; then
    printf 'KEEP active: %s (%s)\n' "$resolved" "$branch"
    continue
  fi

  reason="merged-into-origin/main"
  [[ "$merged" == 1 ]] || reason="origin-branch-gone"
  candidates=$((candidates + 1))
  printf 'CANDIDATE [%s]: %s (%s)\n' "$reason" "$resolved" "$branch"
  if [[ "$apply" == 1 ]]; then
    bash "$METHODOLOGY_ROOT/scripts/remove-worktree.sh" "$resolved"
    removed=$((removed + 1))
  fi
done < <(git worktree list --porcelain)

if [[ "$apply" == 1 ]]; then
  printf 'worktree-gc: removed %s of %s candidate(s)\n' "$removed" "$candidates"
else
  printf 'worktree-gc: %s candidate(s). Re-run with --apply to remove clean merged trees.\n' \
    "$candidates"
fi
