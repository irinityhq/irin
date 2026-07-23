#!/usr/bin/env bash
# Keep Gortex bound to the exact linked worktree and provide the CLI continuity
# path when an agent client fails to mount the always-on MCP service.
set -euo pipefail

action="${1:-doctor}"
path="${2:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
path="$(cd "$path" && pwd -P)"

have_gortex() {
  command -v gortex >/dev/null 2>&1
}

without_gortex() {
  if [[ "${IRIN_REQUIRE_GORTEX:-0}" == "1" ]]; then
    printf 'ERROR: Gortex is required for this operator workflow but is not on PATH\n' >&2
    return 1
  fi
  printf 'Gortex: UNAVAILABLE (public fallback: source reads + selected tests)\n'
}

repo_record() {
  gortex repos --json | python3 -c '
import json, os, sys
target = os.path.realpath(sys.argv[1])
for repo in json.load(sys.stdin):
    if os.path.realpath(repo.get("path", "")) == target:
        print(json.dumps(repo, separators=(",", ":")))
        raise SystemExit(0)
raise SystemExit(3)
' "$path"
}

wait_for_fresh_index() {
  local record="" stale="true"
  for _ in $(seq 1 "${IRIN_GORTEX_INDEX_CHECKS:-60}"); do
    record="$(repo_record 2>/dev/null || true)"
    if [[ -n "$record" ]]; then
      stale="$(python3 -c 'import json,sys; print(str(json.loads(sys.argv[1]).get("stale", True)).lower())' "$record")"
      if [[ "$stale" == "false" ]]; then
        printf '%s\n' "$record"
        return 0
      fi
    fi
    sleep "${IRIN_GORTEX_INDEX_DELAY:-0.5}"
  done
  printf 'ERROR: Gortex did not produce a fresh index for %s\n' "$path" >&2
  [[ -z "$record" ]] || printf 'last record: %s\n' "$record" >&2
  return 1
}

case "$action" in
  track)
    if ! have_gortex; then without_gortex; exit $?; fi
    if ! repo_record >/dev/null 2>&1; then
      gortex track "$path" --as-worktree
      gortex daemon reload >/dev/null
    fi
    gortex call index_repository --index "$path" --arg "path=$path" --format text >/dev/null
    record="$(wait_for_fresh_index)"
    printf 'Gortex: TRACKED %s\n' "$path"
    printf 'Gortex index: %s\n' "$record"
    ;;

  untrack)
    if ! have_gortex; then without_gortex; exit $?; fi
    if repo_record >/dev/null 2>&1; then
      gortex untrack "$path"
      gortex daemon reload >/dev/null
      printf 'Gortex: UNTRACKED %s\n' "$path"
    else
      printf 'Gortex: already untracked %s\n' "$path"
    fi
    ;;

  doctor)
    if ! have_gortex; then without_gortex; exit $?; fi
    doctor="$(gortex init doctor --json)"
    python3 -c '
import json, sys
report = json.loads(sys.stdin.read())
env = report.get("environment", {})
if not env.get("daemon_running"):
    raise SystemExit("Gortex daemon is not running")
configured = {a.get("name"): a.get("configured") for a in report.get("agents", [])}
print("Gortex daemon: ready")
for name in ("codex", "claude-code", "cursor"):
    state = "configured" if configured.get(name) else "not configured for this project/client"
    print(f"Gortex {name}: {state}")
' <<<"$doctor"
    if ! repo_record >/dev/null 2>&1; then
      printf 'ERROR: Gortex is running but this exact worktree is not tracked\n' >&2
      printf 'Run: scripts/gortex-worktree.sh track %q\n' "$path" >&2
      exit 1
    fi
    record="$(repo_record)"
    stale="$(python3 -c 'import json,sys; print(str(json.loads(sys.argv[1]).get("stale", True)).lower())' "$record")"
    if [[ "$stale" == "true" ]]; then
      printf 'Gortex index: STALE; refreshing exact worktree before continuing\n'
      gortex call index_repository --index "$path" --arg "path=$path" --format text >/dev/null
      record="$(wait_for_fresh_index)"
    fi
    printf 'Gortex index: FRESH %s\n' "$record"
    ;;

  detect)
    if ! have_gortex; then without_gortex; exit $?; fi
    # Agents use MCP detect_changes first. This is the named continuity path
    # when the client reports that the MCP tool is not mounted/discoverable.
    repo_record >/dev/null 2>&1 || {
      printf 'ERROR: cannot run Gortex detect_changes: exact worktree is untracked\n' >&2
      exit 1
    }
    printf 'Gortex CLI continuity: detect_changes (MCP must be attempted first by the agent)\n'
    gortex call detect_changes --index "$path" --arg scope=all --format text
    ;;

  *)
    printf 'usage: %s {doctor|track|untrack|detect} [worktree-path]\n' "$0" >&2
    exit 2
    ;;
esac
