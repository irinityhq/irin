//! Tests that the Triage cabinet emits the correct C11 fence shape.
//! These must pass before any Phase 3 dispatcher work can succeed.

use std::fs;

#[test]
fn triage_yaml_emits_proposal_v1_not_payload_v1() {
    let yaml = fs::read_to_string("cabinets/triage.yaml").expect("failed to read triage.yaml");

    assert!(
        yaml.contains("irin.directive.proposal.v1"),
        "cabinet must instruct chair to emit proposal.v1 fence (C11)"
    );
    assert!(
        !yaml.contains("irin.directive.payload.v1"),
        "cabinet MUST NOT instruct chair to emit payload.v1 — that's the dispatcher-only persisted shape"
    );
}

#[test]
fn triage_yaml_does_not_template_session_or_cost_in_fence() {
    let yaml = fs::read_to_string("cabinets/triage.yaml").expect("failed to read triage.yaml");

    // session_id + cost_usd are dispatcher-injected from response headers
    // (X-Council-Session-Id + X-Total-Cost-Usd). Cabinet must not template them.
    let in_required = yaml
        .lines()
        .skip_while(|l| !l.contains("Required Directive shape"))
        .take_while(|l| !l.contains("If verdict=Dismiss"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !in_required.contains("council_session_id"),
        "fence template must not include council_session_id — dispatcher-injected"
    );
    assert!(
        !in_required.contains("council_cost_usd"),
        "fence template must not include council_cost_usd — dispatcher-injected"
    );
}

#[test]
fn triage_yaml_scope_tenant_is_act_only_and_dismiss_omits_scope() {
    let yaml = fs::read_to_string("cabinets/triage.yaml").expect("failed to read triage.yaml");

    // Must explicitly restrict scope.tenant to Act proposals (P5.2 P0-A + C11 safety)
    assert!(
        yaml.contains("Act proposals only") || yaml.contains("Act-only"),
        "cabinet must explicitly say scope.tenant applies to Act proposals only; Dismiss must omit scope"
    );

    // Guard against misleading wording that implies the whole fence is Act-only
    assert!(
        !yaml.contains("(Act proposal)"),
        "cabinet must not use '(Act proposal)' phrasing; the proposal.v1 fence supports both Act and Dismiss"
    );

    // Must explicitly say Dismiss proposals omit scope (prevents chair from emitting scope on Dismiss)
    let dismiss_section = yaml
        .lines()
        .skip_while(|l| !l.to_lowercase().contains("if verdict=dismiss"))
        .take_while(|l| !l.contains("Note:"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();

    assert!(
        dismiss_section.contains("omit") && dismiss_section.contains("scope"),
        "Dismiss branch must explicitly say scope (and job/stop/return) must be omitted"
    );
}
