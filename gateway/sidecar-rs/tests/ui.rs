// Trybuild harness for Phase 3a.5 HydrationToken compile-fail tests (P0-β)

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
