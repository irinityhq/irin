#!/usr/bin/env bash
# Fast, deterministic entry gate for an IRIN development worktree.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: preflight must run inside an IRIN Git worktree\n' >&2
  exit 1
}
cd "$ROOT"
METHODOLOGY_ROOT="${IRIN_METHODOLOGY_ROOT:-$ROOT}"

mode="${1:-start}"
[[ "$mode" == "start" || "$mode" == "ship" ]] || {
  printf 'usage: %s [start|ship]\n' "$0" >&2
  exit 2
}

if [[ "$mode" == "ship" || -f "$ROOT/.irin-worktree.env" ]]; then
  export IRIN_REQUIRE_GORTEX=1
fi

branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
[[ -n "$branch" ]] || { printf 'ERROR: detached worktrees cannot ship IRIN changes\n' >&2; exit 1; }
[[ "$branch" != "main" && "$branch" != "master" ]] || {
  printf 'ERROR: development and shipping checks must run on a feature branch\n' >&2
  exit 1
}

if [[ "$mode" == "start" && -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  printf 'ERROR: start preflight requires a clean worktree\n' >&2
  git status --short >&2
  exit 1
fi

if [[ "${IRIN_PREFLIGHT_SKIP_FETCH:-0}" != "1" ]]; then
  git fetch origin main
fi
git rev-parse --verify origin/main >/dev/null

head_sha="$(git rev-parse HEAD)"
origin_sha="$(git rev-parse origin/main)"
merge_base="$(git merge-base HEAD origin/main)"
ahead="$(git rev-list --count origin/main..HEAD)"
behind="$(git rev-list --count HEAD..origin/main)"
local_main="$(git rev-parse main 2>/dev/null || true)"

printf 'Branch: %s\n' "$branch"
printf 'HEAD: %s\n' "$head_sha"
printf 'origin/main: %s\n' "$origin_sha"
printf 'merge-base: %s (ahead=%s behind=%s)\n' "$merge_base" "$ahead" "$behind"
if [[ -n "$local_main" && "$local_main" != "$origin_sha" ]]; then
  printf 'WARNING: local main differs from origin/main; do not park shipping work on local main\n'
fi

base_file="$ROOT/.irin-worktree-base"
if [[ "$mode" == "start" ]]; then
  printf '%s\n' "$origin_sha" >"$base_file"
else
  [[ -f "$base_file" ]] || {
    printf 'ERROR: no recorded preflight base; run make preflight before editing\n' >&2
    exit 1
  }
  recorded="$(tr -d '[:space:]' <"$base_file")"
  if [[ "$recorded" != "$origin_sha" ]] || ! git merge-base --is-ancestor origin/main HEAD; then
    printf 'ERROR: ship receipt is stale because origin/main moved\n' >&2
    printf 'Recorded: %s\nCurrent:  %s\n' "$recorded" "$origin_sha" >&2
    printf 'Update this branch from origin/main, rerun make preflight, then rerun make ship-check.\n' >&2
    exit 1
  fi
fi

if [[ -f "$ROOT/.irin-worktree.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  . "$ROOT/.irin-worktree.env"
  set +a
  printf 'Council: http://127.0.0.1:%s\n' "${IRIN_COUNCIL_PORT:?}"
  printf 'War Room: http://127.0.0.1:%s\n' "${IRIN_WEB_PORT:?}"
  printf 'Gateway: http://127.0.0.1:%s\n' "${IRIN_GATEWAY_PORT:?}"
else
  printf 'Runtime: canonical ports (no .irin-worktree.env)\n'
fi

"$METHODOLOGY_ROOT/scripts/gortex-worktree.sh" doctor "$ROOT"

if [[ "$mode" == "start" ]]; then
  printf 'Next: edit in this worktree, run make check while iterating, and make ship-check before claiming done.\n'
fi
