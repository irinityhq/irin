#!/usr/bin/env bash
# Classify changed repository paths into independent CI lanes.
#
# Usage:
#   scripts/classify-ci-paths.sh PATH...
#   printf '%s\n' PATH... | scripts/classify-ci-paths.sh
#
# Synthetic event paths beginning with __ force the full proof matrix. Unknown
# repository paths also force full proof so a new surface cannot silently skip
# validation.
#
# Additional shipping-method outputs (W4):
#   exact_candidate — app / runtime / packaging / gateway-pack / release path
#   exact_install   — narrower subset: DMG extract/install, bundled resources,
#                     Gateway Pack install, Arm/Watch pack-install wiring
#
# exact_* lines are always computed. They are printed by default locally and
# whenever IRIN_CLASSIFIER_INCLUDE_EXACT=1. On GitHub Actions without that
# flag they are omitted so a base-controlled ci.yml@main that still expects
# only the classic nine keys can classify a PR that adds these outputs.
# Post-merge detect-changes sets the flag; bootstrap scope and self-tests do too.

set -euo pipefail

if (( $# > 0 )); then
  paths=("$@")
else
  paths=()
  while IFS= read -r path; do
    paths+=("$path")
  done
fi

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

set_full_matrix() {
  full_matrix=true
  gateway_rust=true
  council_rust=true
  sentinel_rust=true
  warroom_web=true
  warroom_tauri=true
  workspace_supply_chain=true
  tauri_supply_chain=true
  # Fail-safe: full synthetic matrices also select exact candidate+install so
  # unknown surfaces and manual/scheduled proof never skip durable rebuilds.
  exact_candidate=true
  exact_install=true
}

for path in "${paths[@]}"; do
  [[ -z "$path" ]] && continue

  case "$path" in
    __manual_dispatch__|__scheduled_proof__)
      set_full_matrix
      sbom=true
      ;;

    # Retained as an explicit full-matrix sentinel for callers that still need
    # it. Main push classification must use the real before...sha path list
    # instead of this token (see .github/workflows/ci.yml).
    __integrated_main__)
      set_full_matrix
      ;;

    __*__|.github/workflows/*|.github/actions/*|*/.github/workflows/*|*/.github/actions/*|scripts/classify-ci-paths.sh|scripts/test-classify-ci-paths.sh)
      set_full_matrix
      ;;

    # Public prose and component documentation retain only the always-on light
    # checks in ci.yml. Negative control: docs must not select exact_candidate.
    *.md|docs/*|gateway/docs/*|sentinel/docs/*|council-rs/docs/*|council-rs/warroom/docs/*)
      ;;

    # The bootstrap installs cargo-deny for both dependency-policy lanes.
    scripts/bootstrap-dev-tools.sh)
      workspace_supply_chain=true
      tauri_supply_chain=true
      ;;

    # Development and shipping tooling stays on the light always-on checks. Do
    # not tax product lanes (Rust matrix, War Room, Tauri visual) for script-only edits.
    # Exception: candidate export/import and worktree evidence scripts are
    # method surface — light checks only (no product candidate rebuild).
    scripts/dev-*.sh|scripts/new-worktree.sh|scripts/remove-worktree.sh|scripts/worktree-gc.sh|scripts/link-agent-context.sh|scripts/test-link-agent-context.sh|scripts/check-*.sh|scripts/with-test-ports.sh|scripts/export-candidate.sh|scripts/import-candidate.sh|scripts/test-export-import-candidate.sh|scripts/shipping-method-smoke.sh|scripts/test-publish-fake-gh.sh|scripts/test-remove-worktree-evidence.sh|scripts/link-ship-board.sh)
      ;;

    # Root workspace manifests affect every member. The standalone Tauri crate
    # is intentionally excluded from the root workspace.
    Cargo.toml|Cargo.lock)
      gateway_rust=true
      council_rust=true
      sentinel_rust=true
      workspace_supply_chain=true
      ;;

    # The shared deny policy governs both the root workspace and the standalone
    # Tauri crate.
    deny.toml)
      workspace_supply_chain=true
      tauri_supply_chain=true
      ;;

    # The shared wire crate is a path dependency of Gateway and Council.
    sentinel/sovereign-protocol/Cargo.toml|sentinel/sovereign-protocol/Cargo.lock)
      gateway_rust=true
      council_rust=true
      sentinel_rust=true
      workspace_supply_chain=true
      ;;

    sentinel/sovereign-protocol/*)
      gateway_rust=true
      council_rust=true
      sentinel_rust=true
      ;;

    # --- exact_candidate / exact_install surfaces (plan W4) -----------------
    # Packaging tree: always candidate; install for DMG/app/smoke/gateway-pack.
    packaging/build-dmg.sh|packaging/verify-dmg.sh|packaging/smoke-full-app.sh|packaging/smoke-gateway-pack.sh|packaging/build-app-bundle.sh|packaging/app-bundle-lock.sh)
      exact_candidate=true
      exact_install=true
      warroom_tauri=true
      ;;

    packaging/gateway-pack/*)
      exact_candidate=true
      exact_install=true
      warroom_tauri=true
      ;;

    packaging/*)
      exact_candidate=true
      warroom_tauri=true
      ;;

    # Embedded War Room + Tauri shell.
    council-rs/warroom/web/*)
      warroom_web=true
      warroom_tauri=true
      exact_candidate=true
      ;;

    council-rs/scripts/warroom-*|scripts/smoke-macos-tauri-app.sh|scripts/macos-window-proof.swift)
      warroom_web=true
      warroom_tauri=true
      exact_candidate=true
      ;;

    council-rs/warroom-tauri/src-tauri/Cargo.toml|council-rs/warroom-tauri/src-tauri/Cargo.lock|council-rs/src-tauri/Cargo.toml|council-rs/src-tauri/Cargo.lock)
      warroom_tauri=true
      tauri_supply_chain=true
      exact_candidate=true
      ;;

    council-rs/warroom-tauri/*|council-rs/src-tauri/*)
      warroom_tauri=true
      exact_candidate=true
      # Bundled resources / pack-install / arm wiring → high-risk install proof.
      case "$path" in
        */resources/*|*gateway*|*pack*|*install*|*arm-attest*|*arm_*)
          exact_install=true
          ;;
      esac
      ;;

    # Gateway Pack staging/build/provenance + release transaction.
    scripts/stage-gateway-pack.sh|scripts/build-gateway-pack-dev-images.sh|scripts/build-gateway-pack-prod-images.sh|scripts/generate-production-manifest.sh|scripts/verify-production-image-provenance.sh|scripts/test-gateway-pack-*.sh|scripts/test-production-image-provenance.sh)
      exact_candidate=true
      exact_install=true
      warroom_tauri=true
      ;;

    scripts/install-verify-candidate.sh)
      exact_candidate=true
      exact_install=true
      ;;

    scripts/release-transaction.sh|scripts/test-release-transaction-w3.sh|scripts/record-acceptance.sh|scripts/candidate-status.sh|scripts/test-candidate-status.sh|scripts/ci-build-adhoc-candidate.sh)
      exact_candidate=true
      ;;

    # Root Makefile can change packaging / candidate targets.
    Makefile)
      set_full_matrix
      ;;

    gateway/sidecar-rs/Cargo.toml|gateway/sidecar-rs/Cargo.lock|gateway/Cargo.toml|gateway/Cargo.lock)
      gateway_rust=true
      workspace_supply_chain=true
      ;;

    gateway/sidecar-rs/*)
      gateway_rust=true
      ;;

    sentinel/*)
      sentinel_rust=true
      ;;

    council-rs/Cargo.toml|council-rs/Cargo.lock)
      council_rust=true
      workspace_supply_chain=true
      ;;

    council-rs/build.rs|council-rs/src/*|council-rs/tests/*|council-rs/examples/*)
      council_rust=true
      ;;

    gateway/*)
      gateway_rust=true
      ;;

    council-rs/*)
      council_rust=true
      ;;

    *)
      set_full_matrix
      ;;
  esac
done

include_exact=1
if [[ -n "${GITHUB_ACTIONS:-}" && "${IRIN_CLASSIFIER_INCLUDE_EXACT:-}" != "1" ]]; then
  include_exact=0
fi

{
  cat <<EOF
full_matrix=$full_matrix
gateway_rust=$gateway_rust
council_rust=$council_rust
sentinel_rust=$sentinel_rust
warroom_web=$warroom_web
warroom_tauri=$warroom_tauri
workspace_supply_chain=$workspace_supply_chain
tauri_supply_chain=$tauri_supply_chain
sbom=$sbom
EOF
  if [[ "$include_exact" == "1" ]]; then
    cat <<EOF
exact_candidate=$exact_candidate
exact_install=$exact_install
EOF
  fi
}
