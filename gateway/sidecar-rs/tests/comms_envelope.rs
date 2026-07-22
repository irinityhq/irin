//! T22 — CommsEnvelope CloudEvents 1.0 profile golden fixtures.
//!
//! Verifies the CE 1.0 required attributes (`specversion`, `id`, `source`,
//! `type`, `time`, `datacontenttype`, `data`) and the IRIN
//! schema-versioned wrapper `{"v":1, "envelope":{...}}`.

use gateway_sidecar::comms::envelope::{CommsEnvelope, EnvelopeKind, EnvelopeWrapper};
use serde_json::json;

/// Build a representative Escalation envelope for golden assertions.
fn build_escalation() -> EnvelopeWrapper {
    CommsEnvelope::builder(EnvelopeKind::Escalation)
        .sentinel_name("file-inbox-watch")
        .tenant("sovereign")
        .ttl_seconds(60)
        .budget_hint("council:triage")
        .reply_to("https://gateway.local/audit/correlate/abc")
        .data(json!({"reason": "new file in inbox", "path": "/in/test.pdf"}))
        .build()
        .expect("test envelope: all required fields set")
        .wrap()
}

#[test]
fn t22a_wrapper_has_envelope_schema_version_one() {
    let wrapper = build_escalation();
    let v = serde_json::to_value(&wrapper).unwrap();
    assert_eq!(v["v"], json!(1), "wrapper.v MUST be 1 for v0.1");
    assert!(v["envelope"].is_object(), "wrapper.envelope MUST be object");
}

#[test]
fn t22b_cloudevents_1_0_required_attributes_present() {
    let wrapper = build_escalation();
    let v = serde_json::to_value(&wrapper).unwrap();
    let env = &v["envelope"];

    // CloudEvents 1.0 §3.1 REQUIRED context attributes
    assert_eq!(
        env["specversion"],
        json!("1.0"),
        "CE specversion MUST be 1.0"
    );
    assert!(
        env["id"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "CE id MUST be non-empty string"
    );
    assert!(env["source"].is_string(), "CE source MUST be present");
    assert!(env["type"].is_string(), "CE type MUST be present");

    // IRIN profile additions (all required for our v0.1)
    assert!(
        env["time"].is_string(),
        "time MUST be present (IRIN profile)"
    );
    assert_eq!(env["datacontenttype"], json!("application/json"));
    assert!(env["data"].is_object(), "data MUST be a JSON object");
}

#[test]
fn t22c_source_is_uri_with_irin_sentinel_urn_scheme() {
    let wrapper = build_escalation();
    let v = serde_json::to_value(&wrapper).unwrap();
    let source = v["envelope"]["source"].as_str().unwrap();
    assert_eq!(
        source, "urn:irin:sentinel:file-inbox-watch",
        "source MUST be urn:irin:sentinel:{{name}} (Grok G6)"
    );
}

#[test]
fn t22d_time_is_rfc3339_utc() {
    let wrapper = build_escalation();
    let v = serde_json::to_value(&wrapper).unwrap();
    let time = v["envelope"]["time"].as_str().unwrap();
    // Format: YYYY-MM-DDTHH:MM:SSZ  (RFC 3339 with Z designator)
    let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
    assert!(re.is_match(time), "time MUST be RFC3339 UTC, got {time}");
}

#[test]
fn t22e_type_namespaced_by_kind() {
    let esc = build_escalation();
    let ev = serde_json::to_value(&esc).unwrap();
    assert_eq!(
        ev["envelope"]["type"],
        json!("irin.escalation.v0.1"),
        "Escalation type MUST be irin.escalation.v0.1"
    );

    let dir = CommsEnvelope::builder(EnvelopeKind::Directive)
        .sentinel_name("council-chair")
        .tenant("sovereign")
        .ttl_seconds(120)
        .budget_hint("worker:30m")
        .reply_to("https://gateway.local/audit/dir/x")
        .data(json!({"job": "summarize", "stop_after": "1"}))
        .build()
        .expect("test envelope: all required fields set")
        .wrap();
    let dv = serde_json::to_value(&dir).unwrap();
    assert_eq!(
        dv["envelope"]["type"],
        json!("irin.directive.v0.1"),
        "Directive type MUST be irin.directive.v0.1"
    );
}

#[test]
fn t22f_data_carries_comms_spine_fields() {
    let wrapper = build_escalation();
    let v = serde_json::to_value(&wrapper).unwrap();
    let data = &v["envelope"]["data"];

    // COMMS_CONTRACT v0.1 spine fields live inside CE `data`.
    assert_eq!(data["contract"], json!("irin.comms.v0.1"));
    assert_eq!(data["kind"], json!("Escalation"));
    assert_eq!(data["tenant"], json!("sovereign"));
    assert_eq!(data["ttl_seconds"], json!(60));
    assert_eq!(data["budget_hint"], json!("council:triage"));
    assert_eq!(
        data["reply_to"],
        json!("https://gateway.local/audit/correlate/abc")
    );
    assert!(
        data["payload"].is_object(),
        "payload MUST be product-owned JSON object"
    );
    assert_eq!(data["payload"]["reason"], json!("new file in inbox"));
}

#[test]
fn t22g_round_trip_serde_preserves_envelope() {
    let wrapper = build_escalation();
    let s = serde_json::to_string(&wrapper).unwrap();
    let back: EnvelopeWrapper = serde_json::from_str(&s).unwrap();
    let v1 = serde_json::to_value(&wrapper).unwrap();
    let v2 = serde_json::to_value(&back).unwrap();
    assert_eq!(v1, v2, "round-trip serde must be lossless");
}

#[test]
fn t22h_ids_are_unique_per_build() {
    let a = build_escalation();
    let b = build_escalation();
    let va = serde_json::to_value(&a).unwrap();
    let vb = serde_json::to_value(&b).unwrap();
    assert_ne!(
        va["envelope"]["id"], vb["envelope"]["id"],
        "each envelope MUST have a unique id"
    );
}
