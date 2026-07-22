//! Provenance fabrication-guard + provider-provenance coverage (audit #8).
//!
//! `WorkerProvenanceGuard` is the fabrication guard: it records whether the
//! worker-leg provenance is an opaque correlation handle, a verified-exact run,
//! or unavailable — so the council never *fabricates* a "verified" claim from a
//! correlation it cannot prove. These tests pin the constructor invariants
//! (`fabrication_guard` always set, status matches, handle only present when it
//! exists) and the wire shape (`opaque_handle: None` is omitted, snake_case
//! status round-trips).
//!
//! NOT covered here (deferred by design, not an oversight): cryptographic
//! integrity of client-supplied `worker_provenance` on `/api/deliberate` (audit
//! H-4). The guard is a *honesty* marker, not an authenticity seal — a client
//! can still assert `verified_exact`. Sealing it (gateway MAC / signed header)
//! is the end-to-end council-authorship signing seam (Tier-3, deferred per D2b),
//! a wire-contract change requiring full ceremony; it is intentionally out of
//! scope for this tests-only pass.

use sovereign_protocol::types::{
    ProviderProvenance, ProviderResponse, WorkerProvenanceGuard, WorkerProvenanceStatus,
};

// ---------------------------------------------------------------------------
// WorkerProvenanceGuard constructors
// ---------------------------------------------------------------------------

#[test]
fn new_opaque_with_handle_sets_status_and_guard() {
    let g = WorkerProvenanceGuard::new_opaque(Some("w-12345".to_string()));
    assert_eq!(g.status, WorkerProvenanceStatus::OpaqueHandleOnly);
    assert!(g.fabrication_guard, "fabrication guard must be active");
    assert_eq!(g.opaque_handle.as_deref(), Some("w-12345"));
}

#[test]
fn new_opaque_without_handle_keeps_status_but_none_handle() {
    let g = WorkerProvenanceGuard::new_opaque(None);
    assert_eq!(g.status, WorkerProvenanceStatus::OpaqueHandleOnly);
    assert!(g.fabrication_guard);
    assert!(g.opaque_handle.is_none());
}

#[test]
fn new_unavailable_has_no_handle_and_guard_set() {
    let g = WorkerProvenanceGuard::new_unavailable();
    assert_eq!(g.status, WorkerProvenanceStatus::Unavailable);
    assert!(g.fabrication_guard);
    assert!(g.opaque_handle.is_none());
}

#[test]
fn opaque_handle_none_is_omitted_from_json() {
    // skip_serializing_if = "Option::is_none" — the key must be ABSENT, not null,
    // so a missing handle never round-trips into a fabricated empty string.
    let json = serde_json::to_value(WorkerProvenanceGuard::new_unavailable()).unwrap();
    assert!(
        json.get("opaque_handle").is_none(),
        "opaque_handle key must be absent when None, got {json}"
    );
    assert_eq!(json["status"], "unavailable");
    assert_eq!(json["fabrication_guard"], true);
}

#[test]
fn guard_round_trips_for_every_status() {
    for g in [
        WorkerProvenanceGuard::new_opaque(Some("w-1".to_string())),
        WorkerProvenanceGuard::new_opaque(None),
        WorkerProvenanceGuard::new_unavailable(),
        WorkerProvenanceGuard {
            status: WorkerProvenanceStatus::VerifiedExact,
            fabrication_guard: true,
            opaque_handle: None,
        },
    ] {
        let bytes = serde_json::to_vec(&g).unwrap();
        let back: WorkerProvenanceGuard = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(g, back, "guard must survive a serde round-trip");
    }
}

#[test]
fn status_deserializes_from_snake_case() {
    let verified: WorkerProvenanceStatus = serde_json::from_str("\"verified_exact\"").unwrap();
    assert_eq!(verified, WorkerProvenanceStatus::VerifiedExact);
    let opaque: WorkerProvenanceStatus = serde_json::from_str("\"opaque_handle_only\"").unwrap();
    assert_eq!(opaque, WorkerProvenanceStatus::OpaqueHandleOnly);
    let unavailable: WorkerProvenanceStatus = serde_json::from_str("\"unavailable\"").unwrap();
    assert_eq!(unavailable, WorkerProvenanceStatus::Unavailable);
}

// ---------------------------------------------------------------------------
// ProviderProvenance constructors
// ---------------------------------------------------------------------------

#[test]
fn provider_provenance_api_shape() {
    let p = ProviderProvenance::api("grok");
    assert_eq!(p.runner, "grok");
    assert_eq!(p.access_mode, "api_text_only");
    assert_eq!(p.accounting, "reported_tokens_estimated_cost");
    assert_eq!(p.filesystem, "none");
}

#[test]
fn provider_provenance_api_web_shape() {
    let p = ProviderProvenance::api_web("gemini");
    assert_eq!(p.runner, "gemini");
    assert_eq!(p.access_mode, "api_web_tool");
    assert_eq!(p.accounting, "reported_tokens_estimated_cost");
    assert_eq!(p.filesystem, "none");
}

#[test]
fn provider_provenance_cli_readonly_shape() {
    let p = ProviderProvenance::cli_readonly("codex", "unavailable");
    assert_eq!(p.runner, "codex");
    assert_eq!(p.access_mode, "cli_agent_readonly");
    assert_eq!(p.accounting, "unavailable");
    assert_eq!(p.filesystem, "read_only");
}

#[test]
fn provider_provenance_cli_tools_shape() {
    let p = ProviderProvenance::cli_tools("codex", "unavailable");
    assert_eq!(p.runner, "codex");
    assert_eq!(p.access_mode, "cli_agent_tools");
    assert_eq!(p.accounting, "unavailable");
    assert_eq!(p.filesystem, "tools_unspecified");
}

#[test]
fn provider_provenance_gateway_shape() {
    let p = ProviderProvenance::gateway();
    assert_eq!(p.runner, "gateway");
    assert_eq!(p.access_mode, "gateway");
    assert_eq!(p.accounting, "gateway_reported");
    assert_eq!(p.filesystem, "gateway");
}

#[test]
fn provider_provenance_round_trips() {
    let p = ProviderProvenance::cli_readonly("codex", "unavailable");
    let bytes = serde_json::to_vec(&p).unwrap();
    let back: ProviderProvenance = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(p, back);
}

// ---------------------------------------------------------------------------
// ProviderResponse default + builder
// ---------------------------------------------------------------------------

#[test]
fn provider_response_default_is_zeroed_and_empty() {
    let r = ProviderResponse::default();
    assert_eq!(r.text, "");
    assert_eq!(r.model, "");
    assert_eq!(r.tokens_in, 0);
    assert_eq!(r.tokens_out, 0);
    assert_eq!(r.cached_in, 0);
    assert_eq!(r.latency_ms, 0);
    assert_eq!(r.cost_usd, 0.0);
    assert!(r.error.is_none());
    assert!(r.gateway_provenance.is_none());
    assert!(r.provider_provenance.is_none());
}

#[test]
fn provider_response_default_omits_optional_fields_in_json() {
    let json = serde_json::to_value(ProviderResponse::default()).unwrap();
    for absent in ["error", "gateway_provenance", "provider_provenance"] {
        assert!(
            json.get(absent).is_none(),
            "{absent} must be omitted when None, got {json}"
        );
    }
}

#[test]
fn with_provider_provenance_attaches_provenance() {
    let r = ProviderResponse::default().with_provider_provenance(ProviderProvenance::api("grok"));
    let prov = r.provider_provenance.expect("provenance attached");
    assert_eq!(prov.access_mode, "api_text_only");
    assert_eq!(prov.runner, "grok");
}
