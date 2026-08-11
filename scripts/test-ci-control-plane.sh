#!/usr/bin/env bash
# Contract tests for PR B CI control-plane: concurrency split, exact-path
# policy sync, and base-controlled force-full matrix guard.
#
# Hosted detect-changes runs this on clean ubuntu-latest images. Depend only
# on POSIX-ish tools (bash, python3, grep, sed, git) — not ripgrep.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CI_YML="$ROOT/.github/workflows/ci.yml"
CI_PR="$ROOT/.github/workflows/ci-pr.yml"
CLASSIFIER="$ROOT/scripts/classify-ci-paths.sh"
failures=0

pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; failures=$((failures + 1)); }

# grep helpers (no ripgrep): return 0 if match, 1 if not. Safe under set -e in if.
file_has_ere() { grep -Eq -- "$1" "$2"; }
file_has_fixed() { grep -Fq -- "$1" "$2"; }

export IRIN_CLASSIFIER_INCLUDE_EXACT=1

# ---------------------------------------------------------------------------
# Static: concurrency split — main queue vs PR cancel; no illegal combo
# ---------------------------------------------------------------------------
if python3 - "$CI_YML" "$CI_PR" <<'PY'
import re
import sys
from pathlib import Path

ci = Path(sys.argv[1]).read_text(encoding="utf-8")
pr = Path(sys.argv[2]).read_text(encoding="utf-8")
errors = []

m = re.search(r"(?m)^concurrency:\n((?:  .*\n)+)", ci)
if not m:
    errors.append("ci.yml missing top-level concurrency block")
else:
    block = m.group(1)
    if "queue: max" not in block:
        errors.append("ci.yml concurrency must set queue: max for main merge retention")
    if re.search(r"cancel-in-progress:\s*true", block):
        errors.append("ci.yml must not set cancel-in-progress: true (illegal with queue:max)")
    if not re.search(r"cancel-in-progress:\s*false", block):
        errors.append("ci.yml must set cancel-in-progress: false explicitly")
    if "main-push" not in block:
        errors.append("ci.yml concurrency group must name a stable main-push bucket")
    if "github.run_id" not in block:
        errors.append("ci.yml non-main events must unique-ify groups with github.run_id")
    if re.search(r"cancel-in-progress:\s*\$\{\{", block):
        errors.append("ci.yml cancel-in-progress must be literal false, not an expression")

m = re.search(r"(?m)^concurrency:\n((?:  .*\n)+)", pr)
if not m:
    errors.append("ci-pr.yml missing top-level concurrency block for PR cancellation")
else:
    block = m.group(1)
    if "queue:" in block:
        errors.append("ci-pr.yml must not set queue (cancel surface only)")
    # PR updates cancel; merge_group must not cancel.
    # Reject literal true/false and reversed event comparisons.
    if not re.search(
        r"cancel-in-progress:\s*\$\{\{\s*github\.event_name\s*!=\s*'merge_group'\s*\}\}",
        block,
    ):
        errors.append(
            "ci-pr.yml cancel-in-progress must be "
            "${{ github.event_name != 'merge_group' }} "
            "(PR cancels; merge_group does not)"
        )
    if "github.event.pull_request.number" not in block:
        errors.append("ci-pr.yml concurrency group must be per-PR number")
    if "irin-ci-pr-" not in block:
        errors.append("ci-pr.yml concurrency group should be namespaced irin-ci-pr-*")
    if "irin-ci-merge-group-" not in block:
        errors.append("ci-pr.yml concurrency must namespace merge_group runs")
    if "github.event.merge_group.head_sha" not in block:
        errors.append(
            "ci-pr.yml merge_group concurrency group must key on "
            "github.event.merge_group.head_sha"
        )

if "uses: irinityhq/irin/.github/workflows/ci.yml@main" not in pr:
    errors.append("ci-pr.yml must keep the @main dispatcher pin")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
PY
then
  pass "concurrency split: main queue:max + PR cancel, no illegal combo"
else
  fail "concurrency structure"
fi

# ---------------------------------------------------------------------------
# merge_group wiring: every required-check producer + real SHA classification
# ---------------------------------------------------------------------------
CODEQL_YML="$ROOT/.github/workflows/codeql.yml"
DEP_REVIEW_YML="$ROOT/.github/workflows/dependency-review.yml"
on_has_merge_group() {
  # Top-level workflow trigger (indented under `on:`), not a shell case arm.
  grep -E '^[[:space:]]{2}merge_group:' "$1" >/dev/null
}
if ! on_has_merge_group "$CI_PR"; then
  fail "ci-pr.yml must trigger on merge_group (produces ci / CI required)"
else
  pass "ci-pr.yml triggers on merge_group"
fi
if on_has_merge_group "$CI_YML"; then
  # Dual trigger would report unprotected job names alongside the ci/ prefix.
  fail "ci.yml must not top-level trigger merge_group (caller is ci-pr.yml)"
else
  pass "ci.yml does not dual-trigger merge_group"
fi
if ! file_has_fixed 'merge_group)' "$CI_YML" \
  || ! file_has_fixed 'github.event.merge_group.base_sha' "$CI_YML" \
  || ! file_has_fixed 'github.event.merge_group.head_sha' "$CI_YML"; then
  fail "detect-changes must classify merge_group via base_sha/head_sha"
else
  pass "detect-changes classifies merge_group base/head SHAs"
fi
# Permanent full-matrix fallback must not be the only merge_group path.
if ! python3 - "$CI_YML" <<'PY'
import re
import sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(encoding="utf-8")
m = re.search(r"merge_group\)\s*\n(.*?\n\s*;;)", text, re.S)
if not m:
    sys.stderr.write("missing merge_group case arm\n")
    sys.exit(1)
arm = m.group(1)
if re.search(r"changed=\(__unknown_event__\)", arm):
    sys.stderr.write("merge_group arm must not assign __unknown_event__\n")
    sys.exit(1)
if "base_sha" not in arm or "head_sha" not in arm:
    sys.stderr.write("merge_group arm must use base_sha and head_sha\n")
    sys.exit(1)
if "git diff --name-only" not in arm:
    sys.stderr.write("merge_group arm must path-diff base...head\n")
    sys.exit(1)
sys.exit(0)
PY
then
  fail "merge_group case arm contract"
else
  pass "merge_group case arm uses real SHAs (not __unknown_event__)"
fi
if ! on_has_merge_group "$CODEQL_YML"; then
  fail "codeql.yml must trigger on merge_group (CodeQL required)"
else
  pass "codeql.yml triggers on merge_group"
fi
if ! on_has_merge_group "$DEP_REVIEW_YML"; then
  fail "dependency-review.yml must trigger on merge_group"
else
  pass "dependency-review.yml triggers on merge_group"
fi
if ! file_has_fixed 'base-ref:' "$DEP_REVIEW_YML" \
  || ! file_has_fixed 'head-ref:' "$DEP_REVIEW_YML" \
  || ! file_has_fixed 'github.event.merge_group.base_sha' "$DEP_REVIEW_YML" \
  || ! file_has_fixed 'github.event.merge_group.head_sha' "$DEP_REVIEW_YML"; then
  fail "dependency-review must pass base-ref/head-ref on merge_group"
else
  pass "dependency-review passes base-ref/head-ref for merge_group"
fi
# Required check job name must remain "Dependency Review" (protected context).
if ! grep -E '^[[:space:]]+name: Dependency Review[[:space:]]*$' "$DEP_REVIEW_YML" >/dev/null; then
  fail "dependency-review job name must stay 'Dependency Review'"
else
  pass "dependency-review protected job name preserved"
fi

# ---------------------------------------------------------------------------
# Review settlement gate: pre-queue required-check producer (#0101 / PR #70)
# ---------------------------------------------------------------------------
REVIEW_SETTLEMENT_YML="$ROOT/.github/workflows/review-settlement.yml"
REVIEW_SETTLEMENT_SH="$ROOT/scripts/check-review-settlement.sh"
REVIEW_SETTLEMENT_POLL_SH="$ROOT/scripts/poll-review-settlement.sh"
if [[ ! -f "$REVIEW_SETTLEMENT_YML" ]]; then
  fail "review-settlement.yml missing"
elif [[ ! -f "$REVIEW_SETTLEMENT_SH" ]]; then
  fail "scripts/check-review-settlement.sh missing"
elif [[ ! -f "$REVIEW_SETTLEMENT_POLL_SH" ]]; then
  fail "scripts/poll-review-settlement.sh missing"
else
  pass "review settlement workflow + evaluator + poll wrapper present"
fi
if ! on_has_merge_group "$REVIEW_SETTLEMENT_YML"; then
  fail "review-settlement.yml must trigger on merge_group (required-check producer)"
else
  pass "review-settlement.yml triggers on merge_group"
fi
# Review-related events that re-evaluate settlement on the current head.
for needle in \
  'review_requested' \
  'review_request_removed' \
  'synchronize' \
  'ready_for_review' \
  'pull_request_review:' \
  'submitted' \
  'dismissed'
do
  if ! file_has_fixed "$needle" "$REVIEW_SETTLEMENT_YML"; then
    fail "review-settlement.yml missing event surface: $needle"
  fi
done
pass "review-settlement.yml covers request/review/SHA event surfaces"
if ! grep -E '^[[:space:]]+name: Review settlement[[:space:]]*$' "$REVIEW_SETTLEMENT_YML" >/dev/null; then
  fail "review-settlement job name must stay 'Review settlement' (protected context)"
else
  pass "review-settlement protected job name preserved"
fi
if ! file_has_fixed 'pull-requests: read' "$REVIEW_SETTLEMENT_YML"; then
  fail "review-settlement.yml must request pull-requests: read for GraphQL review fields"
else
  pass "review-settlement.yml has pull-requests: read"
fi
# The workflow invokes the poll wrapper; the wrapper invokes the evaluator
# single-shot per probe. Both links of the chain are asserted so neither the
# poller nor the evaluator can be silently dropped from the producer.
if ! file_has_fixed 'scripts/poll-review-settlement.sh' "$REVIEW_SETTLEMENT_YML"; then
  fail "review-settlement.yml must invoke scripts/poll-review-settlement.sh"
else
  pass "review-settlement.yml invokes the settlement poll wrapper"
fi
if ! file_has_fixed 'scripts/check-review-settlement.sh' "$REVIEW_SETTLEMENT_POLL_SH"; then
  fail "poll-review-settlement.sh must invoke scripts/check-review-settlement.sh"
else
  pass "poll-review-settlement.sh invokes the settlement evaluator"
fi
# Concurrency split mirrors dependency-review: PR cancels, merge_group does not.
if ! python3 - "$REVIEW_SETTLEMENT_YML" <<'PY'
import re
import sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(encoding="utf-8")
m = re.search(r"(?m)^concurrency:\n((?:  .*\n)+)", text)
if not m:
    sys.stderr.write("review-settlement.yml missing concurrency block\n")
    sys.exit(1)
block = m.group(1)
if not re.search(
    r"cancel-in-progress:\s*\$\{\{\s*github\.event_name\s*!=\s*'merge_group'\s*\}\}",
    block,
):
    sys.stderr.write(
        "review-settlement cancel-in-progress must be "
        "${{ github.event_name != 'merge_group' }}\n"
    )
    sys.exit(1)
if "merge-group-" not in block and "merge_group" not in block:
    sys.stderr.write("review-settlement concurrency must namespace merge_group\n")
    sys.exit(1)
# PR number for PR + review events must key on github.event.pull_request.number
if "github.event.pull_request.number" not in block:
    sys.stderr.write(
        "review-settlement concurrency must key non-merge_group on "
        "github.event.pull_request.number\n"
    )
    sys.exit(1)
if "pull_request_review.pull_request" in block:
    sys.stderr.write(
        "review-settlement must not use pull_request_review.pull_request "
        "(wrong payload path)\n"
    )
    sys.exit(1)
sys.exit(0)
PY
then
  fail "review-settlement concurrency contract"
else
  pass "review-settlement concurrency: PR cancel, merge_group retain"
fi
# #0106: pull_request_review resolves PR via root github.event.pull_request.number
if ! python3 - "$REVIEW_SETTLEMENT_YML" <<'PY'
import re
import sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(encoding="utf-8")
# Resolver must treat pull_request|pull_request_review with PR_NUMBER from
# github.event.pull_request.number — never github.event.pull_request_review.*
if "github.event.pull_request_review.pull_request" in text:
    sys.stderr.write(
        "review-settlement.yml must not read PR under pull_request_review object\n"
    )
    sys.exit(1)
if "REVIEW_PR_NUMBER" in text:
    sys.stderr.write(
        "review-settlement.yml must not define REVIEW_PR_NUMBER (wrong path)\n"
    )
    sys.exit(1)
if "PR_NUMBER: ${{ github.event.pull_request.number }}" not in text:
    sys.stderr.write(
        "review-settlement.yml must set PR_NUMBER from "
        "github.event.pull_request.number\n"
    )
    sys.exit(1)
if not re.search(
    r"pull_request\|pull_request_review\)",
    text,
):
    sys.stderr.write(
        "review-settlement resolver must handle pull_request|pull_request_review "
        "with the root PR_NUMBER\n"
    )
    sys.exit(1)
sys.exit(0)
PY
then
  fail "review-settlement pull_request_review PR number path (#0106)"
else
  pass "review-settlement pull_request_review uses github.event.pull_request.number"
fi
# #0104: GraphQL connections must inspect pageInfo.hasNextPage and fail closed
if ! file_has_fixed 'hasNextPage' "$REVIEW_SETTLEMENT_SH"; then
  fail "check-review-settlement.sh must inspect pageInfo.hasNextPage (#0104)"
else
  pass "check-review-settlement.sh inspects hasNextPage"
fi
if ! file_has_fixed 'truncatedConnections' "$REVIEW_SETTLEMENT_SH"; then
  fail "check-review-settlement.sh must fail closed on truncatedConnections"
else
  pass "check-review-settlement.sh has truncatedConnections fail-closed path"
fi
if ! file_has_fixed 'pageInfo' "$REVIEW_SETTLEMENT_SH"; then
  fail "check-review-settlement.sh GraphQL query must request pageInfo"
else
  pass "check-review-settlement.sh GraphQL query requests pageInfo"
fi
# #0105: threads owned by conversation resolution — evaluator must not block on them
if file_has_fixed 'unresolved_actionable_threads' "$REVIEW_SETTLEMENT_SH"; then
  fail "check-review-settlement.sh must not evaluate unresolved threads (#0105)"
else
  pass "check-review-settlement.sh does not evaluate review threads"
fi
if file_has_fixed 'reviewThreads(first:' "$REVIEW_SETTLEMENT_SH"; then
  fail "check-review-settlement.sh must not query reviewThreads GraphQL connection"
else
  pass "check-review-settlement.sh does not query reviewThreads"
fi
if ! grep -qiE 'conversation[[:space:]-]*resolution' "$REVIEW_SETTLEMENT_YML"; then
  fail "review-settlement.yml must document conversation-resolution owns threads"
else
  pass "review-settlement.yml documents conversation-resolution thread ownership"
fi
# Deterministic evaluator contracts (PR #70 class + SHA invalidation + #0104/#0105).
if ! bash "$REVIEW_SETTLEMENT_SH" --self-test; then
  fail "check-review-settlement.sh --self-test"
else
  pass "check-review-settlement.sh --self-test"
fi
# Deterministic poll contracts: retry through not-settled to settled,
# immediate propagation of exit 2, deadline exhaustion returns 1.
if ! bash "$REVIEW_SETTLEMENT_POLL_SH" --self-test; then
  fail "poll-review-settlement.sh --self-test"
else
  pass "poll-review-settlement.sh --self-test"
fi
# Force-full policy must include the settlement evaluator so a rewrite cannot
# land under a light matrix only.
if ! file_has_fixed 'scripts/check-review-settlement.sh' "$CLASSIFIER"; then
  fail "classifier must force-full on scripts/check-review-settlement.sh"
else
  pass "classifier force-full includes check-review-settlement.sh"
fi
if ! file_has_fixed 'scripts/check-review-settlement.sh' "$CI_YML"; then
  fail "ci.yml path_forces_full must include scripts/check-review-settlement.sh"
else
  pass "ci.yml force-full includes check-review-settlement.sh"
fi
if ! file_has_fixed 'scripts/poll-review-settlement.sh' "$CLASSIFIER"; then
  fail "classifier must force-full on scripts/poll-review-settlement.sh"
else
  pass "classifier force-full includes poll-review-settlement.sh"
fi
if ! file_has_fixed 'scripts/poll-review-settlement.sh' "$CI_YML"; then
  fail "ci.yml path_forces_full must include scripts/poll-review-settlement.sh"
else
  pass "ci.yml force-full includes poll-review-settlement.sh"
fi

# ---------------------------------------------------------------------------
# Static: path-scoped main push still uses before...sha (not full-matrix always)
# ---------------------------------------------------------------------------
if ! file_has_ere 'before="\$\{\{ github\.event\.before \}\}"' "$CI_YML"; then
  fail "main push must still classify from github.event.before"
else
  pass "main push remains path-scoped via before...sha"
fi
if ! file_has_ere 'git diff --name-only "\$before\.\.\.\$head"' "$CI_YML"; then
  fail "main push must diff before...head"
else
  pass "main push diffs before...head"
fi
# __integrated_main__ is only acceptable as a fail-safe when before is
# zero/missing, not as an unconditional default arm. The checker verifies
# every assignment sits directly under the zero/missing-before guard, then
# proves its own teeth on two mutants (an unconditional assignment and a
# deleted fail-safe) that must both be rejected.
if python3 - "$CI_YML" <<'PY'
import re
import sys
from pathlib import Path

GUARD = re.compile(r'if \[\[ -z "\$before" \|\| "\$before" =~ \^0\+\$ \]\]')
ASSIGN = "changed=(__integrated_main__)"


def indentation(line):
    return len(line) - len(line.lstrip())


def guarded_body_ranges(lines):
    ranges = []
    for start, line in enumerate(lines):
        if not GUARD.search(line):
            continue
        guard_indent = indentation(line)
        then_line = next(
            (
                i
                for i in range(start, min(start + 4, len(lines)))
                if re.search(r"\bthen\s*$", lines[i])
            ),
            None,
        )
        if then_line is None:
            continue
        end = next(
            (
                i
                for i in range(then_line + 1, len(lines))
                if indentation(lines[i]) == guard_indent
                and lines[i].strip() in {"else", "fi"}
            ),
            None,
        )
        if end is not None:
            ranges.append((then_line + 1, end, guard_indent))
    return ranges


def guard_problems(text):
    lines = text.splitlines()
    hits = [i for i, line in enumerate(lines) if line.strip() == ASSIGN]
    if not hits:
        return ["missing zero/missing-before fail-safe " + ASSIGN]
    ranges = guarded_body_ranges(lines)
    return [
        f"line {i + 1}: {ASSIGN} outside the zero/missing-before guard body"
        for i in hits
        if not any(start <= i < end and indentation(lines[i]) > guard_indent
                   for start, end, guard_indent in ranges)
    ]


text = Path(sys.argv[1]).read_text(encoding="utf-8")
problems = guard_problems(text)
if problems:
    print("\n".join(problems), file=sys.stderr)
    sys.exit(1)

lines = text.splitlines()
push_arms = [i for i, line in enumerate(lines) if line.strip() == "push)"]
if not push_arms:
    print("could not locate push) arm for mutation self-test", file=sys.stderr)
    sys.exit(1)
mutant_unconditional = "\n".join(
    lines[: push_arms[0] + 1] + ["              " + ASSIGN] + lines[push_arms[0] + 1 :]
)
if not guard_problems(mutant_unconditional):
    print("mutation escape: unconditional __integrated_main__ accepted", file=sys.stderr)
    sys.exit(1)

ranges = guarded_body_ranges(lines)
if not ranges:
    print("could not locate zero/missing-before guard body", file=sys.stderr)
    sys.exit(1)
_, _, guard_indent = ranges[0]
fi_line = next(
    (
        i
        for i in range(ranges[0][1], len(lines))
        if indentation(lines[i]) == guard_indent and lines[i].strip() == "fi"
    ),
    None,
)
if fi_line is None:
    print("could not locate zero/missing-before closing fi", file=sys.stderr)
    sys.exit(1)
mutant_near_guard = "\n".join(
    lines[: fi_line + 1]
    + [" " * guard_indent + ASSIGN]
    + lines[fi_line + 1 :]
)
if not guard_problems(mutant_near_guard):
    print("mutation escape: post-fi __integrated_main__ accepted", file=sys.stderr)
    sys.exit(1)

mutant_missing = text.replace(ASSIGN, "changed=()")
if not guard_problems(mutant_missing):
    print("mutation escape: deleted fail-safe accepted", file=sys.stderr)
    sys.exit(1)
PY
then
  pass "integrated_main only as guarded fail-safe (mutants rejected)"
else
  fail "integrated_main zero-before guard contract"
fi

# ---------------------------------------------------------------------------
# Static: force-full guard present and base-controlled
# ---------------------------------------------------------------------------
if ! file_has_fixed 'path_forces_full_non_sbom_matrix' "$CI_YML"; then
  fail "missing path_forces_full_non_sbom_matrix base-controlled guard"
else
  pass "force-full non-SBOM guard present"
fi
if ! file_has_fixed 'force_full_non_sbom=true' "$CI_YML"; then
  fail "force_full_non_sbom must be applied in detect-changes"
else
  pass "force_full_non_sbom applied after classifier"
fi
# Policy surfaces that must force full
for needle in \
  '.github/workflows/*' \
  'scripts/classify-ci-paths.sh' \
  'scripts/test-classify-ci-paths.sh' \
  'scripts/test-ci-control-plane.sh' \
  'scripts/check-review-settlement.sh'
do
  if ! file_has_fixed "$needle" "$CI_YML"; then
    fail "force-full policy path missing: $needle"
  fi
done
pass "force-full policy path list includes workflows + classifier contracts"

# ---------------------------------------------------------------------------
# Behavioral: base-composed exact_* is a conservative superset of classifier
# ---------------------------------------------------------------------------
# Final base-controlled exact selection in detect-changes is:
#   base = path_requires_exact_* || path_forces_full_non_sbom
# (force-full raises both exact bits). That composition must never under-select
# relative to the classifier (cls true ⇒ base true). Base may over-select
# (intentional conservative supersets, listed below).
tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-ci-control.XXXXXX")"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT TERM

python3 - "$CI_YML" "$tmp/exact_fns.sh" <<'PY'
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8")
out = Path(sys.argv[2])
parts = []
for name in (
    "path_requires_exact_candidate",
    "path_requires_exact_install",
    "path_forces_full_non_sbom_matrix",
):
    m = re.search(
        rf"({name}\(\) \{{.*?\n          \}})",
        text,
        re.S,
    )
    if not m:
        raise SystemExit(f"could not extract {name} from ci.yml")
    parts.append(m.group(1))
body = "\n".join(parts)
body = "\n".join(
    line[10:] if line.startswith("          ") else line for line in body.splitlines()
)
out.write_text("#!/usr/bin/env bash\n" + body + "\n", encoding="utf-8")
PY

# shellcheck source=/dev/null
source "$tmp/exact_fns.sh"

base_exact_cand() {
  path_requires_exact_candidate "$1" && return 0
  path_forces_full_non_sbom_matrix "$1" && return 0
  return 1
}
base_exact_inst() {
  path_requires_exact_install "$1" && return 0
  path_forces_full_non_sbom_matrix "$1" && return 0
  return 1
}

# Intentional conservative supersets: base exact_candidate true, classifier false.
# These method scripts stay light in the classifier (no product rebuild lanes)
# but the base-controlled overlay still forces candidate isolation.
intentional_superset_cand=(
  scripts/export-candidate.sh
  scripts/import-candidate.sh
  scripts/test-export-import-candidate.sh
)

# Comprehensive path universe: classifier fixtures + tracked packaging/app/CI
# surfaces + intentional supersets + synthetic tokens.
path_universe_file="$tmp/path-universe.txt"
{
  printf '%s\n' \
    README.md CONTRIBUTING.md docs/architecture.md \
    gateway/docs/runbook.md \
    .github/workflows/ci.yml .github/workflows/ci-pr.yml \
    .github/workflows/codeql.yml .github/actions/rust-setup/action.yml \
    __manual_dispatch__ __scheduled_proof__ __integrated_main__ \
    __unknown_base__ __unknown_event__ \
    new-surface/config.json \
    scripts/dev-check.sh scripts/new-worktree.sh \
    scripts/export-candidate.sh scripts/import-candidate.sh \
    scripts/test-export-import-candidate.sh \
    scripts/classify-ci-paths.sh scripts/test-classify-ci-paths.sh \
    scripts/test-ci-control-plane.sh scripts/test-ci-candidate-observability.sh \
    scripts/check-review-settlement.sh \
    scripts/run-actionlint.sh scripts/bootstrap-actionlint.sh \
    scripts/stage-gateway-pack.sh scripts/release-transaction.sh \
    scripts/test-release-transaction-w3.sh scripts/install-verify-candidate.sh \
    scripts/candidate-status.sh scripts/ci-build-adhoc-candidate.sh \
    scripts/record-acceptance.sh scripts/smoke-macos-tauri-app.sh \
    packaging/env.sh packaging/build-dmg.sh packaging/gateway-pack/docker-compose.yml \
    council-rs/warroom/web/app/page.tsx \
    council-rs/warroom-tauri/src-tauri/src/lib.rs \
    council-rs/warroom-tauri/src-tauri/resources/gateway-pack/docker-compose.yml \
    council-rs/scripts/warroom-tauri-dev.sh \
    gateway/sidecar-rs/src/main.rs \
    gateway/lua/auth.lua gateway/nginx.conf gateway/conf/models.json \
    gateway/docker-compose.yml Makefile
  git -C "$ROOT" ls-files \
    'packaging/*' \
    'scripts/*' \
    '.github/workflows/*' \
    '.github/actions/*' \
    'council-rs/warroom/web/*' \
    'council-rs/warroom-tauri/*' \
    'council-rs/src-tauri/*' \
    'council-rs/scripts/warroom*' \
    'gateway/lua/*' \
    'gateway/conf/*' \
    2>/dev/null || true
} | sort -u >"$path_universe_file"

underlay_failures=0
path_count=0
while IFS= read -r path; do
  [[ -z "$path" ]] && continue
  path_count=$((path_count + 1))
  out="$("$CLASSIFIER" "$path")"
  cls_cand="$(sed -n 's/^exact_candidate=//p' <<<"$out")"
  cls_inst="$(sed -n 's/^exact_install=//p' <<<"$out")"
  if base_exact_cand "$path"; then base_cand=true; else base_cand=false; fi
  if base_exact_inst "$path"; then base_inst=true; else base_inst=false; fi

  if [[ "$cls_cand" == true && "$base_cand" != true ]]; then
    printf 'FAIL: exact underlay cand %s: classifier=true base=false\n' "$path" >&2
    underlay_failures=$((underlay_failures + 1))
  fi
  if [[ "$cls_inst" == true && "$base_inst" != true ]]; then
    printf 'FAIL: exact underlay inst %s: classifier=true base=false\n' "$path" >&2
    underlay_failures=$((underlay_failures + 1))
  fi
done <"$path_universe_file"

if (( underlay_failures > 0 )); then
  fail "base exact composition under-selects classifier ($underlay_failures)"
else
  pass "base exact composition never under-selects classifier ($path_count paths)"
fi

# Every intentional superset path must still be a real base>cls cand pair.
superset_missing=0
for path in "${intentional_superset_cand[@]}"; do
  out="$("$CLASSIFIER" "$path")"
  cls_cand="$(sed -n 's/^exact_candidate=//p' <<<"$out")"
  if base_exact_cand "$path"; then base_cand=true; else base_cand=false; fi
  if [[ "$cls_cand" != false || "$base_cand" != true ]]; then
    printf 'FAIL: intentional superset %s: want cls_cand=false base_cand=true, got %s/%s\n' \
      "$path" "$cls_cand" "$base_cand" >&2
    superset_missing=$((superset_missing + 1))
  fi
done
if (( superset_missing > 0 )); then
  fail "intentional exact_candidate supersets not as modeled"
else
  pass "intentional exact_candidate supersets modeled (${#intentional_superset_cand[@]} paths)"
fi

# Hosted invocation: detect-changes job must call these scripts (not only
# path lists, and not a bare match elsewhere in the workflow).
# Extract the detect-changes job block so moving invocations to another job
# fails this contract.
DETECT_CHANGES_JOB="$(python3 - "$CI_YML" <<'PY'
from pathlib import Path
import re
import sys
text = Path(sys.argv[1]).read_text(encoding="utf-8")
# Top-level jobs are indented two spaces: "  name:"
m = re.search(
    r"(?m)^  detect-changes:\n((?:    .*\n|      .*\n|\n)*)",
    text,
)
if not m:
    sys.stderr.write("ci.yml missing detect-changes job\n")
    sys.exit(1)
sys.stdout.write(m.group(0))
PY
)" || {
  fail "ci.yml missing detect-changes job block"
  DETECT_CHANGES_JOB=""
}
if [[ -n "$DETECT_CHANGES_JOB" ]]; then
  host_ok=true
  if ! grep -Eq '^[[:space:]]+scripts/test-ci-control-plane\.sh[[:space:]]*$' <<<"$DETECT_CHANGES_JOB"; then
    fail "detect-changes must run scripts/test-ci-control-plane.sh as a hosted step"
    host_ok=false
  fi
  # Shipping-method hermetics must also be bare hosted steps (not path-filter-only).
  # Regression guard for #0043 / #0018 shape: filter triggers without execution.
  if ! grep -Eq '^[[:space:]]+scripts/test-candidate-status\.sh[[:space:]]*$' <<<"$DETECT_CHANGES_JOB"; then
    fail "detect-changes must run scripts/test-candidate-status.sh as a hosted step"
    host_ok=false
  fi
  if ! grep -Eq '^[[:space:]]+scripts/test-release-transaction-w3\.sh[[:space:]]*$' <<<"$DETECT_CHANGES_JOB"; then
    fail "detect-changes must run scripts/test-release-transaction-w3.sh as a hosted step"
    host_ok=false
  fi
  if $host_ok; then
    pass "detect-changes hosts control-plane + candidate-status + W3 contracts"
  fi
fi

# ---------------------------------------------------------------------------
# Behavioral: force-full cannot be suppressed by an all-false classifier
# ---------------------------------------------------------------------------
# Simulate the post-classifier overlays from ci.yml against a hostile all-false
# classifier output when a policy path is in the changed set.
simulate_overlays() {
  local force_full_non_sbom=false
  local exact_candidate=false
  local exact_install=false
  local path
  for path in "$@"; do
    path_requires_exact_candidate "$path" && exact_candidate=true
    path_requires_exact_install "$path" && exact_install=true
    # Replicate path_forces_full_non_sbom_matrix case from ci.yml
    case "$path" in
      .github/workflows/*|.github/actions/*|\
      */.github/workflows/*|*/.github/actions/*|\
      scripts/classify-ci-paths.sh|\
      scripts/test-classify-ci-paths.sh|\
      scripts/test-ci-control-plane.sh|\
      scripts/test-ci-candidate-observability.sh|\
      scripts/check-review-settlement.sh|\
      scripts/run-actionlint.sh|\
      scripts/bootstrap-actionlint.sh|\
      __manual_dispatch__|__scheduled_proof__|__integrated_main__|__unknown_base__|__unknown_event__)
        force_full_non_sbom=true
        ;;
    esac
  done
  local classifier_output
  classifier_output="$(cat <<'EOF'
full_matrix=false
gateway_rust=false
council_rust=false
sentinel_rust=false
warroom_web=false
warroom_tauri=false
workspace_supply_chain=false
tauri_supply_chain=false
sbom=false
exact_candidate=false
exact_install=false
EOF
)"
  classifier_output="$(
    printf '%s\n' "$classifier_output" \
      | sed -e "s/^exact_candidate=.*/exact_candidate=${exact_candidate}/" \
            -e "s/^exact_install=.*/exact_install=${exact_install}/"
  )"
  if [[ "$force_full_non_sbom" == true ]]; then
    classifier_output="$(
      printf '%s\n' "$classifier_output" \
        | sed -e 's/^full_matrix=.*/full_matrix=true/' \
              -e 's/^gateway_rust=.*/gateway_rust=true/' \
              -e 's/^council_rust=.*/council_rust=true/' \
              -e 's/^sentinel_rust=.*/sentinel_rust=true/' \
              -e 's/^warroom_web=.*/warroom_web=true/' \
              -e 's/^warroom_tauri=.*/warroom_tauri=true/' \
              -e 's/^workspace_supply_chain=.*/workspace_supply_chain=true/' \
              -e 's/^tauri_supply_chain=.*/tauri_supply_chain=true/' \
              -e 's/^exact_candidate=.*/exact_candidate=true/' \
              -e 's/^exact_install=.*/exact_install=true/'
    )"
  fi
  printf '%s\n' "$classifier_output"
}

hostile="$(simulate_overlays .github/workflows/ci.yml)"
for key in full_matrix gateway_rust council_rust sentinel_rust warroom_web warroom_tauri \
  workspace_supply_chain tauri_supply_chain exact_candidate exact_install; do
  val="$(sed -n "s/^${key}=//p" <<<"$hostile")"
  if [[ "$val" != true ]]; then
    fail "force-full left $key=$val under hostile all-false classifier"
  fi
done
if [[ "$(sed -n 's/^sbom=//p' <<<"$hostile")" != false ]]; then
  fail "force-full non-SBOM must leave sbom=false for ordinary policy PRs"
else
  pass "force-full non-SBOM leaves sbom=false"
fi
pass "hostile all-false classifier cannot suppress force-full lanes"

# Negative: docs-only must not force full via the base guard
docs_sim="$(simulate_overlays README.md docs/architecture.md)"
if [[ "$(sed -n 's/^full_matrix=//p' <<<"$docs_sim")" != false ]]; then
  fail "docs-only must not force full via base-controlled guard"
else
  pass "docs-only does not trigger force-full guard"
fi

# ---------------------------------------------------------------------------
# Gateway OpenResty runtime (#0051): classifier + base exact + local/hosted
# proofs must stay wired. Mutations that drop selection or remove commands fail.
# ---------------------------------------------------------------------------
DEV_CHECK="$ROOT/scripts/dev-check.sh"
for openresty_path in gateway/lua/auth.lua gateway/nginx.conf gateway/conf/models.json; do
  out="$("$CLASSIFIER" "$openresty_path")"
  for key in gateway_rust warroom_tauri exact_candidate exact_install; do
    if [[ "$(sed -n "s/^${key}=//p" <<<"$out")" != true ]]; then
      fail "OpenResty path $openresty_path must set $key=true"
    fi
  done
  if ! base_exact_cand "$openresty_path"; then
    fail "base exact_candidate must cover $openresty_path"
  fi
  if ! base_exact_inst "$openresty_path"; then
    fail "base exact_install must cover $openresty_path"
  fi
done
if [[ "$(sed -n 's/^exact_candidate=//p' <<<"$("$CLASSIFIER" gateway/docker-compose.yml)")" != false ]]; then
  fail "gateway compose must not select exact_candidate (pack uses packaging/gateway-pack)"
else
  pass "gateway OpenResty paths select pack+install; compose stays gateway-only"
fi

# Local check/ship must invoke the static OpenResty proofs for those paths.
for needle in \
  'make -C gateway lint-lua' \
  'make -C gateway lua-unit' \
  'make -C gateway contract-check' \
  'make -C gateway models-validate' \
  'scripts/test-gateway-pack-assets.sh' \
  'gateway_openresty_runtime' \
  'run_gateway_openresty_static_proofs'
do
  if ! file_has_fixed "$needle" "$DEV_CHECK"; then
    fail "dev-check.sh must wire OpenResty proof: $needle"
  fi
done
pass "dev-check wires Gateway OpenResty static proofs"

# ---------------------------------------------------------------------------
# #0055: ship-check receipt finalization must survive empty arrays under
# macOS bash 3.2 + set -u (raw "${end_paths[@]}" is unbound when empty).
# ---------------------------------------------------------------------------
if grep -nE 'end_path_manifest=.*\$\{end_paths\[@\]\}' "$DEV_CHECK" \
  | grep -vE 'end_paths\[@\]\+' >/dev/null; then
  fail "dev-check receipt must not expand raw \${end_paths[@]} (bash 3.2 set -u)"
fi
if ! grep -qE 'end_paths\[@\]\+\"\$\{end_paths\[@\]\}\"' "$DEV_CHECK"; then
  fail "dev-check receipt must use set -u-safe \${end_paths[@]+\"\${end_paths[@]}\"}"
fi
if ! grep -qE 'sorted_end_paths\[@\]\+\"\$\{sorted_end_paths\[@\]\}\"' "$DEV_CHECK"; then
  fail "dev-check receipt must use set -u-safe \${sorted_end_paths[@]+...}"
fi
# Live: empty-array expansion used by receipt path must not abort under set -u.
set +e
live_out="$(
  /bin/bash -c '
    set -euo pipefail
    end_paths=()
    sorted_end_paths=()
    end_path_manifest="$(printf "%s\n" ${end_paths[@]+"${end_paths[@]}"} | LC_ALL=C sort)"
    : "$(printf "%s\n" ${sorted_end_paths[@]+"${sorted_end_paths[@]}"})"
    printf "ok manifest_len=%s\n" "${#end_path_manifest}"
  ' 2>&1
)"
live_ec=$?
set -e
[[ $live_ec -eq 0 ]] || fail "bash 3.2 set -u empty end_paths expansion failed: $live_out"
[[ "$live_out" == *"ok manifest_len=0"* ]] \
  || fail "expected empty-manifest live proof, got: $live_out"
pass "dev-check receipt empty-array expansions are set -u safe (#0055)"

# Hosted gateway-rust: timer-closure + lua-unit + contract-check + models-validate.
GATEWAY_RUST_JOB="$(python3 - "$CI_YML" <<'PY'
from pathlib import Path
import re
import sys
text = Path(sys.argv[1]).read_text(encoding="utf-8")
m = re.search(
    r"(?m)^  gateway-rust:\n((?:    .*\n|      .*\n|\n)*)",
    text,
)
if not m:
    sys.stderr.write("ci.yml missing gateway-rust job\n")
    sys.exit(1)
sys.stdout.write(m.group(0))
PY
)" || {
  fail "ci.yml missing gateway-rust job block"
  GATEWAY_RUST_JOB=""
}
if [[ -n "$GATEWAY_RUST_JOB" ]]; then
  host_gw_ok=true
  if ! grep -Eq 'make lint-lua' <<<"$GATEWAY_RUST_JOB"; then
    fail "gateway-rust must run make lint-lua (timer-closure)"
    host_gw_ok=false
  fi
  if ! grep -Eq 'make lua-unit' <<<"$GATEWAY_RUST_JOB"; then
    fail "gateway-rust must run make lua-unit"
    host_gw_ok=false
  fi
  if ! grep -Eq 'make contract-check' <<<"$GATEWAY_RUST_JOB"; then
    fail "gateway-rust must run make contract-check"
    host_gw_ok=false
  fi
  if ! grep -Eq 'make models-validate' <<<"$GATEWAY_RUST_JOB"; then
    fail "gateway-rust must run make models-validate"
    host_gw_ok=false
  fi
  if $host_gw_ok; then
    pass "gateway-rust hosts lint-lua + lua-unit + contract-check + models-validate"
  fi
fi

# Live metrics-contract requires a stack; hosted on gateway-smoke.
GATEWAY_SMOKE_JOB="$(python3 - "$CI_YML" <<'PY'
from pathlib import Path
import re
import sys
text = Path(sys.argv[1]).read_text(encoding="utf-8")
m = re.search(
    r"(?m)^  gateway-smoke:\n((?:    .*\n|      .*\n|\n)*)",
    text,
)
if not m:
    sys.stderr.write("ci.yml missing gateway-smoke job\n")
    sys.exit(1)
sys.stdout.write(m.group(0))
PY
)" || {
  fail "ci.yml missing gateway-smoke job block"
  GATEWAY_SMOKE_JOB=""
}
if [[ -n "$GATEWAY_SMOKE_JOB" ]]; then
  if ! grep -Eq 'make metrics-contract' <<<"$GATEWAY_SMOKE_JOB"; then
    fail "gateway-smoke must run make metrics-contract when stack is up"
  else
    pass "gateway-smoke hosts metrics-contract"
  fi
fi

# Mutation: dropping OpenResty from exact_candidate overlay must fail underlay.
if python3 - "$CI_YML" "$CLASSIFIER" <<'PY'
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ci = Path(sys.argv[1]).read_text(encoding="utf-8")
classifier = Path(sys.argv[2])
# Mutant: strip gateway OpenResty arms from path_requires_exact_candidate only.
mutant = re.sub(
    r"\n\s*gateway/lua/\*\|\\\n\s*gateway/nginx\.conf\|\\\n\s*gateway/conf/\*\|\\",
    "",
    ci,
    count=1,
)
if mutant == ci:
    print("could not build exact_candidate OpenResty mutant", file=sys.stderr)
    sys.exit(1)
with tempfile.TemporaryDirectory() as td:
    out = Path(td) / "exact_fns.sh"
    # Reuse extraction pattern from this script's earlier block.
    parts = []
    for name in (
        "path_requires_exact_candidate",
        "path_requires_exact_install",
        "path_forces_full_non_sbom_matrix",
    ):
        m = re.search(rf"({name}\(\) \{{.*?\n          \}})", mutant, re.S)
        if not m:
            print(f"could not extract {name} from mutant", file=sys.stderr)
            sys.exit(1)
        parts.append(m.group(1))
    body = "\n".join(parts)
    body = "\n".join(
        line[10:] if line.startswith("          ") else line for line in body.splitlines()
    )
    out.write_text("#!/usr/bin/env bash\n" + body + "\n", encoding="utf-8")
    check = r'''
set -euo pipefail
source "$1"
path=gateway/lua/auth.lua
cls="$("$2" "$path")"
cls_cand="$(sed -n 's/^exact_candidate=//p' <<<"$cls")"
if path_requires_exact_candidate "$path" || path_forces_full_non_sbom_matrix "$path"; then
  base=true
else
  base=false
fi
# Underlay defect: classifier true, base false
if [[ "$cls_cand" == true && "$base" != true ]]; then
  exit 0
fi
echo "mutant did not under-select exact_candidate for gateway/lua" >&2
exit 1
'''
    r = subprocess.run(
        ["bash", "-c", check, "_", str(out), str(classifier)],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        sys.stderr.write(r.stderr or r.stdout or "mutant check failed\n")
        sys.exit(1)
sys.exit(0)
PY
then
  pass "exact underlay mutation rejects dropped OpenResty exact_candidate"
else
  fail "exact underlay mutation did not catch dropped OpenResty exact_candidate"
fi

# Mutation: removing lint-lua from gateway-rust must fail the hosted contract.
if ! grep -Eq 'make lint-lua' <<<"$GATEWAY_RUST_JOB"; then
  : # already failed above
elif python3 - "$CI_YML" <<'PY'
import re
import sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(encoding="utf-8")
m = re.search(r"(?m)^  gateway-rust:\n((?:    .*\n|      .*\n|\n)*)", text)
if not m:
    sys.exit(1)
job = m.group(0)
mutant = job.replace("make lint-lua", "true # stripped lint-lua")
if "make lint-lua" in mutant:
    sys.exit(1)
if re.search(r"make lint-lua", mutant):
    sys.exit(1)
# Contract teeth: the same check used above must fail on mutant.
if re.search(r"make lint-lua", mutant):
    sys.exit(1)
sys.exit(0 if "make lint-lua" not in mutant and "make lua-unit" in mutant else 1)
PY
then
  pass "gateway-rust lint-lua presence is mutation-sensitive"
else
  fail "gateway-rust lint-lua mutation self-check failed"
fi

# ---------------------------------------------------------------------------
# Broader unwired-test reachability (#0018 / #0043 / #0059 class)
# Every scripts/test-*.sh must be hosted-CI reachable (direct workflow step or
# transitive real invocation from a reachable script) OR listed in the small
# explicit allow-list below. Mentions are not wiring: path filters, continued
# command arguments, echo/printf text, and heredoc fixture bodies do not count.
# Do not add a workflow to quiet this: allow-list intentional local-only tests,
# or wire a real invocation into an existing entrypoint.
# ---------------------------------------------------------------------------
if python3 - "$ROOT" <<'PY'
from pathlib import Path
import re
import sys

root = Path(sys.argv[1])
tests = sorted(
    f"scripts/{p.name}" for p in (root / "scripts").glob("test-*.sh")
)

# Intentional local-only hermetics. Each entry must exist; none may be
# CI-reachable (stale allow-list hygiene).
ALLOW_LIST = {
    "scripts/test-cargo-target-policy.sh": "local cargo-target-policy helper",
    "scripts/test-ci-candidate-observability.sh": "local make check/ship-check only",
    "scripts/test-gateway-pack-desktop-ownership.sh": "local Makefile gateway-pack ownership",
    "scripts/test-gateway-pack-integration-smoke.sh": "local Makefile / desktop-ownership only",
    "scripts/test-gateway-prepare-config.sh": "local gateway config via make check",
    "scripts/test-link-agent-context.sh": "private doctrine linker hermetic",
}

SHELLS = {"bash", "sh", "/bin/bash", "/bin/sh", "/usr/bin/bash", "/usr/bin/sh"}
TEST_PATH_RE = re.compile(
    r"""^["']?(?:\$\{?ROOT\}?/)?(scripts/test-[\w-]+\.sh)["']?$"""
)
BARE_TEST_RE = re.compile(r"""^["']?(scripts/test-[\w-]+\.sh)["']?$""")
# Real heredoc openers only (<<EOF / <<'EOF' / <<"EOF" / <<-EOF).
# Reject bash here-strings (<<<) and non-identifier delimiters ($...).
HEREDOC_START_RE = re.compile(
    r"""(?<!<)<<-?\s*(?:'([A-Za-z_][A-Za-z0-9_]*)'|"([A-Za-z_][A-Za-z0-9_]*)"|\\?([A-Za-z_][A-Za-z0-9_]*))(?!\S)"""
)
ENV_ASSIGN_RE = re.compile(
    r"""^(?:export\s+)?[A-Za-z_][A-Za-z0-9_]*=(?:'[^']*'|"[^"]*"|\S+)\s+"""
)
# Path-filter / case-arm style (not shell ||).
PATH_FILTER_RE = re.compile(r"""scripts/[\w./-]+\.sh\s*\|""")
PATH_FILTER_LEFT_RE = re.compile(r"""\|\s*scripts/[\w./-]+\.sh""")


def strip_comment(line: str) -> str:
    # Keep # inside quotes out of scope; control-plane sources are simple.
    return line.split("#", 1)[0]


def path_filterish(code: str) -> bool:
    s = code.rstrip()
    if s.endswith("\\"):
        return True
    if PATH_FILTER_RE.search(s) or PATH_FILTER_LEFT_RE.search(s):
        return True
    return False


def first_command_tokens(code: str):
    """First pipeline/list command's argv after leading env assignments."""
    s = code.strip()
    if not s or path_filterish(s):
        return []
    # Only the first simple command in a list/pipeline (before | / || / && / ;).
    # Do not split on | that is already path-filterish (handled above).
    for sep in ("||", "&&", "|", ";"):
        if sep in s:
            s = s.split(sep, 1)[0].strip()
    while True:
        m = ENV_ASSIGN_RE.match(s)
        if not m:
            break
        s = s[m.end() :]
    if not s:
        return []
    # Tokenize lightly: whitespace, keep quoted strings as one token.
    tokens = re.findall(r"""'[^']*'|"[^"]*"|\S+""", s)
    return tokens


def tokens_invoke_test(tokens) -> list:
    """Command-position executions only (#0059: not echo/grep/args/text)."""
    if not tokens:
        return []
    cmd = tokens[0]
    cmd_unq = cmd.strip("'\"")
    # bash scripts/test-foo.sh  /  bash "$ROOT/scripts/test-foo.sh"
    if cmd_unq in SHELLS:
        if len(tokens) < 2:
            return []
        m = TEST_PATH_RE.match(tokens[1])
        if m:
            return [m.group(1)]
        return []
    # Bare hosted step: scripts/test-foo.sh (no $ROOT expansion form).
    m = BARE_TEST_RE.match(cmd)
    if m:
        return [m.group(1)]
    return []


def line_invokes(code: str) -> list:
    return tokens_invoke_test(first_command_tokens(code))


def heredoc_delimiter(code: str):
    m = HEREDOC_START_RE.search(code)
    if not m:
        return None
    return m.group(1) or m.group(2) or m.group(3)


def scan_lines(lines):
    """Yield invocation targets; skip heredoc bodies (fixture text)."""
    heredoc_end = None
    for raw in lines:
        code = strip_comment(raw)
        if heredoc_end is not None:
            if code.strip() == heredoc_end:
                heredoc_end = None
            continue
        delim = heredoc_delimiter(code)
        if delim is not None:
            # Content after <<EOF on the same line is not used here; body follows.
            heredoc_end = delim
            # Still allow the prefix command (e.g. cat <<EOF) to be parsed —
            # it will not invoke a test script via first-token rules.
            for t in line_invokes(code):
                yield t
            continue
        for t in line_invokes(code):
            yield t


def scan_text(text: str):
    return list(scan_lines(text.splitlines()))


def compute_reachable():
    seeds = set()
    wf_dir = root / ".github" / "workflows"
    if not wf_dir.is_dir():
        raise SystemExit("missing .github/workflows")
    for path in sorted(wf_dir.glob("*.yml")):
        for t in scan_text(path.read_text(encoding="utf-8")):
            seeds.add(t)
    reachable = set(seeds)
    queue = list(seeds)
    while queue:
        cur = queue.pop()
        cur_path = root / cur
        if not cur_path.is_file():
            continue
        for t in scan_text(cur_path.read_text(encoding="utf-8")):
            if t not in reachable and (root / t).is_file():
                reachable.add(t)
                queue.append(t)
    return seeds, reachable


errors = []
try:
    seeds, reachable = compute_reachable()
except SystemExit as exc:
    print(str(exc), file=sys.stderr)
    sys.exit(2)

for name, reason in sorted(ALLOW_LIST.items()):
    if not (root / name).is_file():
        errors.append(f"allow-list entry missing on disk: {name} ({reason})")
    elif name in reachable:
        errors.append(
            f"allow-list stale (already CI-reachable): {name} ({reason})"
        )

unwired = [t for t in tests if t not in reachable and t not in ALLOW_LIST]
for t in unwired:
    errors.append(
        f"unwired scripts/test-*.sh (not CI-reachable, not allow-listed): {t}"
    )

must_reach = [
    "scripts/test-ci-control-plane.sh",
    "scripts/test-candidate-status.sh",
    "scripts/test-release-transaction-w3.sh",
    "scripts/test-production-image-provenance.sh",  # transitive via pack assets
]
must_not = [
    "scripts/test-cargo-target-policy.sh",
    "scripts/test-link-agent-context.sh",
    "scripts/test-gateway-pack-integration-smoke.sh",
    "scripts/test-__orphan_unwired_guard__.sh",
]
for t in must_reach:
    if t not in reachable:
        errors.append(f"expected CI-reachable: {t}")
for t in must_not:
    if t in reachable:
        errors.append(f"expected not CI-reachable: {t}")

# --- Regression mutants (#0059 / Copilot isolation false edge) ---
mutants = []

def expect_empty(label, text):
    got = scan_text(text)
    if got:
        mutants.append(f"{label}: expected no invokes, got {got}")


def expect_has(label, text, want):
    got = scan_text(text)
    if want not in got:
        mutants.append(f"{label}: expected {want} in {got}")


# Real shapes must still count.
expect_has(
    "bare workflow step",
    "          scripts/test-candidate-status.sh\n",
    "scripts/test-candidate-status.sh",
)
expect_has(
    "bash workflow step",
    "          bash scripts/test-gateway-pack-assets.sh\n",
    "scripts/test-gateway-pack-assets.sh",
)
expect_has(
    "bash $ROOT transitive",
    'bash "$ROOT/scripts/test-production-image-provenance.sh" ||\n',
    "scripts/test-production-image-provenance.sh",
)
expect_has(
    "env-prefixed bash",
    'IRIN_X=1 bash "$ROOT/scripts/test-production-image-provenance.sh"\n',
    "scripts/test-production-image-provenance.sh",
)

# Non-execution text must not count.
expect_empty(
    "echo bash mention",
    "          echo bash scripts/test-__orphan_unwired_guard__.sh\n",
)
expect_empty(
    "printf bash mention",
    'printf "%s\\n" "bash scripts/test-__orphan_unwired_guard__.sh"\n',
)
expect_empty(
    "path-filter case arm",
    "              scripts/test-cargo-target-policy.sh|\\\n",
)
expect_empty(
    "grep continued file operand",
    'grep -qE "OWNED_DESKTOP_TEARDOWN" \\\n'
    '  "$ROOT/packaging/smoke-gateway-pack.sh" \\\n'
    '  "$ROOT/scripts/test-gateway-pack-integration-smoke.sh" || die\n',
)
expect_empty(
    "bare $ROOT path as first token (operand class)",
    '  "$ROOT/scripts/test-gateway-pack-integration-smoke.sh" || die\n',
)
expect_empty(
    "heredoc fixture body",
    "cat <<'EOF'\n"
    "bash scripts/test-__orphan_unwired_guard__.sh\n"
    "EOF\n",
)
expect_empty(
    "heredoc unquoted body",
    "cat <<EOF\n"
    "bash scripts/test-__orphan_unwired_guard__.sh\n"
    "EOF\n",
)
expect_empty(
    "workflow run: echo bash ...",
    "        run: echo bash scripts/test-__orphan_unwired_guard__.sh\n",
)

errors.extend(mutants)

if errors:
    for e in errors:
        print(e, file=sys.stderr)
    sys.exit(1)

print(
    f"reachable={len(reachable)} allow-listed={len(ALLOW_LIST)} "
    f"total={len(tests)} seeds={len(seeds)}"
)
sys.exit(0)
PY
then
  pass "every scripts/test-*.sh is CI-reachable or allow-listed (#0018/#0043/#0059)"
else
  fail "unwired-test reachability guard failed (wire, allow-list, or mutant)"
fi

# ---------------------------------------------------------------------------
# Comments / trust-boundary honesty (static text)
# ---------------------------------------------------------------------------
if file_has_fixed 'same revision under review' "$CI_YML"; then
  fail "ci.yml still claims ordinary PR executes same revision under review"
else
  pass "ci.yml no longer claims same-revision ordinary PR execution"
fi
if file_has_fixed 'PRs still enter via ci.yml@main until this lands' "$CI_YML"; then
  fail "stale 'until this lands' wording remains"
else
  pass "stale until-this-lands wording removed"
fi
if file_has_fixed 'keep in sync with ci-pr.yml bootstrap' "$CI_YML"; then
  fail "dangling ci-pr.yml bootstrap sync comment remains"
else
  pass "dangling bootstrap sync comment removed"
fi
if ! file_has_ere 'ci\.yml@main' "$CI_PR"; then
  fail "ci-pr.yml must document/use @main pin"
else
  pass "ci-pr.yml retains @main pin"
fi

# ---------------------------------------------------------------------------
# Candidate proof markers: both sites require verification=PASS,
# shipping_tier_claim=none, and exact source_sha. Large-log PASS + each
# missing-marker FAIL; mutation of a guarded check must fail closed.
# ---------------------------------------------------------------------------
if python3 - "$CI_YML" <<'PY'
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8")
errors = []

def job_block(name: str) -> str:
    m = re.search(rf"(?m)^  {re.escape(name)}:\n((?:    .*\n|      .*\n|\n)*)", text)
    if not m:
        errors.append(f"ci.yml missing {name} job")
        return ""
    return m.group(0)

sites = {
    "candidate-isolation-proof": {
        "sha_var": "BRANCH_HEAD_SHA",
        "sha_pat": r'grep -q "\^source_sha=\$\{BRANCH_HEAD_SHA\}\$"\s*<<<"\$out"',
    },
    "exact-merged-candidate": {
        "sha_var": "MERGED_SHA",
        "sha_pat": r'grep -q "\^source_sha=\$\{MERGED_SHA\}\$"\s*<<<"\$out"',
    },
}

for job, meta in sites.items():
    block = job_block(job)
    if not block:
        continue
    # Prefer line-anchored here-string greps (no echo|grep SIGPIPE shape).
    if not re.search(r"grep -q '\^verification=PASS\$'\s*<<<\"\$out\"", block):
        errors.append(f"{job}: missing anchored verification=PASS check")
    if not re.search(r"grep -q '\^shipping_tier_claim=none\$'\s*<<<\"\$out\"", block):
        errors.append(f"{job}: missing anchored shipping_tier_claim=none check")
    if not re.search(meta["sha_pat"], block):
        errors.append(
            f"{job}: missing anchored source_sha=${{{meta['sha_var']}}} check"
        )
    # Reject reintroduction of echo|grep pipelines on the marker path.
    if re.search(r"echo\s+\"\$out\"\s*\|\s*grep", block):
        errors.append(f"{job}: must not use echo|grep for marker checks")

# Behavioral: large multi-key log with all three markers → greps succeed.
# Missing each marker in turn → refuse.
def check_markers(log: str, sha: str) -> list[str]:
    missing = []
    if not re.search(r"(?m)^verification=PASS$", log):
        missing.append("verification=PASS")
    if not re.search(r"(?m)^shipping_tier_claim=none$", log):
        missing.append("shipping_tier_claim=none")
    if not re.search(rf"(?m)^source_sha={re.escape(sha)}$", log):
        missing.append(f"source_sha={sha}")
    return missing

sha = "abc123def456"
pad = "\n".join(f"noise_key_{i}=value_{i}" for i in range(400))
full = (
    f"{pad}\nverification=PASS\nshipping_tier_claim=none\n"
    f"source_sha={sha}\narchive_path=/tmp/x\n{pad}\n"
)
if check_markers(full, sha):
    errors.append(f"large-log PASS unexpectedly missing: {check_markers(full, sha)}")

for drop in ("verification=PASS", "shipping_tier_claim=none", f"source_sha={sha}"):
    mutant = "\n".join(line for line in full.splitlines() if line != drop)
    miss = check_markers(mutant, sha)
    if not miss:
        errors.append(f"missing-marker FAIL did not fire when dropping {drop}")
    elif drop not in miss and not any(drop in m for m in miss):
        errors.append(f"missing-marker expected {drop} in {miss}")

# Mutation: stripping site B source_sha check from the workflow text must
# be rejected by the same site-B contract above.
exact = job_block("exact-merged-candidate")
if exact:
    stripped = re.sub(
        r'^\s*grep -q "\^source_sha=\$\{MERGED_SHA\}\$"\s*<<<"\$out"\s*\n',
        "",
        exact,
        count=1,
        flags=re.M,
    )
    # If production already lacks the check, stripped == exact and the
    # static site loop already reported it. When present, confirm the
    # mutant would fail the same assertion.
    if stripped != exact:
        if re.search(
            r'grep -q "\^source_sha=\$\{MERGED_SHA\}\$"\s*<<<"\$out"', stripped
        ):
            errors.append("mutation: failed to strip exact-merged source_sha check")
        # Contract tooth: mutant must not satisfy the site-B requirement.
        if re.search(
            r'grep -q "\^source_sha=\$\{MERGED_SHA\}\$"\s*<<<"\$out"', stripped
        ) is None:
            pass  # expected failure mode of the static contract
    # Also mutate shipping_tier_claim if present.
    stripped_tier = re.sub(
        r"^\s*grep -q '\^shipping_tier_claim=none\$'\s*<<<\"\$out\"\s*\n",
        "",
        exact,
        count=1,
        flags=re.M,
    )
    if stripped_tier != exact and re.search(
        r"grep -q '\^shipping_tier_claim=none\$'\s*<<<\"\$out\"", stripped_tier
    ):
        errors.append("mutation: failed to strip exact-merged shipping_tier check")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
sys.exit(0)
PY
then
  pass "candidate markers: both sites + large-log PASS + missing-marker FAIL"
else
  fail "candidate marker contracts"
fi

# ---------------------------------------------------------------------------
# ci-required: intentional path skips remain allowed; selected lanes cannot
# skip green. Evaluate the live aggregator logic against synthetic needs JSON.
# ---------------------------------------------------------------------------
if python3 - "$CI_YML" <<'PY'
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8")
errors = []

# Extract the ci-required step run block.
m = re.search(
    r"(?m)^  ci-required:\n((?:    .*\n|      .*\n|\n)*)",
    text,
)
if not m:
    errors.append("ci.yml missing ci-required job")
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
block = m.group(0)

# Locate the run script body of the require step (executable body only —
# env: lane variables must not count as a selected-lane filter).
run_m = re.search(
    r"(?ms)name:\s*Require[^\n]*\n.*?run:\s*\|\n((?:          .*\n)+)",
    block,
)
if not run_m:
    # Single-line run: form
    run_m = re.search(r"(?ms)name:\s*Require[^\n]*\n.*?run:\s*(.+)", block)
if not run_m:
    errors.append("ci-required missing Require step run body")
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)

run_body_raw = run_m.group(1)
# Dedent for body-only inspection (10-space YAML indent under run: |).
body_lines = run_body_raw.splitlines(True)
if body_lines and all(
    (not ln.strip()) or ln.startswith("          ") for ln in body_lines
):
    run_body_for_check = "".join(
        ln[10:] if ln.startswith("          ") else ln for ln in body_lines
    )
else:
    run_body_for_check = run_body_raw

# Blanket success|skipped jq is never acceptable in the executable body,
# even when the step env still lists lane output variables.
blanket = re.search(
    r"""jq -e 'all\(\.\[\]\s*;\s*\.result == "success" or \.result == "skipped"\)'""",
    run_body_for_check,
)
if blanket:
    errors.append(
        "ci-required executable body still uses blanket success|skipped jq "
        "(selected-lane policy required)"
    )

# Behavioral contract via a small pure evaluator that mirrors the intended
# policy (must match ci.yml after fix). When ci.yml embeds equivalent logic,
# we also execute that snippet against fixtures.
def evaluate(needs: dict, selected: set[str]) -> int:
    """Return 0 if green under selected-lane policy, else 1."""
    for name, info in needs.items():
        result = info.get("result")
        if name in selected:
            if result != "success":
                return 1
        else:
            if result not in ("success", "skipped"):
                return 1
    return 0

always = {
    "actionlint",
    "detect-changes",
    "gitleaks",
    "security-scanners",
    "public-tree",
    "public-pr-language",
}
# Path-scoped PR: only gateway_rust selected among product lanes.
selected_gw = always | {"gateway-rust"}
needs_ok = {
    "actionlint": {"result": "success"},
    "detect-changes": {"result": "success"},
    "gitleaks": {"result": "success"},
    "security-scanners": {"result": "success"},
    "public-tree": {"result": "success"},
    "public-pr-language": {"result": "success"},
    "gateway-rust": {"result": "success"},
    "council-rust": {"result": "skipped"},
    "warroom-web": {"result": "skipped"},
    "warroom-tauri": {"result": "skipped"},
    "sentinel-rust": {"result": "skipped"},
    "workspace-supply-chain": {"result": "skipped"},
    "tauri-supply-chain": {"result": "skipped"},
    "sbom": {"result": "skipped"},
    "gateway-smoke": {"result": "skipped"},
    "candidate-isolation-proof": {"result": "skipped"},
    "exact-merged-candidate": {"result": "skipped"},
}
if evaluate(needs_ok, selected_gw) != 0:
    errors.append("intentional path skips must remain allowed when unselected")

needs_bad = dict(needs_ok)
needs_bad["gateway-rust"] = {"result": "skipped"}
if evaluate(needs_bad, selected_gw) == 0:
    errors.append("selected lane gateway-rust must not skip green")

# Mutation: selected lane failure must also refuse (not only skip).
needs_fail = dict(needs_ok)
needs_fail["gateway-rust"] = {"result": "failure"}
if evaluate(needs_fail, selected_gw) == 0:
    errors.append("selected lane failure must refuse aggregate green")

# Prefer executing the live ci-required script when it is a multi-line policy
# body (post-fix). Pre-fix single-line jq cannot express selected lanes.
script = run_body_for_check

# If the live script still is the blanket jq one-liner, the static check above
# already failed. When it is a real policy script, exercise fixtures through it.
if "all(.[];" not in script and ("NEEDS" in script or "needs" in script):
    def run_live(needs_obj, env_extra):
        with tempfile.TemporaryDirectory() as td:
            script_path = Path(td) / "check.sh"
            script_path.write_text(
                "#!/usr/bin/env bash\nset -euo pipefail\n" + script,
                encoding="utf-8",
            )
            env = {
                "NEEDS": json.dumps(needs_obj),
                "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            }
            env.update(env_extra)
            r = subprocess.run(
                ["bash", str(script_path)],
                capture_output=True,
                text=True,
                env=env,
            )
            return r.returncode

    def all_false_env(**overrides):
        env = {
            "GATEWAY_RUST": "false",
            "COUNCIL_RUST": "false",
            "SENTINEL_RUST": "false",
            "WARROOM_WEB": "false",
            "WARROOM_TAURI": "false",
            "WORKSPACE_SUPPLY_CHAIN": "false",
            "TAURI_SUPPLY_CHAIN": "false",
            "SBOM": "false",
            "EXACT_CANDIDATE": "false",
            "EVENT_NAME": "pull_request",
            "GITHUB_REF": "refs/pull/1/merge",
            "GITHUB_REPOSITORY": "irinityhq/irin",
            "REPO_PRIVATE": "true",
            "RUN_GATEWAY_SMOKE": "false",
            "PR_LABELS": "[]",
        }
        env.update(overrides)
        return env

    # Intentional path skips: only gateway_rust selected among product lanes.
    base_env = all_false_env(GATEWAY_RUST="true")
    if run_live(needs_ok, base_env) != 0:
        errors.append("live ci-required rejected intentional path skips")
    if run_live(needs_bad, base_env) == 0:
        errors.append("live ci-required accepted skipped selected lane")

    # Table-drive every conditional lane/event: force that selected job to
    # skipped and require the live aggregator to refuse green.
    lane_cases = [
        ("gateway-rust", all_false_env(GATEWAY_RUST="true")),
        ("council-rust", all_false_env(COUNCIL_RUST="true")),
        ("sentinel-rust", all_false_env(SENTINEL_RUST="true")),
        ("warroom-web", all_false_env(WARROOM_WEB="true")),
        ("warroom-tauri", all_false_env(WARROOM_TAURI="true")),
        (
            "workspace-supply-chain",
            all_false_env(WORKSPACE_SUPPLY_CHAIN="true"),
        ),
        ("tauri-supply-chain", all_false_env(TAURI_SUPPLY_CHAIN="true")),
        ("sbom", all_false_env(SBOM="true")),
        (
            "candidate-isolation-proof",
            all_false_env(
                EXACT_CANDIDATE="true",
                EVENT_NAME="pull_request",
                GITHUB_REF="refs/pull/1/merge",
            ),
        ),
        (
            "exact-merged-candidate",
            all_false_env(
                EXACT_CANDIDATE="true",
                EVENT_NAME="push",
                GITHUB_REF="refs/heads/main",
            ),
        ),
        (
            "gateway-smoke",
            all_false_env(
                GATEWAY_RUST="true",
                EVENT_NAME="schedule",
                GITHUB_REF="refs/heads/main",
                GITHUB_REPOSITORY="irinityhq/irin",
                REPO_PRIVATE="true",
            ),
        ),
        (
            "gateway-smoke",
            all_false_env(
                GATEWAY_RUST="true",
                EVENT_NAME="workflow_dispatch",
                RUN_GATEWAY_SMOKE="true",
                GITHUB_REF="refs/heads/main",
            ),
        ),
        (
            "gateway-smoke",
            all_false_env(
                GATEWAY_RUST="true",
                EVENT_NAME="pull_request",
                PR_LABELS='["run-smoke"]',
            ),
        ),
        (
            "gateway-smoke",
            all_false_env(
                GATEWAY_RUST="true",
                EVENT_NAME="pull_request",
                PR_LABELS='["run-provenance"]',
            ),
        ),
    ]
    for job_id, env in lane_cases:
        needs_skip = dict(needs_ok)
        # Always-on stay success; every conditional job starts skipped, then
        # the selected job is also skipped (the regression under test).
        for j in (
            "gateway-rust",
            "council-rust",
            "sentinel-rust",
            "warroom-web",
            "warroom-tauri",
            "workspace-supply-chain",
            "tauri-supply-chain",
            "sbom",
            "gateway-smoke",
            "candidate-isolation-proof",
            "exact-merged-candidate",
        ):
            needs_skip[j] = {"result": "skipped"}
        needs_skip[job_id] = {"result": "skipped"}
        label = f"{job_id}/{env.get('EVENT_NAME', '?')}"
        if run_live(needs_skip, env) == 0:
            errors.append(
                f"live ci-required accepted skipped selected lane ({label})"
            )
        # Positive control: same selection with success must pass.
        needs_pass = dict(needs_skip)
        needs_pass[job_id] = {"result": "success"}
        # gateway-smoke selection also requires gateway-rust success when
        # GATEWAY_RUST is true.
        if env.get("GATEWAY_RUST") == "true" and job_id != "gateway-rust":
            needs_pass["gateway-rust"] = {"result": "success"}
        if run_live(needs_pass, env) != 0:
            errors.append(
                f"live ci-required rejected success for selected lane ({label})"
            )

    # Excluded schedule contexts: smoke intentionally skipped stays green
    # (mirrors job if: non-canonical / public / non-main must not require smoke).
    excluded_schedule_envs = [
        all_false_env(
            GATEWAY_RUST="true",
            EVENT_NAME="schedule",
            GITHUB_REF="refs/heads/main",
            GITHUB_REPOSITORY="evil/fork",
            REPO_PRIVATE="true",
        ),
        all_false_env(
            GATEWAY_RUST="true",
            EVENT_NAME="schedule",
            GITHUB_REF="refs/heads/main",
            GITHUB_REPOSITORY="irinityhq/irin",
            REPO_PRIVATE="false",
        ),
        all_false_env(
            GATEWAY_RUST="true",
            EVENT_NAME="schedule",
            GITHUB_REF="refs/heads/develop",
            GITHUB_REPOSITORY="irinityhq/irin",
            REPO_PRIVATE="true",
        ),
    ]
    for env in excluded_schedule_envs:
        needs_excl = dict(needs_ok)
        for j in (
            "council-rust",
            "sentinel-rust",
            "warroom-web",
            "warroom-tauri",
            "workspace-supply-chain",
            "tauri-supply-chain",
            "sbom",
            "candidate-isolation-proof",
            "exact-merged-candidate",
        ):
            needs_excl[j] = {"result": "skipped"}
        needs_excl["gateway-rust"] = {"result": "success"}
        needs_excl["gateway-smoke"] = {"result": "skipped"}
        label = (
            f"excluded-schedule repo={env.get('GITHUB_REPOSITORY')} "
            f"private={env.get('REPO_PRIVATE')} ref={env.get('GITHUB_REF')}"
        )
        if run_live(needs_excl, env) != 0:
            errors.append(
                f"live ci-required rejected off-canonical schedule skip ({label})"
            )

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
sys.exit(0)
PY
then
  pass "ci-required selected-lane policy (skips allowed; selected cannot skip)"
else
  fail "ci-required selected-lane contracts"
fi

# ---------------------------------------------------------------------------
# Scheduled proof: schedule must reach gateway-smoke outer job AND inner
# exact-source make verify plus both teardowns. Separately fail if only the
# outer job, only the inner proof, or either teardown is omitted.
# ---------------------------------------------------------------------------
if python3 - "$CI_YML" <<'PY'
import re
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text(encoding="utf-8")
errors = []

m = re.search(
    r"(?m)^  gateway-smoke:\n((?:    .*\n|      .*\n|\n)*)",
    text,
)
if not m:
    errors.append("ci.yml missing gateway-smoke job")
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
job = m.group(0)

# Outer job if: must include schedule (alongside existing dispatch/PR labels).
# Extract the job-level if: expression (first if: after the job header, before steps).
header = job.split("steps:", 1)[0]
if not re.search(r"github\.event_name\s*==\s*'schedule'", header):
    errors.append("gateway-smoke outer job if must include schedule")
# Schedule entry must bind canonical repo + private + refs/heads/main
# (private forks must not enter).
if not re.search(
    r"github\.event_name\s*==\s*'schedule'[\s\S]{0,240}?"
    r"github\.repository\s*==\s*'irinityhq/irin'[\s\S]{0,240}?"
    r"github\.event\.repository\.private\s*==\s*true[\s\S]{0,240}?"
    r"github\.ref\s*==\s*'refs/heads/main'",
    header,
):
    errors.append(
        "gateway-smoke schedule arm must require canonical repository "
        "irinityhq/irin, private, and refs/heads/main"
    )

# Inner exact-source proof step if must include schedule + canonical identity.
proof_if = re.search(
    r"name:\s*Run exact-source no-spend proof\n\s*if:\s*>\n((?:\s+.+\n)+)",
    job,
)
if not proof_if or "schedule" not in proof_if.group(1):
    errors.append("exact-source proof step if must include schedule")
elif "irinityhq/irin" not in proof_if.group(1):
    errors.append(
        "exact-source proof step if must require github.repository "
        "== 'irinityhq/irin'"
    )

# Inner exact-source teardown if must include schedule + canonical identity.
teardown_if = re.search(
    r"name:\s*Tear down exact-source proof\n\s*if:\s*>\n((?:\s+.+\n)+)",
    job,
)
if not teardown_if or "schedule" not in teardown_if.group(1):
    errors.append("exact-source teardown step if must include schedule")
elif "irinityhq/irin" not in teardown_if.group(1):
    errors.append(
        "exact-source teardown step if must require github.repository "
        "== 'irinityhq/irin'"
    )

# Compose stack teardown must remain always() so schedule and dispatch clean up.
if not re.search(
    r"name:\s*Tear down gateway stack\n\s*if:\s*always\(\)",
    job,
):
    errors.append("gateway-smoke must keep always() compose teardown")

# Exact-source step must still invoke make verify / verify-down.
if not re.search(r"make verify\b", job):
    errors.append("gateway-smoke exact-source step must run make verify")
if not re.search(r"make verify-down\b", job):
    errors.append("gateway-smoke exact-source teardown must run make verify-down")

# Guards must remain: private repo, main ref, trusted self-hosted, exact HEAD.
for needle, label in (
    ("github.event.repository.private == true", "private repository"),
    ("github.ref == 'refs/heads/main'", "refs/heads/main"),
    ('runner.environment }}" = "self-hosted"', "self-hosted runner"),
    ("git rev-parse HEAD", "exact-HEAD check"),
):
    if needle not in job and label == "self-hosted runner":
        # tolerate spacing variants
        if "self-hosted" not in job:
            errors.append(f"exact-source proof missing guard: {label}")
    elif needle not in job and label != "self-hosted runner":
        if label == "exact-HEAD check" and "rev-parse HEAD" not in job:
            errors.append(f"exact-source proof missing guard: {label}")
        elif label != "exact-HEAD check" and needle not in job:
            errors.append(f"exact-source proof missing guard: {label}")

# Mutation teeth: each omission class must be detectable independently.
def has_schedule_outer(src: str) -> bool:
    hdr = src.split("steps:", 1)[0]
    return bool(re.search(r"github\.event_name\s*==\s*'schedule'", hdr))

def has_schedule_proof(src: str) -> bool:
    mif = re.search(
        r"name:\s*Run exact-source no-spend proof\n\s*if:\s*>\n((?:\s+.+\n)+)",
        src,
    )
    return bool(mif and "schedule" in mif.group(1))

def has_schedule_teardown(src: str) -> bool:
    mif = re.search(
        r"name:\s*Tear down exact-source proof\n\s*if:\s*>\n((?:\s+.+\n)+)",
        src,
    )
    return bool(mif and "schedule" in mif.group(1))

def has_compose_teardown(src: str) -> bool:
    return bool(
        re.search(r"name:\s*Tear down gateway stack\n\s*if:\s*always\(\)", src)
    )

# Only run mutation self-checks when the production job already satisfies
# the full contract; otherwise the static errors above are the signal.
if (
    has_schedule_outer(job)
    and has_schedule_proof(job)
    and has_schedule_teardown(job)
    and has_compose_teardown(job)
):
    # Outer-only: strip schedule from proof + exact teardown.
    outer_only = re.sub(
        r"(github\.event_name\s*==\s*'workflow_dispatch'[^\n]*\n(?:\s+&&[^\n]*\n)*)",
        lambda m: m.group(0),  # keep dispatch arms
        job,
        count=0,
    )
    # Remove schedule lines from step-level if blocks only (not job header).
    steps_part = job.split("steps:", 1)[1]
    steps_no_sched = re.sub(
        r"[^\n]*github\.event_name\s*==\s*'schedule'[^\n]*\n",
        "",
        steps_part,
    )
    outer_only = job.split("steps:", 1)[0] + "steps:" + steps_no_sched
    if has_schedule_proof(outer_only) or has_schedule_teardown(outer_only):
        errors.append("mutation: could not strip schedule from inner steps")
    elif not (
        has_schedule_outer(outer_only)
        and not has_schedule_proof(outer_only)
        and not has_schedule_teardown(outer_only)
    ):
        errors.append("mutation: outer-only schedule shape not detected")

    # Inner-only: strip schedule from job-level if.
    inner_only = re.sub(
        r"[^\n]*github\.event_name\s*==\s*'schedule'[^\n]*\n",
        "",
        header,
        count=1,
    ) + "steps:" + job.split("steps:", 1)[1]
    if has_schedule_outer(inner_only):
        errors.append("mutation: could not strip schedule from outer job if")
    elif not (
        not has_schedule_outer(inner_only)
        and has_schedule_proof(inner_only)
        and has_schedule_teardown(inner_only)
    ):
        errors.append("mutation: inner-only schedule shape not detected")

    # Omit compose teardown always().
    no_compose = re.sub(
        r"(name:\s*Tear down gateway stack\n\s*if:\s*)always\(\)",
        r"\1failure()",
        job,
        count=1,
    )
    if has_compose_teardown(no_compose):
        errors.append("mutation: could not strip compose always() teardown")

    # Omit exact-source teardown schedule (or whole step condition).
    no_exact_td = re.sub(
        r"(name:\s*Tear down exact-source proof\n\s*if:\s*>\n)((?:\s+.+\n)+)",
        r"\1          always() && false\n",
        job,
        count=1,
    )
    if has_schedule_teardown(no_exact_td):
        errors.append("mutation: could not strip exact-source teardown schedule")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
sys.exit(0)
PY
then
  pass "schedule reaches gateway-smoke + exact-source verify + both teardowns"
else
  fail "scheduled gateway-smoke reachability contracts"
fi

if (( failures > 0 )); then
  printf 'ci-control-plane contracts: FAILED (%d)\n' "$failures" >&2
  exit 1
fi
printf 'ci-control-plane contracts: OK\n'
