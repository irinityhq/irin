#!/usr/bin/env bash
# Advisory PR check for changes that carry signing, wire-shape, or CI authority.
# A missing declaration emits a GitHub warning but intentionally exits zero.
set -euo pipefail

FILES_FILE=""
BODY_ENV=""
SELF_TEST=0

usage() {
  printf 'usage: %s --files-file PATH --body-env ENV_VAR\n' "$0" >&2
  printf '       %s --self-test\n' "$0" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --files-file)
      FILES_FILE="${2:-}"
      shift 2
      ;;
    --body-env)
      BODY_ENV="${2:-}"
      shift 2
      ;;
    --self-test)
      SELF_TEST=1
      shift
      ;;
    *) usage ;;
  esac
done

is_contract_surface() {
  local path="$1"
  case "$path" in
    .github/workflows/*|\
    sentinel/COMMS_CONTRACT.md|\
    sentinel/sovereign-protocol/*|\
    */comms/*|*/comms*.rs|\
    gateway/sidecar-rs/src/keymgmt.rs|\
    gateway/sidecar-rs/src/ledger.rs|\
    gateway/sidecar-rs/src/watch/dispatcher.rs|\
    gateway/sidecar-rs/src/watch/worker.rs|\
    */*escalation*|*/directive.rs|*/directive_fence.rs|\
    */*outbox*|*capability*token*|*directive*envelope*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

has_checked_declaration() {
  local body="$1"
  printf '%s\n' "$body" \
    | grep -Eiq '^[[:space:]]*-[[:space:]]*\[[xX]\][[:space:]]+Contract-surface review:[[:space:]]+(not applicable|required)([.[:space:]]|$)'
}

run_self_test() {
  is_contract_surface '.github/workflows/ci.yml'
  is_contract_surface 'sentinel/sovereign-protocol/src/directive.rs'
  is_contract_surface 'gateway/sidecar-rs/src/keymgmt.rs'
  is_contract_surface 'gateway/sidecar-rs/src/ledger.rs'
  is_contract_surface 'gateway/sidecar-rs/src/watch/dispatcher.rs'
  is_contract_surface 'gateway/sidecar-rs/src/watch/outbox.rs'
  is_contract_surface 'gateway/sidecar-rs/src/watch/worker.rs'
  ! is_contract_surface 'README.md'
  has_checked_declaration '- [x] Contract-surface review: required — wire change.'
  has_checked_declaration '- [X] Contract-surface review: not applicable.'
  ! has_checked_declaration '- [ ] Contract-surface review: required — explain.'
  printf 'contract-surface declaration self-test: PASS\n'
}

if [[ "$SELF_TEST" -eq 1 ]]; then
  run_self_test
  exit 0
fi

[[ -n "$FILES_FILE" && -f "$FILES_FILE" && -n "$BODY_ENV" ]] || usage

sensitive=()
while IFS= read -r path; do
  [[ -n "$path" ]] || continue
  if is_contract_surface "$path"; then
    sensitive+=("$path")
  fi
done < "$FILES_FILE"

if [[ "${#sensitive[@]}" -eq 0 ]]; then
  printf 'contract-surface declaration: not required\n'
  exit 0
fi

body="${!BODY_ENV-}"
if has_checked_declaration "$body"; then
  printf 'contract-surface declaration: present (%s sensitive path(s))\n' "${#sensitive[@]}"
  exit 0
fi

printf '::warning title=Contract-surface review declaration::Sensitive paths changed, but neither Contract-surface review box is checked in the PR body. This check is advisory.\n'
printf 'Sensitive paths:\n'
printf '  %s\n' "${sensitive[@]}"
exit 0
