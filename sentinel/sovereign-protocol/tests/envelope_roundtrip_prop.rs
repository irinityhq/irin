//! Randomized seam hardening for `CommsEnvelope` / `EnvelopeWrapper`.
//!
//! The wire goldens (`wire_golden.rs`) pin ONE fixed envelope. This file adds
//! property-based coverage over random *valid* envelope inputs — tricky strings
//! (unicode, quotes, escapes) and finite floats (the `ryu` canonicalization path):
//!
//!   1. serde round-trip is lossless: `to_string -> from_str` preserves the value.
//!   2. JCS encoding is deterministic and valid UTF-8 for every input it accepts.
//!
//! This does NOT re-test JCS *correctness* — the `json-canon` differential oracle
//! in `jcs_conformance.rs` owns that. It owns "random valid input stays stable and
//! never panics on the signed-bytes path." Cheap (proptest is dev-only), it
//! complements the oracle and the goldens.

use proptest::prelude::*;
use serde_json::json;
use sovereign_protocol::comms::envelope::{CommsEnvelope, EnvelopeKind, EnvelopeWrapper};
use sovereign_protocol::jcs;

/// Printable ASCII + a band of BMP unicode, bounded. Excludes control chars to
/// keep the strategy fast; control-char JCS escaping is already pinned by the
/// cyberphone vectors in `jcs_conformance.rs`.
/// Use `\x{...}` (the `regex` crate's Unicode-scalar escape — proptest parses the
/// pattern with `regex-syntax`). Both `\u{...}` and `\x{...}` parse here, but `\x`
/// is the canonical regex form and avoids confusion with Rust string-literal `\u`.
const TEXT: &str = r"[\x20-\x7e\x{a1}-\x{2fff}]{0,32}";

/// Fixed valid RFC 3339 UTC stamp so the generated envelope is fully derived from
/// the proptest RNG. `CommsEnvelope::build()` otherwise injects a wall-clock `time`
/// and a random `id`, which would make shrinking/replay non-deterministic
/// (regression review). `id` is drawn from the RNG below; `time` is pinned.
const FIXED_TIME: &str = "2026-01-01T00:00:00Z";

prop_compose! {
    fn arb_envelope()(
        is_escalation in any::<bool>(),
        name in TEXT,
        tenant in TEXT,
        ttl in any::<u64>(),
        budget in TEXT,
        reply in TEXT,
        // id from the RNG (hex32, matches the production id shape) → reproducible.
        id in "[0-9a-f]{32}",
        s_val in TEXT,
        // finite only: JCS check_finite rejects NaN/inf by design (tested elsewhere).
        f_val in any::<f64>().prop_filter("finite", |x| x.is_finite()),
        n_val in any::<i64>(),
        b_val in any::<bool>(),
    ) -> EnvelopeWrapper {
        let kind = if is_escalation { EnvelopeKind::Escalation } else { EnvelopeKind::Directive };
        CommsEnvelope::builder(kind)
            .sentinel_name(&name)
            .tenant(&tenant)
            .ttl_seconds(ttl)
            .budget_hint(&budget)
            .reply_to(&reply)
            .id(&id)
            .time(FIXED_TIME)
            .data(json!({ "s": s_val, "f": f_val, "n": n_val, "b": b_val }))
            .build()
            .expect("test envelope: all required fields set")
            .wrap()
    }
}

proptest! {
    /// serde round-trip must be lossless for any valid envelope.
    #[test]
    fn prop_round_trip_lossless(wrapper in arb_envelope()) {
        let s = serde_json::to_string(&wrapper).expect("serialize");
        let back: EnvelopeWrapper = serde_json::from_str(&s).expect("deserialize valid envelope");
        prop_assert_eq!(
            serde_json::to_value(&wrapper).unwrap(),
            serde_json::to_value(&back).unwrap(),
        );
    }

    /// JCS over the signed bytes is deterministic (idempotent) and valid UTF-8.
    #[test]
    fn prop_jcs_deterministic(wrapper in arb_envelope()) {
        let a = jcs::to_jcs_bytes(&wrapper).expect("jcs encode");
        let b = jcs::to_jcs_bytes(&wrapper).expect("jcs encode (repeat)");
        prop_assert_eq!(&a, &b, "JCS must be deterministic for identical input");
        prop_assert!(String::from_utf8(a).is_ok(), "JCS output must be valid UTF-8");
    }
}
