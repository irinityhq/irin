#!/usr/bin/env bash
# Hermetic contract tests for scripts/new-worktree.sh canonical-checkout guard.
# Disposable git repo; no network: the guard must refuse before any fetch or
# worktree creation, and must pass only for a real .projectmem/plan.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER="$ROOT/scripts/new-worktree.sh"
[[ -x "$HELPER" ]] || { printf 'FAIL: helper missing or not executable: %s\n' "$HELPER" >&2; exit 1; }

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

TEST_HOME="$(mktemp -d "/tmp/irin-new-worktree.XXXXXX")"
cleanup() { rm -rf "$TEST_HOME"; }
trap cleanup EXIT

GUARD_MSG='must run from the canonical IRIN checkout'

make_fake_checkout() {
  local dir="$1"
  mkdir -p "$dir/.projectmem"
  git -C "$dir" init -q -b main
  git -C "$dir" config user.email "test@example.invalid"
  git -C "$dir" config user.name "new-worktree guard test"
  printf 'tracked\n' >"$dir/README.md"
  git -C "$dir" add README.md
  git -C "$dir" commit -q -m "init"
  # Doctrine as real files (untracked, like the canonical checkout).
  printf 'agents doctrine\n' >"$dir/AGENTS.md"
  printf 'claude doctrine\n' >"$dir/CLAUDE.md"
  printf 'rtk doctrine\n' >"$dir/RTK.md"
}

run_guard() {
  local dir="$1"; shift
  (cd "$dir" && bash "$HELPER" "$@" >/dev/null 2>"$TEST_HOME/guard.err")
}

# --- guard passes with a real plan.md; the run fails at fetch, not at the guard ---
CK="$TEST_HOME/pass-checkout"
make_fake_checkout "$CK"
printf 'plan\n' >"$CK/.projectmem/plan.md"
set +e
run_guard "$CK" feature/guard-pass
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "expected a later failure (no origin remote), not success"
if grep -q "$GUARD_MSG" "$TEST_HOME/guard.err"; then
  fail "guard refused a legitimate canonical checkout (real plan.md)"
fi
# The nonzero exit must be the fetch itself (no origin remote), not any earlier
# unrelated failure — assert git's deterministic fetch diagnostic.
grep -q "does not appear to be a git repository" "$TEST_HOME/guard.err" \
  || fail "expected failure at 'git fetch origin main', got something earlier: $(tail -1 "$TEST_HOME/guard.err")"
pass "guard accepts a real plan.md (run failed at fetch, after the guard)"

# --- guard refuses a missing plan.md before any worktree exists ---
CK="$TEST_HOME/missing-checkout"
make_fake_checkout "$CK"
set +e
run_guard "$CK" feature/missing-plan
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "should refuse missing canonical plan.md"
grep -q "$GUARD_MSG" "$TEST_HOME/guard.err" || fail "missing guard refusal message"
grep -q '\.projectmem/plan\.md' "$TEST_HOME/guard.err" || fail "diagnostic must name .projectmem/plan.md"
[[ ! -e "$TEST_HOME/irin-wt-feature-missing-plan" ]] || fail "refusal must happen before worktree creation"
pass "refuse missing canonical plan.md before any worktree exists"

# --- guard refuses a symlinked plan.md ---
CK="$TEST_HOME/symlink-checkout"
make_fake_checkout "$CK"
printf 'elsewhere\n' >"$TEST_HOME/plan.real"
ln -s "$TEST_HOME/plan.real" "$CK/.projectmem/plan.md"
set +e
run_guard "$CK" feature/symlink-plan
rc=$?
set -e
[[ "$rc" -ne 0 ]] || fail "should refuse symlinked canonical plan.md"
grep -q "$GUARD_MSG" "$TEST_HOME/guard.err" || fail "missing guard refusal message (symlink case)"
grep -q '\.projectmem/plan\.md' "$TEST_HOME/guard.err" || fail "diagnostic must name .projectmem/plan.md (symlink case)"
pass "refuse symlinked canonical plan.md"

printf 'All new-worktree guard contracts passed.\n'
