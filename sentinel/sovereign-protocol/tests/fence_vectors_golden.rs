//! Authoring guards for the cross-component directive-fence golden corpus
//! (`src/vectors/directive_fence_cases.json`). These do NOT run a fence — that
//! happens in each consumer component (gateway + council-rs). They guard that the
//! shared vector is well-formed so consumers can treat the loader as infallible.

use sovereign_protocol::fence_vectors::{FenceExpect, directive_fence_golden_cases};
use std::collections::HashSet;

#[test]
fn corpus_parses_and_is_non_trivial() {
    let cases = directive_fence_golden_cases();
    assert!(
        cases.len() >= 5,
        "expected a non-trivial corpus, got {}",
        cases.len()
    );
    assert!(
        cases.iter().any(|c| c.expect == FenceExpect::Accept),
        "corpus MUST contain at least one accept case"
    );
    assert!(
        cases.iter().any(|c| c.expect == FenceExpect::Reject),
        "corpus MUST contain at least one reject case"
    );
}

#[test]
fn case_names_are_unique() {
    let cases = directive_fence_golden_cases();
    let mut seen = HashSet::new();
    for c in &cases {
        assert!(
            seen.insert(c.name.clone()),
            "duplicate case name: {}",
            c.name
        );
    }
}

#[test]
fn reject_cases_carry_a_reason_substring_accept_cases_do_not() {
    for c in directive_fence_golden_cases() {
        match c.expect {
            FenceExpect::Reject => assert!(
                c.reason_substring.as_deref().is_some_and(|s| !s.is_empty()),
                "reject case {} MUST carry a non-empty reason_substring",
                c.name
            ),
            FenceExpect::Accept => assert!(
                c.reason_substring.is_none(),
                "accept case {} MUST NOT carry a reason_substring",
                c.name
            ),
        }
    }
}

#[test]
fn every_proposal_is_a_json_object() {
    for c in directive_fence_golden_cases() {
        assert!(
            c.proposal.is_object(),
            "case {} proposal must be a JSON object",
            c.name
        );
    }
}

#[test]
fn act_cases_pin_scope_tenant_to_the_case_tenant() {
    // Guards the receiver-adapter contract: a gateway Act case only accepts
    // when scope.tenant == the tenant the adapter passes as expected_tenant.
    // Every Act case MUST set scope.tenant (presence is required, not merely
    // checked-if-present) and it MUST equal c.tenant — else the case would
    // diverge across fences for the wrong reason. A future Act case missing
    // scope/scope.tenant is an authoring error this guard now fails on.
    for c in directive_fence_golden_cases() {
        let is_act = c.proposal.get("verdict").and_then(|v| v.as_str()) == Some("Act");
        if !is_act {
            continue;
        }
        let scope_tenant = c
            .proposal
            .get("scope")
            .and_then(|s| s.get("tenant"))
            .and_then(|t| t.as_str())
            .unwrap_or_else(|| {
                panic!("Act case {} MUST set scope.tenant", c.name);
            });
        assert_eq!(
            scope_tenant, c.tenant,
            "Act case {} scope.tenant must equal case.tenant",
            c.name
        );
    }
}
