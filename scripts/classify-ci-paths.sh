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

set_full_matrix() {
  full_matrix=true
  gateway_rust=true
  council_rust=true
  sentinel_rust=true
  warroom_web=true
  warroom_tauri=true
  workspace_supply_chain=true
  tauri_supply_chain=true
}

for path in "${paths[@]}"; do
  [[ -z "$path" ]] && continue

  case "$path" in
    __manual_dispatch__|__scheduled_proof__)
      set_full_matrix
      sbom=true
      ;;

    __*__|.github/workflows/*|.github/actions/*|*/.github/workflows/*|*/.github/actions/*|scripts/classify-ci-paths.sh|scripts/test-classify-ci-paths.sh)
      set_full_matrix
      ;;

    # Public prose and component documentation retain only the always-on light
    # checks in ci.yml.
    *.md|docs/*|gateway/docs/*|sentinel/docs/*|council-rs/docs/*|council-rs/warroom/docs/*)
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

    council-rs/warroom/web/*)
      warroom_web=true
      ;;

    council-rs/warroom-tauri/src-tauri/Cargo.toml|council-rs/warroom-tauri/src-tauri/Cargo.lock|council-rs/src-tauri/Cargo.toml|council-rs/src-tauri/Cargo.lock)
      warroom_tauri=true
      tauri_supply_chain=true
      ;;

    council-rs/warroom-tauri/*|council-rs/src-tauri/*)
      warroom_tauri=true
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
