#!/usr/bin/env bash
# Recoverably tear down a clean IRIN linked worktree; retain its branch.
set -euo pipefail

SOURCE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run from an IRIN checkout\n' >&2
  exit 1
}
destination="${1:-}"
[[ -n "$destination" ]] || { printf 'usage: %s /absolute/worktree/path\n' "$0" >&2; exit 2; }
[[ "$destination" == /* ]] || { printf 'ERROR: worktree path must be absolute\n' >&2; exit 1; }
[[ -d "$destination" ]] || { printf 'ERROR: worktree does not exist: %s\n' "$destination" >&2; exit 1; }
destination="$(cd "$destination" && pwd -P)"
[[ "$destination" != "$(cd "$SOURCE_ROOT" && pwd -P)" ]] || {
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

runtime_state_dir=""
if [[ -f "$destination/.irin-worktree.env" ]]; then
  runtime_state_dir="$(sed -n 's/^IRIN_RUNTIME_STATE_DIR=//p' "$destination/.irin-worktree.env" | head -n 1)"
  export IRIN_REQUIRE_GORTEX=1
fi
if [[ -n "$runtime_state_dir" ]]; then
  allowed_prefix="${HOME}/.local/state/irin/worktrees/"
  [[ "$runtime_state_dir" == "$allowed_prefix"* && "$runtime_state_dir" != "$allowed_prefix" ]] || {
    printf 'ERROR: refusing unexpected runtime state path: %s\n' "$runtime_state_dir" >&2
    exit 1
  }
fi

make -s -C "$destination" runtime-down >/dev/null 2>&1 || true
if [[ "${IRIN_REQUIRE_GORTEX:-0}" == 1 ]]; then
  command -v gortex >/dev/null 2>&1 || {
    printf 'ERROR: Gortex CLI is required to remove a managed worktree cleanly\n' >&2
    exit 1
  }
fi
if command -v gortex >/dev/null 2>&1; then
  # Untrack while the path still exists. If Git refuses removal, restore the
  # index registration so the failed teardown leaves no hidden state change.
  gortex untrack "$destination" >/dev/null
  if ! git -C "$SOURCE_ROOT" worktree remove "$destination"; then
    gortex track "$destination" --as-worktree >/dev/null 2>&1 || true
    gortex daemon reload >/dev/null 2>&1 || true
    exit 1
  fi
  gortex daemon reload >/dev/null 2>&1 || true
else
  git -C "$SOURCE_ROOT" worktree remove "$destination"
fi
if [[ -n "$runtime_state_dir" && -d "$runtime_state_dir" ]]; then
  rm -rf -- "$runtime_state_dir"
  printf 'Removed generated runtime state: %s\n' "$runtime_state_dir"
fi
printf 'Removed worktree: %s\n' "$destination"
printf 'Retained branch: %s\n' "$branch"
