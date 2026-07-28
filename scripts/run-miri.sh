#!/usr/bin/env bash
# Selective Miri for sovereign-protocol pure/lib tests (Phase 1E).
#
# Scope: package sovereign-protocol, lib unit tests under the jcs module only.
# Avoids envelope builder tests that need OsRng / SystemTime (isolation).
#
# Exit policy:
#   - IRIN_MIRI_FAIL=1  → missing tool or test failure is hard-fail
#   - otherwise         → missing tool is advisory (exit 0 + message);
#                         test failure still fails when miri is present
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
}
cd "$ROOT"

PKG="sovereign-protocol"
fail_hard="${IRIN_MIRI_FAIL:-0}"
# Filter matches jcs unit tests (module path jcs::tests::...).
TEST_FILTER="${IRIN_MIRI_FILTER:-jcs::}"

advisory() {
  printf 'run-miri: advisory — %s\n' "$1" >&2
  if [[ "$fail_hard" == "1" ]]; then
    exit 1
  fi
  exit 0
}

if ! command -v cargo >/dev/null 2>&1; then
  advisory "cargo not found"
fi

if ! command -v rustup >/dev/null 2>&1; then
  advisory "rustup not found (needed for +nightly miri)"
fi

if ! rustup run nightly rustc --version >/dev/null 2>&1; then
  advisory "nightly toolchain not installed (install: rustup toolchain install nightly)"
fi

if ! rustup run nightly cargo miri --version >/dev/null 2>&1; then
  advisory "miri component missing (install: rustup +nightly component add miri && cargo +nightly miri setup)"
fi

printf 'run-miri: cargo +nightly miri test -p %s --lib %s\n' "$PKG" "$TEST_FILTER"
# Selective: --lib + filter keeps us on pure JCS logic (no OsRng).
cargo +nightly miri test -p "$PKG" --lib "$TEST_FILTER"
