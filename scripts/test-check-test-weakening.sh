#!/usr/bin/env bash
# Self-test for scripts/check-test-weakening.sh against a throwaway repo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK="$ROOT/scripts/check-test-weakening.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/irin-tw-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

cd "$work"
git init -q -b main .
git config user.email test@example.invalid
git config user.name test
mkdir -p src tests scripts web
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); }
}
EOF
cat >src/brace.rs <<'EOF'
pub fn label() -> &'static str { "}" }
#[cfg(test)]
mod tests {
    const MARKER: &str = "}"; // a '}' here must not end the region
    const WRAPPED: &str = "line one \
        line two";
    #[test]
    fn labels() { assert_eq!(super::label(), MARKER); assert_eq!(WRAPPED.len(), 25); assert!(true); }
}
EOF
cat >tests/contract.rs <<'EOF'
#[test]
fn contract_holds() { assert_eq!(1, 1); }
EOF
cat >web/app.test.ts <<'EOF'
it("renders", () => { expect(1).toBe(1); });
EOF
cat >scripts/proof.sh <<'EOF'
#!/usr/bin/env bash
for _ in $(seq 1 30); do check && break; sleep 1; done
run_proof
EOF
git add -A && git commit -q -m base

# The tripwire honors IRIN_TEST_WEAKENING_ACK from the environment; the
# self-test must not inherit an ack from the caller's shell.
run_check() { local rc=0; env -u IRIN_TEST_WEAKENING_ACK bash "$CHECK" main >"$work/out.txt" 2>&1 || rc=$?; echo "$rc"; }
run_check_ack() { local rc=0; env IRIN_TEST_WEAKENING_ACK="$1" bash "$CHECK" main >"$work/out.txt" 2>&1 || rc=$?; echo "$rc"; }
expect_finding() { grep -q "test-weakening: $1" "$work/out.txt" || { cat "$work/out.txt" >&2; fail "missing finding: $1"; }; }

# --- clean source + test change passes ---
printf 'pub fn sub(a: u32, b: u32) -> u32 { a - b }\n' >>src/lib.rs
cat >>tests/contract.rs <<'EOF'
#[test]
fn sub_holds() { assert_eq!(2 - 1, 1); }
EOF
[[ "$(run_check)" == 0 ]] || { cat "$work/out.txt" >&2; fail "clean change should pass"; }
grep -q 'no findings' "$work/out.txt" || fail "clean change should report no findings"
git checkout -q -- . && git clean -qfd

# --- every weakening kind is named and the check refuses ---
rm web/app.test.ts                                        # deleted-test-file
cat >src/lib.rs <<'EOF'                                   # removed-test-cases + assertion-loss
pub fn add(a: u32, b: u32) -> u32 { a + b }
#[cfg(test)]
mod tests {
    // gone
    fn adds() { assert_eq!(super::add(1, 2), 3); }
}
EOF
cat >tests/contract.rs <<'EOF'                            # added-skip
#[test]
#[ignore]
fn contract_holds() { assert_eq!(1, 1); }
EOF
cat >scripts/proof.sh <<'EOF'                             # raised-tunable + proof-escape-hatch
#!/usr/bin/env bash
for _ in $(seq 1 90); do check && break; sleep 1; done
kill "$pid" 2>/dev/null || true
run_proof || true
EOF
cat >scripts/new-hatch.sh <<'EOF'                         # untracked proof file with a hatch
touch done.txt
maybe || true
EOF
rc="$(run_check)"
[[ "$rc" == 1 ]] || { cat "$work/out.txt" >&2; fail "weakened diff should refuse (rc=$rc)"; }
expect_finding 'deleted-test-file web/app.test.ts'
expect_finding 'removed-test-cases src/lib.rs'
expect_finding 'assertion-loss src/lib.rs'
expect_finding 'added-skip tests/contract.rs'
expect_finding 'raised-tunable scripts/proof.sh'
expect_finding 'proof-escape-hatch scripts/proof.sh'
expect_finding 'proof-escape-hatch scripts/new-hatch.sh'
grep -q 'proof-escape-hatch scripts/proof.sh:3' "$work/out.txt" && fail "cleanup kill || true must be tolerated"

# --- explicit ack still prints findings but exits 0 ---
rc="$(run_check_ack 'fixture rewrite, reviewed')"
[[ "$rc" == 0 ]] || { cat "$work/out.txt" >&2; fail "ack should pass"; }
grep -q 'acknowledged by IRIN_TEST_WEAKENING_ACK' "$work/out.txt" || fail "ack not recorded"
expect_finding 'deleted-test-file web/app.test.ts'
git checkout -q -- . && git clean -qfd

# --- source touched with no test touched refuses ---
printf 'pub fn mul(a: u32, b: u32) -> u32 { a * b }\n' >>src/lib.rs
[[ "$(run_check)" == 1 ]] || { cat "$work/out.txt" >&2; fail "untested source should refuse"; }
expect_finding 'source-without-tests src/lib.rs'
git checkout -q -- . && git clean -qfd

# --- editing an inline test assertion counts as touching tests ---
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b + 0 }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); assert_eq!(super::add(0, 0), 0); }
}
EOF
[[ "$(run_check)" == 0 ]] || { cat "$work/out.txt" >&2; fail "inline test edit should count as test touched"; }
git checkout -q -- . && git clean -qfd

# --- production change that drops one inline-test assertion refuses ---
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b + 0 }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); }
}
EOF
[[ "$(run_check)" == 1 ]] || { cat "$work/out.txt" >&2; fail "inline assertion loss should refuse"; }
expect_finding 'assertion-loss src/lib.rs'
git checkout -q -- . && git clean -qfd

# --- production-only assert! is not a test change ---
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { assert!(a < 1000); a + b }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); }
}
EOF
[[ "$(run_check)" == 1 ]] || { cat "$work/out.txt" >&2; fail "production assert must not count as test touched"; }
expect_finding 'source-without-tests src/lib.rs'
git checkout -q -- . && git clean -qfd

# --- a brace inside a test string does not end the test region ---
cat >src/brace.rs <<'EOF'
pub fn label() -> &'static str { "}" }
#[cfg(test)]
mod tests {
    const MARKER: &str = "}"; // a '}' here must not end the region
    const WRAPPED: &str = "line one \
        line two";
    #[test]
    fn labels() { assert_eq!(super::label(), MARKER); assert_eq!(WRAPPED.len(), 25); }
}
EOF
[[ "$(run_check)" == 1 ]] || { cat "$work/out.txt" >&2; fail "assertion after a braced string should still count"; }
expect_finding 'assertion-loss src/brace.rs'
git checkout -q -- . && git clean -qfd

# --- cfg(all(test, ...)) modules are test regions too ---
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b + 0 }
#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); assert_eq!(super::add(0, 0), 0); }
}
EOF
[[ "$(run_check)" == 0 ]] || { cat "$work/out.txt" >&2; fail "cfg(all(test, ..)) edit should count as test touched"; }
git checkout -q -- . && git clean -qfd

# --- `#[cfg(test)] mod tests;` marks no inline region ---
cat >src/extra.rs <<'EOF'
#[cfg(test)]
mod tests;
pub fn later() -> u32 { assert!(true); 2 }
EOF
[[ "$(run_check)" == 1 ]] || { cat "$work/out.txt" >&2; fail "code after a mod tests; declaration is production"; }
expect_finding 'source-without-tests src/extra.rs'
git checkout -q -- . && git clean -qfd

# --- cfg(not(test)) is production, not a test region ---
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b }
#[cfg(not(test))]
pub fn prod() -> u32 { assert!(true); 1 }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); }
}
EOF
[[ "$(run_check)" == 1 ]] || { cat "$work/out.txt" >&2; fail "cfg(not(test)) must not count as test touched"; }
expect_finding 'source-without-tests src/lib.rs'
git checkout -q -- . && git clean -qfd

# --- removing #[ignore] from an inline test counts as touching tests ---
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); }
    #[test]
    #[ignore = "standing reminder"]
    fn armed_later() { assert_eq!(super::add(2, 2), 4); }
}
EOF
git add -A && git commit -q -m ignored-base
cat >src/lib.rs <<'EOF'
pub fn add(a: u32, b: u32) -> u32 { a + b + 0 }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); assert!(true); }
    #[test]
    fn armed_later() { assert_eq!(super::add(2, 2), 4); }
}
EOF
[[ "$(run_check)" == 0 ]] || { cat "$work/out.txt" >&2; fail "un-ignoring an inline test should count as test touched"; }
git checkout -q -- . && git clean -qfd

# --- docs-only change passes ---
printf 'notes\n' >README.md
[[ "$(run_check)" == 0 ]] || { cat "$work/out.txt" >&2; fail "docs-only change should pass"; }

printf 'test-check-test-weakening: PASS\n'
