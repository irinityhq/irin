#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

scripts_to_parse=(
  scripts/gortex-worktree.sh
  scripts/dev-preflight.sh
  scripts/dev-check.sh
  scripts/bootstrap-dev-tools.sh
  scripts/bootstrap-actionlint.sh
  scripts/new-worktree.sh
  scripts/remove-worktree.sh
  scripts/worktree-gc.sh
  scripts/smoke-macos-tauri-app.sh
  scripts/with-test-ports.sh
  council-rs/scripts/warroom-browser-dev.sh
  council-rs/scripts/warroom-tauri-dev.sh
)
for script in "${scripts_to_parse[@]}"; do bash -n "$script"; done

web_scope="$(scripts/classify-ci-paths.sh council-rs/warroom/web/app/page.tsx)"
[[ "$(sed -n 's/^warroom_web=//p' <<<"$web_scope")" == true ]]
[[ "$(sed -n 's/^warroom_tauri=//p' <<<"$web_scope")" == true ]]

operator_scope="$(scripts/classify-ci-paths.sh scripts/dev-check.sh scripts/new-worktree.sh)"
[[ "$(sed -n 's/^full_matrix=//p' <<<"$operator_scope")" == false ]]
[[ "$(sed -n 's/^gateway_rust=//p' <<<"$operator_scope")" == false ]]
[[ "$(sed -n 's/^warroom_web=//p' <<<"$operator_scope")" == false ]]

makefile_scope="$(scripts/classify-ci-paths.sh Makefile)"
[[ "$(sed -n 's/^full_matrix=//p' <<<"$makefile_scope")" == true ]]
[[ "$(sed -n 's/^gateway_rust=//p' <<<"$makefile_scope")" == true ]]
[[ "$(sed -n 's/^warroom_web=//p' <<<"$makefile_scope")" == true ]]
[[ "$(sed -n 's/^warroom_tauri=//p' <<<"$makefile_scope")" == true ]]

gateway_ship="$(scripts/dev-check.sh --ship --dry-run gateway/sidecar-rs/src/main.rs)"
grep -Fq 'Package tests (gateway-sidecar)' <<<"$gateway_ship"
! grep -Fq 'Workspace tests' <<<"$gateway_ship"

operator_ship="$(scripts/dev-check.sh --ship --dry-run scripts/dev-check.sh)"
! grep -Fq 'Workspace tests' <<<"$operator_ship"
! grep -Fq 'Hosted, embedded-export, and Tauri regression' <<<"$operator_ship"
grep -Fq 'Release tree' <<<"$operator_ship"

grep -Fq 'CARGO_TARGET_DIR' scripts/new-worktree.sh
grep -Fq 'worktree-gc' Makefile

check_plan="$(scripts/dev-check.sh --dry-run council-rs/warroom/web/app/page.tsx)"
grep -Fq 'War Room dependencies' <<<"$check_plan"
grep -Fq 'War Room unit tests' <<<"$check_plan"
grep -Fq 'Embedded export build' <<<"$check_plan"
grep -Fq 'Tauri Rust tests' <<<"$check_plan"

ship_plan="$(scripts/dev-check.sh --ship --dry-run council-rs/warroom/web/app/page.tsx)"
grep -Fq 'Hosted, embedded-export, and Tauri regression' <<<"$ship_plan"
if [[ "$(uname -s)" == Darwin ]]; then
  grep -Fq 'Native macOS Tauri visual smoke' <<<"$ship_plan"
fi

set +e
disabled_native="$(IRIN_NATIVE_TAURI_SMOKE=0 scripts/dev-check.sh --ship --dry-run council-rs/warroom/web/app/page.tsx 2>&1)"
disabled_native_status=$?
set -e
[[ "$disabled_native_status" -ne 0 ]]
grep -Fq 'native visual proof cannot be disabled' <<<"$disabled_native"
grep -Fq 'env -u IRIN_NATIVE_APP' scripts/dev-check.sh
grep -Fq 'requires the native macOS visual proof' scripts/dev-check.sh
grep -Fq 'GitHub Actions lint' scripts/dev-check.sh
grep -Fq 'command -v shasum' scripts/dev-check.sh
grep -Fq 'sha256sum' scripts/dev-check.sh
grep -Fq 'IRIN_GORTEX_DETECT_TIMEOUT' scripts/gortex-worktree.sh
grep -Fq 'subprocess.TimeoutExpired' scripts/gortex-worktree.sh
grep -Fq 'partial_results' scripts/gortex-worktree.sh
grep -Fq -- '--wait-timeout' scripts/gortex-worktree.sh
! grep -Eq 'call[[:space:]]+index_repository' scripts/gortex-worktree.sh
[[ "$(grep -Fc 'Gortex affected-test analysis' scripts/dev-check.sh)" == 2 ]]
grep -Fq 'binary_sha=' scripts/bootstrap-dev-tools.sh
grep -Fq 'cargo-deny executable checksum mismatch' scripts/bootstrap-dev-tools.sh
grep -Fq 'open -n -F -W' scripts/smoke-macos-tauri-app.sh
grep -Fq 'binary_pattern="$(printf' scripts/smoke-macos-tauri-app.sh
[[ "$(grep -Fc 'pgrep -f -x "$binary_pattern"' scripts/smoke-macos-tauri-app.sh)" == 3 ]]
grep -Fq 'kill "$launcher_pid"' scripts/smoke-macos-tauri-app.sh

if [[ "$(uname -s)" == Darwin ]]; then
  set +e
  invalid_pid_output="$(swift scripts/macos-window-proof.swift \
    --pid not-a-pid \
    --output "${TMPDIR:-/tmp}/unused-window-proof.png" \
    --contains 'IRIN' 2>&1)"
  invalid_pid_status=$?
  set -e
  [[ "$invalid_pid_status" -ne 0 ]]
  grep -Fq 'invalid PID: not-a-pid' <<<"$invalid_pid_output"
fi

! grep -Fq 'kill -9' council-rs/scripts/warroom-browser-dev.sh
grep -Fq 'uses: irinityhq/irin/.github/workflows/ci.yml@main' .github/workflows/ci-pr.yml
! grep -Fq 'uses: ./.github/workflows/ci.yml' .github/workflows/ci-pr.yml
grep -Fq 'make -C council-rs warroom-web-check' .github/workflows/ci.yml
grep -Fq 'make warroom-tauri-check' .github/workflows/ci.yml
grep -Fq 'changed=(__integrated_main__)' .github/workflows/ci.yml
grep -Fq 'with-test-ports.sh' council-rs/Makefile
grep -Fq 'npm audit --omit=dev --audit-level=high' council-rs/Makefile
grep -Fq 'cargo build --release -p council-rs --bin council --locked' council-rs/Makefile
grep -Fq 'com.irinity.irin.smoke' scripts/smoke-macos-tauri-app.sh
grep -Fq 'a non-default IRIN_COUNCIL_PORT requires TAURI_CONFIG with exact' \
  council-rs/warroom-tauri/src-tauri/build.rs
grep -Fq 'native exact-build adoption proof: PASS' scripts/smoke-macos-tauri-app.sh
grep -Fq 'native webview Council request proof: PASS' scripts/smoke-macos-tauri-app.sh
grep -Fq 'WARROOM_SMOKE_SKIP_TAURI_TESTS' \
  council-rs/warroom-tauri/scripts/smoke-hybrid-build.sh
[[ "$(grep -Fc 'git diff --check origin/main --' scripts/dev-check.sh)" == 2 ]]
python3 - <<'PY'
from pathlib import Path

makefile = Path("council-rs/Makefile").read_text()
target = makefile.split("warroom-product-check:", 1)[1].split("\n\n", 1)[0]
assert target.index("build-warroom-assets.sh") < target.index("npm run test:export")
workflow = Path(".github/workflows/ci.yml").read_text()
assert "npm run build:tauri\n          npm run test:export" not in workflow
web_job = workflow.split("  warroom-web:", 1)[1].split("\n  warroom-tauri:", 1)[0]
assert "warroom-web-check" in web_job
assert "warroom-check" not in web_job.replace("warroom-web-check", "")
codeowners = Path(".github/CODEOWNERS").read_text()
for build_script in (
    "/council-rs/build.rs",
    "/council-rs/warroom-tauri/src-tauri/build.rs",
    "/gateway/sidecar-rs/build.rs",
    "/sentinel/sovereign-protocol/build.rs",
):
    assert f"{build_script} @iws17" in codeowners
PY

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-workflow-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
git init -q --bare "$tmp/origin.git"
git clone -q "$tmp/origin.git" "$tmp/repo"
git -C "$tmp/repo" config user.name test
git -C "$tmp/repo" config user.email test@example.invalid
mkdir -p "$tmp/repo/scripts"
cp scripts/dev-preflight.sh scripts/gortex-worktree.sh scripts/dev-check.sh \
  scripts/bootstrap-dev-tools.sh \
  scripts/classify-ci-paths.sh "$tmp/repo/scripts/"
printf 'baseline\n' >"$tmp/repo/README.md"
git -C "$tmp/repo" add README.md scripts
git -C "$tmp/repo" commit -qm baseline
git -C "$tmp/repo" branch -M main
git -C "$tmp/repo" push -q -u origin main

gc_origin="$tmp/gc-origin.git"
gc_repo="$tmp/gc-repo"
git init -q --bare "$gc_origin"
git clone -q "$gc_origin" "$gc_repo"
git -C "$gc_repo" config user.name test
git -C "$gc_repo" config user.email test@example.invalid
mkdir -p "$gc_repo/scripts"
cp scripts/worktree-gc.sh scripts/remove-worktree.sh "$gc_repo/scripts/"
printf 'runtime-down:\n\t@:\n' >"$gc_repo/Makefile"
printf 'baseline\n' >"$gc_repo/README.md"
git -C "$gc_repo" add Makefile README.md scripts
git -C "$gc_repo" commit -qm baseline
git -C "$gc_repo" branch -M main
git -C "$gc_repo" push -q -u origin main

active_worktree="$tmp/irin-wt-active-unpushed"
git -C "$gc_repo" worktree add -q -b feature/active-unpushed "$active_worktree" main
active_worktree="$(cd "$active_worktree" && pwd -P)"
git -C "$active_worktree" branch --set-upstream-to=origin/main >/dev/null
printf 'active\n' >"$active_worktree/active.txt"
git -C "$active_worktree" add active.txt
git -C "$active_worktree" commit -qm 'active unpushed work'

merged_worktree="$tmp/irin-wt-merged"
git -C "$gc_repo" worktree add -q -b feature/merged "$merged_worktree" main
merged_worktree="$(cd "$merged_worktree" && pwd -P)"
printf 'merged\n' >"$merged_worktree/merged.txt"
git -C "$merged_worktree" add merged.txt
git -C "$merged_worktree" commit -qm 'merged work'
git -C "$gc_repo" merge -q --ff-only feature/merged
git -C "$gc_repo" push -q origin main

deleted_worktree="$tmp/irin-wt-deleted-tracking"
git -C "$gc_repo" worktree add -q -b feature/deleted-tracking "$deleted_worktree" main
deleted_worktree="$(cd "$deleted_worktree" && pwd -P)"
printf 'squash merged\n' >"$deleted_worktree/squash.txt"
git -C "$deleted_worktree" add squash.txt
git -C "$deleted_worktree" commit -qm 'squash merged work'
git -C "$deleted_worktree" push -q -u origin feature/deleted-tracking
git -C "$gc_repo" merge -q --squash feature/deleted-tracking
git -C "$gc_repo" commit -qm 'squash feature/deleted-tracking'
git -C "$gc_repo" push -q origin main
git -C "$gc_repo" push -q origin --delete feature/deleted-tracking

dirty_worktree="$tmp/irin-wt-dirty"
git -C "$gc_repo" worktree add -q -b feature/dirty "$dirty_worktree" main
dirty_worktree="$(cd "$dirty_worktree" && pwd -P)"
printf 'dirty\n' >"$dirty_worktree/dirty.txt"

gc_dry_run="$(cd "$gc_repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 scripts/worktree-gc.sh)"
grep -Fq "KEEP active: $active_worktree (feature/active-unpushed)" <<<"$gc_dry_run"
grep -Fq "SKIP dirty: $dirty_worktree (feature/dirty)" <<<"$gc_dry_run"
grep -Fq "CANDIDATE [merged-into-origin/main]: $merged_worktree (feature/merged)" \
  <<<"$gc_dry_run"
grep -Fq "CANDIDATE [origin-branch-gone-and-cherry-empty]: $deleted_worktree (feature/deleted-tracking)" \
  <<<"$gc_dry_run"
[[ -d "$active_worktree" && -d "$merged_worktree" && -d "$deleted_worktree" \
  && -d "$dirty_worktree" ]]

origin_url="$(git -C "$gc_repo" remote get-url origin)"
git -C "$gc_repo" remote set-url origin "$tmp/missing-origin.git"
set +e
gc_offline="$(cd "$gc_repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 scripts/worktree-gc.sh --apply 2>&1)"
gc_offline_status=$?
set -e
git -C "$gc_repo" remote set-url origin "$origin_url"
[[ "$gc_offline_status" -ne 0 ]]
grep -Fq 'unable to refresh and prune origin; no worktrees were changed' <<<"$gc_offline"
[[ -d "$merged_worktree" && -d "$deleted_worktree" ]]

gc_apply="$(cd "$gc_repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 scripts/worktree-gc.sh --apply)"
grep -Fq 'worktree-gc: removed 2 of 2 candidate(s)' <<<"$gc_apply"
[[ -d "$active_worktree" && -d "$dirty_worktree" ]]
[[ ! -d "$merged_worktree" && ! -d "$deleted_worktree" ]]

gortex_fake_bin="$tmp/gortex-fake-bin"
gortex_fake_state="$tmp/gortex-fake-state"
mkdir -p "$gortex_fake_bin"
printf '%s\n' \
  '#!/bin/sh' \
  'set -eu' \
  'if [ -n "${IRIN_COUNCIL_PORT:-}" ]; then' \
  '  printf '\''%s\n'\'' '\''IRIN_COUNCIL_PORT leaked into a check command'\'' >&2' \
  '  exit 24' \
  'fi' \
  'case "${1:-}" in' \
  '  repos)' \
  '    stale=false' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = stale ] && [ ! -f "$IRIN_GORTEX_TEST_STATE.reindexed" ]; then stale=true; fi' \
  '    head="$(git -C "$IRIN_GORTEX_TEST_REPO" rev-parse HEAD)"' \
  '    indexed="$head"' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = mismatch ] && [ ! -f "$IRIN_GORTEX_TEST_STATE.reindexed" ]; then indexed=0000000000000000000000000000000000000000; fi' \
  '    printf '"'"'[{"path":"%s","stale":%s,"indexed":true,"head_commit":"%s","indexed_commit":"%s"}]\n'"'"' "$IRIN_GORTEX_TEST_REPO" "$stale" "$head" "$indexed"' \
  '    ;;' \
  '  call)' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = sleep ]; then sleep 10; fi' \
  '    count=0' \
  '    [ ! -f "$IRIN_GORTEX_TEST_STATE" ] || count="$(cat "$IRIN_GORTEX_TEST_STATE")"' \
  '    count=$((count + 1))' \
  '    printf '"'"'%s\n'"'"' "$count" >"$IRIN_GORTEX_TEST_STATE"' \
  '    if [ "$count" -eq 1 ]; then' \
  '      printf '"'"'%s\n'"'"' '"'"'{"warming":{"partial_results":true,"percent":50}}'"'"'' \
  '    else' \
  '      printf '"'"'%s\n'"'"' '"'"'{"warming":{"partial_results":false},"changed_files":["README.md"],"risk":"LOW"}'"'"'' \
  '    fi' \
  '    ;;' \
  '  track)' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = refresh-fail ]; then exit 23; fi' \
  '    : >"$IRIN_GORTEX_TEST_STATE.reindexed"' \
  '    ;;' \
  '  untrack)' \
  '    ;;' \
  '  affected)' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = affected-fail ]; then exit 1; fi' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = affected-none ]; then exit 3; fi' \
  '    if [ "${IRIN_GORTEX_TEST_MODE:-partial}" = affected-empty ]; then printf '"'"'%s\n'"'"' '"'"'{"affected_tests":[]}'"'"'; exit 0; fi' \
  '    printf '"'"'%s\n'"'"' '"'"'{"affected_tests":["tests/readme_test.rs"]}'"'"'' \
  '    ;;' \
  '  *) exit 2 ;;' \
  'esac' \
  >"$gortex_fake_bin/gortex"
chmod +x "$gortex_fake_bin/gortex"

printf '%s\n' \
  'IRIN_COUNCIL_PORT=48765' \
  'IRIN_WEB_PORT=48766' \
  'IRIN_GATEWAY_PORT=48767' \
  >"$tmp/repo/.irin-worktree.env"
port_neutralized_output="$(
  cd "$tmp/repo" &&
    PATH="$gortex_fake_bin:/usr/bin:/bin" \
    IRIN_GORTEX_TEST_REPO="$tmp/repo" \
    IRIN_GORTEX_TEST_STATE="$gortex_fake_state-port" \
    IRIN_GORTEX_DETECT_TIMEOUT=5 \
      scripts/dev-check.sh README.md 2>&1
)"
grep -Fq 'Mode: check' <<<"$port_neutralized_output"
rm "$tmp/repo/.irin-worktree.env"

partial_output="$(
  cd "$tmp/repo" &&
    PATH="$gortex_fake_bin:/usr/bin:/bin" \
    IRIN_GORTEX_TEST_REPO="$tmp/repo" \
    IRIN_GORTEX_TEST_STATE="$gortex_fake_state" \
    IRIN_GORTEX_DETECT_TIMEOUT=5 \
      "$ROOT/scripts/gortex-worktree.sh" detect "$tmp/repo" 2>&1
)"
grep -Fq 'partial graph (50%)' <<<"$partial_output"
grep -Fq '"README.md"' <<<"$partial_output"
[[ "$(cat "$gortex_fake_state")" == 2 ]]

affected_output="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state" \
    "$ROOT/scripts/gortex-worktree.sh" affected "$tmp/repo" README.md 2>&1
)"
grep -Fq 'Gortex affected tests: SELECTED' <<<"$affected_output"
grep -Fq 'tests/readme_test.rs' <<<"$affected_output"

set +e
empty_output="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state" \
  IRIN_GORTEX_TEST_MODE=affected-empty \
    "$ROOT/scripts/gortex-worktree.sh" affected "$tmp/repo" README.md 2>&1
)"
empty_status=$?
set -e
[[ "$empty_status" -ne 0 ]]
grep -Fq 'without a non-empty affected_tests list' <<<"$empty_output"

none_output="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state" \
  IRIN_GORTEX_TEST_MODE=affected-none \
    "$ROOT/scripts/gortex-worktree.sh" affected "$tmp/repo" README.md 2>&1
)"
grep -Fq 'Gortex affected tests: NONE' <<<"$none_output"

set +e
affected_failure="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state" \
  IRIN_GORTEX_TEST_MODE=affected-fail \
    "$ROOT/scripts/gortex-worktree.sh" affected "$tmp/repo" README.md 2>&1
)"
affected_failure_status=$?
set -e
[[ "$affected_failure_status" -eq 1 ]]
grep -Fq 'affected-test analysis failed' <<<"$affected_failure"

stale_output="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state-stale" \
  IRIN_GORTEX_TEST_MODE=stale \
  IRIN_GORTEX_DETECT_TIMEOUT=5 \
    "$ROOT/scripts/gortex-worktree.sh" detect "$tmp/repo" 2>&1
)"
grep -Fq 'STALE OR COMMIT-MISMATCHED' <<<"$stale_output"
[[ -f "$gortex_fake_state-stale.reindexed" ]]

mismatch_output="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state-mismatch" \
  IRIN_GORTEX_TEST_MODE=mismatch \
  IRIN_GORTEX_DETECT_TIMEOUT=5 \
    "$ROOT/scripts/gortex-worktree.sh" detect "$tmp/repo" 2>&1
)"
grep -Fq 'STALE OR COMMIT-MISMATCHED' <<<"$mismatch_output"
[[ -f "$gortex_fake_state-mismatch.reindexed" ]]

set +e
refresh_failure="$(
  PATH="$gortex_fake_bin:/usr/bin:/bin" \
  IRIN_GORTEX_TEST_REPO="$tmp/repo" \
  IRIN_GORTEX_TEST_STATE="$gortex_fake_state-refresh-fail" \
  IRIN_GORTEX_TEST_MODE=refresh-fail \
    "$ROOT/scripts/gortex-worktree.sh" track "$tmp/repo" 2>&1
)"
refresh_failure_status=$?
set -e
[[ "$refresh_failure_status" -ne 0 ]]
grep -Fq 'failed to track exact worktree' <<<"$refresh_failure"

set +e
timeout_output="$(
  cd "$tmp/repo" &&
    PATH="$gortex_fake_bin:/usr/bin:/bin" \
    IRIN_GORTEX_TEST_REPO="$tmp/repo" \
    IRIN_GORTEX_TEST_STATE="$gortex_fake_state" \
    IRIN_GORTEX_TEST_MODE=sleep \
    IRIN_GORTEX_DETECT_TIMEOUT=1 \
      "$ROOT/scripts/gortex-worktree.sh" detect "$tmp/repo" 2>&1
)"
timeout_status=$?
set -e
[[ "$timeout_status" -eq 124 ]]
grep -Fq 'exceeded 1s' <<<"$timeout_output"

fake_tool_bin="$tmp/fake-tool-bin"
mkdir -p "$fake_tool_bin" "$tmp/repo/.irin-tools/bin"
printf '%s\n' '#!/bin/sh' 'printf "%s\n" "cargo-deny 0.19.9"' \
  >"$tmp/repo/.irin-tools/bin/cargo-deny"
chmod +x "$tmp/repo/.irin-tools/bin/cargo-deny"
printf '%s\n' '#!/bin/sh' 'exit 42' >"$fake_tool_bin/curl"
chmod +x "$fake_tool_bin/curl"
set +e
forged_cache_output="$(
  cd "$tmp/repo" &&
    PATH="$fake_tool_bin:/usr/bin:/bin" \
      scripts/bootstrap-dev-tools.sh 2>&1
)"
forged_cache_status=$?
set -e
[[ "$forged_cache_status" -eq 42 ]]
grep -Fq 'cargo-deny cache: rejected' <<<"$forged_cache_output"
rm -rf "$tmp/repo/.irin-tools"

set +e
main_output="$(cd "$tmp/repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 IRIN_PREFLIGHT_SKIP_FETCH=1 scripts/dev-preflight.sh 2>&1)"
main_status=$?
set -e
[[ "$main_status" -ne 0 ]]
grep -Fq 'feature branch' <<<"$main_output"

git -C "$tmp/repo" switch -qc feature/workflow
(cd "$tmp/repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 IRIN_PREFLIGHT_SKIP_FETCH=1 scripts/dev-preflight.sh >/dev/null)
[[ -s "$tmp/repo/.irin-worktree-base" ]]

test_home="$tmp/home"
cleanup_worktree="$tmp/cleanup-worktree"
outside_state="$test_home/.local/state/irin/outside"
mkdir -p "$test_home/.local/state/irin/worktrees" "$outside_state"
printf 'preserve\n' >"$outside_state/sentinel"
printf '.irin-worktree.env\n' >>"$tmp/repo/.git/info/exclude"
git -C "$tmp/repo" worktree add -q -b feature/cleanup "$cleanup_worktree" main
printf 'IRIN_RUNTIME_STATE_DIR=%s\n' \
  "$test_home/.local/state/irin/worktrees/../outside" \
  >"$cleanup_worktree/.irin-worktree.env"
set +e
cleanup_escape_output="$(
  cd "$tmp/repo" &&
    HOME="$test_home" PATH=/usr/bin:/bin \
      "$ROOT/scripts/remove-worktree.sh" "$cleanup_worktree" 2>&1
)"
cleanup_escape_status=$?
set -e
[[ "$cleanup_escape_status" -ne 0 ]]
grep -Fq 'refusing unexpected runtime state path' <<<"$cleanup_escape_output"
[[ -f "$outside_state/sentinel" ]]
[[ -d "$cleanup_worktree" ]]

teardown_worktree="$tmp/teardown-worktree"
git -C "$tmp/repo" worktree add -q -b feature/teardown "$teardown_worktree" main
fake_bin="$tmp/fake-bin"
mkdir -p "$fake_bin"
printf '%s\n' '#!/bin/sh' 'exit 23' >"$fake_bin/make"
chmod +x "$fake_bin/make"
set +e
teardown_output="$(
  cd "$tmp/repo" &&
    PATH="$fake_bin:/usr/bin:/bin" \
      "$ROOT/scripts/remove-worktree.sh" "$teardown_worktree" 2>&1
)"
teardown_status=$?
set -e
[[ "$teardown_status" -ne 0 ]]
grep -Fq 'runtime teardown failed; retaining worktree and runtime state' <<<"$teardown_output"
[[ -d "$teardown_worktree" ]]
git -C "$tmp/repo" worktree remove --force "$teardown_worktree"

printf 'dirty\n' >>"$tmp/repo/README.md"
set +e
dirty_output="$(cd "$tmp/repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 IRIN_PREFLIGHT_SKIP_FETCH=1 scripts/dev-preflight.sh 2>&1)"
dirty_status=$?
set -e
[[ "$dirty_status" -ne 0 ]]
grep -Fq 'clean worktree' <<<"$dirty_output"
git -C "$tmp/repo" restore README.md

printf 'untracked\n' >"$tmp/repo/new-untracked.rs"
untracked_plan="$(cd "$tmp/repo" && scripts/dev-check.sh --dry-run)"
grep -Fq 'new-untracked.rs' <<<"$untracked_plan"
grep -Fq 'full_matrix=true' <<<"$untracked_plan"
set +e
override_output="$(cd "$tmp/repo" && scripts/dev-check.sh --ship README.md 2>&1)"
override_status=$?
set -e
[[ "$override_status" -ne 0 ]]
grep -Fq 'explicit paths are not allowed' <<<"$override_output"
rm "$tmp/repo/new-untracked.rs"

printf 'committed whitespace  \n' >>"$tmp/repo/README.md"
git -C "$tmp/repo" add README.md
git -C "$tmp/repo" commit -qm 'committed whitespace fixture'
set +e
whitespace_output="$(
  cd "$tmp/repo" &&
    PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 \
      scripts/dev-check.sh README.md 2>&1
)"
whitespace_status=$?
set -e
[[ "$whitespace_status" -ne 0 ]]
grep -Fq 'trailing whitespace' <<<"$whitespace_output"
git -C "$tmp/repo" reset -q --hard origin/main

printf 'ship failure\n' >>"$tmp/repo/README.md"
set +e
(cd "$tmp/repo" && PATH=/usr/bin:/bin scripts/dev-check.sh --ship >/dev/null 2>&1)
failed_ship_status=$?
set -e
[[ "$failed_ship_status" -ne 0 ]]
failed_receipt="$(find "$tmp/repo/.irin-receipts" -type f -name 'ship-*.txt' -print -quit)"
[[ -n "$failed_receipt" ]]
grep -Fq 'status=FAIL' "$failed_receipt"
git -C "$tmp/repo" restore README.md
rm -rf "$tmp/repo/.irin-receipts"

git -C "$tmp/repo" commit --allow-empty -qm simulated-upstream
new_origin="$(git -C "$tmp/repo" rev-parse HEAD)"
git -C "$tmp/repo" reset -q --hard HEAD^
git -C "$tmp/repo" update-ref refs/remotes/origin/main "$new_origin"
set +e
drift_output="$(cd "$tmp/repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 IRIN_PREFLIGHT_SKIP_FETCH=1 scripts/dev-preflight.sh ship 2>&1)"
drift_status=$?
set -e
[[ "$drift_status" -ne 0 ]]
grep -Fq 'origin/main moved' <<<"$drift_output"

printf 'development workflow self-test: PASS\n'
