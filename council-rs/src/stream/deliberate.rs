//! Streaming deliberation — the core async generator.
//!
//! Mirrors council_stream.py:deliberate_stream() — re-orchestrates the engine's
//! deliberation loop to yield StreamEvents via a tokio mpsc channel.
//! Includes pause/resume, intervention handling, SpecOps escalation,
//! and the race-condition fix (re-pause after escalation).

use chrono::Utc;
use serde_json::json;
use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::events::StreamEvent;
use super::intervention::{Intervention, InterventionQueue};
use crate::config::Config;
use crate::engine::context::RequestContext;
use crate::engine::deliberate::{
    JudgeUsage, convergence_quality_penalty_enabled, effective_convergence_threshold,
    governed_alternative_transport_model_groups, governed_required_transport_models, judge_round,
    seat_preamble_for, should_pause_for_budget,
};
use crate::mode::Mode;
use crate::precedent;
use crate::provider;
use crate::types::BudgetRecord;
use crate::types::*;

/// Configuration for a streaming deliberation.
pub struct StreamConfig {
    pub topic: String,
    pub cabinet_name: String,
    pub custom_cabinet: Option<crate::types::Cabinet>,
    pub context: String,
    pub mode: Mode,
    pub blind: bool,
    pub frame_check: bool,
    /// Whether to run scope auditor (steering/boundary review). Not yet wired to engine.
    pub scope_auditor: bool,
    pub max_rounds: Option<u32>,
    pub pause_after_each_round: bool,
    pub auto_specops_threshold: f64,
    pub parent_session_id: Option<String>,
    pub swaps: Vec<serde_json::Value>,
    pub validate: bool,
    pub validate_provider: String,
    pub validate_gate: bool,
    pub worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
    pub budget_max_usd: Option<f64>,
    pub tier: String,
    pub then_tear_down: bool,
    /// Per-session gateway routing (feature contract). `None` falls back to the
    /// process-wide `COUNCIL_VIA_GATEWAY` state.
    pub via_gateway: Option<bool>,
    /// Per-session sensitivity (UPPERCASE GREEN/YELLOW/RED, normalized from
    /// the lowercase WS wire values). `None` falls back to the process default.
    pub sensitivity: Option<String>,
    /// Direct-fire single-shot mode (feature contract): contrarian | munger | kiss |
    /// specops | premortem. When set, `run` skips council rounds entirely.
    pub direct_fire: Option<String>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            topic: String::new(),
            cabinet_name: "standard".to_string(),
            custom_cabinet: None,
            context: String::new(),
            mode: Mode::TearDown,
            blind: false,
            frame_check: true,
            scope_auditor: false,
            max_rounds: None,
            pause_after_each_round: true,
            auto_specops_threshold: 0.8,
            parent_session_id: None,
            swaps: vec![],
            validate: false,
            validate_provider: "grok_hermes".to_string(),
            validate_gate: false,
            worker_provenance: None,
            budget_max_usd: None,
            tier: "best".to_string(),
            then_tear_down: false,
            via_gateway: None,
            sensitivity: None,
            direct_fire: None,
        }
    }
}

/// Per-session provider context (feature contract) — carries the gateway override and
/// sensitivity into every `provider::ask_with_context` call for this session.
fn request_context(stream_config: &StreamConfig) -> RequestContext {
    RequestContext {
        via_gateway: stream_config.via_gateway,
        sensitivity: stream_config.sensitivity.clone(),
        ..Default::default()
    }
}

/// Await work only while the owning WebSocket remains connected. Dropping the
/// future stops local dispatch best-effort; an upstream provider may still
/// finish a request it already accepted.
async fn until_cancelled<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        output = future => Some(output),
    }
}

/// Run a streaming deliberation, sending events through the channel.
/// This is the Rust equivalent of council_stream.py:deliberate_stream().
pub async fn run(
    config: Arc<Config>,
    stream_config: StreamConfig,
    event_tx: mpsc::Sender<StreamEvent>,
    mut interventions: InterventionQueue,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }
    let provisional_session_id = Uuid::new_v4().to_string()[..12].to_string();

    // Direct-fire (feature contract): single-shot, no council rounds, no interventions.
    if stream_config.direct_fire.is_some() {
        run_direct_fire(config, stream_config, event_tx, cancel).await;
        return;
    }

    // Minimal stub for scope_auditor (from bs-detector plan): prevent silent no-op.
    // Real impl (early/late investigator, declared_slice, report hoisting) pending.
    // Use info() so it appears in OperatorLog (info_messages) during streaming.
    if stream_config.scope_auditor {
        let _ = event_tx
            .send(StreamEvent::info(
                &provisional_session_id,
                "scope_auditor requested (Scope Auditor / steering detection). Engine wiring not yet implemented per the spec. Flag parsed but ignored for this run.",
            ))
            .await;
    }
    if cancel.is_cancelled() {
        return;
    }
    let cabinet_owned;
    let cabinet: &crate::types::Cabinet = if let Some(ref custom) = stream_config.custom_cabinet {
        if let Err(e) = config.validate_cabinet_for_execution("custom_cabinet", custom) {
            let _ = event_tx
                .send(StreamEvent::error(
                    &provisional_session_id,
                    &format!("Cabinet validation failed: {}", e),
                    true,
                ))
                .await;
            return;
        }
        cabinet_owned = custom.clone();
        &cabinet_owned
    } else {
        // resolve_cabinet_owned (feature contract): registry hit returns a clone; a miss
        // falls back to <base_dir>/cabinets/<name>.yaml so cabinets saved after
        // server start are launchable by registry name (the startup Arc<Config>
        // is immutable). The disk path runs the same per-run execution gate the
        // custom_cabinet branch above uses.
        match config.resolve_cabinet_owned(&stream_config.cabinet_name) {
            Ok(c) => {
                cabinet_owned = c;
                &cabinet_owned
            }
            Err(e) => {
                let _ = event_tx
                    .send(StreamEvent::error(
                        &provisional_session_id,
                        &format!("Cabinet load failed: {}", e),
                        true,
                    ))
                    .await;
                return;
            }
        }
    };

    // WS guard (the invariant, P1-1 belt-and-suspenders): the
    // streaming path does NOT enforce the D2 directive fence and its chair is not
    // the strict directive chair, so it cannot safely produce a directive_proposal_v1
    // result. Refuse rather than emit an unvalidated proposal. The REST path
    // (/api/deliberate -> engine::deliberate) is the only supported triage surface
    // today; lift this guard only when the WS path also validates the fence.
    if cabinet.synthesis_mode == SynthesisMode::DirectiveProposalV1 {
        let _ = event_tx
            .send(StreamEvent::error(
                &provisional_session_id,
                "synthesis_mode 'directive_proposal_v1' (council-triage) is not supported on the \
                 streaming/WS path — it does not enforce the directive fence. Use the REST \
                 /api/deliberate surface for triage.",
                true,
            ))
            .await;
        return;
    }
    if cancel.is_cancelled() {
        return;
    }

    let rounds_planned = stream_config.max_rounds.unwrap_or(cabinet.rounds);
    let frame_check_enabled = stream_frame_check_enabled(&stream_config, cabinet);

    // Per-session gateway routing (feature contract): override the process default for
    // every provider call carrying session content — seat fan-out,
    // escalations, synthesis, the convergence judge, and the Sheldon
    // validator (sole exception: the grok validator's native web search).
    let req_ctx = request_context(&stream_config);
    let effective_via_gateway = stream_config
        .via_gateway
        .unwrap_or_else(provider::default_via_gateway);
    let effective_sensitivity = stream_config
        .sensitivity
        .clone()
        .unwrap_or_else(provider::default_sensitivity);

    let required_models = governed_required_transport_models(cabinet);
    let alternatives = governed_alternative_transport_model_groups(
        &config,
        frame_check_enabled,
        stream_config.validate,
    );
    if effective_via_gateway
        && let Err(err) =
            provider::gateway::preflight_pairs_with_alternatives(&required_models, &alternatives)
                .await
    {
        let _ = event_tx
            .send(StreamEvent::error(
                &provisional_session_id,
                &format!("Governed Gateway preflight failed: {err}"),
                true,
            ))
            .await;
        return;
    }

    // Check which providers are available — gateway mode unlocks all of them.
    let available = provider::check_providers_with_gateway(effective_via_gateway);
    let available_set: std::collections::HashSet<&str> = available
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(name, _)| *name)
        .collect();

    // A cabinet is an explicit set of seats, not a best-effort pool. Running a
    // partial cabinet changes the deliberation without operator consent, so
    // fail closed before any seat starts.
    let unavailable_seats: Vec<&Seat> = cabinet
        .seats
        .iter()
        .filter(|s| !available_set.contains(s.provider.as_str()))
        .collect();
    let chair_unavailable = !available_set.contains(cabinet.chair.provider.as_str());
    if !unavailable_seats.is_empty() || chair_unavailable {
        let mut missing = unavailable_seats
            .iter()
            .map(|seat| format!("{} ({})", seat.name, seat.provider))
            .collect::<Vec<_>>();
        if chair_unavailable {
            missing.push(format!("Chair ({})", cabinet.chair.provider));
        }
        let _ = event_tx
            .send(StreamEvent::error(
                &provisional_session_id,
                &format!(
                    "Cabinet cannot start because required provider transports are unavailable: {}",
                    missing.join(", ")
                ),
                true,
            ))
            .await;
        return;
    }
    let active_seats: Vec<&Seat> = cabinet.seats.iter().collect();
    let dropped_seats: Vec<&Seat> = Vec::new();

    let phases_total = if stream_config.then_tear_down && stream_config.mode == Mode::Pathfind {
        2
    } else {
        1
    };
    let mut pathfinder_session_id = String::new();
    let mut pathfinder_synthesis = String::new();
    let mut cumulative_tokens: u32 = 0;
    let mut cumulative_cost: f64 = 0.0;
    let mut cumulative_latency: u64 = 0;
    let mut final_synthesis_text = String::new();
    let mut final_conv = 1.0f64;
    let mut final_rounds_run = 0u32;
    let mut last_session_id = String::new();
    let mut cumulative_spend: f64 = 0.0;
    let mut phases_completed: u32 = 0;
    let mut phase_summaries: Vec<serde_json::Value> = Vec::new();

    'phases: for phase_idx in 0..phases_total {
        if cancel.is_cancelled() {
            return;
        }
        if stream_config
            .budget_max_usd
            .is_some_and(|max| cumulative_spend >= max)
        {
            break 'phases;
        }
        let session_id = Uuid::new_v4().to_string()[..12].to_string();
        last_session_id = session_id.clone();
        let (phase_mode, phase_topic, phase_context) = if phase_idx == 0 {
            (
                stream_config.mode,
                stream_config.topic.clone(),
                stream_config.context.clone(),
            )
        } else {
            let teardown_context = format!(
                "## PATHFINDER OUTPUT TO STRESS-TEST\n\n{}\n\n---\n\n{}",
                pathfinder_synthesis, stream_config.context
            );
            let teardown_topic = format!(
                "STRESS-TEST the following plan produced by a Pathfinder deliberation on: {}",
                stream_config.topic
            );
            (Mode::TearDown, teardown_topic, teardown_context)
        };

        if phase_idx > 0 {
            let _ = event_tx
                .send(StreamEvent::phase_started(
                    &session_id,
                    2,
                    "Tear-down — stress-testing pathfinder plan",
                    &pathfinder_session_id,
                ))
                .await;
        }

        let topic = phase_topic.as_str();
        let context = phase_context.as_str();
        let mode = phase_mode;

        // ── session_started ──
        let _ = event_tx.send(StreamEvent::session_started(&session_id, json!({
        "topic": topic,
        "cabinet_name": cabinet.name,
        "rounds_planned": rounds_planned,
        "mode": if stream_config.blind { "blind" } else { "normal" },
        "active_seats": active_seats.iter().map(|s| json!({
            "name": s.name, "provider": s.provider, "model": s.model
        })).collect::<Vec<_>>(),
        "dropped_seats": dropped_seats.iter().map(|s| json!({
            "name": s.name, "provider": s.provider
        })).collect::<Vec<_>>(),
        "chair": {
            "provider": &cabinet.chair.provider,
            "model": &cabinet.chair.model,
        },
        "available_providers": available.iter().filter(|(_, ok)| *ok).map(|(n, _)| n).collect::<Vec<_>>(),
        "council_version": env!("CARGO_PKG_VERSION"),
        "stream_version": "rs-1.0.0",
        "tier": stream_config.tier,
        "phase": phase_idx + 1,
        "phases_total": phases_total,
        "deliberation_mode": match mode {
            Mode::Pathfind => "pathfind",
            Mode::TearDown => "teardown",
            Mode::Harden => "harden",
        },
        // feature contract: also emitted by the smoke shim (src/server.rs) — keep in sync.
        "via_gateway": effective_via_gateway,
        "execution_route": if effective_via_gateway { "governed" } else { "direct" },
        "sensitivity": effective_sensitivity.to_lowercase(),
    }))).await;

        // ── Precedent injection ──
        // One retrieval receipt per phase: the `precedent_loaded` WS event,
        // the injected prompt text, and the saved `precedent_ids` all come
        // from this single retrieve(). The idle preview uses the same fn +
        // defaults but re-queries while typing — same ranker, not the same
        // frozen object.
        let (precedent_text, precedent_ids) = if !stream_config.blind {
            // precedent::retrieve performs synchronous filesystem work (and may
            // block on embedding-model init). Offload from streaming engine
            // path (post-delib / WarRoom WS). On join err: log + empty.
            let topic_c = topic.to_string();
            let join_res = tokio::task::spawn_blocking(move || {
                precedent::retrieve(
                    &topic_c,
                    precedent::RETRIEVE_LIMIT,
                    precedent::RETRIEVE_THRESHOLD,
                    false,
                )
            })
            .await;
            match join_res {
                Ok(receipt) if !receipt.hits.is_empty() => {
                    let text = precedent::format_for_injection(&receipt);
                    let matches = precedent::receipt_to_match_values(&receipt);
                    let _ = event_tx
                        .send(StreamEvent::precedent_loaded(&session_id, matches))
                        .await;
                    let _ = event_tx
                        .send(StreamEvent::info(
                            &session_id,
                            &format!(
                                "Precedent: {} prior sessions found (engine={})",
                                receipt.hits.len(),
                                receipt.engine
                            ),
                        ))
                        .await;
                    (text, receipt.ids())
                }
                Ok(_) => (String::new(), vec![]),
                Err(e) => {
                    eprintln!(
                        "ERROR: stream precedent retrieve spawn_blocking join failed session_id={}: {}",
                        session_id, e
                    );
                    (String::new(), vec![])
                }
            }
        } else {
            (String::new(), vec![])
        };

        // ── Deliberation loop ──
        let mut all_rounds: Vec<RoundResult> = Vec::new();
        let mut extra_context = String::new();
        let mut manual_specops_signal = String::new();
        let mut early_exit = false;
        let mut budget_paused = false;
        let mut validator_cost_usd = 0.0;
        let mut judge_usage = JudgeUsage::default();

        // Per-phase evidence cache for validate dedup (topic stable within phase).
        let evidence_cache = crate::engine::sheldon::EvidenceCache::default();
        let (budget_signal, _budget_tier) = crate::engine::deliberate::fetch_budget_signal(
            std::env::var("HERMES_PROFILE").ok().as_deref(),
            Some(&session_id),
        );
        // Working copy of seats (for swap_seat mutations)
        let mut live_seats: Vec<Seat> = active_seats.iter().map(|s| (*s).clone()).collect();

        for round_num in 1..=rounds_planned {
            if early_exit || cancel.is_cancelled() {
                if cancel.is_cancelled() {
                    return;
                }
                break;
            }

            // ── round_started ──
            let _ = event_tx
                .send(StreamEvent::round_started(
                    &session_id,
                    round_num,
                    rounds_planned,
                ))
                .await;

            // Build prompts
            let mut prompts: Vec<(String, String)> = live_seats
                .iter()
                .map(|seat| {
                    let prompt = build_round_prompt(
                        topic,
                        context,
                        &extra_context,
                        &precedent_text,
                        &all_rounds,
                        &budget_signal,
                        seat,
                        round_num,
                    );
                    (seat.name.clone(), prompt)
                })
                .collect();

            // v9.10.0: Frame check — scan R1 prompts for embedded assumptions
            if round_num == 1 && frame_check_enabled {
                // Run frame check on the first seat's prompt (all share the same topic base)
                if let Some((_, first_prompt)) = prompts.first() {
                    let Some(checked) = until_cancelled(
                        &cancel,
                        crate::engine::deliberate::frame_check_prompt(
                            first_prompt,
                            &config.roles,
                            &config.models,
                            &req_ctx,
                        ),
                    )
                    .await
                    else {
                        return;
                    };
                    if checked != *first_prompt {
                        // Frame check found assumptions — apply the warning suffix to all prompts
                        let suffix = &checked[first_prompt.len()..];
                        for (_, prompt) in prompts.iter_mut() {
                            prompt.push_str(suffix);
                        }
                    }
                }
            }

            // Emit seat_started for all seats
            for seat in &live_seats {
                let _ = event_tx
                    .send(StreamEvent::seat_started(
                        &session_id,
                        round_num,
                        &seat.name,
                        &seat.provider,
                        &seat.model,
                    ))
                    .await;
            }

            // Fan-out: all seats in parallel
            let mut set = JoinSet::new();
            for (seat, (_, prompt)) in live_seats.iter().zip(prompts.iter()) {
                if cancel.is_cancelled() {
                    set.abort_all();
                    return;
                }
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
                        seat.system.clone()
                    }
                };
                let system = format!("{}\n\n{}", base_system, seat_preamble_for(cabinet, mode));
                let prompt = prompt.clone();
                let ctx = req_ctx.clone();
                // N01: token streaming. For streaming-capable providers
                // (openai_compat SSE family, gateway routing excluded), forward
                // each visible delta as a `seat_chunk` between seat_started and
                // seat_complete. seat_complete.text stays authoritative. Other
                // providers fall back to the buffered call (zero chunks — legal).
                let stream_chunks = provider::is_streaming_capable(&prov, effective_via_gateway);
                let chunk_tx = event_tx.clone();
                let chunk_sid = session_id.clone();

                set.spawn(async move {
                    let resp = if stream_chunks {
                        let mut seq: u32 = 0;
                        // try_send (non-blocking) keeps a slow consumer from
                        // back-pressuring the provider read; seat_complete still
                        // carries the full text, so a dropped chunk is cosmetic.
                        // T24: deltas stream to the authed loopback warroom client
                        // raw (pre-redaction). Per-fragment scrubbing would
                        // false-positive and still miss a secret spanning a
                        // fragment boundary; the persisted form (seat_complete via
                        // the from_provider closure below) is redacted. See
                        // SECURITY.md#t24-data-flow-redaction.
                        let on_delta = |delta: &str| {
                            let evt = StreamEvent::seat_chunk(
                                &chunk_sid, round_num, &seat_name, delta, seq,
                            );
                            seq = seq.saturating_add(1);
                            let _ = chunk_tx.try_send(evt);
                        };
                        provider::ask_streaming_with_context(
                            &prov, &prompt, &system, &model, &ctx, on_delta,
                        )
                        .await
                    } else {
                        provider::ask_with_context(&prov, &prompt, &system, &model, &ctx).await
                    };
                    tracing::info!(
                        provider = %prov,
                        model = %model,
                        seat = %seat_name,
                        tokens_in = resp.tokens_in,
                        tokens_out = resp.tokens_out,
                        latency_ms = resp.latency_ms,
                        "Provider call completed"
                    );
                    SeatResponse::from_provider(&seat_name, &prov, round_num, resp, |s| {
                        let text = crate::scrub::redact(s);
                        crate::librarian::redaction::redact_secrets(&text).0
                    })
                });
            }

            // Collect responses as they complete
            let mut responses: Vec<SeatResponse> = Vec::new();
            loop {
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        set.abort_all();
                        while set.join_next().await.is_some() {}
                        return;
                    }
                    result = set.join_next() => result,
                };
                let Some(result) = result else {
                    break;
                };
                match result {
                    Ok(resp) => {
                        // Calculate cost
                        let mut resp = resp;
                        resp.cost_usd = config.models.estimate_cost(
                            &resp.model,
                            resp.tokens_in,
                            resp.tokens_out,
                            resp.cached_in,
                        );
                        let _ = event_tx
                            .send(StreamEvent::seat_complete(
                                &session_id,
                                serde_json::to_value(&resp).unwrap_or_default(),
                            ))
                            .await;
                        responses.push(resp);
                    }
                    Err(e) => {
                        let _ = event_tx
                            .send(StreamEvent::error(
                                &session_id,
                                &format!("Seat task panicked: {}", e),
                                false,
                            ))
                            .await;
                    }
                }
            }

            // v9.12.0: Structured convergence judge (shared with CLI engine)
            let _ = event_tx
                .send(StreamEvent::info(&session_id, "Scoring convergence…"))
                .await;
            let Some(judge) = until_cancelled(
                &cancel,
                judge_round(&responses, topic, &req_ctx, &config.roles, &config.models),
            )
            .await
            else {
                return;
            };
            judge_usage += judge.usage;
            let score = judge.score;
            let judge_prov = judge.provider;
            let judge_assess = judge.assessment;
            let judge_gateway_attempts = judge.gateway_attempts;
            // Use mode's convergence_threshold as base (matching CLI engine in run_with_cancel),
            // not auto_specops_threshold (which is for low-conv auto-specops trigger).
            // This ensures homogeneity/quality penalties from effective_convergence_threshold
            // are applied against the mode-intended bar (TearDown 0.8, Pathfind 0.6, Harden 0.7).
            let base_threshold = mode.convergence_threshold();
            let effective_threshold = effective_convergence_threshold(
                base_threshold,
                judge_assess.as_ref(),
                convergence_quality_penalty_enabled(stream_config.validate),
            );
            let converged = score >= effective_threshold;

            let _ = event_tx
                .send(StreamEvent::convergence_scored(
                    &session_id,
                    round_num,
                    score,
                    converged,
                ))
                .await;

            // N02: per-seat divergence map. Embed the seats' responses for this
            // round and project to 2D via PCA, then emit `round_divergence`.
            // Embedding is sync (fastembed lock + possible model download), so
            // it runs in spawn_blocking per the async-hygiene convention. If
            // embeddings are unavailable or there are < 2 usable seats, the
            // event is omitted — the UI tolerates absence.
            if until_cancelled(
                &cancel,
                emit_round_divergence(&event_tx, &session_id, round_num, &responses),
            )
            .await
            .is_none()
            {
                return;
            }

            let flip_hash = judge_assess.as_ref().map(|a| {
                let mut hasher = DefaultHasher::new();
                format!("{}|{}", a.drift.as_deref().unwrap_or(""), a.recommendation)
                    .hash(&mut hasher);
                format!("{:x}", hasher.finish())[..8].to_string()
            });

            // Sheldon claim validator (mirrors CLI engine/deliberate.rs)
            // Tries full claim_validator cascade from roles.yaml until one succeeds.
            let mut validation_report = None;
            if stream_config.validate && round_num <= rounds_planned {
                let claim_role = &config.roles.claim_validator; // note: stream has access via outer config
                if crate::engine::sheldon::claim_validator_ready(claim_role, round_num) {
                    for step in &claim_role.cascade {
                        if cancel.is_cancelled() {
                            return;
                        }
                        let v_provider = step.provider.clone();
                        let v_model = Some(step.model.clone());
                        let vcfg = crate::engine::sheldon::ValidatorConfig {
                            provider: v_provider.clone(),
                            model: v_model,
                            gate: stream_config.validate_gate,
                            verbose: false,
                        };
                        let _ = event_tx
                            .send(StreamEvent::info(&session_id, "Validating round…"))
                            .await;
                        let Some(val_result) = until_cancelled(
                            &cancel,
                            crate::engine::sheldon::validate_round(
                                &responses,
                                topic,
                                context,
                                round_num,
                                &vcfg,
                                &req_ctx,
                                Some(&evidence_cache),
                            ),
                        )
                        .await
                        else {
                            return;
                        };
                        match val_result {
                            crate::engine::sheldon::ValidateRoundOutcome::Ok(report, c) => {
                                validation_report = Some(report.clone());
                                validator_cost_usd += c;
                                // Gate decision moved below: see P2 early-stop handling.
                                if let Some(ref r) = validation_report {
                                    let verdicts_json =
                                        serde_json::to_value(r).unwrap_or(json!([]));
                                    let _ = event_tx
                                        .send(StreamEvent::round_validation(
                                            &session_id,
                                            round_num,
                                            stream_config.validate_gate,
                                            &verdicts_json,
                                        ))
                                        .await;
                                }
                                break;
                            }
                            crate::engine::sheldon::ValidateRoundOutcome::Skipped(_) => {
                                break;
                            }
                            crate::engine::sheldon::ValidateRoundOutcome::ProviderFailed => {}
                        }
                    }
                }
            }

            // P2: Gate redaction (responses) only on continuing intermediate rounds.
            // On terminating rounds (planned last, or early via budget/convergence)
            // leave responses un-gated so the Chair transcript + append_validation_context
            // receives full evidence alongside the authoritative validation_report.
            // Compute would-budget using post-validator spend to decide "terminating".
            if stream_config.validate_gate && validation_report.is_some() {
                let this_seat_cost: f64 = responses.iter().map(|r| r.cost_usd).sum();
                let spend_at =
                    cumulative_spend + this_seat_cost + validator_cost_usd + judge_usage.cost_usd;
                let would_budget = should_pause_for_budget(
                    stream_config.budget_max_usd,
                    spend_at,
                    round_num,
                    rounds_planned,
                );
                let is_terminating = round_num >= rounds_planned || converged || would_budget;
                if !is_terminating && let Some(ref rpt) = validation_report {
                    responses = crate::engine::sheldon::gate_responses(&responses, rpt);
                }
            }

            all_rounds.push(RoundResult {
                round_num,
                responses,
                convergence_score: score,
                converged,
                judge_provider: judge_prov,
                judge_assessment: judge_assess,
                judge_gateway_attempts,
                flip_flop_hash: flip_hash,
                // T24: claim/reasoning are raw validator output that bypasses the
                // per-seat from_provider redaction closure — scrub before persist.
                validation_report: validation_report.map(crate::scrub::redact_validation_report),
            });

            let is_last = round_num >= rounds_planned;
            let _ = event_tx
                .send(StreamEvent::round_complete(
                    &session_id,
                    round_num,
                    score,
                    converged,
                    converged && !is_last,
                ))
                .await;

            let phase_seat_cost: f64 = all_rounds
                .iter()
                .flat_map(|r| &r.responses)
                .map(|r| r.cost_usd)
                .sum();
            let spend_at_boundary =
                cumulative_spend + phase_seat_cost + validator_cost_usd + judge_usage.cost_usd;
            if should_pause_for_budget(
                stream_config.budget_max_usd,
                spend_at_boundary,
                round_num,
                rounds_planned,
            ) {
                budget_paused = true;
                if let Some(max) = stream_config.budget_max_usd {
                    let _ = event_tx
                        .send(StreamEvent::budget_paused(
                            &session_id,
                            round_num,
                            spend_at_boundary,
                            max,
                        ))
                        .await;
                }
                break;
            }

            // Early convergence exit (no pause)
            if converged && !is_last && !stream_config.pause_after_each_round {
                break;
            }

            // ── Pause for operator input ──
            if stream_should_await_operator_input(
                stream_config.pause_after_each_round,
                is_last,
                early_exit,
            ) {
                let full_options = &[
                    "continue",
                    "end_early",
                    "escalate_specops",
                    "escalate_munger",
                    "escalate_contrarian",
                    "escalate_kiss",
                    "inject_context",
                    "swap_seat",
                ];
                let _ = event_tx
                    .send(StreamEvent::awaiting_input(
                        &session_id,
                        round_num,
                        score,
                        converged,
                        full_options,
                        None,
                    ))
                    .await;

                let action = loop {
                    let Some(candidate) = until_cancelled(&cancel, interventions.wait(900)).await
                    else {
                        return;
                    };
                    if effective_via_gateway && candidate.is_escalation() {
                        let mode = candidate.escalation_mode().unwrap_or("specops");
                        let required = crate::engine::direct_fire::spec(mode)
                            .map(|spec| {
                                vec![provider::gateway::TransportModel::new(
                                    spec.provider,
                                    spec.model,
                                )]
                            })
                            .unwrap_or_default();
                        if let Err(error) = provider::gateway::preflight_pairs(&required).await {
                            let _ = event_tx
                                .send(StreamEvent::error(
                                    &session_id,
                                    &format!(
                                        "Governed escalation rejected before dispatch: {error}"
                                    ),
                                    false,
                                ))
                                .await;
                            let _ = event_tx
                                .send(StreamEvent::awaiting_input(
                                    &session_id,
                                    round_num,
                                    score,
                                    converged,
                                    full_options,
                                    None,
                                ))
                                .await;
                            continue;
                        }
                    }
                    break candidate;
                };
                let _ = event_tx
                    .send(StreamEvent::intervention_received(
                        &session_id,
                        serde_json::to_value(&action).unwrap_or_default(),
                    ))
                    .await;

                extra_context.clear(); // Reset per-round injection

                match &action {
                    Intervention::EndEarly => {
                        early_exit = true;
                    }
                    Intervention::InjectContext { text } => {
                        extra_context = text.clone();
                    }
                    Intervention::SwapSeat {
                        seat_name,
                        provider,
                        model,
                        system,
                    } => {
                        let replacement_ready = if effective_via_gateway {
                            let current = live_seats.iter().find(|seat| &seat.name == seat_name);
                            let effective_provider = provider
                                .as_deref()
                                .or_else(|| current.map(|seat| seat.provider.as_str()));
                            let effective_model = model
                                .as_deref()
                                .or_else(|| current.map(|seat| seat.model.as_str()));
                            if let (Some(transport), Some(model)) =
                                (effective_provider, effective_model)
                            {
                                let required = [provider::gateway::TransportModel::new(
                                    crate::provider::canonical_provider_name(transport),
                                    model,
                                )];
                                match provider::gateway::preflight_pairs(&required).await {
                                    Ok(()) => true,
                                    Err(error) => {
                                        let _ = event_tx
                                            .send(StreamEvent::error(
                                                &session_id,
                                                &format!(
                                                    "Governed seat swap rejected before dispatch: {error}"
                                                ),
                                                false,
                                            ))
                                            .await;
                                        false
                                    }
                                }
                            } else {
                                true
                            }
                        } else {
                            true
                        };
                        if replacement_ready
                            && let Some(seat) = live_seats.iter_mut().find(|s| &s.name == seat_name)
                        {
                            if let Some(p) = provider {
                                seat.provider = p.clone();
                            }
                            if let Some(m) = model {
                                seat.model = m.clone();
                            }
                            if let Some(s) = system {
                                seat.system = s.clone();
                            }
                        }
                    }
                    _ if action.is_escalation() => {
                        let esc_mode = action.escalation_mode().unwrap_or("specops");
                        let _ = event_tx
                            .send(StreamEvent::specops_started(
                                &session_id,
                                "manual",
                                esc_mode,
                            ))
                            .await;

                        // Run escalation
                        let Some(sig) = until_cancelled(
                            &cancel,
                            run_escalation(
                                &config,
                                topic,
                                &all_rounds,
                                esc_mode,
                                &available_set,
                                &req_ctx,
                            ),
                        )
                        .await
                        else {
                            return;
                        };
                        let _ = event_tx
                            .send(StreamEvent::specops_signal(
                                &session_id,
                                serde_json::to_value(&sig).unwrap_or_default(),
                            ))
                            .await;

                        let sig_text = sig.text.clone();
                        manual_specops_signal = sig_text.clone();
                        extra_context = format!("INTERVENTION ({}): {}", esc_mode, sig_text);

                        // ── RE-PAUSE after escalation (race condition fix) ──
                        // Without this, next round starts immediately and user
                        // never sees the signal. Matches Python fix.
                        let post_options = &["continue", "end_early", "inject_context"];
                        let _ = event_tx
                            .send(StreamEvent::awaiting_input(
                                &session_id,
                                round_num,
                                score,
                                converged,
                                post_options,
                                Some(&sig_text),
                            ))
                            .await;

                        let Some(post_action) =
                            until_cancelled(&cancel, interventions.wait(900)).await
                        else {
                            return;
                        };
                        let _ = event_tx
                            .send(StreamEvent::intervention_received(
                                &session_id,
                                serde_json::to_value(&post_action).unwrap_or_default(),
                            ))
                            .await;

                        match &post_action {
                            Intervention::EndEarly => {
                                early_exit = true;
                            }
                            Intervention::InjectContext { text } => {
                                extra_context = format!(
                                    "{}\n\nADDITIONAL OPERATOR NOTE: {}",
                                    extra_context, text
                                );
                            }
                            _ => {} // Continue
                        }
                    }
                    Intervention::Continue if converged => {
                        break; // Converged + continue = done
                    }
                    _ => {} // Continue to next round
                }
            }
        }

        if cancel.is_cancelled() {
            return;
        }

        if budget_paused {
            // Strip accumulation to avoid double-count with normal tail (which will now run).
            // Keep the phase summary for paused info.
            phase_summaries.push(json!({
                "phase": phase_idx + 1,
                "session_id": session_id,
                "deliberation_mode": match mode {
                    Mode::Pathfind => "pathfind",
                    Mode::TearDown => "teardown",
                    Mode::Harden => "harden",
                },
                "rounds_run": all_rounds.len(),
                "budget_paused": true,
            }));
            // Fall through to specops/synthesize/save (like CLI), then break after.
            // Do NOT break here.
        }

        // ── Auto SpecOps if low convergence ──
        let mut specops_text = manual_specops_signal.clone();
        let phase_final_conv = all_rounds
            .last()
            .map(|r| r.convergence_score)
            .unwrap_or(1.0);
        let mut specops_tokens: u32 = 0;
        let wants_auto_specops = specops_text.is_empty()
            && phase_final_conv < stream_config.auto_specops_threshold
            && available_set.contains("grok");
        let auto_specops_ready = if wants_auto_specops && effective_via_gateway {
            let required = crate::engine::direct_fire::spec("specops")
                .map(|spec| {
                    vec![provider::gateway::TransportModel::new(
                        spec.provider,
                        spec.model,
                    )]
                })
                .unwrap_or_default();
            match provider::gateway::preflight_pairs(&required).await {
                Ok(()) => true,
                Err(error) => {
                    let _ = event_tx
                        .send(StreamEvent::error(
                            &session_id,
                            &format!("Governed auto-SpecOps skipped before dispatch: {error}"),
                            false,
                        ))
                        .await;
                    false
                }
            }
        } else {
            true
        };

        if wants_auto_specops && auto_specops_ready {
            let _ = event_tx
                .send(StreamEvent::specops_started(&session_id, "auto", "specops"))
                .await;
            let Some(sig) = until_cancelled(
                &cancel,
                run_escalation(
                    &config,
                    topic,
                    &all_rounds,
                    "specops",
                    &available_set,
                    &req_ctx,
                ),
            )
            .await
            else {
                return;
            };
            let _ = event_tx
                .send(StreamEvent::specops_signal(
                    &session_id,
                    serde_json::to_value(&sig).unwrap_or_default(),
                ))
                .await;
            specops_text = sig.text;
            specops_tokens = sig.tokens_in.saturating_add(sig.tokens_out);
            cumulative_spend += sig.cost_usd;
        }

        // A chair cannot synthesize an empty deliberation. Partial seat
        // participation remains valid, but zero usable responses is a failed
        // Council run and must not be persisted as apparent success.
        if !has_usable_seat_response(&all_rounds) {
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    "All Council seats failed; synthesis was not attempted.",
                    true,
                ))
                .await;
            return;
        }

        // ── Chair synthesis ──
        let _ = event_tx
            .send(StreamEvent::synthesis_started(
                &session_id,
                &cabinet.chair.model,
            ))
            .await;

        let Some(synth) = until_cancelled(
            &cancel,
            synthesize(
                &config,
                cabinet,
                topic,
                context,
                &all_rounds,
                mode,
                &specops_text,
                &req_ctx,
            ),
        )
        .await
        else {
            return;
        };

        if let Some(err) = synth.error.as_deref() {
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    &format!("Council chair synthesis failed: {err}"),
                    true,
                ))
                .await;
            return;
        }
        if synth.text.trim().is_empty() {
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    "Council chair synthesis returned no usable response.",
                    true,
                ))
                .await;
            return;
        }

        tracing::info!(
            provider = %cabinet.chair.provider,
            model = %cabinet.chair.model,
            latency_ms = synth.latency_ms,
            "Chair synthesis completed"
        );

        let _ = event_tx
            .send(StreamEvent::synthesis_complete(
                &session_id,
                &synth.text,
                &synth.model,
                synth.latency_ms,
                synth.cost_usd,
                synth.provider_provenance.clone(),
            ))
            .await;

        // ── Persist ──
        let seat_tok: u32 = all_rounds
            .iter()
            .flat_map(|r| &r.responses)
            .map(|r| r.tokens_in + r.tokens_out)
            .sum();
        let total_tok: u32 = seat_tok
            .saturating_add(synth.tokens_in + synth.tokens_out + specops_tokens)
            .saturating_add(judge_usage.tokens);
        let total_lat: u64 = all_rounds
            .iter()
            .flat_map(|r| &r.responses)
            .map(|r| r.latency_ms)
            .sum::<u64>()
            .saturating_add(synth.latency_ms)
            .saturating_add(judge_usage.latency_ms);
        let total_cost: f64 = all_rounds
            .iter()
            .flat_map(|r| &r.responses)
            .map(|r| r.cost_usd)
            .sum::<f64>()
            + synth.cost_usd
            + validator_cost_usd
            + judge_usage.cost_usd;

        let session = CouncilSession {
            session_id: session_id.clone(),
            topic: crate::scrub::redact(&phase_topic),
            cabinet_name: cabinet.name.clone(),
            rounds: all_rounds.clone(),
            synthesis: Some(synth.text.clone()),
            synthesis_model: Some(synth.model),
            total_tokens: total_tok,
            total_latency_ms: total_lat,
            total_cost_usd: total_cost,
            specops_triggered: !specops_text.is_empty(),
            specops_cost_usd: 0.0,
            mode: match mode {
                Mode::TearDown => SessionMode::TearDown,
                Mode::Pathfind => SessionMode::Pathfind,
                Mode::Harden => SessionMode::Harden,
            },
            precedent_ids: precedent_ids.clone(),
            timestamp: Utc::now(),
            schema_version: 2,
            tier: stream_config.tier.clone(),
            budget: stream_config.budget_max_usd.map(|max| BudgetRecord {
                max_usd: max,
                paused: budget_paused,
                action_taken: budget_paused.then_some("end_early".to_string()),
            }),
            context_sources: vec![],
            // Streaming path is the warroom UI — tag as Warroom for §4.4 filtering.
            // chair token plumbing for the streaming path is v0.1.1 work; emit 0
            // for now so the serde shape is correct.
            origin: crate::types::SessionOrigin::Warroom,
            execution_route: if effective_via_gateway {
                crate::types::ExecutionRoute::Governed
            } else {
                crate::types::ExecutionRoute::Direct
            },
            gateway_sensitivity: effective_via_gateway.then_some(effective_sensitivity.clone()),
            chair_tokens_in: synth.tokens_in,
            chair_tokens_out: synth.tokens_out,
            chair_cost_usd: synth.cost_usd,
            chair_provider_provenance: synth.provider_provenance,
            chair_gateway_provenance: synth.gateway_provenance,
            parent_request_id: None,
            worker_provenance: stream_config.worker_provenance.clone(),
            worker_metrics: None,
        };

        if cancel.is_cancelled() {
            return;
        }

        // Save session
        let save_path = match save_session(&session) {
            Ok(p) => p,
            Err(e) => {
                let _ = event_tx
                    .send(StreamEvent::error(
                        &session_id,
                        &format!("Session save failed: {}", e),
                        false,
                    ))
                    .await;
                String::new()
            }
        };
        if !save_path.is_empty() {
            let _ = event_tx
                .send(StreamEvent::session_saved(&session_id, &save_path))
                .await;
        }

        if cancel.is_cancelled() {
            return;
        }

        // Index for precedent engine
        if let Err(e) = precedent::index_session(&session) {
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    &format!("Precedent indexing failed: {}", e),
                    false,
                ))
                .await;
        }

        if cancel.is_cancelled() {
            return;
        }

        // Incremental embedding index — keeps semantic search fresh
        let sid = session_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::warroom::embeddings::append_session(&sid);
        })
        .await;

        if phase_idx == 0
            && let Some(ref parent_id) = stream_config.parent_session_id
        {
            let cab_label = cabinet.name.clone();
            if let Err(e) = crate::warroom::lineage::record_fork(
                &session_id,
                parent_id,
                &stream_config.swaps,
                &cab_label,
            ) {
                let _ = event_tx
                    .send(StreamEvent::error(
                        &session_id,
                        &format!("Lineage record failed: {}", e),
                        false,
                    ))
                    .await;
            }
        }

        if cancel.is_cancelled() {
            return;
        }

        // Flight recorder
        match precedent::write_flight_record(&session) {
            Ok(path) => {
                let _ = event_tx
                    .send(StreamEvent::info(
                        &session_id,
                        &format!("Flight record: {}", path),
                    ))
                    .await;
            }
            Err(e) => {
                let _ = event_tx
                    .send(StreamEvent::error(
                        &session_id,
                        &format!("Flight record failed: {}", e),
                        false,
                    ))
                    .await;
            }
        }

        if phase_idx == 0 && phases_total > 1 {
            pathfinder_session_id = session_id.clone();
            pathfinder_synthesis = synth.text.clone();
        }
        cumulative_tokens += total_tok;
        cumulative_cost += total_cost;
        cumulative_latency += total_lat;
        cumulative_spend += total_cost;
        final_synthesis_text = synth.text.clone();
        final_conv = phase_final_conv;
        final_rounds_run = all_rounds.len() as u32;
        phases_completed += 1;
        phase_summaries.push(json!({
            "phase": phase_idx + 1,
            "session_id": session_id,
            "deliberation_mode": match mode {
                Mode::Pathfind => "pathfind",
                Mode::TearDown => "teardown",
                Mode::Harden => "harden",
            },
            "rounds_run": all_rounds.len(),
            "convergence_final": phase_final_conv,
            "total_cost_usd": total_cost,
        }));

        if budget_paused {
            break 'phases;
        }
    }

    if cancel.is_cancelled() {
        return;
    }

    // ── done ──
    let _ = event_tx
        .send(StreamEvent::done(
            &last_session_id,
            cumulative_tokens,
            cumulative_cost,
            cumulative_latency,
            &final_synthesis_text,
            final_conv,
            final_rounds_run,
            Some(json!({
                "phases_completed": phases_completed,
                "phases_total": phases_total,
                "phase_summaries": phase_summaries,
                "cumulative_spend_usd": cumulative_spend,
            })),
        ))
        .await;
}

/// `session_started` payload for direct-fire runs — shared with the smoke
/// shim in src/server.rs so the synthetic and real sequences stay
/// field-for-field identical (Phase 5 parity rule).
pub(crate) fn direct_fire_session_started_data(
    topic: &str,
    spec: &crate::engine::direct_fire::DirectFireSpec,
    available: &[(&'static str, bool)],
    tier: &str,
    via_gateway: bool,
    sensitivity: &str,
) -> serde_json::Value {
    json!({
        "topic": topic,
        "cabinet_name": "direct-fire",
        "rounds_planned": 0,
        "mode": "normal",
        "direct_fire": spec.slug,
        "active_seats": [],
        "dropped_seats": [],
        "chair": { "provider": spec.provider, "model": spec.model },
        "available_providers": available
            .iter()
            .filter(|(_, ok)| *ok)
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        "council_version": env!("CARGO_PKG_VERSION"),
        "stream_version": "rs-1.0.0",
        "tier": tier,
        "via_gateway": via_gateway,
        "execution_route": if via_gateway { "governed" } else { "direct" },
        "sensitivity": sensitivity.to_lowercase(),
    })
}

/// Direct-fire single-shot for the WS path (feature contract).
///
/// Same personas as the CLI handlers via `engine::direct_fire`, but unlike
/// the CLI (which prints and exits without saving) the warroom run persists
/// a `CouncilSession` so History/precedent can surface it.
/// Pinned event sequence: session_started → synthesis_started →
/// synthesis_complete → session_saved → done.
async fn run_direct_fire(
    config: Arc<Config>,
    stream_config: StreamConfig,
    event_tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }
    let session_id = Uuid::new_v4().to_string()[..12].to_string();
    let slug = stream_config.direct_fire.clone().unwrap_or_default();
    let spec = match crate::engine::direct_fire::spec(&slug) {
        Some(spec) => spec,
        None => {
            // Unreachable from the WS path — parse_ws_start_fields rejects
            // unknown modes — but keep a hard error for other callers.
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    &format!("Unknown direct_fire mode: {slug}"),
                    true,
                ))
                .await;
            return;
        }
    };

    let req_ctx = request_context(&stream_config);
    let via_gateway = stream_config
        .via_gateway
        .unwrap_or_else(provider::default_via_gateway);
    let sensitivity = stream_config
        .sensitivity
        .clone()
        .unwrap_or_else(provider::default_sensitivity);
    if via_gateway {
        let required_models = [provider::gateway::TransportModel::new(
            spec.provider,
            spec.model,
        )];
        if let Err(err) = provider::gateway::preflight_pairs(&required_models).await {
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    &format!("Governed Gateway preflight failed: {err}"),
                    true,
                ))
                .await;
            return;
        }
    }
    let available = provider::check_providers_with_gateway(via_gateway);

    let _ = event_tx
        .send(StreamEvent::session_started(
            &session_id,
            direct_fire_session_started_data(
                &stream_config.topic,
                spec,
                &available,
                &stream_config.tier,
                via_gateway,
                &sensitivity,
            ),
        ))
        .await;

    let _ = event_tx
        .send(StreamEvent::synthesis_started(&session_id, spec.model))
        .await;

    let prompt =
        crate::engine::direct_fire::build_prompt(&stream_config.topic, &stream_config.context);
    let Some(resp) = until_cancelled(
        &cancel,
        provider::ask_with_context(spec.provider, &prompt, spec.system, spec.model, &req_ctx),
    )
    .await
    else {
        return;
    };

    if let Some(err) = &resp.error {
        let _ = event_tx
            .send(StreamEvent::error(
                &session_id,
                &format!("{} direct-fire failed: {}", spec.display, err),
                true,
            ))
            .await;
        return;
    }

    let cost =
        config
            .models
            .estimate_cost(&resp.model, resp.tokens_in, resp.tokens_out, resp.cached_in);
    let text = {
        let scrubbed = crate::scrub::redact(&resp.text);
        crate::librarian::redaction::redact_secrets(&scrubbed).0
    };

    let _ = event_tx
        .send(StreamEvent::synthesis_complete(
            &session_id,
            &text,
            &resp.model,
            resp.latency_ms,
            cost,
            resp.provider_provenance.clone(),
        ))
        .await;

    let total_tokens = resp.tokens_in.saturating_add(resp.tokens_out);
    let session = CouncilSession {
        session_id: session_id.clone(),
        topic: crate::scrub::redact(&stream_config.topic),
        cabinet_name: "direct-fire".to_string(),
        rounds: vec![],
        synthesis: Some(text.clone()),
        synthesis_model: Some(resp.model.clone()),
        total_tokens,
        total_latency_ms: resp.latency_ms,
        total_cost_usd: cost,
        specops_triggered: false,
        specops_cost_usd: 0.0,
        mode: crate::engine::direct_fire::session_mode(&slug),
        precedent_ids: vec![],
        timestamp: Utc::now(),
        schema_version: 2,
        tier: stream_config.tier.clone(),
        budget: None,
        context_sources: vec![],
        origin: crate::types::SessionOrigin::Warroom,
        execution_route: if via_gateway {
            crate::types::ExecutionRoute::Governed
        } else {
            crate::types::ExecutionRoute::Direct
        },
        gateway_sensitivity: via_gateway.then_some(sensitivity.clone()),
        chair_tokens_in: resp.tokens_in,
        chair_tokens_out: resp.tokens_out,
        chair_cost_usd: cost,
        chair_provider_provenance: resp.provider_provenance,
        chair_gateway_provenance: resp.gateway_provenance,
        parent_request_id: None,
        worker_provenance: stream_config.worker_provenance.clone(),
        worker_metrics: None,
    };

    if cancel.is_cancelled() {
        return;
    }

    match save_session(&session) {
        Ok(path) => {
            let _ = event_tx
                .send(StreamEvent::session_saved(&session_id, &path))
                .await;
        }
        Err(e) => {
            let _ = event_tx
                .send(StreamEvent::error(
                    &session_id,
                    &format!("Session save failed: {}", e),
                    false,
                ))
                .await;
        }
    }

    if cancel.is_cancelled() {
        return;
    }

    // Index so History (sessions_list reads index.jsonl) shows the run —
    // `--reindex` would pick the saved file up anyway, so index now for
    // consistency rather than drifting until the next rebuild.
    if let Err(e) = precedent::index_session(&session) {
        let _ = event_tx
            .send(StreamEvent::error(
                &session_id,
                &format!("Precedent indexing failed: {}", e),
                false,
            ))
            .await;
    }

    if cancel.is_cancelled() {
        return;
    }

    let _ = event_tx
        .send(StreamEvent::done(
            &session_id,
            total_tokens,
            cost,
            resp.latency_ms,
            &text,
            1.0,
            0,
            Some(json!({ "direct_fire": slug })),
        ))
        .await;
}

// ═══════════════════════════════════════════════════════════════════════
// Internal helpers
// ═══════════════════════════════════════════════════════════════════════

/// N02: embed the seat responses for a round and emit a `round_divergence`
/// event with their 2D PCA projection. Silently omits the event when there are
/// fewer than two usable (non-empty, non-errored) responses or when embeddings
/// are unavailable.
async fn emit_round_divergence(
    event_tx: &mpsc::Sender<StreamEvent>,
    session_id: &str,
    round_num: u32,
    responses: &[SeatResponse],
) {
    let usable: Vec<(String, String)> = responses
        .iter()
        .filter(|r| r.error.is_none() && !r.text.trim().is_empty())
        .map(|r| (r.seat_name.clone(), r.text.clone()))
        .collect();
    if usable.len() < 2 {
        return;
    }
    let labels: Vec<String> = usable.iter().map(|(n, _)| n.clone()).collect();
    let texts: Vec<String> = usable.into_iter().map(|(_, t)| t).collect();

    let projected = tokio::task::spawn_blocking(move || {
        crate::warroom::divergence::project_seats(&labels, &texts)
    })
    .await;
    let points = match projected {
        Ok(Some(points)) => points,
        // Join error or embeddings unavailable — omit the event (UI tolerates).
        _ => return,
    };
    let _ = event_tx
        .send(StreamEvent::round_divergence(session_id, round_num, points))
        .await;
}

/// Build prompt for a seat in a given round.
#[allow(clippy::too_many_arguments)]
fn build_round_prompt(
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
        // Cold Eyes: precedent enters in R2+ cross-pollination
        if !precedent_text.is_empty() {
            prompt.push_str("## Prior Council Precedent\n\n");
            prompt.push_str(precedent_text);
            prompt.push_str("\n---\n\n");
        }
        // Cross-pollination: cumulative history
        if !extra_context.is_empty() {
            prompt.push_str(&format!(
                "OPERATOR INTERVENTION:\n{}\n\n---\n\n",
                extra_context
            ));
        }
        prompt.push_str(&format!("TOPIC: {}\n\n", topic));

        // Own prior response
        let mut own_text = "(no prior response)".to_string();
        for rnd in prior_rounds {
            for resp in &rnd.responses {
                if resp.seat_name == seat.name && !resp.text.is_empty() && resp.error.is_none() {
                    own_text = resp.text.clone();
                }
            }
        }
        prompt.push_str(&format!("YOUR MOST RECENT ANALYSIS:\n{}\n\n", own_text));

        // Other seats' responses (full history)
        prompt.push_str("FULL DELIBERATION HISTORY:\n");
        prompt.push_str(&"─".repeat(40));
        prompt.push('\n');
        for rnd in prior_rounds {
            prompt.push_str(&format!("\n### Round {}\n", rnd.round_num));
            for resp in &rnd.responses {
                if resp.seat_name != seat.name && !resp.text.is_empty() && resp.error.is_none() {
                    let truncated = if resp.text.len() > 2000 {
                        &resp.text[..2000]
                    } else {
                        &resp.text
                    };
                    prompt.push_str(&format!(
                        "**{} ({})**: {}\n\n",
                        resp.seat_name, resp.provider, truncated
                    ));
                }
            }
            crate::engine::deliberate::append_validation_context(&mut prompt, rnd);
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

// Convergence judge now shared from crate::engine::deliberate::judge_round

/// Escalation result (SpecOps, Munger, Contrarian, KISS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EscalationResult {
    pub text: String,
    pub model: String,
    pub latency_ms: u64,
    pub cost_usd: f64,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

use serde::{Deserialize, Serialize};

/// Run an escalation (SpecOps swarm, Munger, Contrarian, KISS, or Premortem).
pub(crate) async fn run_escalation(
    config: &Config,
    topic: &str,
    rounds: &[RoundResult],
    mode: &str,
    _available: &std::collections::HashSet<&str>,
    req_ctx: &RequestContext,
) -> EscalationResult {
    // Build transcript from all rounds
    let mut transcript = String::new();
    for rnd in rounds {
        for resp in &rnd.responses {
            if !resp.text.is_empty() && resp.error.is_none() {
                let truncated = if resp.text.len() > 1000 {
                    &resp.text[..1000]
                } else {
                    &resp.text
                };
                transcript.push_str(&format!(
                    "[{} R{}]: {}\n\n",
                    resp.seat_name, resp.round_num, truncated
                ));
            }
        }
    }

    // Shared persona specs (engine::direct_fire) — same prompts/providers as
    // the CLI direct-fire handlers, single source of truth.
    let (system, provider_name, model) = match crate::engine::direct_fire::spec(mode) {
        Some(spec) => (spec.system, spec.provider, spec.model),
        None => ("Analyze and provide your verdict.", "grok", ""),
    };

    let prompt = if transcript.is_empty() {
        topic.to_string()
    } else {
        format!(
            "TOPIC: {}\n\nDELIBERATION TRANSCRIPT:\n{}\n\n---\n\n\
             Cut through the noise. What is the ONE thing that actually matters? \
             Give me the signal, not the framework. One paragraph max.",
            topic, transcript
        )
    };

    let resp = provider::ask_with_context(provider_name, &prompt, system, model, req_ctx).await;
    let cost =
        config
            .models
            .estimate_cost(&resp.model, resp.tokens_in, resp.tokens_out, resp.cached_in);

    EscalationResult {
        text: resp.text,
        model: resp.model,
        latency_ms: resp.latency_ms,
        cost_usd: cost,
        tokens_in: resp.tokens_in,
        tokens_out: resp.tokens_out,
        mode: mode.to_string(),
        error: resp.error,
    }
}

/// Chair synthesis for streaming mode.
struct StreamChairResult {
    text: String,
    model: String,
    latency_ms: u64,
    cost_usd: f64,
    tokens_in: u32,
    tokens_out: u32,
    provider_provenance: Option<ProviderProvenance>,
    gateway_provenance: Option<crate::types::GatewayProvenance>,
    error: Option<String>,
}

fn has_usable_seat_response(rounds: &[RoundResult]) -> bool {
    rounds
        .iter()
        .flat_map(|round| &round.responses)
        .any(|response| response.error.is_none() && !response.text.trim().is_empty())
}

/// Whether the stream should block on operator input after a round.
pub(crate) fn stream_should_await_operator_input(
    pause_after_each_round: bool,
    is_last: bool,
    early_exit: bool,
) -> bool {
    pause_after_each_round && !is_last && !early_exit
}

const DEFAULT_STREAM_CHAIR_SYSTEM: &str = "You are the adult in the room. No hedging, no both-sides-ing. \
                      Synthesize, resolve, and decide.\n\n\
                      Sheldon validation reports (if present) use this taxonomy:\n\
                      - SUPPORTED: evidence-backed — you may build on them.\n\
                      - CONTRADICTED: directly challenged — an Act/harden verdict must flag the conflict explicitly.\n\
                      - NO_EVIDENCE: unverified assumption/local claim — treat as such, do not present as fact.";

fn stream_chair_system(cabinet: &Cabinet, mode: Mode) -> String {
    let base_chair = cabinet
        .chair
        .system
        .as_deref()
        .map(str::trim)
        .filter(|system| !system.is_empty())
        .unwrap_or(DEFAULT_STREAM_CHAIR_SYSTEM);
    format!("{}\n\n{}", base_chair, mode.chair_instruction())
}

fn stream_frame_check_enabled(stream_config: &StreamConfig, cabinet: &Cabinet) -> bool {
    stream_config.frame_check && !cabinet.local_code_only
}

#[allow(clippy::too_many_arguments)]
async fn synthesize(
    config: &Config,
    cabinet: &Cabinet,
    topic: &str,
    context: &str,
    rounds: &[RoundResult],
    mode: Mode,
    specops_signal: &str,
    req_ctx: &RequestContext,
) -> StreamChairResult {
    let mut prompt = String::from(
        "You are the Council Chair — the final reviewer in a multi-model deliberation. \
         You run LAST, after all other models.\n\n",
    );

    prompt.push_str(&format!("TOPIC:\n{}\n\n", topic));
    if !context.is_empty() {
        prompt.push_str(&format!("CONTEXT:\n{}\n\n", context));
    }

    prompt.push_str("FULL DELIBERATION TRANSCRIPT:\n");
    for rnd in rounds {
        prompt.push_str(&format!(
            "\n## Round {} (convergence: {:.0}%)\n",
            rnd.round_num,
            rnd.convergence_score * 100.0
        ));
        for resp in &rnd.responses {
            if !resp.text.is_empty() && resp.error.is_none() {
                prompt.push_str(&format!(
                    "\n### {} ({})\n{}\n",
                    resp.seat_name, resp.provider, resp.text
                ));
            }
        }
        crate::engine::deliberate::append_validation_context(&mut prompt, rnd);
    }

    prompt.push_str(
        "\nProduce your FINAL RULING with this structure:\n\
         1. **Consensus** — where all models agree\n\
         2. **Disagreements** — where they diverge, with your assessment\n\
         3. **Blind Spots** — what NO model addressed but should have\n\
         4. **Ruling** — your decision, one clear paragraph\n\
         5. **Confidence** — HIGH / MEDIUM / LOW with justification\n\
         6. **Unresolved Questions** — what remains genuinely uncertain\n\
         7. **Actions** — concrete next steps, ordered by priority",
    );

    if !specops_signal.is_empty() {
        prompt.push_str(&format!(
            "\n\n── SPECOPS SIGNAL ──\n\
             A Grok multi-agent swarm was deployed:\n\n\
             \"{}\"\n\n\
             You may incorporate, challenge, or overrule this signal.",
            specops_signal
        ));
    }

    // claim-validation path: hoist the validator report as an authoritative section for the Chair.
    prompt.push_str("\n\n## Sheldon Validator Report (AUTHORITATIVE GROUND TRUTH)\n\n");
    for rnd in rounds {
        crate::engine::deliberate::append_validation_context(&mut prompt, rnd);
    }
    prompt.push_str("--- END AUTHORITATIVE VALIDATOR REPORT ---\n\n");

    let system = stream_chair_system(cabinet, mode);

    let resp = provider::ask_with_context(
        &cabinet.chair.provider,
        &prompt,
        &system,
        &cabinet.chair.model,
        req_ctx,
    )
    .await;
    let cost =
        config
            .models
            .estimate_cost(&resp.model, resp.tokens_in, resp.tokens_out, resp.cached_in);

    let error = resp.error;
    StreamChairResult {
        text: if error.is_some() {
            String::new()
        } else {
            let text = crate::scrub::redact(&resp.text);
            crate::librarian::redaction::redact_secrets(&text).0
        },
        model: resp.model,
        latency_ms: resp.latency_ms,
        cost_usd: cost,
        tokens_in: resp.tokens_in,
        tokens_out: resp.tokens_out,
        provider_provenance: resp.provider_provenance,
        gateway_provenance: resp.gateway_provenance,
        error,
    }
}

/// Save session to sessions/ directory. Returns path string.
///
/// output-fidelity invariant:
/// "full-fidelity raw chat transcripts in sessions/*.json... strictly limit
/// envelope_json_canonical to the parsed, fenced JSON directive proposal."
/// Identical to engine save_session: full CouncilSession via serde (no clip on raw seat/chair text or metadata).
/// War Room streaming path. Human previews labeled in precedent::flight_record_markdown.
/// Non-goal: no change to provider finish_reason or broad streaming.
fn save_session(session: &CouncilSession) -> anyhow::Result<String> {
    let dir = std::env::var("COUNCIL_SESSIONS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("sessions"));

    std::fs::create_dir_all(&dir)?;

    let filename = format!(
        "council_{}_{}.json",
        Utc::now().format("%Y%m%d_%H%M%S"),
        session.session_id
    );
    let path = dir.join(&filename);
    let json = serde_json::to_string_pretty(session)?;
    std::fs::write(&path, json)?;

    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pre_cancelled_run_emits_nothing_and_starts_no_work() {
        let config = Arc::new(Config::load(std::path::Path::new(".")).unwrap());
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        cancel.cancel();

        run(
            config,
            StreamConfig {
                topic: "must not dispatch".into(),
                ..StreamConfig::default()
            },
            event_tx,
            InterventionQueue::new(),
            cancel,
        )
        .await;

        assert!(event_rx.recv().await.is_none());
    }

    #[test]
    fn code_verify_stream_chair_uses_verifier_system_prompt() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cabinet = config.get_cabinet("code-verify").unwrap();
        let system = stream_chair_system(cabinet, Mode::Harden);

        assert!(system.contains("local-code verifier Chair"));
        assert!(system.contains("seat outputs"));
        assert!(system.contains("NO_EVIDENCE"));
        assert!(system.contains(Mode::Harden.chair_instruction()));
    }

    #[test]
    fn stream_should_not_await_input_when_early_exit() {
        assert!(!stream_should_await_operator_input(true, false, true));
        assert!(stream_should_await_operator_input(true, false, false));
        assert!(!stream_should_await_operator_input(true, true, false));
    }

    #[test]
    fn should_pause_for_budget_matches_engine_helper() {
        use crate::engine::deliberate::should_pause_for_budget;
        assert!(!should_pause_for_budget(None, 1.0, 1, 3));
        assert!(!should_pause_for_budget(Some(1.0), 0.5, 3, 3));
        assert!(should_pause_for_budget(Some(0.01), 0.02, 1, 3));
    }

    #[test]
    fn direct_fire_session_started_data_matches_pinned_shape() {
        let spec = crate::engine::direct_fire::spec("premortem").unwrap();
        let data = direct_fire_session_started_data(
            "Topic",
            spec,
            &[("grok_hermes", true), ("claude_code", false)],
            "best",
            true,
            "YELLOW",
        );
        assert_eq!(data["direct_fire"], "premortem");
        assert_eq!(data["cabinet_name"], "direct-fire");
        assert_eq!(data["rounds_planned"], 0);
        assert_eq!(data["chair"]["provider"], "grok_hermes");
        assert_eq!(data["chair"]["model"], "grok-4.3");
        assert_eq!(
            data["available_providers"],
            serde_json::json!(["grok_hermes"])
        );
        assert_eq!(data["via_gateway"], true);
        assert_eq!(data["execution_route"], "governed");
        // Wire casing is lowercase even though provider internals are UPPERCASE.
        assert_eq!(data["sensitivity"], "yellow");
    }

    #[test]
    fn stream_config_defaults_leave_phase6_fields_unset() {
        let config = StreamConfig::default();
        assert_eq!(config.via_gateway, None);
        assert_eq!(config.sensitivity, None);
        assert_eq!(config.direct_fire, None);
        let ctx = request_context(&config);
        assert_eq!(ctx.via_gateway, None);
        assert_eq!(ctx.sensitivity, None);
    }

    #[test]
    fn governed_preflight_collects_selected_cabinet_and_utility_models() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cabinet = config.get_cabinet("quick").unwrap();
        let stream = StreamConfig {
            cabinet_name: "quick".into(),
            frame_check: true,
            ..StreamConfig::default()
        };
        let transport_models = governed_required_transport_models(cabinet);
        let transport_alternatives =
            governed_alternative_transport_model_groups(&config, true, stream.validate);

        assert!(transport_models.iter().any(|pair| pair.model == "grok-4.3"));
        assert!(
            transport_models
                .iter()
                .any(|pair| pair.model == "gpt-5.6-sol")
        );
        assert!(
            transport_models
                .iter()
                .any(|pair| pair.model == "gemini-3.1-pro-preview")
        );
        assert!(
            transport_alternatives
                .iter()
                .flatten()
                .any(|pair| pair.model == "grok-4.20-0309-reasoning")
        );
        assert!(
            transport_alternatives
                .iter()
                .flatten()
                .any(|pair| { pair.model == "mistralai/mistral-large-3-675b-instruct-2512" })
        );
        assert!(
            transport_models
                .iter()
                .any(|pair| pair.transport == "grok_hermes" && pair.model == "grok-4.3")
        );
        assert!(transport_alternatives.iter().flatten().any(|pair| {
            pair.transport == "grok_hermes" && pair.model == "grok-4.20-0309-reasoning"
        }));
    }

    #[test]
    fn code_verify_stream_disables_frame_check_even_when_requested() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cabinet = config.get_cabinet("code-verify").unwrap();
        let stream_config = StreamConfig {
            cabinet_name: "code-verify".to_string(),
            frame_check: true,
            ..StreamConfig::default()
        };

        assert!(!stream_frame_check_enabled(&stream_config, cabinet));
    }

    // claim-validation path Phase 4: assert validation report appears in stream R2+ prompt assembly.
    #[test]
    fn build_round_prompt_includes_validation_report_for_r2plus() {
        let report = vec![crate::types::ClaimVerdictEntry {
            claim: "Streaming must include validator report".into(),
            seat: "Mirror".into(),
            verdict: crate::types::ClaimVerdict::Supported,
            evidence_citations: vec![],
            reasoning: "parity test".into(),
            confidence: 0.9,
            impact: crate::types::ClaimImpact::High,
            _overridden_from: None,
        }];

        let round = crate::types::RoundResult {
            round_num: 1,
            responses: vec![SeatResponse {
                seat_name: "Mirror".into(),
                provider: "grok".into(),
                text: "prior response".into(),
                ..Default::default()
            }],
            convergence_score: 0.9,
            converged: true,
            judge_provider: None,
            judge_assessment: None,
            judge_gateway_attempts: vec![],
            flip_flop_hash: None,
            validation_report: Some(report),
        };

        let seat = crate::types::Seat {
            name: "Checker".into(),
            provider: "gpt".into(),
            model: "gpt".into(),
            system: "test".into(),
        };

        let prompt = build_round_prompt(
            "test topic",
            "",
            "",
            "",
            &[round],
            "",
            &seat,
            2, // r2+
        );

        assert!(
            prompt.contains("VALIDATOR REPORT"),
            "stream build must contain validator header for prior round"
        );
        assert!(
            prompt.contains("Streaming must include validator report"),
            "stream build must contain the claim"
        );
    }

    #[test]
    fn synthesis_requires_at_least_one_usable_seat_response() {
        let failed = RoundResult {
            round_num: 1,
            responses: vec![SeatResponse {
                seat_name: "Failed".into(),
                provider: "test".into(),
                error: Some("provider failed".into()),
                ..Default::default()
            }],
            convergence_score: 0.0,
            converged: false,
            judge_provider: None,
            judge_assessment: None,
            judge_gateway_attempts: vec![],
            flip_flop_hash: None,
            validation_report: None,
        };
        assert!(!has_usable_seat_response(&[failed]));

        let usable = RoundResult {
            round_num: 1,
            responses: vec![SeatResponse {
                seat_name: "Useful".into(),
                provider: "test".into(),
                text: "evidence".into(),
                ..Default::default()
            }],
            convergence_score: 0.0,
            converged: false,
            judge_provider: None,
            judge_assessment: None,
            judge_gateway_attempts: vec![],
            flip_flop_hash: None,
            validation_report: None,
        };
        assert!(has_usable_seat_response(&[usable]));
    }
}
