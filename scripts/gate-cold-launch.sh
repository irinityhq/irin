#!/usr/bin/env bash
# Cold-launch gate: build the exact native bundle once, then cold-launch it
# IRIN_COLD_LAUNCH_RUNS times (minimum 5). Every launch must pass the full
# native smoke including the visible War Room proof. The first red launch
# fails the gate; there is no rerun path and no way to lower the bar from env.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[[ "$(uname -s)" == Darwin ]] || {
  printf 'ERROR: cold-launch gate requires macOS\n' >&2
  exit 1
}

runs="${IRIN_COLD_LAUNCH_RUNS:-5}"
[[ "$runs" =~ ^[0-9]+$ && "$runs" -ge 5 ]] || {
  printf 'ERROR: cold-launch gate refuses fewer than 5 runs (IRIN_COLD_LAUNCH_RUNS=%s)\n' "$runs" >&2
  exit 1
}
[[ "${IRIN_NATIVE_VISUAL:-1}" == 1 ]] || {
  printf 'ERROR: cold-launch gate cannot run without the visible-window proof\n' >&2
  exit 1
}
[[ "${IRIN_NATIVE_SKIP_BUILD:-0}" == 0 && -z "${IRIN_NATIVE_APP:-}" ]] || {
  printf 'ERROR: cold-launch gate always builds the exact working tree (unset IRIN_NATIVE_SKIP_BUILD / IRIN_NATIVE_APP)\n' >&2
  exit 1
}

if [[ -f "$ROOT/.irin-worktree.env" ]]; then
  set -a
  # Generated worktree routing only; this file contains no operator secrets.
  # shellcheck disable=SC1091
  . "$ROOT/.irin-worktree.env"
  set +a
fi
# The smoke bundle id and CSP embed the Council port at build time, so every
# launch of the same bundle must use the same port. Pin it here.
if [[ -z "${IRIN_COUNCIL_PORT:-}" ]]; then
  IRIN_COUNCIL_PORT="$(python3 -c '
import socket
while True:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    if port != 8765:
        print(port)
        break
')"
fi
export IRIN_COUNCIL_PORT

mkdir -p "$ROOT/.irin-receipts"

# Fingerprint the exact working tree (tracked + untracked, ignores honored).
# A red launch binds to this fingerprint: the same tree is not launched again
# unless the operator states a reason, and that reason lands in the receipt.
tree_index="$(mktemp "${TMPDIR:-/tmp}/irin-cold-launch-index.XXXXXX")"
rm -f "$tree_index"
tree="$(GIT_INDEX_FILE="$tree_index" git add -A . >/dev/null 2>&1 && GIT_INDEX_FILE="$tree_index" git write-tree)"
rm -f "$tree_index"
[[ -n "$tree" ]] || { printf 'ERROR: could not fingerprint the working tree\n' >&2; exit 1; }
# Verdict ledger: one line per gate run, shared by every worktree of this
# repository. The newest verdict for a tree decides. It lives under the Git
# common dir so it survives receipt cleanup and a fresh worktree.
# ponytail: local file; an operator who edits .git can still erase it. Remote
# attestation is the upgrade if that ever matters.
ledger="$(git rev-parse --git-common-dir)/irin-cold-launch-ledger"
last_verdict=""
last_receipt=""
if [[ -f "$ledger" ]]; then
  last_line="$(awk -v t="$tree" '$2 == t { line = $0 } END { print line }' "$ledger")"
  last_verdict="$(printf '%s' "$last_line" | awk '{print $3}')"
  last_receipt="$(printf '%s' "$last_line" | awk '{print $4}')"
fi
rerun_reason="${IRIN_COLD_LAUNCH_RERUN_REASON:-}"
if [[ "$last_verdict" == FAIL && -z "$rerun_reason" ]]; then
  printf 'ERROR: this exact tree already failed the cold-launch gate: %s\n' "$last_receipt" >&2
  printf 'Change the tree (fix the cause) before launching again. If the failure was\n' >&2
  printf 'environmental, set IRIN_COLD_LAUNCH_RERUN_REASON="<reason>"; it is recorded.\n' >&2
  exit 1
fi
[[ "$last_verdict" == FAIL ]] || rerun_reason=""
prior_fail="$last_receipt"
[[ "$last_verdict" == FAIL ]] || prior_fail=""
record_verdict() {
  printf '%s %s %s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$tree" "$1" "$receipt" >>"$ledger"
}

target_dir="$(mktemp -d "${TMPDIR:-/tmp}/irin-cold-launch.XXXXXX")"
trap 'rm -rf "$target_dir"' EXIT INT TERM

receipt="$ROOT/.irin-receipts/cold-launch-$(date '+%Y%m%dT%H%M%S%z').txt"
{
  printf 'IRIN COLD-LAUNCH GATE\n'
  printf 'started=%s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')"
  printf 'head=%s\n' "$(git rev-parse HEAD 2>/dev/null || printf unknown)"
  printf 'tree=%s\n' "$tree"
  printf 'runs_required=%s\n' "$runs"
  printf 'council_port=%s\n' "$IRIN_COUNCIL_PORT"
  [[ -z "$prior_fail" ]] || printf 'rerun_of=%s\nrerun_reason=%s\n' "$prior_fail" "$rerun_reason"
} >"$receipt"
[[ -z "$prior_fail" ]] || printf 'cold-launch gate: RERUN of failed %s (reason: %s)\n' "$prior_fail" "$rerun_reason"

app_binary="$target_dir/release/bundle/macos/IRIN.app/Contents/MacOS/council-warroom-tauri"
bundle_sha=""

passed=0
for (( i = 1; i <= runs; i++ )); do
  skip_build=1
  (( i == 1 )) && skip_build=0
  printf '\n== cold launch %d/%d (build=%s) ==\n' "$i" "$runs" "$(( 1 - skip_build ))"
  # Wait bounds stay at the smoke's defaults; raising them from the environment
  # would hide a slow launch without showing up in any receipt.
  if env -u IRIN_NATIVE_APP \
    -u IRIN_NATIVE_WINDOW_CHECKS -u IRIN_NATIVE_PROCESS_CHECKS -u IRIN_NATIVE_HEALTH_CHECKS \
    IRIN_NATIVE_TARGET_DIR="$target_dir" \
    IRIN_NATIVE_SKIP_BUILD="$skip_build" \
    IRIN_NATIVE_VISUAL=1 \
    bash "$ROOT/scripts/smoke-macos-tauri-app.sh"; then
    # Every launch must run the bundle built on launch 1; refuse a swapped app.
    this_sha="$(shasum -a 256 "$app_binary" 2>/dev/null | awk '{print $1}')"
    if (( i == 1 )); then
      bundle_sha="$this_sha"
      printf 'bundle_sha256=%s\n' "$bundle_sha" >>"$receipt"
    elif [[ -z "$this_sha" || "$this_sha" != "$bundle_sha" ]]; then
      printf 'run=%d result=FAIL reason=bundle-changed\n' "$i" >>"$receipt"
      printf 'status=FAIL passed=%d/%d finished=%s\n' "$passed" "$runs" "$(date '+%Y-%m-%dT%H:%M:%S%z')" >>"$receipt"
      record_verdict FAIL
      printf 'ERROR: app bundle changed between launches (receipt %s)\n' "$receipt" >&2
      exit 1
    fi
    passed=$(( passed + 1 ))
    printf 'run=%d result=PASS\n' "$i" >>"$receipt"
  else
    printf 'run=%d result=FAIL\n' "$i" >>"$receipt"
    printf 'status=FAIL passed=%d/%d finished=%s\n' "$passed" "$runs" "$(date '+%Y-%m-%dT%H:%M:%S%z')" >>"$receipt"
    record_verdict FAIL
    printf '\ncold-launch gate: FAIL on launch %d/%d (%d passed before it)\n' "$i" "$runs" "$passed" >&2
    printf 'A red launch is a red launch. Do not rerun the gate; fix the cause.\n' >&2
    printf 'receipt=%s\n' "$receipt" >&2
    exit 1
  fi
done

printf 'status=PASS passed=%d/%d finished=%s\n' "$passed" "$runs" "$(date '+%Y-%m-%dT%H:%M:%S%z')" >>"$receipt"
record_verdict PASS
printf '\ncold-launch gate: PASS %d/%d\n' "$passed" "$runs"
printf 'receipt=%s\n' "$receipt"
