//! Seat execution: the per-round user prompt, the seat preamble selector, and
//! the buffered parallel fan-out used by the CLI/REST engine path.

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::engine::context::RequestContext;
use crate::engine::sheldon;
use crate::mode::Mode;
use crate::provider;
use crate::text::truncate_utf8;
use crate::types::*;

/// Seat preamble for `synthesis_mode: directive_proposal_v1` triage seats.
///
/// Replaces `Mode::seat_preamble()` (which, defaulting to TearDown, tells every
/// seat to "find every reason this should NOT proceed" — structurally biasing
/// every escalation to Dismiss). Triage is evidence-assessment of a Sentinel
/// escalation, not adversarial teardown of a proposal. The conservative posture
/// lives at Worker arming (authority=recommend, capability tokens, default-OFF),
/// not in the evidence→recommend filter. Neutral between kill-bias and
/// forced-action: weak signals still Dismiss. (Invariant follow-up.)
pub(crate) const TRIAGE_SEAT_PREAMBLE: &str = "DELIBERATION MODE: TRIAGE ASSESSMENT.\n\n\
You are evaluating a Sentinel escalation (observed state, reason, urgency, proposed_action=ConsultCouncil). \
Assess whether the concrete evidence in the escalation warrants recommending a Worker dispatch under \
recommend-only authority. Weigh the severity of the observed condition, the specificity and credibility \
of the reason, the blast radius of inaction, and whether the signal is actionable enough that a narrow \
directive (job + scope + stop_condition) would be the responsible next step.\n\n\
Do not meta-attack alert quality, instrumentation gaps, or \"alert fatigue\" unless those render the \
evidence literally unusable for a decision; those concerns belong upstream in Sentinel policy, not in \
this assessment. Act if the facts clear a bar for \"this merits a recommended directive to investigate \
or contain.\" Dismiss if the evidence is insufficient, ambiguous, points to a transient issue, or does \
not justify involving a Worker. Cite specific observations from the escalation and transcript to justify \
your chosen path.";

/// Single source of truth for which seat preamble a deliberation gets.
///
/// Triage (`synthesis_mode: directive_proposal_v1`) is evidence-assessment of a
/// Sentinel escalation and gets the neutral [`TRIAGE_SEAT_PREAMBLE`]; every other
/// cabinet keeps the operator-facing `Mode` preamble (TearDown/Pathfind/Harden).
/// Both the REST fan-out and the streaming/WS fan-out MUST route through here so
/// the contract cannot drift between paths. (Invariant, P1-1.)
pub(crate) fn seat_preamble_for(cabinet: &Cabinet, mode: Mode) -> &'static str {
    if cabinet.synthesis_mode == SynthesisMode::DirectiveProposalV1 {
        TRIAGE_SEAT_PREAMBLE
    } else {
        mode.seat_preamble()
    }
}

/// Fan-out to all seats in parallel.
///
/// Phase 0.5 §4.5: per-seat `tokio::select!` against `cancel`. v0.1 scope-cut
/// — task-level cancel only; the in-flight `reqwest::send()` is not aborted,
/// so one round of seat costs is potentially wasted on a hostile disconnect.
#[allow(clippy::too_many_arguments)]
pub(super) async fn fan_out(
    config: &Config,
    cabinet: &Cabinet,
    prompts: &[String],
    round_num: u32,
    mode: Mode,
    verbose: bool,
    req_ctx: &RequestContext,
    cancel: Option<&CancellationToken>,
) -> Vec<SeatResponse> {
    debug_assert_eq!(cabinet.seats.len(), prompts.len());
    let mut set = JoinSet::new();

    for (seat, prompt) in cabinet.seats.iter().zip(prompts) {
        let seat_name = seat.name.clone();
        let prov = seat.provider.clone();
        let model = seat.model.clone();
        let base_system = match config.render_system_prompt(&seat.system) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "ERROR: render_system_prompt failed for seat {}: {}",
                    seat.name, e
                );
                // Fallback to raw to avoid total brain death, but error is logged
                seat.system.clone()
            }
        };
        // Inject the seat preamble after the base system prompt. seat_preamble_for
        // routes triage (directive_proposal_v1) to the neutral TRIAGE_SEAT_PREAMBLE
        // and everything else to the operator-facing Mode preamble — the single
        // selector both the REST and WS fan-outs share so the contract can't drift.
        let system = format!("{}\n\n{}", base_system, seat_preamble_for(cabinet, mode));
        let prompt = prompt.to_string();
        let ctx = req_ctx.clone();
        let token = cancel.cloned();

        set.spawn(async move {
            let resp = if let Some(c) = token {
                tokio::select! {
                    biased;
                    _ = c.cancelled() => crate::types::ProviderResponse {
                        error: Some("cancelled".into()),
                        model: model.clone(),
                        ..Default::default()
                    },
                    r = provider::ask_with_context(&prov, &prompt, &system, &model, &ctx) => r,
                }
            } else {
                provider::ask_with_context(&prov, &prompt, &system, &model, &ctx).await
            };
            SeatResponse::from_provider(&seat_name, &prov, round_num, resp, |s| {
                let text = crate::scrub::redact(s);
                crate::librarian::redaction::redact_secrets(&text).0
            })
        });
    }

    let mut responses = Vec::new();
    while let Some(result) = set.join_next().await {
        match result {
            Ok(resp) => {
                if verbose {
                    let status = if resp.error.is_some() { "❌" } else { "✅" };
                    eprintln!(
                        "   {} {} ({}) — {}ms, {} tok",
                        status, resp.seat_name, resp.provider, resp.latency_ms, resp.tokens_out
                    );
                    if let Some(ref err) = resp.error {
                        let snippet: String = err.chars().take(240).collect();
                        eprintln!("      ↳ {}", snippet);
                    }
                }
                responses.push(resp);
            }
            Err(e) => eprintln!("   ❌ Task panicked: {}", e),
        }
    }

    responses
}

/// Build the user prompt for one seat in a deliberation round.
#[allow(clippy::too_many_arguments)]
pub fn build_round_prompt(
    topic: &str,
    context: &str,
    extra_context: &str,
    precedent_text: &str,
    prior_rounds: &[RoundResult],
    budget_signal: &str,
    seat: &Seat,
    round_num: u32,
) -> String {
    let mut prompt = String::new();

    if !budget_signal.is_empty() {
        prompt.push_str(budget_signal);
        prompt.push_str("\n\n---\n\n");
    }

    if round_num == 1 {
        // Cold Eyes (v9.13.3): NO precedent in R1 — fresh exploration.
        // Precedent enters in R2+ via cross-pollination below.
        if !extra_context.is_empty() {
            prompt.push_str(&format!(
                "OPERATOR INTERVENTION:\n{}\n\n---\n\n",
                extra_context
            ));
        }
        if !context.is_empty() {
            prompt.push_str(context);
            prompt.push_str("\n\n---\n\n");
        }
        prompt.push_str(topic);
    } else {
        // Cold Eyes: precedent enters in R2+ cross-pollination.
        if !precedent_text.is_empty() {
            prompt.push_str("## Prior Council Precedent\n\n");
            prompt.push_str(precedent_text);
            prompt.push_str("\n---\n\n");
        }
        if !extra_context.is_empty() {
            prompt.push_str(&format!(
                "OPERATOR INTERVENTION:\n{}\n\n---\n\n",
                extra_context
            ));
        }
        prompt.push_str(&format!("TOPIC: {}\n\n", topic));

        let mut own_text = "(no prior response)".to_string();
        for round in prior_rounds {
            for response in &round.responses {
                if response.seat_name == seat.name
                    && !response.text.is_empty()
                    && response.error.is_none()
                {
                    own_text = response.text.clone();
                }
            }
        }
        prompt.push_str(&format!("YOUR MOST RECENT ANALYSIS:\n{}\n\n", own_text));

        prompt.push_str("FULL DELIBERATION HISTORY:\n");
        prompt.push_str(&"─".repeat(40));
        prompt.push('\n');
        for round in prior_rounds {
            prompt.push_str(&format!("\n### Round {}\n", round.round_num));
            for response in &round.responses {
                if response.seat_name != seat.name
                    && !response.text.is_empty()
                    && response.error.is_none()
                {
                    // PARITY: engine and stream intentionally expose at most 2000 bytes per peer view.
                    let truncated = truncate_utf8(&response.text, 2000);
                    prompt.push_str(&format!(
                        "**{} ({})**: {}\n\n",
                        response.seat_name, response.provider, truncated
                    ));
                }
            }
            append_validation_context(&mut prompt, round);
        }
        prompt.push_str(&"─".repeat(40));
        prompt.push_str(&format!(
            "\n\nThis is round {}. Considering the full history, refine your analysis. \
             Where do you agree? Where do you push back? What new insight emerges?",
            round_num
        ));
    }

    prompt
}

/// Appends the Sheldon validation report context for a round (if present) to a transcript prompt.
/// Shared between CLI and streaming to keep last-round validation parity.
pub(crate) fn append_validation_context(prompt: &mut String, round: &RoundResult) {
    if let Some(ref report) = round.validation_report {
        let ctx = sheldon::format_validation_context(report);
        if !ctx.is_empty() {
            prompt.push_str(&ctx);
            prompt.push_str("\n\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(text: &str) -> SeatResponse {
        SeatResponse {
            text: text.into(),
            ..Default::default()
        }
    }

    fn cabinet_with_mode(extra: &str) -> Cabinet {
        let json = format!(
            r#"{{"name":"t","rounds":1,"seats":[],"chair":{{"name":"c","provider":"p","model":"m"}}{}}}"#,
            extra
        );
        serde_json::from_str(&json).expect("minimal cabinet must deserialize")
    }

    // P2-3 (the invariant): the seat-preamble selector must route a
    // non-triage cabinet to the operator Mode preamble (NOT the triage one), and a
    // triage cabinet to TRIAGE_SEAT_PREAMBLE regardless of Mode. Guards against a
    // future refactor leaking triage framing into TearDown/Pathfind/Harden councils.
    #[test]
    fn seat_preamble_selector_routes_by_synthesis_mode() {
        let generic = cabinet_with_mode("");
        let triage = cabinet_with_mode(r#","synthesis_mode":"directive_proposal_v1""#);

        assert_eq!(
            seat_preamble_for(&triage, Mode::TearDown),
            TRIAGE_SEAT_PREAMBLE
        );
        assert_eq!(
            seat_preamble_for(&triage, Mode::Harden),
            TRIAGE_SEAT_PREAMBLE
        );

        assert_eq!(
            seat_preamble_for(&generic, Mode::TearDown),
            Mode::TearDown.seat_preamble()
        );
        assert_ne!(
            seat_preamble_for(&generic, Mode::TearDown),
            TRIAGE_SEAT_PREAMBLE
        );
    }

    // claim-validation path Phase 4: integration test for prompt assembly.
    // When validation_report is present, the built prompt for chair/prior rounds must contain the report block.
    #[test]
    fn build_round_prompt_includes_validation_report() {
        let report = vec![crate::types::ClaimVerdictEntry {
            claim: "The API must return 200 on success".into(),
            seat: "Analyst".into(),
            verdict: crate::types::ClaimVerdict::Supported,
            evidence_citations: vec!["log line 42".into()],
            reasoning: "observed in run".into(),
            confidence: 0.95,
            impact: crate::types::ClaimImpact::High,
            _overridden_from: None,
        }];

        let round = crate::types::RoundResult {
            round_num: 1,
            responses: vec![seat("response text")],
            convergence_score: 0.88,
            converged: true,
            judge_provider: None,
            judge_assessment: None,
            judge_gateway_attempts: vec![],
            flip_flop_hash: None,
            validation_report: Some(report),
        };

        let seat = Seat {
            name: "Checker".into(),
            provider: "mock".into(),
            model: "mock".into(),
            system: "test".into(),
        };
        let prompt = build_round_prompt(
            "test topic for claim validation",
            "",
            "",
            "",
            &[round],
            "",
            &seat,
            2,
        );

        assert!(
            prompt.contains("VALIDATOR REPORT"),
            "prompt must contain the validator report header"
        );
        assert!(
            prompt.contains("The API must return 200 on success"),
            "prompt must contain a claim from the validation report"
        );
        assert!(
            prompt.contains("SUPPORTED"),
            "prompt must contain the verdict"
        );
    }
}
