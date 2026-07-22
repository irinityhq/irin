#!/usr/bin/env bash
# Create an isolated IRIN development worktree from origin/main.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: run this command from an IRIN Git checkout\n' >&2
  exit 1
}

branch="${1:-}"
[[ -n "$branch" ]] || {
  printf 'usage: %s <branch> [destination]\n' "$0" >&2
  exit 2
}
[[ "$branch" != "main" ]] || {
  printf 'ERROR: development worktrees must not use main\n' >&2
  exit 1
}

slug="$(printf '%s' "$branch" | tr '/[:space:]' '--' | tr -cd '[:alnum:]_.-' | cut -c1-48)"
[[ -n "$slug" ]] || {
  printf 'ERROR: branch does not produce a usable worktree name\n' >&2
  exit 1
}
destination="${2:-$(dirname "$ROOT")/irin-wt-$slug}"
[[ ! -e "$destination" ]] || {
  printf 'ERROR: destination already exists: %s\n' "$destination" >&2
  exit 1
}

git -C "$ROOT" fetch origin main
git -C "$ROOT" worktree add -b "$branch" "$destination" origin/main

slot="$(( $(printf '%s' "$destination" | cksum | awk '{print $1}') % 1000 ))"
short="$(printf '%03d' "$slot")"
cat >"$destination/.irin-worktree.env" <<EOF
IRIN_RUNTIME_PROFILE=worktree
IRIN_COMPOSE_PROJECT=irin-wt-$short
IRIN_COUNCIL_PORT=$((20000 + slot))
IRIN_WEB_PORT=$((22000 + slot))
IRIN_GATEWAY_PORT=$((24000 + slot))
IRIN_RUNTIME_STATE_DIR=${HOME}/.local/state/irin/worktrees/$short
IRIN_RUNTIME_LAUNCHD_LABEL=com.irinity.irin-runtime.worktree-$short
IRIN_TAILSCALE_SERVE=0
EOF
chmod 600 "$destination/.irin-worktree.env"

printf 'Created %s\n' "$destination"
printf 'Branch: %s\n' "$branch"
printf 'Council: http://127.0.0.1:%d\n' "$((20000 + slot))"
printf 'War Room: http://127.0.0.1:%d\n' "$((22000 + slot))"
printf 'Gateway: http://127.0.0.1:%d\n' "$((24000 + slot))"
printf 'Tailscale Serve: disabled for this worktree\n'
