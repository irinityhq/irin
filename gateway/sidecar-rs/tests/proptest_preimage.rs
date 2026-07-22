//! Property test on the length-prefixed v3 preimage builder.
//!
//! Asserts no field-permutation collisions even with adversarial inputs
//! containing `|` and `:` literals. Pure-function test — no dependency on
//! the production database. This proves the preimage scheme is sound in
//! isolation.

use proptest::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

fn preimage_v3(
    tenant: &str,
    sentinel: &str,
    fired_at: i64,
    state_json: &str,
    reason: &str,
    prev_hash: &str,
) -> String {
    let fired_at_str = fired_at.to_string();
    format!(
        "{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}",
        tenant.len(),
        tenant,
        sentinel.len(),
        sentinel,
        fired_at_str.len(),
        fired_at_str,
        state_json.len(),
        state_json,
        reason.len(),
        reason,
        prev_hash.len(),
        prev_hash
    )
}

proptest! {
    /// No two field-sets with adversarial `|:\n\t\0` content produce the same
    /// preimage when fields differ. Length-prefixing should make collision
    /// impossible.
    #[test]
    fn preimage_v3_is_collision_resistant_under_adversarial_field_content(
        t1 in "[A-Za-z0-9 |:\\n\\t\\x00]{0,200}",
        s1 in "[A-Za-z0-9 |:\\n\\t\\x00]{0,200}",
        t2 in "[A-Za-z0-9 |:\\n\\t\\x00]{0,200}",
        s2 in "[A-Za-z0-9 |:\\n\\t\\x00]{0,200}",
    ) {
        let p1 = preimage_v3(&t1, &s1, 0, "", "", "");
        let p2 = preimage_v3(&t2, &s2, 0, "", "", "");
        prop_assert_eq!(p1 == p2, t1 == t2 && s1 == s2);
    }

    /// Permuting field assignments must produce distinct preimages whenever
    /// the underlying field values differ.
    #[test]
    fn preimage_v3_field_permutation_is_injective(
        a in "[A-Za-z0-9 |:\\n\\t\\x00]{0,100}",
        b in "[A-Za-z0-9 |:\\n\\t\\x00]{0,100}",
    ) {
        prop_assume!(a != b);
        let preimage_ab = preimage_v3(&a, &b, 0, "", "", "");
        let preimage_ba = preimage_v3(&b, &a, 0, "", "", "");
        prop_assert_ne!(preimage_ab, preimage_ba);
    }
}

/// Golden corpus coverage for watch audit events.
#[test]
fn preimage_corpus_covers_audit_table() {
    let expected_events = expected_audit_event_types();
    let fixtures = corpus_fixtures();
    let corpus_events: BTreeSet<_> = fixtures
        .iter()
        .map(|fixture| fixture.event_type.clone())
        .collect();

    assert_eq!(
        fixtures.len(),
        corpus_events.len(),
        "preimage corpus must not contain duplicate event_type fixtures"
    );

    let missing: Vec<_> = expected_events
        .difference(&corpus_events)
        .cloned()
        .collect();
    let extra: Vec<_> = corpus_events
        .difference(&expected_events)
        .cloned()
        .collect();

    assert!(
        missing.is_empty() && extra.is_empty(),
        "preimage corpus must match the watch audit event contract; missing={missing:?}, extra={extra:?}"
    );
}

#[test]
fn preimage_corpus_hashes_match_watch_v3_format() {
    for fixture in corpus_fixtures() {
        assert!(
            fixture.state_json.starts_with("{\"event_type\":"),
            "state_json for {} must keep event_type first as part of the committed corpus",
            fixture.event_type
        );

        let preimage = preimage_v3(
            &fixture.tenant,
            &fixture.sentinel,
            fixture.fired_at,
            &fixture.state_json,
            &fixture.reason,
            &fixture.prev_hash,
        );
        assert_eq!(
            fixture.hash,
            sha256_hex(preimage.as_bytes()),
            "hash drift for {}",
            fixture.event_type
        );
    }
}

#[test]
fn committed_escalation_received_vector_matches_anchor() {
    let fixture = corpus_fixtures()
        .into_iter()
        .find(|fixture| fixture.event_type == "escalation_received")
        .expect("escalation_received fixture missing");

    assert_eq!(
        fixture.hash,
        "37b45ae77c065d081827a1bef87fe12d6e1771f16de25d11f8254f990c9dad09"
    );
}

#[test]
fn directive_dismissed_fixture_omits_act_only_fields() {
    let fixture = corpus_fixtures()
        .into_iter()
        .find(|fixture| fixture.event_type == "directive_dismissed")
        .expect("directive_dismissed fixture missing");
    let state: Value =
        serde_json::from_str(&fixture.state_json).expect("directive_dismissed state_json parses");
    let payload = state
        .get("payload")
        .and_then(Value::as_object)
        .expect("directive_dismissed fixture should carry persisted payload object");

    assert_eq!(
        payload.get("verdict").and_then(Value::as_str),
        Some("Dismiss")
    );
    for key in ["job", "scope", "stop_condition", "return_expectation"] {
        assert!(
            !payload.contains_key(key),
            "Dismiss payload must omit Act-only field {key}, not encode it as null"
        );
    }
}

#[derive(Debug, Deserialize)]
struct PreimageFixture {
    event_type: String,
    tenant: String,
    sentinel: String,
    fired_at: i64,
    state_json: String,
    reason: String,
    prev_hash: String,
    hash: String,
}

fn corpus_fixtures() -> Vec<PreimageFixture> {
    serde_json::from_str(include_str!("fixtures/preimage_corpus.json"))
        .expect("preimage corpus fixture should parse")
}

fn expected_audit_event_types() -> BTreeSet<String> {
    [
        "directive_ack_tenant_mismatch",
        "directive_acked",
        "directive_authority_rejected",
        "directive_clock_skew_normalized",
        "directive_correlation_failed",
        "directive_cost_excessive",
        "directive_dismissed",
        "directive_expired_in_outbox",
        "directive_parse_failed",
        "directive_received",
        "directive_staged",
        "directive_tenant_mismatch",
        "directive_verdict_invalid",
        "dispatch_dead_lettered",
        "escalation_channel_dropped",
        "escalation_dispatched",
        "escalation_expired",
        "escalation_failed",
        "escalation_origin_invalid",
        "escalation_rate_limited",
        "escalation_received",
        "escalation_recovered_pre_response",
        "escalation_recovered_response_intact",
        "escalation_recovered_resume_outbox",
        "escalation_recovery_max_iterations",
        "escalation_replay_detected",
        "escalation_replay_terminal",
        "escalation_schema_rejected",
        "escalation_unknown_sentinel",
        "escalation_unparseable_envelope",
        "escalation_watchdog_drained_staged",
        "escalation_watchdog_recovered_response",
        "escalation_watchdog_wedged",
        "outbox_recovered_from_restart",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
