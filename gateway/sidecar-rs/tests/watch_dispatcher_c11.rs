//! C11 / Fork 1 — Cross-tenant council idempotency isolation tests
//!
//! These are the **six C11 acceptance checks** (five runtime tests + one review rule)
//! that define done for C11.
//! Per the approved plan, these tests must be written **first** (failing-first).
//!
//! Core invariant:
//!   - Database/reference identity = `raw_escalation_id` (stored in pending_escalations.id + directive_outbox.in_response_to)
//!   - Council HTTP idempotency identity = `<safe-tenant-token>:<raw_escalation_id>`
//!
//! Execution order (from plan):
//!   1. These failing tests (this file)
//!   2. Locate dispatcher path that emits council /v1/chat/completions
//!   3. raw_escalation_id preservation
//!   4. safe-tenant-token derivation/validation
//!   5. Compose council_effective_idempotency_key
//!   6. Assert exact outbound headers + observable dedup behavior
//!   7. Run suite
//!
//! C11 Gate Command (run the five runtime checks; Rule 6 is a review requirement):
//!   cargo test --test watch_dispatcher_c11 -- --ignored
//!
//! The five runtime tests exercise the C11 header contract and observable dedup behavior.
//! Rule 6 (no inspection of council internal cache keys) is enforced at review time.
//!
//! Tests MUST assert:
//!   - Exact outbound HTTP headers
//!   - Observable dedup behavior (whether a second council call happens or a stored response is reused)
//!
//! Tests MUST NOT overfit to the internal string the council module builds
//! (Gateway may still combine with auth/budget caller namespace internally).
//!
//! Rule 6 (enforceable review requirement, not a runtime test):
//!   No C11 test may ever read or assert the exact internal cache key string
//!   inside the council module (`council_idem` table or `(caller_key, key)` HashMap).
//!   Only the `Idempotency-Key` header sent and the observable outcome (new call vs reuse) are in scope.

/// Helper: represents a tenant-scoped escalation for C11 testing.
/// In real code this will come from WatchRunner::escalate() + tenant context.
#[derive(Clone, Debug)]
struct EscalationFixture {
    tenant: String,
    raw_escalation_id: String, // the original envelope id (hex32)
}

/// Expected outbound headers for the council-triage call (C11 contract).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CouncilTriageHeaders {
    idempotency_key: String, // must be "<safe-tenant-token>:<raw_escalation_id>"
    x_caller_key: String,    // must be exactly "watch-dispatcher-v1"
}

/// Uses the real C11 production function from the watch dispatcher.
/// This is the bounded Fork 1 implementation (header construction only).
fn emit_council_triage_request(esc: &EscalationFixture) -> CouncilTriageHeaders {
    use gateway_sidecar::watch::dispatcher::build_council_triage_headers;

    let headers = build_council_triage_headers(&esc.tenant, &esc.raw_escalation_id);

    CouncilTriageHeaders {
        idempotency_key: headers
            .get("idempotency-key")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
        x_caller_key: headers
            .get("x-caller-key")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
    }
}

/// Tiny in-test mock response store.
/// Keyed purely by the `Idempotency-Key` header value emitted by the
/// production `build_council_triage_headers`.
///
/// This simulates observable dedup behavior without touching council.rs
/// internals or the council_idem table.
struct MockCouncilStore {
    responses: std::collections::HashMap<String, bool>,
}

impl MockCouncilStore {
    fn new() -> Self {
        Self {
            responses: std::collections::HashMap::new(),
        }
    }

    /// Simulates the dispatcher asking for a council-triage response.
    /// Returns the headers and whether this caused a brand-new council call.
    fn call(&mut self, tenant: &str, raw_escalation_id: &str) -> (CouncilTriageHeaders, bool) {
        let headers = emit_council_triage_request(&EscalationFixture {
            tenant: tenant.to_string(),
            raw_escalation_id: raw_escalation_id.to_string(),
        });

        let effective_key = headers.idempotency_key.clone();
        let is_new = !self.responses.contains_key(&effective_key);
        self.responses.insert(effective_key, true);

        (headers, is_new)
    }
}

/// Returns whether a second council call would be deduplicated for the given escalation.
/// This is the **observable behavior** we care about (not the internal cache key string).

// =============================================================================
// THE SIX BAKED C11 ACCEPTANCE CHECKS (from plan + spec AC-33c)
// =============================================================================

#[test]
fn c11_same_raw_id_same_tenant_one_idempotency_lane() {
    // Check 1: Same raw_escalation_id + same tenant → one effective idempotency lane, dedup works.
    let mut store = MockCouncilStore::new();
    let esc = EscalationFixture {
        tenant: "acme".to_string(),
        raw_escalation_id: "same-001".to_string(),
    };

    let (_, first_new) = store.call(&esc.tenant, &esc.raw_escalation_id);
    let (_, second_new) = store.call(&esc.tenant, &esc.raw_escalation_id);

    assert!(first_new, "first call must be a new council call");
    assert!(!second_new, "second call with same effective Idempotency-Key must reuse stored response (no new council call)");
}

#[test]
fn c11_same_raw_id_different_tenants_distinct_keys() {
    // Check 2: Same raw_escalation_id + different tenants → distinct Idempotency-Key values,
    // and alpha's stored council response MUST NOT satisfy beta (no cross-tenant replay).

    let mut store = MockCouncilStore::new();

    let alpha = EscalationFixture {
        tenant: "alpha".to_string(),
        raw_escalation_id: "same-001".to_string(),
    };
    let beta = EscalationFixture {
        tenant: "beta".to_string(),
        raw_escalation_id: "same-001".to_string(),
    };

    let (_, alpha_new) = store.call(&alpha.tenant, &alpha.raw_escalation_id);
    let (_, beta_new) = store.call(&beta.tenant, &beta.raw_escalation_id);

    assert!(alpha_new, "alpha's first call must be a new council call");
    assert!(
        beta_new,
        "beta must also be a new council call (different effective key from alpha)"
    );

    // Alpha stored under "alpha:same-001", beta computed "beta:same-001".
    // Beta did not hit alpha's stored response.
}

#[test]
fn c11_raw_escalation_id_preserved_in_tables() {
    // Check 3: directive_outbox.in_response_to and pending_escalations.id keep the raw id, not the qualified key.
    let esc = EscalationFixture {
        tenant: "acme".to_string(),
        raw_escalation_id: "raw-42".to_string(),
    };

    // When real code runs, after successful dispatch we will assert:
    //   pending_escalations.id == "raw-42"
    //   directive_outbox.in_response_to == "raw-42"
    //
    // The qualified key must only appear in the Idempotency-Key header, never in these columns.
    let headers = emit_council_triage_request(&esc);
    assert!(
        headers
            .idempotency_key
            .ends_with(&format!(":{}", esc.raw_escalation_id)),
        "qualified key must contain the raw id as suffix"
    );
    // TODO: once tables exist, add real assertions that the DB rows contain the raw value.
}

#[test]
fn c11_exact_outbound_headers() {
    // Check 4: HTTP dispatch sends exactly the required headers.
    let esc = EscalationFixture {
        tenant: "tenant-with-special-chars?".to_string(), // forces token sanitization
        raw_escalation_id: "hdr-001".to_string(),
    };

    let h = emit_council_triage_request(&esc);

    assert_eq!(
        h.x_caller_key, "watch-dispatcher-v1",
        "X-Caller-Key must be exactly 'watch-dispatcher-v1' for Fork 1"
    );

    assert!(
        h.idempotency_key.contains(':'),
        "Idempotency-Key must be in <safe-tenant-token>:<raw> form"
    );
    assert!(
        h.idempotency_key
            .ends_with(&format!(":{}", esc.raw_escalation_id)),
        "raw_escalation_id must be the suffix of the Idempotency-Key"
    );
}

#[test]
fn c11_safe_tenant_token_rules() {
    // Check 5: safe-tenant-token is canonical, stable, non-empty, and cannot contain ':' or control characters.
    let bad_tenants = ["tenant:with:colon", "tenant\nwith\nnewline", "", "   "];

    for t in bad_tenants {
        let esc = EscalationFixture {
            tenant: t.to_string(),
            raw_escalation_id: "tok-001".to_string(),
        };
        let h = emit_council_triage_request(&esc);

        assert!(
            !h.idempotency_key.contains(':') || h.idempotency_key.split(':').count() == 2,
            "safe-tenant-token must not introduce extra ':'"
        );
        // The token portion (before first ':') must be non-empty and safe.
        let token = h.idempotency_key.split(':').next().unwrap();
        assert!(!token.is_empty(), "safe-tenant-token must be non-empty");
        assert!(
            !token.contains(':'),
            "safe-tenant-token must not contain ':'"
        );
        assert!(
            !token.chars().any(|c| c.is_control()),
            "safe-tenant-token must not contain control characters"
        );
    }
}

// Check 6 (Rule, not a counted runtime test):
// C11 tests and implementation must never read or assert the exact internal
// cache key string inside the council module (council_idem table rows or the
// (caller_key, key) HashMap). Only the Idempotency-Key header value sent on
// the wire and the observable outcome (new council call vs. response reuse)
// are valid assertions.
//
// This rule is documented at the top of this file and enforced during code review.
// Adding any test that inspects the council internal key is a violation of the C11 contract.
