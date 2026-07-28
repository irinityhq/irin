//! Selective Kani harnesses for JCS pure logic (Phase 1D).
//!
//! Scope is intentional and narrow:
//! - pure finite-check leaf (`FiniteCheck` serializer) — symbolic over bit patterns
//! - pure integer decimal bytes (`canonical_jcs_integer_*`)
//! - concrete fail-closed public entry for nested non-finite fields
//!
//! Intentionally **not** proved here (yet):
//! - full `to_jcs_bytes` symbolic path (serde_json + ryu explode CBMC)
//! - `has_duplicate_keys` (std HashSet → `CCRandomGenerateBytes`, unsupported by
//!   Kani; covered by unit tests + Miri instead)
//!
//! Normal `cargo test` never compiles this module (`#[cfg(kani)]` only).
//!
//! Run:
//!   cargo kani -p sovereign-protocol
//!   IRIN_KANI_HARNESS=proof_nonfinite_f64_rejected scripts/run-kani.sh

use super::finite_check::{FiniteCheck, FiniteCheckError};
use super::{JcsError, canonical_jcs_integer_i64, to_jcs_bytes};
use serde::Serialize;

/// Every non-finite IEEE-754 bit pattern is rejected by the typed-boundary
/// serializer (W5 P0). Stays on `FiniteCheck` only — no `JcsError` conversion
/// (that monomorphizes serde_json error paths and explodes CBMC).
#[kani::proof]
#[kani::unwind(1)]
fn proof_nonfinite_f64_rejected() {
    let bits: u64 = kani::any();
    let f = f64::from_bits(bits);
    kani::assume(!f.is_finite());
    let result = f.serialize(FiniteCheck);
    assert!(
        matches!(result, Err(FiniteCheckError::NonFinite)),
        "non-finite f64 must yield FiniteCheckError::NonFinite"
    );
}

/// Finite bit patterns pass the leaf guard (no false positive on finite floats).
#[kani::proof]
#[kani::unwind(1)]
fn proof_finite_f64_accepted() {
    let bits: u64 = kani::any();
    let f = f64::from_bits(bits);
    kani::assume(f.is_finite());
    let result = f.serialize(FiniteCheck);
    assert!(result.is_ok(), "finite f64 must pass FiniteCheck");
}

/// Nested non-finite fields fail closed through the public signing entry
/// (concrete NaN / ±Inf — production shape, no free vars).
#[kani::proof]
#[kani::unwind(8)]
fn proof_nonfinite_struct_field_public_entry() {
    #[derive(Serialize)]
    struct Money {
        cost_usd: f64,
    }
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            matches!(
                to_jcs_bytes(&Money { cost_usd: bad }),
                Err(JcsError::NonFinite)
            ),
            "to_jcs_bytes must fail-closed on non-finite field"
        );
    }
}

/// Integer fast path: every i8's decimal form is exactly what
/// [`canonical_jcs_integer_i64`] emits. i8 keeps to_string unwind small.
#[kani::proof]
#[kani::unwind(8)]
fn proof_i8_integer_bytes_exact_decimal() {
    let i: i8 = kani::any();
    let as_i64 = i as i64;
    let bytes = canonical_jcs_integer_i64(as_i64);
    assert_eq!(bytes, as_i64.to_string().into_bytes());
}

/// Finite integer leaves pass FiniteCheck (never false-positive).
#[kani::proof]
#[kani::unwind(1)]
fn proof_finite_i32_passes_check() {
    let i: i32 = kani::any();
    assert!(i.serialize(FiniteCheck).is_ok());
}
