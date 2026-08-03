#!/usr/bin/env bash
# Contract tests for PR A candidate observability + install selection.
#
# 1) Static workflow wiring: no command-substitution of the CI helper;
#    failure-log artifact steps exist; PR isolation honors exact_install;
#    W4 bootstrap retired from the dispatcher while @main pin remains.
# 2) Deterministic failure fixture: stream helper output live, retain log path,
#    preserve non-zero exit — without making a live red PR.
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
        r"if:\s*failure\(\)\s*\n\s*uses:\s*actions/upload-artifact",
        body,
    ):
        print(f"missing if: failure() upload-artifact in {name}", file=sys.stderr)
        sys.exit(1)
    if "CI_BUILD_LOG" not in body or ".ci-build-" not in body:
        print(f"missing CI_BUILD_LOG / .ci-build- wiring in {name}", file=sys.stderr)
        sys.exit(1)
    if "args+=(--install)" not in body:
        print(f"missing conditional --install in {name}", file=sys.stderr)
        sys.exit(1)
    if "EXACT_INSTALL" not in body:
        print(f"missing EXACT_INSTALL env in {name}", file=sys.stderr)
        sys.exit(1)
    # install only when exact_install is true
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
# Deterministic failure fixture: observable invoke pattern
# ---------------------------------------------------------------------------
fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/irin-ci-obs.XXXXXX")"
cleanup() { rm -rf "$fixture_dir"; }
trap cleanup EXIT

fake_helper="$fixture_dir/fake-ci-build.sh"
cat >"$fake_helper" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
SOURCE_SHA=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-sha) SOURCE_SHA="$2"; shift 2 ;;
    --install) shift ;;
    --export-dir) shift 2 ;;
    *) shift ;;
  esac
done
: "${SOURCE_SHA:?}"
: "${IRIN_CANDIDATE_ROOT:?}"
log="$IRIN_CANDIDATE_ROOT/.ci-build-${SOURCE_SHA}.log"
echo "=== building (fake) ==="
echo "line-from-build" | tee "$log"
echo "ERROR: build-dmg failed (exit 1)" >&2
exit 1
EOF
chmod +x "$fake_helper"

export IRIN_CANDIDATE_ROOT="$fixture_dir/candidates"
mkdir -p "$IRIN_CANDIDATE_ROOT"
SHA="$(python3 -c 'print("ab" * 20)')"
build_log="$IRIN_CANDIDATE_ROOT/.ci-build-${SHA}.log"
summary_file="$IRIN_CANDIDATE_ROOT/.ci-build-summary-${SHA}.txt"

set +e
bash "$fake_helper" --source-sha "$SHA" --export-dir "$IRIN_CANDIDATE_ROOT/.exports/$SHA" 2>&1 \
  | tee "$summary_file"
build_ec=${PIPESTATUS[0]}
set -e

if [[ "$build_ec" -eq 0 ]]; then
  fail "fixture helper should exit non-zero"
else
  pass "fixture helper preserved non-zero exit ($build_ec)"
fi

if [[ ! -f "$summary_file" ]] || ! grep -q '=== building (fake) ===' "$summary_file"; then
  fail "streamed summary file missing live helper output"
else
  pass "streamed summary file retained live helper output"
fi

if [[ ! -f "$build_log" ]] || ! grep -q 'line-from-build' "$build_log"; then
  fail "retained .ci-build-<sha>.log missing expected content"
else
  pass "retained .ci-build-<sha>.log has build diagnostics"
fi

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
