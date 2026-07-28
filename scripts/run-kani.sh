#!/usr/bin/env bash
# Selective Kani proofs for sovereign-protocol JCS pure logic (Phase 1D).
#
# Scope: package sovereign-protocol only — never the full workspace.
# Harnesses live under #[cfg(kani)] (src/jcs/kani_proofs.rs) and are invisible
# to normal cargo test.
#
# Env:
#   IRIN_KANI_FAIL=1       hard-fail if kani missing (default: advisory exit 0)
#   IRIN_KANI_HARNESS=name run a single harness (e.g. proof_nonfinite_f64_rejected)
#
# Exit policy:
#   - missing tool → advisory exit 0 unless IRIN_KANI_FAIL=1
#   - present tool + proof failure → non-zero (honest)
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
}
cd "$ROOT"

PKG="sovereign-protocol"
fail_hard="${IRIN_KANI_FAIL:-0}"
harness="${IRIN_KANI_HARNESS:-}"

advisory() {
  printf 'run-kani: advisory — %s\n' "$1" >&2
  if [[ "$fail_hard" == "1" ]]; then
    exit 1
  fi
  exit 0
}

if ! command -v cargo >/dev/null 2>&1; then
  advisory "cargo not found"
fi

if ! cargo kani --version >/dev/null 2>&1; then
  advisory "cargo-kani not installed (install: cargo install --locked kani-verifier && cargo kani setup)"
fi

args=(-p "$PKG")
if [[ -n "$harness" ]]; then
  args+=(--harness "$harness")
  printf 'run-kani: cargo kani -p %s --harness %s\n' "$PKG" "$harness"
else
  printf 'run-kani: cargo kani -p %s (JCS harnesses only)\n' "$PKG"
fi

cargo kani "${args[@]}"
