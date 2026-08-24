use council_rs::engine::deliberate::{build_chair_prompt, build_round_prompt};
use council_rs::types::{RoundResult, Seat, SeatResponse};

fn seat(name: &str) -> Seat {
    Seat {
        name: name.into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        system: "test".into(),
    }
}

fn round() -> RoundResult {
    RoundResult {
        round_num: 1,
        responses: vec![
            SeatResponse {
                seat_name: "Analyst".into(),
                provider: "mock".into(),
                text: "Evidence supports a narrow read-only review.".into(),
                ..Default::default()
            },
            SeatResponse {
                seat_name: "Reviewer".into(),
                provider: "grok".into(),
                text: "Peer analysis challenges the timing.".into(),
                ..Default::default()
            },
        ],
        convergence_score: 0.75,
        converged: false,
        judge_provider: None,
        judge_assessment: None,
        judge_gateway_attempts: vec![],
        flip_flop_hash: None,
        validation_report: None,
    }
}

#[test]
fn round_two_seat_prompt_is_golden() {
    let prompt = build_round_prompt(
        "Should IRIN act?",
        "Golden context",
        "Operator asks for scope.",
        "Prior precedent.",
        &[round()],
        "BUDGET SIGNAL",
        &seat("Analyst"),
        2,
    );

    assert_eq!(prompt, include_str!("fixtures/prompts/round2_seat.txt"));
}

#[test]
fn generic_chair_prompt_is_golden() {
    let prompt = build_chair_prompt(
        "Should IRIN act?",
        "Golden context",
        &[round()],
        Some("SpecOps saw the same risk."),
        false,
    );

    // git diff --check forbids a blank line at fixture EOF; append the prompt's final newline here.
    assert_eq!(
        prompt,
        concat!(include_str!("fixtures/prompts/chair_generic.txt"), "\n")
    );
}

#[test]
fn directive_fence_chair_prompt_matches_prechange_capture() {
    let prompt = build_chair_prompt(
        "Should IRIN act?",
        "tenant=fixture\nescalation_id=esc-fixture",
        &[RoundResult {
            responses: vec![round().responses[0].clone()],
            ..round()
        }],
        Some("SpecOps saw the same risk."),
        true,
    );

    // git diff --check forbids a blank line at fixture EOF; append the prompt's final newline here.
    assert_eq!(
        prompt,
        concat!(include_str!("fixtures/prompts/chair_fence.txt"), "\n")
    );
}
