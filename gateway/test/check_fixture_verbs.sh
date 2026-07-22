#!/usr/bin/env bash
# Standing fixture action-allowlist gate.
#
# Fails the build if any test/ fixture (.sh/.yaml/.yml) emits a
# scope.allowed_actions verb OUTSIDE the directive_fence allowlist
# (read/report/notify/review/escalate — mirrors council-rs
# directive_fence.rs). This prevents a stale mock-council stub verb from
# being dead-lettered by the closed fence.
#
# Intentional NEGATIVE/reject fixtures (e.g. a future reject-path smoke that
# sends a bad verb to assert the rejection) must add the marker  A5-ALLOW
# anywhere on the same line to opt out — the gate stays fail-closed for
# everything unmarked.
#
# Scope/limits: scans single-line allowed_actions arrays (all current stubs
# are single-line). Multi-line arrays are not parsed — keep fixtures single-line.
# Rust fixtures are out of scope here because cargo tests cover them.
#
# Usage:    test/check_fixture_verbs.sh [root_dir]    (default root: test)
# Selftest: A5_SELFTEST=1 test/check_fixture_verbs.sh (plants+detects a bad verb)
# Rollback: delete this file and its CI step in .github/workflows/triad-ci.yml.
set -euo pipefail

ALLOW="read report notify review escalate"
root="${1:-test}"

scan() {
  local target="$1" fail=0 hit file rest lineno line arr verbs v
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    file="${hit%%:*}"; rest="${hit#*:}"; lineno="${rest%%:*}"; line="${rest#*:}"
    case "$line" in *A5-ALLOW*) continue;; esac
    arr="${line#*allowed_actions}"
    verbs=$(printf '%s' "$arr" | grep -oE '"[A-Za-z0-9_.*-]+"' | tr -d '"' || true)
    for v in $verbs; do
      if ! printf '%s\n' $ALLOW | grep -qx "$v"; then
        echo "A5 FAIL: $file:$lineno  disallowed verb \"$v\" in allowed_actions (allowlist: $ALLOW)"
        echo "    -> $(printf '%s' "$line" | sed 's/^[[:space:]]*//')"
        fail=1
      fi
    done
  done < <(grep -rn --include='*.sh' --include='*.yaml' --include='*.yml' --exclude='check_fixture_verbs.sh' 'allowed_actions' "$target" 2>/dev/null || true)
  return $fail
}

if [ "${A5_SELFTEST:-0}" = "1" ]; then
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  printf '%s\n' '{"scope":{"allowed_actions":["stage_directive_outbox"]}}' > "$tmp/bad.sh"
  printf '%s\n' '{"scope":{"allowed_actions":["report"]}}' > "$tmp/good.sh"
  printf '%s\n' '{"scope":{"allowed_actions":["delete"]}} # A5-ALLOW intentional reject' > "$tmp/marked.sh"
  if scan "$tmp"; then echo "SELFTEST FAIL: gate did not catch the planted bad verb" >&2; exit 1; fi
  echo "SELFTEST PASS: planted bad verb caught; good + A5-ALLOW-marked lines passed."
  exit 0
fi

if scan "$root"; then
  echo "A5 gate PASS: all $root/ allowed_actions verbs within allowlist ($ALLOW)."
else
  echo "A5 gate FAILED: stale fixture verb(s) above. Fix to an allowlisted verb, or mark an intentional reject fixture with 'A5-ALLOW'." >&2
  exit 1
fi
