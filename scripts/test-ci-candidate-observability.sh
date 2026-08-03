#!/usr/bin/env bash
# Contract tests for PR A candidate observability + install selection.
#
# 1) Static workflow wiring: no command-substitution of the CI helper;
#    non-hidden outer failure log; fail-closed artifact upload; PR isolation
#    honors exact_install; W4 bootstrap retired from the dispatcher while
#    @main pin remains.
# 2) Deterministic failure fixtures: early helper failure (before any
#    helper-internal log) still lands in the outer non-hidden tee log.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CI_YML="$ROOT/.github/workflows/ci.yml"
CI_PR="$ROOT/.github/workflows/ci-pr.yml"
failures=0

pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; failures=$((failures + 1)); }

# ---------------------------------------------------------------------------
# Static: W4 bootstrap retired from dispatcher
# ---------------------------------------------------------------------------
if rg -n 'w4-bootstrap-scope|w4-isolation-bootstrap-|W4 isolation bootstrap required' "$CI_PR" >/dev/null; then
  fail "ci-pr.yml still defines W4 bootstrap jobs or required context"
else
  pass "ci-pr.yml has no W4 bootstrap jobs"
fi

if ! rg -n 'uses: irinityhq/irin/\.github/workflows/ci\.yml@main' "$CI_PR" >/dev/null; then
  fail "ci-pr.yml must keep the @main dispatcher pin"
else
  pass "ci-pr.yml still pins ci.yml@main"
fi

# ---------------------------------------------------------------------------
# Static: no command substitution of the helper; stream via tee
# ---------------------------------------------------------------------------
if rg -n 'out="\$\(bash scripts/ci-build-adhoc-candidate\.sh' "$CI_YML" >/dev/null; then
  fail "ci.yml still captures ci-build-adhoc-candidate.sh via command substitution"
else
  pass "ci.yml does not command-substitute ci-build-adhoc-candidate.sh"
fi

if ! rg -n 'ci-build-adhoc-candidate\.sh .*2>&1 \| tee' "$CI_YML" >/dev/null; then
  fail "ci.yml must stream ci-build-adhoc-candidate.sh through tee"
else
  pass "ci.yml streams helper output through tee"
fi

# ---------------------------------------------------------------------------
# Static: failure log artifact + install selection on permanent jobs
# ---------------------------------------------------------------------------
if ! python3 - "$CI_YML" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
jobs = re.split(r"\n  ([a-z0-9-]+):\n", text)
bodies = {}
for i in range(1, len(jobs), 2):
    bodies[jobs[i]] = jobs[i + 1]

wanted = ("candidate-isolation-proof", "exact-merged-candidate")
for name in wanted:
    body = bodies.get(name)
    if body is None:
        print(f"missing job {name}", file=sys.stderr)
        sys.exit(1)
    if "Upload candidate build log on failure" not in body:
        print(f"missing failure upload in {name}", file=sys.stderr)
        sys.exit(1)
    if not re.search(
        r"if:\s*failure\(\)\s*&&\s*env\.CI_BUILD_LOG\s*!=\s*''",
        body,
    ):
        print(
            f"upload must guard on failure() && env.CI_BUILD_LOG != '' in {name}",
            file=sys.stderr,
        )
        sys.exit(1)
    if not re.search(
        r"if-no-files-found:\s*error",
        body,
    ):
        print(f"failure log upload must use if-no-files-found: error in {name}", file=sys.stderr)
        sys.exit(1)
    if re.search(r"if-no-files-found:\s*ignore", body):
        print(f"failure log upload must not use if-no-files-found: ignore in {name}", file=sys.stderr)
        sys.exit(1)
    # Outer non-hidden log assignment: ci-build-<sha>.log (not .ci-build-*)
    if not re.search(
        r'outer_log="\$IRIN_CANDIDATE_ROOT/ci-build-\$\{[^}]+}\.log"',
        body,
    ):
        print(f"missing non-hidden outer_log=.../ci-build-<sha>.log in {name}", file=sys.stderr)
        sys.exit(1)
    # CI_BUILD_LOG must point at outer_log, not a hidden .ci-build path
    if re.search(r'CI_BUILD_LOG=\$IRIN_CANDIDATE_ROOT/\.ci-build-', body):
        print(f"CI_BUILD_LOG must not target hidden .ci-build- path in {name}", file=sys.stderr)
        sys.exit(1)
    if not re.search(r"CI_BUILD_LOG=\$outer_log", body):
        print(f"CI_BUILD_LOG must be set from outer_log in {name}", file=sys.stderr)
        sys.exit(1)
    # Must tee into outer_log (the artifact file), not a separate hidden summary
    if not re.search(r'\|\s*tee\s+"\$outer_log"', body):
        print(f'helper must tee into $outer_log in {name}', file=sys.stderr)
        sys.exit(1)
    # Must not assign CI_BUILD_LOG to hidden helper-internal path pattern for upload
    if re.search(r'CI_BUILD_LOG=.*\.ci-build-', body):
        print(f"CI_BUILD_LOG still references .ci-build- in {name}", file=sys.stderr)
        sys.exit(1)
    if "args+=(--install)" not in body:
        print(f"missing conditional --install in {name}", file=sys.stderr)
        sys.exit(1)
    if "EXACT_INSTALL" not in body:
        print(f"missing EXACT_INSTALL env in {name}", file=sys.stderr)
        sys.exit(1)
    if not re.search(
        r'if \[\[ "\$EXACT_INSTALL" == "true" \]\]; then\s*\n\s*args\+=\(--install\)',
        body,
    ):
        print(f"--install not gated on EXACT_INSTALL in {name}", file=sys.stderr)
        sys.exit(1)

iso = bodies["candidate-isolation-proof"]
if "needs.detect-changes.outputs.exact_install" not in iso:
    print("isolation job must read exact_install from detect-changes", file=sys.stderr)
    sys.exit(1)

ci_req = bodies.get("ci-required", "")
if "candidate-isolation-proof" not in ci_req:
    print("ci-required must still need candidate-isolation-proof", file=sys.stderr)
    sys.exit(1)

print("workflow structural checks ok")
PY
then
  fail "workflow structural checks failed"
else
  pass "failure-log, install gating, and ci-required wiring look correct"
fi

# ---------------------------------------------------------------------------
# Deterministic failure fixtures: outer non-hidden log contract
# ---------------------------------------------------------------------------
fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/irin-ci-obs.XXXXXX")"
cleanup() { rm -rf "$fixture_dir"; }
trap cleanup EXIT

export IRIN_CANDIDATE_ROOT="$fixture_dir/candidates"
mkdir -p "$IRIN_CANDIDATE_ROOT"
SHA="$(python3 -c 'print("ab" * 20)')"
# Non-hidden outer log — the artifact contract under test
outer_log="$IRIN_CANDIDATE_ROOT/ci-build-${SHA}.log"
# Hidden helper-internal path the OLD (broken) design uploaded — must not be required
hidden_internal="$IRIN_CANDIDATE_ROOT/.ci-build-${SHA}.log"

# Fixture A: early failure BEFORE any helper-internal log is created.
# This is the case upload-artifact+hidden-log silently dropped.
fake_early="$fixture_dir/fake-ci-build-early-fail.sh"
cat >"$fake_early" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
# Fail before writing any internal log (no tee to .ci-build-*).
echo "ERROR: early validation failed (no internal log yet)" >&2
exit 1
EOF
chmod +x "$fake_early"

set +e
bash "$fake_early" --source-sha "$SHA" 2>&1 | tee "$outer_log"
build_ec=${PIPESTATUS[0]}
set -e

if [[ "$build_ec" -eq 0 ]]; then
  fail "early-fail fixture should exit non-zero"
else
  pass "early-fail fixture preserved non-zero exit ($build_ec)"
fi

if [[ ! -f "$outer_log" ]]; then
  fail "outer non-hidden ci-build-<sha>.log missing after early failure"
elif ! grep -q 'early validation failed' "$outer_log"; then
  fail "outer log missing early-failure diagnostics"
else
  pass "outer non-hidden log retains early-failure diagnostics"
fi

if [[ -f "$hidden_internal" ]]; then
  fail "early-fail fixture must not create hidden helper-internal log"
else
  pass "early-fail fixture never created hidden .ci-build-<sha>.log"
fi

# Basename must not start with '.' (upload-artifact default excludes hidden files)
outer_base="$(basename "$outer_log")"
if [[ "$outer_base" == .* ]]; then
  fail "outer artifact log basename is hidden: $outer_base"
else
  pass "outer artifact log basename is non-hidden ($outer_base)"
fi

# Fixture B: mid-stream failure still captured entirely in outer log
fake_mid="$fixture_dir/fake-ci-build-mid-fail.sh"
cat >"$fake_mid" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
SOURCE_SHA=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-sha) SOURCE_SHA="$2"; shift 2 ;;
    *) shift ;;
  esac
done
: "${SOURCE_SHA:?}"
: "${IRIN_CANDIDATE_ROOT:?}"
# Optionally write a helper-internal hidden log (real helper does this for build-dmg).
# Artifact contract must NOT depend on it.
internal="$IRIN_CANDIDATE_ROOT/.ci-build-${SOURCE_SHA}.log"
echo "=== building (fake mid) ==="
echo "progress-line"
echo "line-from-build" | tee "$internal"
echo "ERROR: verify failed after build" >&2
exit 1
EOF
chmod +x "$fake_mid"

outer_log_mid="$IRIN_CANDIDATE_ROOT/ci-build-mid-${SHA}.log"
set +e
bash "$fake_mid" --source-sha "$SHA" 2>&1 | tee "$outer_log_mid"
mid_ec=${PIPESTATUS[0]}
set -e

if [[ "$mid_ec" -eq 0 ]]; then
  fail "mid-fail fixture should exit non-zero"
else
  pass "mid-fail fixture preserved non-zero exit ($mid_ec)"
fi

if ! grep -q 'progress-line' "$outer_log_mid" \
  || ! grep -q 'verify failed after build' "$outer_log_mid"; then
  fail "outer log must capture full stream including post-build failures"
else
  pass "outer log captures pre- and post-build failure stream"
fi

# Install selection unit (pure bash)
select_install_args() {
  local exact_install="$1"
  local args=()
  if [[ "$exact_install" == "true" ]]; then
    args+=(--install)
  fi
  printf '%s\n' "${args[*]-}"
}

if [[ "$(select_install_args true)" != "--install" ]]; then
  fail "exact_install=true must select --install"
else
  pass "exact_install=true selects --install"
fi

if [[ -n "$(select_install_args false)" ]]; then
  fail "exact_install=false must not select --install"
else
  pass "exact_install=false skips --install"
fi

# ---------------------------------------------------------------------------
if [[ "$failures" -ne 0 ]]; then
  printf 'test-ci-candidate-observability: %d failure(s)\n' "$failures" >&2
  exit 1
fi
printf 'test-ci-candidate-observability: OK\n'
