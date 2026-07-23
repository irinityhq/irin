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
created=1
cleanup_failed_creation() {
  status=$?
  trap - EXIT
  [[ -z "${slot_lock:-}" ]] || rmdir "$slot_lock" >/dev/null 2>&1 || true
  if [[ "$status" -ne 0 && "${created:-0}" == "1" ]]; then
    command -v gortex >/dev/null 2>&1 && gortex untrack "$destination" >/dev/null 2>&1 || true
    git -C "$ROOT" worktree remove --force "$destination" >/dev/null 2>&1 || true
    git -C "$ROOT" branch -D "$branch" >/dev/null 2>&1 || true
    printf 'ERROR: worktree setup failed; removed incomplete worktree and branch\n' >&2
  fi
  exit "$status"
}
trap cleanup_failed_creation EXIT

common_git_dir="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir)"
slot_lock="$common_git_dir/irin-worktree-slot.lock"
locked=0
for _ in $(seq 1 100); do
  if mkdir "$slot_lock" 2>/dev/null; then
    locked=1
    break
  fi
  sleep 0.1
done
[[ "$locked" == 1 ]] || {
  printf 'ERROR: timed out waiting for the worktree runtime-slot lock\n' >&2
  exit 1
}

initial_slot="$(( $(printf '%s' "$destination" | cksum | awk '{print $1}') % 1000 ))"
slot_in_use() {
  local candidate="$1" council_port web_port gateway_port line existing env_file existing_port port
  council_port=$((20000 + candidate))
  web_port=$((22000 + candidate))
  gateway_port=$((24000 + candidate))

  while IFS= read -r line; do
    [[ "$line" == worktree\ * ]] || continue
    existing="${line#worktree }"
    env_file="$existing/.irin-worktree.env"
    [[ -f "$env_file" ]] || continue
    existing_port="$(sed -n 's/^IRIN_COUNCIL_PORT=//p' "$env_file" | head -n 1)"
    [[ "$existing_port" != "$council_port" ]] || return 0
  done < <(git -C "$ROOT" worktree list --porcelain)

  if command -v lsof >/dev/null 2>&1; then
    for port in "$council_port" "$web_port" "$gateway_port"; do
      ! lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 || return 0
    done
  fi
  return 1
}

slot=""
for offset in $(seq 0 999); do
  candidate=$(( (initial_slot + offset) % 1000 ))
  if ! slot_in_use "$candidate"; then
    slot="$candidate"
    break
  fi
done
[[ -n "$slot" ]] || {
  printf 'ERROR: no free IRIN worktree runtime slot is available\n' >&2
  exit 1
}
short="$(printf '%03d' "$slot")"
# Share one Cargo target dir across worktrees so the second tree is not a
# cold multi-GB rebuild. Concurrent cargo still serializes via target locks.
cargo_target_dir="${IRIN_CARGO_TARGET_DIR:-${HOME}/.cache/irin/cargo-target}"
mkdir -p "$cargo_target_dir"
# Symlink so path-based tools (Playwright, scripts) see target/release/* while
# cargo still writes into the shared cache via CARGO_TARGET_DIR.
if [[ ! -e "$destination/target" ]]; then
  ln -sfn "$cargo_target_dir" "$destination/target"
fi
cat >"$destination/.irin-worktree.env" <<EOF
IRIN_RUNTIME_PROFILE=worktree
IRIN_COMPOSE_PROJECT=irin-wt-$short
IRIN_COUNCIL_PORT=$((20000 + slot))
IRIN_WEB_PORT=$((22000 + slot))
IRIN_GATEWAY_PORT=$((24000 + slot))
IRIN_RUNTIME_STATE_DIR=${HOME}/.local/state/irin/worktrees/$short-$slug
IRIN_RUNTIME_LAUNCHD_LABEL=com.irinity.irin-runtime.worktree-$short
IRIN_TAILSCALE_SERVE=0
CARGO_TARGET_DIR=$cargo_target_dir
EOF
chmod 600 "$destination/.irin-worktree.env"
rmdir "$slot_lock"
slot_lock=""

# Invoke the creator checkout's methodology so this works before these files
# have landed on origin/main. Managed operator worktrees require Gortex; the
# public contributor fallback remains `make check` in an ordinary checkout.
IRIN_REQUIRE_GORTEX=1 bash "$ROOT/scripts/gortex-worktree.sh" track "$destination"
(cd "$destination" && IRIN_REQUIRE_GORTEX=1 IRIN_METHODOLOGY_ROOT="$ROOT" \
  bash "$ROOT/scripts/dev-preflight.sh")

created=0
trap - EXIT

printf 'Created %s\n' "$destination"
printf 'Branch: %s\n' "$branch"
printf 'Council: http://127.0.0.1:%d\n' "$((20000 + slot))"
printf 'War Room: http://127.0.0.1:%d\n' "$((22000 + slot))"
printf 'Gateway: http://127.0.0.1:%d\n' "$((24000 + slot))"
printf 'Tailscale Serve: disabled for this worktree\n'
printf 'Next: cd %s && make check\n' "$destination"
