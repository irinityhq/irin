#!/usr/bin/env bash
# Run IRIN crypto dylints against the product workspace (advisory by default).
#
# Exit behaviour:
#   - Tools missing / toolchain mismatch: print a clear message.
#       IRIN_DYLINT_FAIL unset/0 → exit 0 (advisory no-op)
#       IRIN_DYLINT_FAIL=1       → exit 1
#   - Lints run and report findings:
#       advisory → exit 0 (findings still printed)
#       IRIN_DYLINT_FAIL=1 → propagate cargo-dylint exit status
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

LINT_PATH="${IRIN_DYLINT_PATH:-tools/dylint/irin-crypto-lints}"
FAIL="${IRIN_DYLINT_FAIL:-0}"

advisory_or_fail() {
  local msg="$1"
  if [[ "$FAIL" == "1" ]]; then
    echo "error: $msg (IRIN_DYLINT_FAIL=1)" >&2
    exit 1
  fi
  echo "advisory: $msg — skipping dylint (set IRIN_DYLINT_FAIL=1 to fail)" >&2
  exit 0
}

if ! command -v cargo >/dev/null 2>&1; then
  advisory_or_fail "cargo not found on PATH"
fi

if ! cargo dylint --version >/dev/null 2>&1; then
  advisory_or_fail "cargo-dylint not installed (cargo install cargo-dylint dylint-link --locked)"
fi

if ! command -v dylint-link >/dev/null 2>&1; then
  advisory_or_fail "dylint-link not installed (cargo install dylint-link --locked)"
fi

if [[ ! -f "$LINT_PATH/Cargo.toml" ]]; then
  advisory_or_fail "dylint library not found at $LINT_PATH"
fi

TOOLCHAIN_FILE="$LINT_PATH/rust-toolchain"
if [[ -f "$TOOLCHAIN_FILE" ]]; then
  # shellcheck disable=SC1091
  channel="$(awk -F'"' '/channel/ {print $2; exit}' "$TOOLCHAIN_FILE" || true)"
  if [[ -n "${channel:-}" ]] && ! rustup run "$channel" rustc --version >/dev/null 2>&1; then
    advisory_or_fail "rustup toolchain '$channel' missing (see tools/dylint/README.md)"
  fi
fi

echo "run-dylint: building library at $LINT_PATH"
if ! (cd "$LINT_PATH" && cargo build 2>&1); then
  advisory_or_fail "failed to build dylint library at $LINT_PATH (nightly/rustc-dev mismatch?)"
fi

echo "run-dylint: cargo dylint --path $LINT_PATH -- --workspace"
set +e
if [[ "$FAIL" == "1" ]]; then
  # Promote IRIN crypto lints to hard errors via rustc flags (not cargo args).
  # cargo-dylint 6.x: DYLINT_RUSTFLAGS is forwarded to rustc for the check.
  DYLINT_RUSTFLAGS="-D no_debug_on_signing_key_types -D prefer_subtle_ct_eq" \
    cargo dylint --path "$LINT_PATH" -- --workspace
else
  cargo dylint --path "$LINT_PATH" -- --workspace
fi
status=$?
set -e

if [[ "$status" -ne 0 ]]; then
  if [[ "$FAIL" == "1" ]]; then
    echo "error: cargo dylint exited $status (IRIN_DYLINT_FAIL=1)" >&2
    exit "$status"
  fi
  echo "advisory: cargo dylint exited $status (findings or check errors; set IRIN_DYLINT_FAIL=1 to fail)" >&2
  exit 0
fi

echo "run-dylint: clean"
exit 0
