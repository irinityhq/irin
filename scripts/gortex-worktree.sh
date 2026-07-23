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
  local record="" fresh="false" expected
  expected="$(git -C "$path" rev-parse HEAD)"
  for _ in $(seq 1 "${IRIN_GORTEX_INDEX_CHECKS:-60}"); do
    record="$(repo_record 2>/dev/null || true)"
    if [[ -n "$record" ]]; then
      fresh="$(python3 -c '
import json, sys
repo = json.loads(sys.argv[1])
expected = sys.argv[2]
fresh = (
    repo.get("stale", True) is False
    and repo.get("indexed", False) is True
    and repo.get("head_commit") == expected
    and repo.get("indexed_commit") == expected
)
print(str(fresh).lower())
' "$record" "$expected")"
      if [[ "$fresh" == "true" ]]; then
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

refresh_index() {
  # Register (or re-register) the exact worktree, then poll repos --json for a
  # fresh index. Prefer track without --wait: some daemon builds fail
  # index_repository under --wait even though ordinary track + background
  # indexing still produces a usable graph. Source and runtime state are
  # never touched here.
  if repo_record >/dev/null 2>&1; then
    if ! gortex untrack "$path" >/dev/null; then
      printf 'ERROR: Gortex failed to unregister stale graph for %s\n' "$path" >&2
      return 1
    fi
  fi
  if ! gortex track "$path" --as-worktree >/dev/null; then
    printf 'ERROR: Gortex failed to track exact worktree %s\n' "$path" >&2
    return 1
  fi
  # Best-effort explicit wait for daemons that support it; ignore tool gaps.
  gortex track "$path" --as-worktree --wait \
    --wait-timeout "${IRIN_GORTEX_TRACK_TIMEOUT:-2m}" >/dev/null 2>&1 || true
  if ! wait_for_fresh_index; then
    return 1
  fi
}

require_fresh_index() {
  local record fresh expected
  record="$(repo_record 2>/dev/null)" || {
    printf 'ERROR: Gortex does not track exact worktree %s\n' "$path" >&2
    return 1
  }
  expected="$(git -C "$path" rev-parse HEAD)"
  fresh="$(python3 -c '
import json, sys
repo = json.loads(sys.argv[1])
expected = sys.argv[2]
fresh = (
    repo.get("stale", True) is False
    and repo.get("indexed", False) is True
    and repo.get("head_commit") == expected
    and repo.get("indexed_commit") == expected
)
print(str(fresh).lower())
' "$record" "$expected")"
  if [[ "$fresh" != "true" ]]; then
    printf 'Gortex index: STALE OR COMMIT-MISMATCHED; refreshing exact worktree before continuing\n' >&2
    if ! record="$(refresh_index)"; then
      printf 'ERROR: Gortex failed to refresh exact worktree %s\n' "$path" >&2
      return 1
    fi
  fi
  printf '%s\n' "$record"
}

case "$action" in
  track)
    if ! have_gortex; then without_gortex; exit $?; fi
    if ! record="$(refresh_index)"; then
      printf 'ERROR: Gortex failed to track exact worktree %s\n' "$path" >&2
      exit 1
    fi
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
    record="$(require_fresh_index)"
    printf 'Gortex index: FRESH %s\n' "$record"
    ;;

  detect)
    if ! have_gortex; then without_gortex; exit $?; fi
    # Agents use MCP detect_changes first. This is the named continuity path
    # when the client reports that the MCP tool is not mounted/discoverable.
    record="$(require_fresh_index)"
    printf 'Gortex index: FRESH %s\n' "$record"
    printf 'Gortex CLI continuity: detect_changes (MCP must be attempted first by the agent)\n'
    python3 - "$path" "${IRIN_GORTEX_DETECT_TIMEOUT:-180}" <<'PY'
import json
import subprocess
import sys
import time

path = sys.argv[1]
timeout = int(sys.argv[2])
command = [
    "gortex",
    "call",
    "detect_changes",
    "--index",
    path,
    "--arg",
    "scope=all",
    "--format",
    "json",
]
deadline = time.monotonic() + timeout
while True:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        print(
            f"ERROR: Gortex detect_changes did not produce a complete result "
            f"within {timeout}s",
            file=sys.stderr,
        )
        raise SystemExit(124)
    try:
        completed = subprocess.run(
            command,
            capture_output=True,
            check=True,
            text=True,
            timeout=remaining,
        )
    except subprocess.TimeoutExpired:
        print(
            f"ERROR: Gortex detect_changes exceeded {timeout}s; "
            "the daemon remains required, but this check will not hang indefinitely",
            file=sys.stderr,
        )
        raise SystemExit(124)
    except subprocess.CalledProcessError as error:
        sys.stdout.write(error.stdout or "")
        sys.stderr.write(error.stderr or "")
        raise SystemExit(error.returncode)

    result = json.loads(completed.stdout)
    warming = result.get("warming") or {}
    if not warming.get("partial_results", False):
        print(json.dumps(result, indent=2, sort_keys=True))
        break
    print(
        "Gortex detect_changes: partial graph "
        f"({warming.get('percent', '?')}%); waiting for a complete result",
        file=sys.stderr,
    )
    time.sleep(min(2, max(0, deadline - time.monotonic())))
PY
    ;;

  affected)
    if ! have_gortex; then without_gortex; exit $?; fi
    record="$(require_fresh_index)"
    printf 'Gortex index: FRESH %s\n' "$record"
    printf 'Gortex CLI continuity: affected-test analysis\n'
    set +e
    if (( $# > 2 )); then
      affected_output="$(gortex affected --index "$path" --json "${@:3}" 2>&1)"
      affected_status=$?
    else
      affected_output="$(gortex affected --index "$path" --json --stdin 2>&1)"
      affected_status=$?
    fi
    set -e
    case "$affected_status" in
      0)
        python3 -c '
import json, sys
payload = json.loads(sys.argv[1])
tests = payload.get("affected_tests")
if not isinstance(tests, list) or not tests:
    raise SystemExit(
        "ERROR: Gortex returned exit 0 without a non-empty affected_tests list"
    )
' "$affected_output"
        printf 'Gortex affected tests: SELECTED (advisory; classifier lanes execute the proof)\n%s\n' \
          "$affected_output"
        ;;
      3)
        printf 'Gortex affected tests: NONE\n'
        [[ -z "$affected_output" ]] || printf '%s\n' "$affected_output"
        ;;
      *)
        printf 'ERROR: Gortex affected-test analysis failed (exit=%s)\n' \
          "$affected_status" >&2
        [[ -z "$affected_output" ]] || printf '%s\n' "$affected_output" >&2
        exit "$affected_status"
        ;;
    esac
    ;;

  *)
    printf 'usage: %s {doctor|track|untrack|detect|affected} [worktree-path] [changed-path ...]\n' "$0" >&2
    exit 2
    ;;
esac
