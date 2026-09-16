#!/usr/bin/env bash
# B-20: `make -C gateway up` from a linked worktree stages a real Git directory
# whose HEAD matches the worktree, and passes that directory as the sidecar
# compose build context. Docker is mocked — this does not build an image.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

[[ -f "$ROOT/.git" ]] || fail "must run from a linked worktree (.git is a file)"
grep -q '^gitdir:' "$ROOT/.git" || fail "must run from a linked worktree (gitdir pointer)"

expected_head="$(git rev-parse HEAD)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-b20-make-up.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/bin"
captured="$tmp/captured"
mkdir -p "$captured"
: >"$tmp/docker.log"

cat >"$tmp/bin/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${DOCKER_LOG:?}"
if [[ "${1:-}" != "compose" ]]; then
  exit 0
fi
overlay=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "-f" && "$arg" != "docker-compose.yml" ]]; then
    overlay="$arg"
  fi
  prev="$arg"
done
[[ -n "$overlay" ]] || { echo "no compose overlay" >"${CAPTURED:?}/error"; exit 1; }
[[ -f "$overlay" ]] || { echo "overlay missing: $overlay" >"$CAPTURED/error"; exit 1; }
cp "$overlay" "$CAPTURED/overlay.yml"
ctx="$(awk '/context:/{print $2; exit}' "$overlay")"
[[ -n "$ctx" ]] || { echo "overlay has no context" >"$CAPTURED/error"; exit 1; }
printf '%s\n' "$ctx" >"$CAPTURED/context"
if [[ -d "$ctx/.git" ]]; then
  echo directory >"$CAPTURED/git_kind"
else
  echo pointer >"$CAPTURED/git_kind"
fi
git -C "$ctx" rev-parse HEAD >"$CAPTURED/head"
exit 0
EOF
chmod +x "$tmp/bin/docker"

PATH="$tmp/bin:$PATH" \
DOCKER_LOG="$tmp/docker.log" \
CAPTURED="$captured" \
IRIN_COMPOSE_LEDGER_KEY="$tmp/compose-ledger-key" \
  make -C "$ROOT/gateway" up

[[ -s "$tmp/docker.log" ]] || fail "docker was not invoked"
grep -q 'compose .*up -d --build' "$tmp/docker.log" \
  || fail "docker compose up -d --build was not invoked: $(cat "$tmp/docker.log")"
grep -q -- '-f docker-compose.yml' "$tmp/docker.log" \
  || fail "compose did not keep docker-compose.yml: $(cat "$tmp/docker.log")"
[[ ! -f "$captured/error" ]] || fail "docker mock: $(cat "$captured/error")"
[[ -s "$captured/context" ]] || fail "sidecar build context was not captured"
ctx="$(cat "$captured/context")"
[[ "$(cat "$captured/git_kind")" == "directory" ]] \
  || fail "staged context .git is not a real directory"
[[ "$(cat "$captured/head")" == "$expected_head" ]] \
  || fail "staged context HEAD $(cat "$captured/head") != worktree $expected_head"
[[ ! -d "$ctx" ]] || fail "staged context was not cleaned up: $ctx"
printf 'ok: make -C gateway up staged worktree HEAD %s\n' "$expected_head"
