#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLASSIFIER="$ROOT/scripts/classify-ci-paths.sh"

keys=(
  full_matrix
  gateway_rust
  council_rust
  sentinel_rust
  warroom_web
  warroom_tauri
  workspace_supply_chain
  tauri_supply_chain
  sbom
)

all_false='false false false false false false false false false'
full_pr='true true true true true true true true false'
full_proof='true true true true true true true true true'

cases=(
  "root docs|$all_false|README.md CONTRIBUTING.md"
  "nested component docs|$all_false|gateway/docs/runbook.md sentinel/docs/YOUR-AGENT.md council-rs/docs/war-room.md council-rs/warroom/docs/TAURI-AUTH.md"
  "workflow forces full|$full_pr|.github/workflows/ci.yml"
  "action forces full|$full_pr|.github/actions/rust-setup/action.yml"
  "manual forces full|$full_proof|__manual_dispatch__"
  "schedule forces full|$full_proof|__scheduled_proof__"
  "integrated main forces full|$full_pr|__integrated_main__"
  "unknown forces full|$full_pr|new-surface/config.json"
  "gateway Rust source|false true false false false false false false false|gateway/sidecar-rs/src/main.rs"
  "gateway manifest|false true false false false false true false false|gateway/sidecar-rs/Cargo.toml"
  "gateway non-Rust runtime|false true false false false false false false false|gateway/docker-compose.yml gateway/lua/auth.lua"
  "gateway docs stay light|$all_false|gateway/README.md gateway/docs/watch-api.md"
  "sentinel runtime|false false false true false false false false false|sentinel/tools/check_protocol_version_drift.py"
  "sentinel docs stay light|$all_false|sentinel/README.md sentinel/docs/protocol-implementation.md"
  "council source|false false true false false false false false false|council-rs/src/main.rs"
  "council manifest|false false true false false false true false false|council-rs/Cargo.toml"
  "council non-Rust runtime|false false true false false false false false false|council-rs/prompts/chair.md.j2"
  "council docs stay light|$all_false|council-rs/README.md council-rs/docs/providers.md"
  "web source and locks also select the embedded desktop|false false false false true true false false false|council-rs/warroom/web/app/page.tsx council-rs/warroom/web/package-lock.json"
  "warroom launchers select both product lanes|false false false false true true false false false|council-rs/scripts/warroom-tauri-dev.sh"
  "native proof selects both product lanes|false false false false true true false false false|scripts/smoke-macos-tauri-app.sh scripts/macos-window-proof.swift"
  "tauri source|false false false false false true false false false|council-rs/warroom-tauri/src-tauri/src/lib.rs"
  "tauri lock|false false false false false true false true false|council-rs/warroom-tauri/src-tauri/Cargo.lock"
  "root cargo workspace|false true true true false false true false false|Cargo.toml Cargo.lock"
  "shared deny policy|false false false false false false true true false|deny.toml"
  "shared protocol source fans out|false true true true false false false false false|sentinel/sovereign-protocol/src/lib.rs"
  "shared protocol manifest fans out|false true true true false false true false false|sentinel/sovereign-protocol/Cargo.toml"
  "mixed paths union lanes|false true false false true true false false false|gateway/lua/auth.lua council-rs/warroom/web/package.json"
)

failures=0
for row in "${cases[@]}"; do
  IFS='|' read -r name expected path_text <<<"$row"
  read -r -a expected_values <<<"$expected"
  read -r -a paths <<<"$path_text"
  output="$($CLASSIFIER "${paths[@]}")"

  for i in "${!keys[@]}"; do
    key="${keys[$i]}"
    actual="$(sed -n "s/^${key}=//p" <<<"$output")"
    if [[ "$actual" != "${expected_values[$i]}" ]]; then
      printf 'FAIL: %s: %s expected %s, got %s\n' "$name" "$key" "${expected_values[$i]}" "${actual:-<missing>}" >&2
      failures=$((failures + 1))
    fi
  done
done

if (( failures > 0 )); then
  printf 'classifier self-test: FAILED (%d assertion(s))\n' "$failures" >&2
  exit 1
fi

stdin_output="$(printf '%s\n' \
  'gateway/lua/auth.lua' \
  'council-rs/warroom/web/package.json' \
  | "$CLASSIFIER")"
if [[ "$(sed -n 's/^gateway_rust=//p' <<<"$stdin_output")" != true ]] \
  || [[ "$(sed -n 's/^warroom_web=//p' <<<"$stdin_output")" != true ]]; then
  printf 'classifier self-test: stdin mode failed\n' >&2
  exit 1
fi

printf 'classifier self-test: OK (%d cases)\n' "${#cases[@]}"
