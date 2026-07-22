//! Cross-repo directive-fence golden vectors (the invariant, Action 3).
//!
//! ONE accept/reject corpus consumed by BOTH sides of the spec'd mirror:
//! - the council-rs EMITTER fence `validate_directive_proposal_v1`
//! - the gateway RECEIVER fence `validate_proposal_v1_shape`
//!
//! Each repo adds a test that loads [`directive_fence_golden_cases`] and runs
//! ITS OWN fence over every case, asserting the recorded polarity (and, for
//! rejects, a shared reason substring). Because the corpus lives in this shared
//! crate, any edit to either fence's `ALLOWED_ACTIONS` / `ALLOWED_TOP_KEYS` /
//! `ALLOWED_SCOPE_KEYS` flips a case and fails that component's build. The
//! symmetry is self-enforcing instead of asserted in prose. Both fences reject
//! whitespace-padded values without trimming.
//!
//! **Single-fault discipline.** Every `reject` case differs from a valid
//! proposal in exactly one way, so a polarity flip pinpoints the drift and the
//! `reason_substring` is unambiguous. The substrings are verified common
//! substrings of both fences' error renderings:
//! - verb allowlist   → `disallowed verb`
//! - top-level keyset  → `unexpected top-level key`
//! - scope keyset      → `unexpected scope key`
//!
//! **Scope of the contract.** The shared invariant is the schema / authority /
//! verdict shape, the closed keysets, and the verb allowlist — everything both
//! fences decide identically regardless of tenant. The gateway-only
//! `scope.tenant == expected_tenant` check is NOT exercised here (the emitter
//! has no tenant parameter); every Act case sets `scope.tenant` to [`tenant`],
//! which the receiver adapter passes as its `expected_tenant`, so accept-cases
//! accept on both fences.
//!
//! [`tenant`]: DirectiveFenceCase::tenant

// `FenceExpect` + `DirectiveFenceCase` live in `fence_case.rs`, `include!`d here
// so `build.rs` can deserialize the corpus through the SAME struct (identical
// `deny_unknown_fields` schema) with zero drift. They appear in this module
// exactly as before: `sovereign_protocol::fence_vectors::{FenceExpect,
// DirectiveFenceCase}`.
include!("fence_case.rs");

/// The golden corpus, embedded at compile time — single source of truth.
pub const DIRECTIVE_FENCE_GOLDEN_JSON: &str = include_str!("vectors/directive_fence_cases.json");

/// Parse the embedded golden corpus.
///
/// Guaranteed non-empty: a consumer that loops over the result and asserts per
/// case can never silently pass on an empty corpus (a feature-gated-out or
/// reset corpus is a hard error here, in ONE place, instead of a vacuous pass
/// in each consumer's test). This closes the cross-component "vacuous pass" gap from
/// the accessor rather than relying on every consumer to re-guard it.
///
/// # Panics
/// Panics if the embedded `directive_fence_cases.json` is malformed OR empty — a
/// build-time authoring error this crate's own `fence_vectors_golden` test
/// guards against, so consumers can treat it as infallible.
pub fn directive_fence_golden_cases() -> Vec<DirectiveFenceCase> {
    let cases: Vec<DirectiveFenceCase> = serde_json::from_str(DIRECTIVE_FENCE_GOLDEN_JSON)
        .expect("embedded directive_fence_cases.json must be valid");
    assert!(
        !cases.is_empty(),
        "directive_fence golden corpus must be non-empty (a vacuous corpus would let \
         every consumer's mirror test pass without checking anything)"
    );
    cases
}
