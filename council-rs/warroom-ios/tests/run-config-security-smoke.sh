#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/irin-ios-config-smoke.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

xcrun swiftc -parse-as-library \
  "$ROOT/council-rs/warroom-ios/WarRoomiOS/WarRoomConfig.swift" \
  "$ROOT/council-rs/warroom-ios/WarRoomiOS/KeychainStore.swift" \
  "$ROOT/council-rs/warroom-ios/WarRoomiOS/WarRoomSettingsStore.swift" \
  "$ROOT/council-rs/warroom-ios/tests/WarRoomConfigSecuritySmoke.swift" \
  -o "$TMP/warroom-config-security-smoke"

"$TMP/warroom-config-security-smoke"
