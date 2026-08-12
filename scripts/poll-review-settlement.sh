#!/usr/bin/env bash
# Poll wrapper for the review settlement evaluator.
#
# The Copilot auto-review ruleset (review_on_push) re-requests a review on
# every push, so settlement is structurally unsettled during the Copilot
# latency window after each commit. This wrapper retries not-settled instead
# of failing the first probe. Wait logic lives here, NOT in
# scripts/check-review-settlement.sh, so the evaluator's snapshot purity and
# self-test semantics stay single-shot. Reviews are head-bound: a reviewer's
# latest review must sit on the current head commit to count as settled.
#
# Exit semantics:
#   evaluator exit 0 (settled)      -> exit 0 immediately
#   evaluator exit 1 (not settled)  -> retry every INTERVAL until DEADLINE,
#                                      then exit 1
#   any other evaluator exit code   -> propagate immediately (2 is
#                                      usage/transport/schema/truncated)
#   probe wall-clock timeout        -> treated as exit 1 (deadline path)
#
# Each probe is hard-bounded by the remaining wait window so a hung
# `gh api` cannot run past the advertised deadline. Window 0 still gets
# one unbounded first probe (deadline_exhausted contract).
#
# Env knobs (deterministic contract tests override these):
#   SETTLEMENT_POLL_INTERVAL_SECONDS  seconds between probes (default 30)
#   SETTLEMENT_POLL_DEADLINE_SECONDS  total wait window (default 600)
#   SETTLEMENT_EVALUATOR              evaluator command (default
#                                     scripts/check-review-settlement.sh)
#
# Modes:
#   --self-test   run deterministic poll-contract fixtures and exit
#   <args...>     passed through verbatim to the evaluator each probe
set -euo pipefail

# Run evaluator under a remaining-window budget. Maps timeout → exit 1.
# budget<=0: run unbounded (zero-window first probe only).
run_probe() {
  local budget=$1
  shift
  local rc=0
  if (( budget <= 0 )); then
    "$@" || rc=$?
    return "$rc"
  fi
  if command -v timeout >/dev/null 2>&1; then
    # GNU coreutils: 124 = timed out. Prefer --kill-after so a stuck child
    # of the evaluator (e.g. gh) cannot outlive the budget by much.
    timeout --kill-after=2 "${budget}s" "$@" || rc=$?
    if (( rc == 124 )); then
      return 1
    fi
    return "$rc"
  fi
  # Portable fallback: background evaluator + watchdog kill.
  "$@" &
  local pid=$!
  (
    sleep "$budget"
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      sleep 1
      kill -9 "$pid" 2>/dev/null || true
    fi
  ) &
  local wd=$!
  wait "$pid" || rc=$?
  kill "$wd" 2>/dev/null || true
  wait "$wd" 2>/dev/null || true
  # Signal death (128+N) → deadline path, not hard-fail.
  if (( rc >= 128 )); then
    return 1
  fi
  return "$rc"
}

poll() {
  local interval="${SETTLEMENT_POLL_INTERVAL_SECONDS:-30}"
  local window="${SETTLEMENT_POLL_DEADLINE_SECONDS:-600}"
  local evaluator="${SETTLEMENT_EVALUATOR:-scripts/check-review-settlement.sh}"
  local deadline=$((SECONDS + window))
  local rc remaining sleep_for probed=0
  while :; do
    # After the first probe, refuse to launch at/after the advertised deadline.
    # Window 0 still gets one probe (deadline_exhausted contract).
    if (( probed > 0 && SECONDS >= deadline )); then
      printf 'review-settlement: not settled within %ss wait window\n' \
        "$window" >&2
      return 1
    fi
    remaining=$((deadline - SECONDS))
    rc=0
    # Bound every positive-remainder probe by the residual window so the
    # evaluator cannot overrun the advertised deadline.
    run_probe "$remaining" "$evaluator" "$@" || rc=$?
    probed=1
    case "$rc" in
      0)
        return 0
        ;;
      1)
        remaining=$((deadline - SECONDS))
        if (( remaining <= 0 )); then
          printf 'review-settlement: not settled within %ss wait window\n' \
            "$window" >&2
          return 1
        fi
        # Sleep at most the remainder so a near-deadline not-settled cannot
        # push the next probe past the advertised window.
        sleep_for=$interval
        if (( sleep_for > remaining )); then
          sleep_for=$remaining
        fi
        sleep "$sleep_for"
        ;;
      *)
        # Usage/transport/schema/truncated: never retried, never softened.
        return "$rc"
        ;;
    esac
  done
}

run_self_test() {
  local tmp failures=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-settlement-poll.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT

  expect() {
    local name="$1" evaluator="$2" window="$3" want_rc="$4" want_calls="$5"
    local interval="${6:-0}"
    local rc=0
    : >"$tmp/calls"
    set +e
    SETTLEMENT_EVALUATOR="$evaluator" \
      SETTLEMENT_POLL_INTERVAL_SECONDS="$interval" \
      SETTLEMENT_POLL_DEADLINE_SECONDS="$window" \
      poll >/dev/null 2>&1
    rc=$?
    set -e
    local calls
    calls="$(wc -l <"$tmp/calls" | tr -d ' ')"
    if [[ "$rc" == "$want_rc" && "$calls" == "$want_calls" ]]; then
      printf 'PASS: %s (rc=%s calls=%s)\n' "$name" "$rc" "$calls"
    else
      printf 'FAIL: %s want rc=%s calls=%s got rc=%s calls=%s\n' \
        "$name" "$want_rc" "$want_calls" "$rc" "$calls" >&2
      failures=$((failures + 1))
    fi
  }

  # Evaluator that is not-settled twice, then settled: poll must retry
  # through exit 1 and return 0.
  cat >"$tmp/settles-third-probe.sh" <<EOF
#!/usr/bin/env bash
echo probe >>"$tmp/calls"
(( \$(wc -l <"$tmp/calls") >= 3 )) && exit 0
exit 1
EOF

  # Evaluator that hard-fails: poll must propagate exit 2 on the first
  # probe, never retry it.
  cat >"$tmp/hard-fail.sh" <<EOF
#!/usr/bin/env bash
echo probe >>"$tmp/calls"
exit 2
EOF

  # Evaluator that never settles: a zero-length window exhausts after the
  # first probe and returns 1.
  cat >"$tmp/never-settles.sh" <<EOF
#!/usr/bin/env bash
echo probe >>"$tmp/calls"
exit 1
EOF

  chmod +x "$tmp"/*.sh

  expect retry_then_settled "$tmp/settles-third-probe.sh" 60 0 3
  expect hard_fail_immediate "$tmp/hard-fail.sh" 60 2 1
  expect deadline_exhausted "$tmp/never-settles.sh" 0 1 1

  # Near-deadline overrun: non-zero interval longer than remaining window.
  # After the first not-settled probe, poll must sleep at most the remainder
  # and must not launch another probe past the advertised deadline.
  # Wall-clock bound: must finish well under a full interval sleep.
  {
    local window=1 interval=5 rc=0 calls elapsed
    local start=$SECONDS
    : >"$tmp/calls"
    set +e
    SETTLEMENT_EVALUATOR="$tmp/never-settles.sh" \
      SETTLEMENT_POLL_INTERVAL_SECONDS="$interval" \
      SETTLEMENT_POLL_DEADLINE_SECONDS="$window" \
      poll >/dev/null 2>&1
    rc=$?
    set -e
    elapsed=$((SECONDS - start))
    calls="$(wc -l <"$tmp/calls" | tr -d ' ')"
    if [[ "$rc" == "1" && "$calls" == "1" && "$elapsed" -le $((window + 1)) ]]; then
      printf 'PASS: near_deadline_no_overrun (rc=%s calls=%s elapsed=%ss)\n' \
        "$rc" "$calls" "$elapsed"
    else
      printf \
        'FAIL: near_deadline_no_overrun want rc=1 calls=1 elapsed<=%ss; got rc=%s calls=%s elapsed=%ss\n' \
        "$((window + 1))" "$rc" "$calls" "$elapsed" >&2
      failures=$((failures + 1))
    fi
  }

  # Window-consuming probe: remainder must be recomputed AFTER probe latency.
  # A stale pre-probe remainder of `window` would sleep ~window after a probe
  # that already burned most of the budget, pushing elapsed well past window.
  # Probe sleeps 2s; window=3; interval=10. Correct clamp → ~3s total, 1 call.
  cat >"$tmp/slow-never-settles.sh" <<EOF
#!/usr/bin/env bash
echo probe >>"$tmp/calls"
sleep 2
exit 1
EOF
  chmod +x "$tmp/slow-never-settles.sh"
  {
    local window=3 interval=10 rc=0 calls elapsed
    local start=$SECONDS
    : >"$tmp/calls"
    set +e
    SETTLEMENT_EVALUATOR="$tmp/slow-never-settles.sh" \
      SETTLEMENT_POLL_INTERVAL_SECONDS="$interval" \
      SETTLEMENT_POLL_DEADLINE_SECONDS="$window" \
      poll >/dev/null 2>&1
    rc=$?
    set -e
    elapsed=$((SECONDS - start))
    calls="$(wc -l <"$tmp/calls" | tr -d ' ')"
    # window+1 allows integer-second slack; stale pre-probe sleep lands ~5s+.
    if [[ "$rc" == "1" && "$calls" == "1" && "$elapsed" -le $((window + 1)) ]]; then
      printf 'PASS: remainder_after_probe_latency (rc=%s calls=%s elapsed=%ss)\n' \
        "$rc" "$calls" "$elapsed"
    else
      printf \
        'FAIL: remainder_after_probe_latency want rc=1 calls=1 elapsed<=%ss; got rc=%s calls=%s elapsed=%ss\n' \
        "$((window + 1))" "$rc" "$calls" "$elapsed" >&2
      failures=$((failures + 1))
    fi
  }

  # Probe wall-clock budget: a hung evaluator must not outrun the window.
  # Probe sleeps 5s; window=2. Without a hard budget this hangs ~5s; with
  # it, elapsed must land near the window and still return 1.
  cat >"$tmp/hangs-past-window.sh" <<EOF
#!/usr/bin/env bash
echo probe >>"$tmp/calls"
sleep 5
exit 1
EOF
  chmod +x "$tmp/hangs-past-window.sh"
  {
    local window=2 interval=10 rc=0 calls elapsed
    local start=$SECONDS
    : >"$tmp/calls"
    set +e
    SETTLEMENT_EVALUATOR="$tmp/hangs-past-window.sh" \
      SETTLEMENT_POLL_INTERVAL_SECONDS="$interval" \
      SETTLEMENT_POLL_DEADLINE_SECONDS="$window" \
      poll >/dev/null 2>&1
    rc=$?
    set -e
    elapsed=$((SECONDS - start))
    calls="$(wc -l <"$tmp/calls" | tr -d ' ')"
    # Allow 2s slack for timeout kill-after; must beat the full 5s sleep.
    if [[ "$rc" == "1" && "$calls" == "1" && "$elapsed" -le $((window + 2)) ]]; then
      printf 'PASS: probe_bounded_by_remaining (rc=%s calls=%s elapsed=%ss)\n' \
        "$rc" "$calls" "$elapsed"
    else
      printf \
        'FAIL: probe_bounded_by_remaining want rc=1 calls=1 elapsed<=%ss; got rc=%s calls=%s elapsed=%ss\n' \
        "$((window + 2))" "$rc" "$calls" "$elapsed" >&2
      failures=$((failures + 1))
    fi
  }

  if (( failures > 0 )); then
    printf 'poll-review-settlement self-test: FAILED (%d)\n' "$failures" >&2
    exit 1
  fi
  printf 'poll-review-settlement self-test: OK\n'
}

if [[ "${1:-}" == "--self-test" ]]; then
  run_self_test
  exit 0
fi

poll "$@"
