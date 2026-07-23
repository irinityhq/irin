#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

scripts_to_parse=(
  scripts/gortex-worktree.sh
  scripts/dev-preflight.sh
  scripts/dev-check.sh
  scripts/bootstrap-dev-tools.sh
  scripts/new-worktree.sh
  scripts/remove-worktree.sh
  scripts/smoke-macos-tauri-app.sh
  scripts/with-test-ports.sh
  council-rs/scripts/warroom-browser-dev.sh
  council-rs/scripts/warroom-tauri-dev.sh
)
for script in "${scripts_to_parse[@]}"; do bash -n "$script"; done

web_scope="$(scripts/classify-ci-paths.sh council-rs/warroom/web/app/page.tsx)"
[[ "$(sed -n 's/^warroom_web=//p' <<<"$web_scope")" == true ]]
[[ "$(sed -n 's/^warroom_tauri=//p' <<<"$web_scope")" == true ]]

check_plan="$(scripts/dev-check.sh --dry-run council-rs/warroom/web/app/page.tsx)"
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

! grep -Fq 'kill -9' council-rs/scripts/warroom-browser-dev.sh
grep -Fq 'make -C council-rs warroom-check' .github/workflows/ci.yml
grep -Fq 'changed=(__integrated_main__)' .github/workflows/ci.yml
grep -Fq 'with-test-ports.sh' council-rs/Makefile
grep -Fq 'npm audit --omit=dev --audit-level=high' council-rs/Makefile
python3 - <<'PY'
from pathlib import Path

makefile = Path("council-rs/Makefile").read_text()
target = makefile.split("warroom-product-check:", 1)[1].split("\n\n", 1)[0]
assert target.index("build-warroom-assets.sh") < target.index("npm run test:export")
workflow = Path(".github/workflows/ci.yml").read_text()
assert "npm run build:tauri\n          npm run test:export" not in workflow
PY

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-workflow-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
git init -q --bare "$tmp/origin.git"
git clone -q "$tmp/origin.git" "$tmp/repo"
git -C "$tmp/repo" config user.name test
git -C "$tmp/repo" config user.email test@example.invalid
mkdir -p "$tmp/repo/scripts"
cp scripts/dev-preflight.sh scripts/gortex-worktree.sh scripts/dev-check.sh \
  scripts/classify-ci-paths.sh "$tmp/repo/scripts/"
printf 'baseline\n' >"$tmp/repo/README.md"
git -C "$tmp/repo" add README.md scripts
git -C "$tmp/repo" commit -qm baseline
git -C "$tmp/repo" branch -M main
git -C "$tmp/repo" push -q -u origin main

set +e
main_output="$(cd "$tmp/repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 IRIN_PREFLIGHT_SKIP_FETCH=1 scripts/dev-preflight.sh 2>&1)"
main_status=$?
set -e
[[ "$main_status" -ne 0 ]]
grep -Fq 'feature branch' <<<"$main_output"

git -C "$tmp/repo" switch -qc feature/workflow
(cd "$tmp/repo" && PATH=/usr/bin:/bin IRIN_REQUIRE_GORTEX=0 IRIN_PREFLIGHT_SKIP_FETCH=1 scripts/dev-preflight.sh >/dev/null)
[[ -s "$tmp/repo/.irin-worktree-base" ]]

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
