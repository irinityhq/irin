//! T1 CI diagnostic (NOT a product surface): print the EMBEDDED build identity
//! of a freshly compiled sidecar so the Phase 3 smoke can prove the overlay
//! binary is clean+identifiable and explicitly test-eligible BEFORE it tries a
//! real arm. Links the same `gateway_sidecar` lib crate the binary
//! compiles, so its baked `option_env!` identity matches the binary's. Tracked
//! (not src/) so its presence does not dirty the tree.
fn main() {
    println!(
        "BUILDID_PROBE build_id={} build_is_dirty={} release_eligible={} may_arm_for_real={}",
        gateway_sidecar::watch::attest::build_id(),
        gateway_sidecar::watch::attest::build_is_dirty(),
        gateway_sidecar::watch::attest::build_is_release_eligible(),
        gateway_sidecar::watch::attest::build_may_arm_for_real(
            gateway_sidecar::watch::attest::build_is_dirty(),
            gateway_sidecar::watch::attest::build_is_release_eligible(),
        )
    );
}
