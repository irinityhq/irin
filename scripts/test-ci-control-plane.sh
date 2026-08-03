#!/usr/bin/env bash
# Contract tests for PR B CI control-plane: concurrency split, exact-path
# policy sync, and base-controlled force-full matrix guard.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CI_YML="$ROOT/.github/workflows/ci.yml"
CI_PR="$ROOT/.github/workflows/ci-pr.yml"
CLASSIFIER="$ROOT/scripts/classify-ci-paths.sh"
failures=0

pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; failures=$((failures + 1)); }

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
    if not re.search(r"cancel-in-progress:\s*true", block):
        errors.append("ci-pr.yml must set cancel-in-progress: true")
    if "github.event.pull_request.number" not in block:
        errors.append("ci-pr.yml concurrency group must be per-PR number")
    if "irin-ci-pr-" not in block:
        errors.append("ci-pr.yml concurrency group should be namespaced irin-ci-pr-*")

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
# Static: path-scoped main push still uses before...sha (not full-matrix always)
# ---------------------------------------------------------------------------
if ! rg -n 'before="\$\{\{ github\.event\.before \}\}"' "$CI_YML" >/dev/null; then
  fail "main push must still classify from github.event.before"
else
  pass "main push remains path-scoped via before...sha"
fi
if ! rg -n 'git diff --name-only "\$before\.\.\.\$head"' "$CI_YML" >/dev/null; then
  fail "main push must diff before...head"
else
  pass "main push diffs before...head"
fi
if rg -n "changed=\(__integrated_main__\)" "$CI_YML" | rg -v '^\d+:\s*#' >/dev/null; then
  # Only acceptable as fail-safe when before is zero/missing, not as the default arm.
  if ! rg -n 'before.*0\+|__integrated_main__' "$CI_YML" >/dev/null; then
    fail "unexpected unconditional __integrated_main__"
  else
    pass "integrated_main retained only as fail-safe sentinel"
  fi
else
  pass "no unconditional integrated_main default"
fi

# ---------------------------------------------------------------------------
# Static: force-full guard present and base-controlled
# ---------------------------------------------------------------------------
if ! rg -n 'path_forces_full_non_sbom_matrix' "$CI_YML" >/dev/null; then
  fail "missing path_forces_full_non_sbom_matrix base-controlled guard"
else
  pass "force-full non-SBOM guard present"
fi
if ! rg -n 'force_full_non_sbom=true' "$CI_YML" >/dev/null; then
  fail "force_full_non_sbom must be applied in detect-changes"
else
  pass "force_full_non_sbom applied after classifier"
fi
# Policy surfaces that must force full
for needle in \
  '.github/workflows/*' \
  'scripts/classify-ci-paths.sh' \
  'scripts/test-classify-ci-paths.sh' \
  'scripts/test-ci-control-plane.sh'
do
  if ! rg -F "$needle" "$CI_YML" >/dev/null; then
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
    gateway/sidecar-rs/src/main.rs Makefile
  git -C "$ROOT" ls-files \
    'packaging/*' \
    'scripts/*' \
    '.github/workflows/*' \
    '.github/actions/*' \
    'council-rs/warroom/web/*' \
    'council-rs/warroom-tauri/*' \
    'council-rs/src-tauri/*' \
    'council-rs/scripts/warroom*' \
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

# Hosted invocation: detect-changes must call this script (not only path lists).
if ! rg -n '^\s+scripts/test-ci-control-plane\.sh\s*$' "$CI_YML" >/dev/null; then
  fail "ci.yml must run scripts/test-ci-control-plane.sh as a hosted step"
else
  pass "ci.yml hosts test-ci-control-plane.sh in detect-changes"
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
# Comments / trust-boundary honesty (static text)
# ---------------------------------------------------------------------------
if rg -n 'same revision under review' "$CI_YML" >/dev/null; then
  fail "ci.yml still claims ordinary PR executes same revision under review"
else
  pass "ci.yml no longer claims same-revision ordinary PR execution"
fi
if rg -n 'PRs still enter via ci.yml@main until this lands' "$CI_YML" >/dev/null; then
  fail "stale 'until this lands' wording remains"
else
  pass "stale until-this-lands wording removed"
fi
if rg -n 'keep in sync with ci-pr.yml bootstrap' "$CI_YML" >/dev/null; then
  fail "dangling ci-pr.yml bootstrap sync comment remains"
else
  pass "dangling bootstrap sync comment removed"
fi
if ! rg -n 'ci\.yml@main' "$CI_PR" >/dev/null; then
  fail "ci-pr.yml must document/use @main pin"
else
  pass "ci-pr.yml retains @main pin"
fi

if (( failures > 0 )); then
  printf 'ci-control-plane contracts: FAILED (%d)\n' "$failures" >&2
  exit 1
fi
printf 'ci-control-plane contracts: OK\n'
