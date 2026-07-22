//! Negative tests for the `CommsEnvelope` custom `Deserialize` reject paths.
//!
//! The three inbound-wire validation branches in `envelope.rs` — bad
//! `specversion`, bad
//! `datacontenttype`, unknown `type` — existed but had only happy-path coverage
//! (`wire_golden.rs` round-trips a *valid* envelope, never feeds a bad one). The
//! custom `Deserialize` IS the inbound-wire validation gate on the comms-contract
//! seam: a stale or tampered envelope that slips a reject branch would be silently
//! accepted. These pin each branch returning `Err`, with a positive control so a
//! green test can never mean "the fixture was already malformed".

use serde_json::{Value, json};
use sovereign_protocol::comms::envelope::{CommsEnvelope, EnvelopeKind};

/// A valid CommsEnvelope as a JSON `Value` (the `envelope` object inside the
/// schema-versioned wrapper) — the exact shape an inbound consumer parses.
fn valid_envelope_value(kind: EnvelopeKind) -> Value {
    let wrapper = CommsEnvelope::builder(kind)
        .sentinel_name("file-inbox-watch")
        .tenant("sovereign")
        .ttl_seconds(60)
        .budget_hint("council:triage")
        .reply_to("https://gateway.local/audit/correlate/abc")
        .data(json!({"reason": "new file in inbox"}))
        .build()
        .expect("test envelope: all required fields set")
        .wrap();
    serde_json::to_value(&wrapper).unwrap()["envelope"].clone()
}

#[test]
fn rejects_bad_specversion() {
    // Positive control: the unmutated value MUST deserialize. If this assert held
    // but the one below did too, the fixture would be the bug, not the gate.
    let env = valid_envelope_value(EnvelopeKind::Escalation);
    assert!(
        serde_json::from_value::<CommsEnvelope>(env.clone()).is_ok(),
        "control: a valid envelope must deserialize"
    );

    let mut bad = env;
    bad["specversion"] = json!("2.0");
    let err = serde_json::from_value::<CommsEnvelope>(bad)
        .expect_err("bad specversion must be rejected")
        .to_string();
    assert!(err.contains("specversion mismatch"), "got: {err}");
}

#[test]
fn rejects_bad_datacontenttype() {
    let mut bad = valid_envelope_value(EnvelopeKind::Escalation);
    bad["datacontenttype"] = json!("text/plain");
    let err = serde_json::from_value::<CommsEnvelope>(bad)
        .expect_err("bad datacontenttype must be rejected")
        .to_string();
    assert!(err.contains("datacontenttype mismatch"), "got: {err}");
}

#[test]
fn rejects_unknown_type() {
    let mut bad = valid_envelope_value(EnvelopeKind::Escalation);
    bad["type"] = json!("irin.evil.v9.9");
    let err = serde_json::from_value::<CommsEnvelope>(bad)
        .expect_err("unknown envelope type must be rejected")
        .to_string();
    assert!(err.contains("unknown envelope type"), "got: {err}");
}

#[test]
fn accepts_both_known_types() {
    // Directive is the other valid `type` arm — proves the type match accepts the
    // full known set, not just escalation, so the unknown-type reject above is a
    // real allowlist and not an accident of the escalation fixture.
    for kind in [EnvelopeKind::Escalation, EnvelopeKind::Directive] {
        let env = valid_envelope_value(kind);
        assert!(
            serde_json::from_value::<CommsEnvelope>(env).is_ok(),
            "known type {kind:?} must deserialize"
        );
    }
}
