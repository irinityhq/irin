//! The shared round loop: fan-out rounds with judge, validation/gate,
//! cancellation, budget pause, and convergence for both the CLI/REST engine
//! path and the WebSocket streaming path.

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::engine::sheldon;
use crate::stream::deliberate::{
    self as streaming, RoundOperatorControl, StreamRunReady, until_cancelled,
};
use crate::stream::events::StreamEvent;
use crate::stream::intervention::InterventionQueue;
use crate::types::*;

use super::DeliberationOptions;
use super::PreparedDeliberation;
use super::budget::should_pause_for_budget;
use super::judge::{
    JudgeRoundResult, JudgeUsage, convergence_quality_penalty_enabled,
    effective_convergence_threshold, judge_round,
};
use super::seats::{build_round_prompt, fan_out};
use super::synthesis::write_cancelled_partial;
use super::utility_roles::run_frame_check;

/// WebSocket transport and operator controls used by the shared round loop.
pub(crate) struct RoundStream<'a> {
    pub(crate) ready: &'a StreamRunReady,
    pub(crate) interventions: &'a mut InterventionQueue,
}

pub(crate) struct RoundExecution {
    pub(crate) manual_specops_signal: String,
    pub(crate) validator_cost_usd: f64,
    pub(crate) judge_usage: JudgeUsage,
    pub(crate) rounds: Vec<RoundResult>,
    pub(crate) total_tokens: u32,
    pub(crate) total_latency_ms: u64,
    pub(crate) total_cost: f64,
    pub(crate) budget_paused: bool,
    pub(crate) budget_action: Option<String>,
    pub(crate) specops_triggered: bool,
    pub(crate) specops_cost_usd: f64,
    pub(crate) specops_signal_text: Option<String>,
}

/// Phase 2 — fan-out rounds with judge, validation/gate, cancel, budget, convergence.
pub(crate) async fn execute_deliberation_rounds(
    config: &Config,
    prepared: &PreparedDeliberation,
    opts: &DeliberationOptions<'_>,
    cancel: Option<&CancellationToken>,
    mut stream: Option<RoundStream<'_>>,
    cumulative_spend: f64,
) -> Result<RoundExecution> {
    let DeliberationOptions {
        cabinet_name,
        topic,
        context,
        mode,
        blind: _,
        frame_check,
        verbose,
        budget_max_usd,
        tier,
        validate,
        validate_provider,
        validate_gate,
        origin,
    } = opts.clone();
    let cabinet = &prepared.cabinet;
    let stream_ready = stream.as_ref().map(|s| s.ready);
    let session_id = &prepared.session_id;
    let mut live_seats = cabinet.seats.clone();
    let mut extra_context = String::new();
    let mut early_exit = false;
    let mut manual_specops_signal = String::new();
    let mut specops_cost_usd = 0.0;
    let mut validator_cost_usd = 0.0;
    let mut judge_usage = JudgeUsage::default();
    let mut rounds: Vec<RoundResult> = Vec::new();
    let mut total_tokens: u32 = 0;
    let mut total_latency_ms: u64 = 0;
    let mut total_cost: f64 = 0.0;
    let mut prev_flip_hash: Option<String> = None;
    let mut budget_paused = false;
    let mut budget_action: Option<String> = None;

    for round_num in 1..=cabinet.rounds {
        if let Some(ready) = stream_ready {
            anyhow::ensure!(!ready.cancel.is_cancelled(), "cancelled");
            if early_exit {
                break;
            }
            let _ = ready
                .event_tx
                .send(StreamEvent::round_started(
                    session_id,
                    round_num,
                    cabinet.rounds,
                ))
                .await;
        }
        if verbose {
            eprintln!(
                "── Round {}/{} ────────────────────────────────────────",
                round_num, cabinet.rounds
            );
        }

        let mut seat_prompts: Vec<String> = live_seats
            .iter()
            .map(|seat| {
                build_round_prompt(
                    topic,
                    context,
                    &extra_context,
                    &prepared.precedent_text,
                    &rounds,
                    &prepared.budget_signal,
                    seat,
                    round_num,
                )
            })
            .collect();
        if round_num == 1
            && frame_check
            && let Some(first) = seat_prompts.first()
        {
            let check = run_frame_check(
                first,
                verbose || stream_ready.is_some(),
                &config.roles,
                &config.models,
                &prepared.req_ctx,
            );
            let checked = if let Some(ready) = stream_ready {
                until_cancelled(&ready.cancel, check)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("cancelled"))?
            } else {
                check.await
            };
            // Round-one prompts share the same topic/context; no seat transcript yet.
            seat_prompts.fill(checked);
        }
        let responses = if let Some(ready) = stream_ready {
            for seat in &live_seats {
                let _ = ready
                    .event_tx
                    .send(StreamEvent::seat_started(
                        session_id,
                        round_num,
                        &seat.name,
                        &seat.provider,
                        &seat.model,
                    ))
                    .await;
            }
            streaming::run_round_fanout(
                ready,
                session_id,
                mode,
                round_num,
                &live_seats,
                &seat_prompts,
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("cancelled"))?
        } else {
            // Pre-round cancel check: cheap escape before seat fan-out.
            if let Some(c) = cancel
                && c.is_cancelled()
            {
                write_cancelled_partial(
                    &prepared.session_id,
                    cabinet_name,
                    topic,
                    tier,
                    &rounds,
                    origin,
                    total_tokens,
                    total_latency_ms,
                    total_cost,
                    verbose,
                    prepared.req_ctx.parent_request_id.clone(),
                );
                anyhow::bail!("cancelled");
            }

            fan_out(
                config,
                cabinet,
                &seat_prompts,
                round_num,
                mode,
                verbose,
                &prepared.req_ctx,
                cancel,
            )
            .await
        };

        for resp in &responses {
            let cost = config.models.estimate_cost(
                &resp.model,
                resp.tokens_in,
                resp.tokens_out,
                resp.cached_in,
            );
            total_tokens += resp.tokens_in + resp.tokens_out;
            total_latency_ms += resp.latency_ms;
            total_cost += cost;
        }

        // v9.12.0: Structured judge replaces naked float. Every round is
        // judged, including the last (parity with the stream core, B-07);
        // the round gate only decides whether convergence may stop early.
        let judge = if let Some(ready) = stream_ready {
            let _ = ready
                .event_tx
                .send(StreamEvent::info(session_id, "Scoring convergence…"))
                .await;
            until_cancelled(
                &ready.cancel,
                judge_round(
                    &responses,
                    topic,
                    &prepared.req_ctx,
                    &config.roles,
                    &config.models,
                ),
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("cancelled"))?
        } else if responses.len() >= 2 {
            judge_round(
                &responses,
                topic,
                &prepared.req_ctx,
                &config.roles,
                &config.models,
            )
            .await
        } else {
            JudgeRoundResult::skipped()
        };
        judge_usage += judge.usage;
        total_tokens = total_tokens.saturating_add(judge.usage.tokens);
        total_latency_ms = total_latency_ms.saturating_add(judge.usage.latency_ms);
        total_cost += judge.usage.cost_usd;
        let convergence_score = judge.score;
        let judge_prov = judge.provider;
        let judge_assess = judge.assessment;
        let judge_gateway_attempts = judge.gateway_attempts;

        let base_threshold = mode.convergence_threshold();
        let quality_penalty_enabled = convergence_quality_penalty_enabled(validate);
        let effective_threshold = effective_convergence_threshold(
            base_threshold,
            judge_assess.as_ref(),
            quality_penalty_enabled,
        );
        let converged = convergence_score >= effective_threshold;
        if let Some(ready) = stream_ready {
            let _ = ready
                .event_tx
                .send(StreamEvent::convergence_scored(
                    session_id,
                    round_num,
                    convergence_score,
                    converged,
                ))
                .await;
            until_cancelled(
                &ready.cancel,
                streaming::emit_round_divergence(
                    &ready.event_tx,
                    session_id,
                    round_num,
                    &responses,
                ),
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("cancelled"))?;
        }

        // v9.12.0: Flip-flop detection — hash (drift, recommendation)
        let flip_hash = judge_assess.as_ref().map(|a| {
            let mut hasher = DefaultHasher::new();
            format!("{}|{}", a.drift.as_deref().unwrap_or(""), a.recommendation).hash(&mut hasher);
            format!("{:x}", hasher.finish())[..8].to_string()
        });

        if verbose {
            let judge_tag = judge_prov
                .as_deref()
                .map(|p| format!(" [{}]", p))
                .unwrap_or_default();
            if converged && round_num < cabinet.rounds {
                eprintln!(
                    "   Convergence: 🟢 CONVERGED ({:.0}%){}",
                    convergence_score * 100.0,
                    judge_tag
                );
                eprintln!("   Early convergence — skipping remaining rounds.");
            } else if round_num < cabinet.rounds {
                eprintln!(
                    "   Convergence: 🔄 {:.0}%{}",
                    convergence_score * 100.0,
                    judge_tag
                );
            }
            if let Some(ref a) = judge_assess {
                if let Some(ref drift) = a.drift {
                    eprintln!("   ⚠️  Drift: {}", drift);
                }
                if let Some(ref qf) = a.quality_flag {
                    if let Some(h) = a.homogeneity_score {
                        let qa = a.quick_agreement.unwrap_or(false);
                        eprintln!("   📊 Homogeneity: {:.2} (quick_agreement: {})", h, qa);
                    }
                    eprintln!("   ⚠️  Quality: {}", qf);
                }
            }
            if effective_threshold > base_threshold && round_num < cabinet.rounds {
                eprintln!(
                    "   ⚠️  Quality-adjusted convergence threshold: {:.0}% (base {:.0}%)",
                    effective_threshold * 100.0,
                    base_threshold * 100.0
                );
            }
            if let (Some(fh), Some(prev)) = (&flip_hash, &prev_flip_hash)
                && fh == prev
                && round_num > 1
            {
                eprintln!("   🔄 FLIP-FLOP DETECTED — same assessment hash as previous round");
            }
            eprintln!();
        }

        prev_flip_hash = flip_hash.clone();

        // v9.13 / claim-validation path: Sheldon claim validator — runs after every round including the final
        // when --validate (or validate in config). For the final round we produce a report
        // for the Chair but do not apply gate_responses (Chair sees raw + evidence).
        // Cascade from roles.yaml provides failover.
        let mut validation_report = None;
        let mut responses = responses;
        if validate && round_num <= cabinet.rounds {
            let _ = validate_provider; // cascade order from roles.yaml now provides failover; param kept for CLI compat
            let claim_role = &config.roles.claim_validator;
            if sheldon::claim_validator_ready(claim_role, round_num) {
                for step in &claim_role.cascade {
                    let v_provider = step.provider.clone();
                    let v_model = Some(step.model.clone());
                    let vcfg = sheldon::ValidatorConfig {
                        provider: v_provider.clone(),
                        model: v_model,
                        gate: validate_gate,
                        verbose,
                    };
                    if let Some(ready) = stream_ready {
                        anyhow::ensure!(!ready.cancel.is_cancelled(), "cancelled");
                        let _ = ready
                            .event_tx
                            .send(StreamEvent::info(session_id, "Validating round…"))
                            .await;
                    }
                    let validation = sheldon::validate_round(
                        &responses,
                        topic,
                        context,
                        round_num,
                        &vcfg,
                        &prepared.req_ctx,
                        Some(&prepared.evidence_cache),
                    );
                    let val_result = if let Some(ready) = stream_ready {
                        until_cancelled(&ready.cancel, validation)
                            .await
                            .ok_or_else(|| anyhow::anyhow!("cancelled"))?
                    } else {
                        validation.await
                    };
                    match val_result {
                        sheldon::ValidateRoundOutcome::Ok(report, cost) => {
                            validation_report = Some(report.clone());
                            total_cost += cost;
                            validator_cost_usd += cost;
                            if let Some(ready) = stream_ready {
                                let verdicts =
                                    serde_json::to_value(&report).unwrap_or(serde_json::json!([]));
                                let _ = ready
                                    .event_tx
                                    .send(StreamEvent::round_validation(
                                        session_id,
                                        round_num,
                                        validate_gate,
                                        &verdicts,
                                    ))
                                    .await;
                            }
                            total_tokens = total_tokens.saturating_add(0);
                            total_latency_ms += 0;
                            if verbose {
                                eprintln!(
                                    "   🔬 Validator succeeded with {} (claim_validator cascade)",
                                    v_provider
                                );
                            }
                            // Gate decision moved post-validate for P2 early-stop parity.
                            break;
                        }
                        sheldon::ValidateRoundOutcome::Skipped(reason) => {
                            if verbose {
                                eprintln!("   ⏭️  Sheldon: skipping validator ({reason:?})");
                            }
                            break;
                        }
                        sheldon::ValidateRoundOutcome::ProviderFailed => {
                            if verbose {
                                eprintln!(
                                    "   ⚠️  Validator step {v_provider} failed; trying next in cascade"
                                );
                            }
                        }
                    }
                }
            }
        }

        // P2: Gate redaction only on continuing intermediate rounds (parity with
        // streaming path). On the terminating round of an early stop (budget or
        // convergence) or the true last round, keep full responses so Chair
        // synthesis receives complete evidence + the validation_report.
        let spend_at = if stream_ready.is_some() {
            cumulative_spend
                + rounds
                    .iter()
                    .flat_map(|r| &r.responses)
                    .map(|r| r.cost_usd)
                    .sum::<f64>()
                + responses.iter().map(|r| r.cost_usd).sum::<f64>()
                + validator_cost_usd
                + judge_usage.cost_usd
        } else {
            total_cost
        };
        if validate_gate && validation_report.is_some() {
            let would_budget =
                should_pause_for_budget(budget_max_usd, spend_at, round_num, cabinet.rounds);
            let is_terminating = round_num >= cabinet.rounds || converged || would_budget;
            if !is_terminating && let Some(ref rpt) = validation_report {
                responses = sheldon::gate_responses(&responses, rpt);
                if verbose {
                    eprintln!("   🛡️  Gate (high-impact only) applied to responses");
                }
            }
        }

        rounds.push(RoundResult {
            round_num,
            responses,
            convergence_score,
            converged,
            judge_provider: judge_prov,
            judge_assessment: judge_assess,
            judge_gateway_attempts,
            flip_flop_hash: flip_hash,
            // T24: claim/reasoning are raw validator output that bypasses the
            // per-seat from_provider redaction closure — scrub before persist.
            validation_report: validation_report.map(crate::scrub::redact_validation_report),
        });

        let is_last = round_num >= cabinet.rounds;
        if let Some(ready) = stream_ready {
            let _ = ready
                .event_tx
                .send(StreamEvent::round_complete(
                    session_id,
                    round_num,
                    convergence_score,
                    converged,
                    converged && !is_last,
                ))
                .await;
        }

        // Post-round cancel check. Persist a partial diagnostic file with
        // origin=ApiCancelled (private side-channel — never returned to the
        // API response, never indexed for precedent) and bail.
        if let Some(c) = cancel
            && c.is_cancelled()
        {
            write_cancelled_partial(
                &prepared.session_id,
                cabinet_name,
                topic,
                tier,
                &rounds,
                origin,
                total_tokens,
                total_latency_ms,
                total_cost,
                verbose,
                prepared.req_ctx.parent_request_id.clone(),
            );
            anyhow::bail!("cancelled");
        }

        // Keep the streamed ledger's summation order after gate processing.
        let spend_at = if stream_ready.is_some() {
            cumulative_spend
                + rounds
                    .iter()
                    .flat_map(|r| &r.responses)
                    .map(|r| r.cost_usd)
                    .sum::<f64>()
                + validator_cost_usd
                + judge_usage.cost_usd
        } else {
            total_cost
        };
        // v9.12.0: Budget pause at round boundary
        if should_pause_for_budget(budget_max_usd, spend_at, round_num, cabinet.rounds) {
            budget_paused = true;
            if let Some(ready) = stream_ready
                && let Some(max) = budget_max_usd
            {
                let _ = ready
                    .event_tx
                    .send(StreamEvent::budget_paused(
                        session_id, round_num, spend_at, max,
                    ))
                    .await;
            }
            budget_action = Some("end_early".to_string());
            if verbose {
                if let Some(max) = budget_max_usd {
                    eprintln!("   💰 BUDGET PAUSE — ${:.4} / ${:.4}", total_cost, max);
                }
                eprintln!("   → Ending early (non-interactive mode).\n");
            }
            break;
        }

        if converged
            && !is_last
            && !stream_ready.is_some_and(|ready| ready.stream_config.pause_after_each_round)
        {
            break;
        }
        if let Some(ref mut stream) = stream {
            match streaming::run_round_operator_pause(
                stream.ready,
                stream.interventions,
                session_id,
                topic,
                round_num,
                convergence_score,
                converged,
                is_last,
                &mut early_exit,
                &mut extra_context,
                &mut live_seats,
                &rounds,
                &mut manual_specops_signal,
                &mut specops_cost_usd,
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("cancelled"))?
            {
                RoundOperatorControl::BreakRounds => break,
                RoundOperatorControl::NextRound => {}
            }
        }
    }

    Ok(RoundExecution {
        manual_specops_signal,
        validator_cost_usd,
        judge_usage,
        rounds,
        total_tokens,
        total_latency_ms,
        total_cost,
        budget_paused,
        budget_action,
        specops_triggered: false,
        specops_cost_usd,
        specops_signal_text: None,
    })
}
