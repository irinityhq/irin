#!/usr/bin/env bash
# Public PR language gate.
#
# Scans PR title/body and git *commit messages* only — never the tree.
# Hard-fails high-confidence process/metadata leak shapes. Soft process
# narrative (seat names, roadmap codes, tool product names in prose) is
# deliberately out of scope; see CONTRIBUTING.md "Public PR language".
#
# Usage:
#   scripts/check-public-pr-language.sh --range BASE..HEAD
#   scripts/check-public-pr-language.sh --range BASE..HEAD --title '…' --body-file PATH
#   scripts/check-public-pr-language.sh --range BASE..HEAD --title '…' --body-env VAR
#   scripts/check-public-pr-language.sh --self-test
#
# Exit 0 = clean, 1 = leak found or usage error.

set -euo pipefail

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

# Specs: name|flags|ERE
# flags: empty = case-sensitive grep -E; "i" = grep -iE (trailer casing variants).
# Keep this list narrow: false positives train people to bypass.
PATTERN_SPECS=(
  'session-trailer||Claude-Session:'
  'session-url||claude\.ai/code/session'
  'personal-path||/(Users|home)/'
  'claude-generated-footer||Generated with \[Claude Code\]'
  'robot-generated-footer||🤖 Generated with'
  'claude-coauthor|i|Co-Authored-By:[[:space:]]*Claude'
)

RANGE=""
TITLE=""
BODY=""
BODY_FILE=""
BODY_ENV=""
SELF_TEST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --range)
      RANGE="${2:-}"
      shift 2
      ;;
    --title)
      TITLE="${2:-}"
      shift 2
      ;;
    --body-file)
      BODY_FILE="${2:-}"
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
    -h|--help)
      usage
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage
      ;;
  esac
done

if [[ -n "$BODY_FILE" && -n "$BODY_ENV" ]]; then
  echo "error: use only one of --body-file or --body-env" >&2
  exit 2
fi

if [[ -n "$BODY_FILE" ]]; then
  BODY="$(cat -- "$BODY_FILE")"
elif [[ -n "$BODY_ENV" ]]; then
  BODY="${!BODY_ENV-}"
fi

failures=0

check_text() {
  local source="$1"
  local text="$2"
  local spec name flags pat match_file grep_opts
  # Empty surfaces are fine.
  [[ -z "$text" ]] && return 0

  match_file="$(mktemp)"
  for spec in "${PATTERN_SPECS[@]}"; do
    name="${spec%%|*}"
    rest="${spec#*|}"
    flags="${rest%%|*}"
    pat="${rest#*|}"
    grep_opts=(-nE)
    if [[ "$flags" == *i* ]]; then
      grep_opts=(-niE)
    fi
    if printf '%s' "$text" | grep "${grep_opts[@]}" -- "$pat" >"$match_file" 2>/dev/null; then
      if [[ "$SELF_TEST" -eq 1 ]]; then
        # Synthetic negative cases prove the checker rejects each pattern, but
        # must not leave scary red annotations on an otherwise-green CI run.
        echo "self-test match: public PR language: ${name} in ${source}"
      else
        echo "::error::public PR language: ${name} in ${source}"
      fi
      echo "  pattern: ${pat}${flags:+ (flags: ${flags})}"
      sed 's/^/  /' "$match_file"
      failures=$((failures + 1))
    fi
    : >"$match_file"
  done
  rm -f "$match_file"
}

check_commit_range() {
  local range="$1"
  local base head log_out sha short entry
  if [[ "$range" != *..* ]]; then
    echo "error: --range must look like BASE..HEAD (got: $range)" >&2
    exit 2
  fi
  base="${range%%..*}"
  head="${range#*..}"

  # New-branch push: GitHub sends before = 40 zeros.
  if [[ "$base" =~ ^0+$ ]]; then
    if ! git cat-file -e "${head}^{commit}" 2>/dev/null; then
      echo "error: head commit not found: $head" >&2
      exit 2
    fi
    log_out="$(git log -1 --format='%H%n%s%n%b' "$head")"
    check_text "commit $(git rev-parse --short "$head")" "$log_out"
    return 0
  fi

  if ! git cat-file -e "${base}^{commit}" 2>/dev/null; then
    echo "error: base commit not found: $base" >&2
    exit 2
  fi
  if ! git cat-file -e "${head}^{commit}" 2>/dev/null; then
    echo "error: head commit not found: $head" >&2
    exit 2
  fi

  # No commits in range (empty PR / no-op push) → clean.
  if ! git rev-list --count "${base}..${head}" | grep -qx '[1-9][0-9]*'; then
    return 0
  fi

  # -z: NUL-separate commits so multi-commit ranges keep a stable first-line SHA.
  while IFS= read -r -d '' entry; do
    # entry: HASH\nsubject\nbody...
    sha="$(printf '%s' "$entry" | head -n1)"
    if [[ -z "$sha" ]]; then
      short="unknown"
    else
      short="$(git rev-parse --short "$sha" 2>/dev/null || printf '%s' "$sha" | cut -c1-7)"
    fi
    check_text "commit ${short}" "$entry"
  done < <(git log -z --format='%H%n%s%n%b' "${base}..${head}")
}

run_self_test() {
  local failed=0
  # Clean surfaces must pass.
  failures=0
  check_text "self-test clean" "docs: clarify public PR language expectations"
  if [[ "$failures" -ne 0 ]]; then
    echo "self-test: clean text unexpectedly failed" >&2
    failed=1
  fi

  # Each hard pattern must fire once on a synthetic sample.
  local samples=(
    'session-trailer|trailer Claude-Session: https://example.invalid/session'
    'session-url|see claude.ai/code/session_abc123 for context'
    'personal-path|path was /Users/someone/Projects/x'
    'personal-path|path was /home/alice/project'
    'claude-generated-footer|Generated with [Claude Code]'
    'robot-generated-footer|🤖 Generated with something'
    'claude-coauthor|Co-Authored-By: Claude Example <noreply@example.invalid>'
    'claude-coauthor|Co-authored-by: Claude Example <noreply@example.invalid>'
  )
  local sample name needle
  for sample in "${samples[@]}"; do
    name="${sample%%|*}"
    needle="${sample#*|}"
    failures=0
    check_text "self-test expect ${name}" "$needle"
    if [[ "$failures" -eq 0 ]]; then
      echo "self-test: expected pattern ${name} to match: ${needle}" >&2
      failed=1
    fi
  done

  # Tool product names in prose must NOT hard-fail.
  failures=0
  check_text "self-test allow tools" \
    "Supports Claude, Codex, and Grok as optional CLI frontends. Red-team review optional."
  if [[ "$failures" -ne 0 ]]; then
    echo "self-test: legitimate tool names should not hard-fail" >&2
    failed=1
  fi

  if [[ "$failed" -ne 0 ]]; then
    echo "self-test FAILED" >&2
    exit 1
  fi
  echo "self-test OK"
  exit 0
}

if [[ "$SELF_TEST" -eq 1 ]]; then
  run_self_test
fi

if [[ -z "$RANGE" ]]; then
  echo "error: --range BASE..HEAD is required (or pass --self-test)" >&2
  exit 2
fi

check_text "PR title" "$TITLE"
check_text "PR body" "$BODY"
check_commit_range "$RANGE"

if [[ "$failures" -gt 0 ]]; then
  echo
  echo "Public PR language check failed (${failures} match group(s))."
  echo "Rewrite the PR title/body and/or commit messages. See CONTRIBUTING.md"
  echo "section \"Public PR language\". Exact hard-fail patterns live in this script."
  exit 1
fi

echo "public PR language: clean"
