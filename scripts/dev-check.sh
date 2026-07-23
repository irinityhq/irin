#!/usr/bin/env bash
# One path classifier, two speeds: fast iteration and the complete ship proof.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  printf 'ERROR: check must run inside an IRIN Git worktree\n' >&2
  exit 1
}
cd "$ROOT"

mode=check
dry_run="${IRIN_CHECK_DRY_RUN:-0}"
if [[ "${1:-}" == "--ship" ]]; then mode=ship; shift; fi
if [[ "${1:-}" == "--dry-run" ]]; then dry_run=1; shift; fi

if [[ "$mode" == "ship" || -f "$ROOT/.irin-worktree.env" ]]; then
  export IRIN_REQUIRE_GORTEX=1
fi
if [[ "$mode" == "ship" ]]; then
  if (( $# > 0 )) && [[ "$dry_run" != "1" ]]; then
    printf 'ERROR: ship scope is always the complete working-tree diff; explicit paths are not allowed\n' >&2
    exit 2
  fi
fi

paths=()
if (( $# > 0 )); then
  paths=("$@")
else
  while IFS= read -r path; do
    [[ -n "$path" ]] && paths+=("$path")
  done < <(
    { git diff --name-only origin/main --; git ls-files --others --exclude-standard; } \
      | awk '!seen[$0]++'
  )
fi

if (( ${#paths[@]} == 0 )); then
  if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
    printf 'ERROR: working tree is dirty but no changed paths could be classified\n' >&2
    git status --short >&2
    exit 1
  fi
  if [[ "$mode" == "ship" ]]; then
    printf 'ERROR: no changes relative to origin/main; no ship receipt was created\n' >&2
    exit 1
  fi
  printf 'No changes relative to origin/main.\n'
  exit 0
fi

classifier="$(scripts/classify-ci-paths.sh "${paths[@]}")"
lane() { sed -n "s/^$1=//p" <<<"$classifier"; }

printf 'Mode: %s\n' "$mode"
printf 'Changed paths:\n'
printf '  %s\n' "${paths[@]}"
printf 'Selected lanes:\n%s\n' "$classifier"

receipt=""
fingerprint_paths() {
  local fingerprint_input path object mode_marker digest
  fingerprint_input="$(mktemp "${TMPDIR:-/tmp}/irin-tested-tree.XXXXXX")"
  for path in "$@"; do
    if [[ -e "$path" || -L "$path" ]]; then
      object="$(git hash-object -- "$path")"
      mode_marker="$(git ls-files -s -- "$path" | awk 'NR == 1 { print $1 }')"
      if [[ -z "$mode_marker" ]]; then
        [[ -x "$path" ]] && mode_marker=100755 || mode_marker=100644
      fi
    else
      object=DELETED
      mode_marker=000000
    fi
    printf '%s %s %s\n' "$mode_marker" "$object" "$path" >>"$fingerprint_input"
  done
  if command -v shasum >/dev/null 2>&1; then
    digest="$(shasum -a 256 "$fingerprint_input" | awk '{print $1}')"
  elif command -v sha256sum >/dev/null 2>&1; then
    digest="$(sha256sum "$fingerprint_input" | awk '{print $1}')"
  else
    rm -f "$fingerprint_input"
    printf 'ERROR: shasum or sha256sum is required\n' >&2
    return 1
  fi
  rm -f "$fingerprint_input"
  printf '%s\n' "$digest"
}
if [[ "$mode" == "ship" && "$dry_run" != "1" ]]; then
  mkdir -p .irin-receipts
  receipt=".irin-receipts/ship-$(date '+%Y%m%dT%H%M%S%z').txt"
  exec > >(tee "$receipt") 2>&1
  printf 'IRIN SHIP RECEIPT\n'
  printf 'started=%s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')"
  printf 'branch=%s\n' "$(git symbolic-ref --quiet --short HEAD)"
  printf 'head=%s\n' "$(git rev-parse HEAD)"
  printf 'origin_main=%s\n' "$(git rev-parse origin/main)"
  start_path_manifest="$(printf '%s\n' "${paths[@]}" | LC_ALL=C sort)"
  sorted_paths=()
  while IFS= read -r path; do sorted_paths+=("$path"); done <<<"$start_path_manifest"
  tested_tree_fingerprint="$(fingerprint_paths "${sorted_paths[@]}")"
  printf 'tested_tree_fingerprint=%s\n' "$tested_tree_fingerprint"
  printf 'working_tree_status_begin\n'
  git status --short --untracked-files=normal
  printf 'working_tree_status_end\n'
  printf '%s\n' "$classifier"
  receipt_complete=0
  finish_receipt() {
    local status=$?
    if [[ "$receipt_complete" != "1" ]]; then
      printf '\nstatus=FAIL exit=%s finished=%s\n' "$status" "$(date '+%Y-%m-%dT%H:%M:%S%z')"
    fi
  }
  trap finish_receipt EXIT
fi

run() {
  local label="$1"
  shift
  printf '\n== %s ==\n' "$label"
  printf 'command:'
  printf ' %q' "$@"
  printf '\n'
  [[ "$dry_run" == "1" ]] || "$@"
}

any_rust=false
for key in gateway_rust council_rust sentinel_rust; do
  [[ "$(lane "$key")" != true ]] || any_rust=true
done

if [[ "$mode" == "check" ]]; then
  run "Gortex detect_changes continuity receipt" scripts/gortex-worktree.sh detect "$ROOT"
  if [[ "$(lane gateway_rust)" == true ]]; then
    run "Gateway focused tests" cargo test -p gateway-sidecar
  fi
  if [[ "$(lane council_rust)" == true ]]; then
    run "Council focused tests" cargo test -p council-rs
  fi
  if [[ "$(lane sentinel_rust)" == true ]]; then
    run "Protocol focused tests" cargo test -p sovereign-protocol
  fi
  if [[ "$(lane warroom_web)" == true || "$(lane warroom_tauri)" == true ]]; then
    run "War Room dependencies" npm --prefix council-rs/warroom/web ci
  fi
  if [[ "$(lane warroom_web)" == true ]]; then
    run "War Room lint" npm --prefix council-rs/warroom/web run lint
    run "War Room typecheck" npm --prefix council-rs/warroom/web run typecheck
    run "War Room unit tests" npm --prefix council-rs/warroom/web run test:unit
  fi
  if [[ "$(lane warroom_tauri)" == true ]]; then
    run "Embedded export build" npm --prefix council-rs/warroom/web run build:tauri
    run "Tauri Rust tests" cargo test --manifest-path council-rs/warroom-tauri/src-tauri/Cargo.toml
  fi
  run "Diff whitespace" git diff --check origin/main --
  exit 0
fi

run "Current-base and Gortex preflight" scripts/dev-preflight.sh ship
run "Gortex detect_changes continuity receipt" scripts/gortex-worktree.sh detect "$ROOT"
run "Classifier self-test" scripts/test-classify-ci-paths.sh
run "Workflow-script self-test" scripts/test-development-workflow.sh
run "Pinned actionlint" scripts/bootstrap-actionlint.sh
run "GitHub Actions lint" .irin-tools/bin/actionlint -color

if [[ "$any_rust" == true ]]; then
  run "Workspace formatting" cargo fmt --all -- --check
  run "Workspace clippy" cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
  run "Workspace tests" cargo test --workspace --locked
fi

deny_bin=""
if [[ "$(lane workspace_supply_chain)" == true || "$(lane tauri_supply_chain)" == true ]]; then
  run "Pinned development tools" scripts/bootstrap-dev-tools.sh
  deny_bin="$ROOT/.irin-tools/bin/cargo-deny"
fi

if [[ "$(lane workspace_supply_chain)" == true ]]; then
  run "Workspace dependency audit" cargo audit -D warnings --ignore RUSTSEC-2024-0436
  run "Workspace dependency policy" "$deny_bin" check
fi
if [[ "$(lane tauri_supply_chain)" == true ]]; then
  run "Tauri dependency audit" cargo audit -D unsound \
    --file council-rs/warroom-tauri/src-tauri/Cargo.lock \
    --ignore RUSTSEC-2026-0194 \
    --ignore RUSTSEC-2026-0195 \
    --ignore RUSTSEC-2024-0429
  run "Tauri dependency policy" "$deny_bin" --manifest-path council-rs/warroom-tauri/src-tauri/Cargo.toml check --config deny.toml
fi

if [[ "$(lane warroom_web)" == true || "$(lane warroom_tauri)" == true ]]; then
  run "Hosted, embedded-export, and Tauri regression" make -C council-rs warroom-check
  if [[ "$(uname -s)" != Darwin ]]; then
    printf 'ERROR: War Room ship-check requires the native macOS visual proof\n' >&2
    exit 1
  fi
  if [[ "${IRIN_NATIVE_TAURI_SMOKE:-1}" == "0" ]]; then
    printf 'ERROR: native visual proof cannot be disabled during make ship-check\n' >&2
    exit 1
  fi
  run "Native macOS Tauri visual smoke" env -u IRIN_NATIVE_APP \
    IRIN_NATIVE_SKIP_BUILD=0 IRIN_NATIVE_VISUAL=1 scripts/smoke-macos-tauri-app.sh
fi

run "Release tree" make release-check
run "Public-language checker self-test" scripts/check-public-pr-language.sh --self-test
run "Public commit language" scripts/check-public-pr-language.sh --range "origin/main..HEAD"
run "Diff whitespace" git diff --check origin/main --
gitleaks_bin="$(command -v gitleaks 2>/dev/null || true)"
[[ -n "$gitleaks_bin" ]] || [[ ! -x /opt/homebrew/bin/gitleaks ]] || gitleaks_bin=/opt/homebrew/bin/gitleaks
if [[ -n "$gitleaks_bin" || "$dry_run" == "1" ]]; then
  run "Root secret scan" "${gitleaks_bin:-gitleaks}" dir --config .gitleaks.toml --redact --no-banner .
else
  printf 'ERROR: gitleaks is required for make ship-check\n' >&2
  exit 1
fi

if [[ -n "$receipt" ]]; then
  end_paths=()
  while IFS= read -r path; do
    [[ -n "$path" ]] && end_paths+=("$path")
  done < <(
    { git diff --name-only origin/main --; git ls-files --others --exclude-standard; } \
      | awk '!seen[$0]++'
  )
  end_path_manifest="$(printf '%s\n' "${end_paths[@]}" | LC_ALL=C sort)"
  [[ "$end_path_manifest" == "$start_path_manifest" ]] || {
    printf 'ERROR: changed-file scope moved during ship-check\n' >&2
    exit 1
  }
  sorted_end_paths=()
  while IFS= read -r path; do sorted_end_paths+=("$path"); done <<<"$end_path_manifest"
  end_fingerprint="$(fingerprint_paths "${sorted_end_paths[@]}")"
  [[ "$end_fingerprint" == "$tested_tree_fingerprint" ]] || {
    printf 'ERROR: tested tree changed during ship-check (start=%s end=%s)\n' \
      "$tested_tree_fingerprint" "$end_fingerprint" >&2
    exit 1
  }
  printf 'tested_tree_fingerprint_end=%s\n' "$end_fingerprint"
fi

if [[ "$dry_run" == "1" ]]; then
  printf '\nstatus=DRY-RUN finished=%s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')"
else
  printf '\nstatus=PASS finished=%s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')"
fi
if [[ -n "$receipt" ]]; then
  receipt_complete=1
  trap - EXIT
fi
[[ -z "$receipt" ]] || printf 'receipt=%s/%s\n' "$ROOT" "$receipt"
