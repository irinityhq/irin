#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POLICY="$ROOT/scripts/cargo-target-policy.sh"
bash -n "$POLICY"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-cargo-policy-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

make_checkout() {
  local checkout="$1"
  mkdir -p "$checkout/council-rs/warroom-tauri/src-tauri"
}

shared="$tmp/shared-target"
checkout="$tmp/checkout"
make_checkout "$checkout"
# shellcheck disable=SC2016 # expansion belongs to the child shell
IRIN_CARGO_TARGET_DIR="$shared" \
IRIN_CARGO_TARGET_MAX_KIB=8192 \
IRIN_CARGO_MIN_FREE_KIB=0 \
IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK=1 \
  "$POLICY" run "$checkout" sh -c 'test "$CARGO_INCREMENTAL" = 0'
[[ -L "$checkout/target" ]]
[[ "$(readlink "$checkout/target")" == "$shared" ]]
[[ -L "$checkout/council-rs/warroom-tauri/src-tauri/target" ]]

mkdir "$shared/.irin-build.lock"
printf '%s\n' "$$" >"$shared/.irin-build.lock/pid"
set +e
locked_output="$(
  IRIN_CARGO_TARGET_DIR="$shared" \
  IRIN_CARGO_TARGET_MAX_KIB=8192 \
  IRIN_CARGO_MIN_FREE_KIB=0 \
  IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK=1 \
    "$POLICY" run "$checkout" true 2>&1
)"
locked_status=$?
set -e
[[ "$locked_status" -ne 0 ]]
grep -Fq 'another IRIN build owns the shared Cargo target' <<<"$locked_output"
find "$shared/.irin-build.lock" -depth -delete

mkdir -p "$shared/debug/incremental/example"
dd if=/dev/zero of="$shared/debug/incremental/example/blob" bs=1024 count=2048 status=none
IRIN_CARGO_TARGET_DIR="$shared" \
IRIN_CARGO_TARGET_MAX_KIB=8192 \
IRIN_CARGO_MIN_FREE_KIB=0 \
IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK=1 \
  "$POLICY" prepare "$checkout"
[[ ! -e "$shared/debug/incremental" ]]

mkdir -p "$shared/debug/deps"
dd if=/dev/zero of="$shared/debug/deps/blob" bs=1024 count=2048 status=none
set +e
oversize_output="$(
  IRIN_CARGO_TARGET_DIR="$shared" \
  IRIN_CARGO_TARGET_MAX_KIB=1024 \
  IRIN_CARGO_MIN_FREE_KIB=0 \
  IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK=1 \
    "$POLICY" prepare "$checkout" 2>&1
)"
oversize_status=$?
set -e
[[ "$oversize_status" -ne 0 ]]
grep -Fq 'exceeds the 1 MiB ceiling' <<<"$oversize_output"

rm -f "$shared/debug/deps/blob"
set +e
headroom_output="$(
  IRIN_CARGO_TARGET_DIR="$shared" \
  IRIN_CARGO_TARGET_MAX_KIB=8192 \
  IRIN_CARGO_MIN_FREE_KIB=999999999999 \
  IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK=1 \
    "$POLICY" prepare "$checkout" 2>&1
)"
headroom_status=$?
set -e
[[ "$headroom_status" -ne 0 ]]
grep -Fq 'free-space floor' <<<"$headroom_output"

conflict_checkout="$tmp/conflict-checkout"
conflict_target="$tmp/conflict-shared"
make_checkout "$conflict_checkout"
mkdir -p "$conflict_checkout/target"
printf 'generated\n' >"$conflict_checkout/target/sentinel"
set +e
conflict_output="$(
  IRIN_CARGO_TARGET_DIR="$conflict_target" \
  IRIN_CARGO_TARGET_MAX_KIB=8192 \
  IRIN_CARGO_MIN_FREE_KIB=0 \
  IRIN_CARGO_POLICY_SKIP_ACTIVE_CHECK=1 \
    "$POLICY" link "$conflict_checkout" 2>&1
)"
conflict_status=$?
set -e
[[ "$conflict_status" -ne 0 ]]
grep -Fq 'private Cargo target already exists' <<<"$conflict_output"
[[ -f "$conflict_checkout/target/sentinel" ]]

printf 'cargo target policy self-test: PASS\n'
