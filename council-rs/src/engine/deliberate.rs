//! Core deliberation entry: prepare → execute rounds → optional escalation →
//! synthesize and persist.
//!
//! 1. Fan-out: all seats respond in parallel (tokio::JoinSet)
//! 2. Cross-pollinate: seats see all prior responses (cumulative)
//! 3. Convergence: LLM judge scores agreement 0.0–1.0 (NIM GLM primary, $0)
//! 4. Chair synthesis: final ruling
//!
//! Convergence judge cascade: grok-4.20-0309-reasoning → grok-4.3 → mistralai/mistral-small-4-119b-2603 (NIM free).
//! Reasoning-aware score extraction strips <reasoning> tags.
//!
//! Responsibilities live in submodules: [`budget`] (BATS signal + pause gate),
//! [`judge`] (convergence judging), [`utility_roles`] (judge/frame-check
//! cascades), [`seats`] (round prompts + buffered fan-out), [`rounds`] (the
//! shared round loop), [`synthesis`] (chair + persistence).

use anyhow::Result;
use std::collections::HashSet;
use std::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::Config;
use crate::engine::context::RequestContext;
use crate::engine::sheldon;
use crate::mode::Mode;
use crate::precedent;
use crate::provider;
use crate::text::truncate_utf8;
use crate::types::*;

mod budget;
mod judge;
mod rounds;
mod seats;
mod synthesis;
mod utility_roles;

pub use budget::{fetch_budget_signal, should_pause_for_budget};
pub use seats::build_round_prompt;
pub use synthesis::{ChairResult, build_chair_prompt};
pub use utility_roles::{frame_check_prompt, print_role_cascades};

pub(crate) use judge::{JudgeUsage, build_judge_prompt, parse_judge_json};
pub(crate) use rounds::{RoundExecution, RoundStream, execute_deliberation_rounds};
pub(crate) use seats::{prepare_seat_call, seat_response_from_provider};
pub(crate) use synthesis::{DEFAULT_CHAIR_SYSTEM, has_usable_seat_response, save_session};
pub(crate) use utility_roles::{
    CascadeCandidate, convergence_judge_candidates, frame_check_candidates,
};

#[cfg(test)]
pub(crate) use judge::{convergence_quality_penalty_enabled, effective_convergence_threshold};
#[cfg(test)]
pub(crate) use synthesis::{DIRECTIVE_TRIAGE_CHAIR_SYSTEM, chair_system_for};

use synthesis::synthesize_and_persist;

/// Normalized internal run settings shared by prepare, the round loop,
/// escalation, and synthesis. Each boundary (CLI/REST `run_with_cancel`,
/// WebSocket `run_phase_rounds`) constructs this once; external signatures
/// at those boundaries are unchanged. Borrowed: nothing is cloned beyond
/// what the spawned seat tasks already owned.
#[derive(Debug, Clone)]
pub(crate) struct DeliberationOptions<'a> {
    pub(crate) cabinet_name: &'a str,
    pub(crate) topic: &'a str,
    pub(crate) context: &'a str,
    pub(crate) mode: Mode,
    pub(crate) blind: bool,
    pub(crate) frame_check: bool,
    pub(crate) verbose: bool,
    pub(crate) budget_max_usd: Option<f64>,
    pub(crate) tier: &'a str,
    pub(crate) validate: bool,
    pub(crate) validate_provider: &'a str,
    pub(crate) validate_gate: bool,
    pub(crate) origin: SessionOrigin,
}

/// Run a full deliberation.
///
/// Backward-compatible thin wrapper around `run_with_cancel`. CLI / warroom /
/// drift / mapmaker callers retain the original 13-parameter signature; the
/// session is tagged `SessionOrigin::Cli` and runs without external
/// cancellation.
///
/// `blind=true` skips precedent injection (used by drift self-audit and
/// the CLI `--blind` flag). The session still gets indexed unless the caller
/// chooses otherwise.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: &Config,
    cabinet_name: &str,
    topic: &str,
    context: &str,
    mode: Mode,
    blind: bool,
    frame_check: bool,
    verbose: bool,
    budget_max_usd: Option<f64>,
    tier: &str,
    validate: bool,
    validate_provider: &str,
    validate_gate: bool,
) -> Result<CouncilSession> {
    run_with_cancel(
        config,
        cabinet_name,
        topic,
        context,
        mode,
        blind,
        frame_check,
        verbose,
        budget_max_usd,
        tier,
        validate,
        validate_provider,
        validate_gate,
        SessionOrigin::Cli,
        RequestContext::default(),
        None,
        None,
    )
    .await
}

/// Full deliberation entry point with explicit origin + cancellation surface
/// (Phase 0.5 §4.5).
///
/// New caller: `POST /api/deliberate` in `server.rs`, which passes
/// `SessionOrigin::Api`, a `RequestContext` carrying the gateway parent
/// request id, and a `CancellationToken` that fires on client disconnect.
///
/// Cancellation contract (v0.1 scope-cut):
///   - Each seat task is wrapped in `tokio::select!` against `cancel`. On
///     cancel the seat task short-circuits — but the in-flight `reqwest`
///     call is NOT aborted (sunk cost up to one round of seat costs;
///     ~$0.05 triage worst-case, ~$0.30 warroom).
///   - Between rounds the cancel flag is polled; if set, the engine writes
///     a partial diagnostic file to `sessions/_cancelled/` with
///     `origin: SessionOrigin::ApiCancelled` and returns `Err(cancelled)`.
///     The partial result is a private diagnostic side-channel — never
///     surfaced in the API response, never indexed for precedent.
///   - v0.1.1 will thread `CancellationToken` into `reqwest::send` to
///     eliminate the in-flight HTTP waste.
///
/// Orchestration is a thin sequence of domain-named phases (PR7). Inputs,
/// outputs, ordering, and authority checks are unchanged.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_cancel(
    config: &Config,
    cabinet_name: &str,
    topic: &str,
    context: &str,
    mode: Mode,
    blind: bool,
    frame_check: bool,
    verbose: bool,
    budget_max_usd: Option<f64>,
    tier: &str,
    validate: bool,
    validate_provider: &str,
    validate_gate: bool,
    origin: SessionOrigin,
    req_ctx: RequestContext,
    worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
    cancel: Option<CancellationToken>,
) -> Result<CouncilSession> {
    let opts = DeliberationOptions {
        cabinet_name,
        topic,
        context,
        mode,
        blind,
        frame_check,
        verbose,
        budget_max_usd,
        tier,
        validate,
        validate_provider,
        validate_gate,
        origin,
    };

    let prepared = prepare_deliberation(config, &opts, req_ctx).await?;

    let rounds =
        execute_deliberation_rounds(config, &prepared, &opts, cancel.as_ref(), None, 0.0).await?;

    let rounds = maybe_escalate_specops(config, &prepared, rounds, &opts).await;

    synthesize_and_persist(config, prepared, rounds, &opts, worker_provenance).await
}

/// Phase 1 product: cabinet, session identity, transport, precedent, BATS.
pub(crate) struct PreparedDeliberation {
    pub(crate) cabinet: Cabinet,
    pub(crate) session_id: String,
    pub(crate) req_ctx: RequestContext,
    pub(crate) effective_via_gateway: bool,
    pub(crate) effective_sensitivity: String,
    pub(crate) budget_signal: String,
    pub(crate) precedent_text: String,
    pub(crate) precedent_ids: Vec<String>,
    pub(crate) evidence_cache: sheldon::EvidenceCache,
}

/// Phase 1 — resolve cabinet, mint session id, gateway preflight, BATS + precedent.
async fn prepare_deliberation(
    config: &Config,
    opts: &DeliberationOptions<'_>,
    mut req_ctx: RequestContext,
) -> Result<PreparedDeliberation> {
    let DeliberationOptions {
        cabinet_name,
        topic,
        mode,
        blind,
        frame_check,
        verbose,
        budget_max_usd,
        validate,
        ..
    } = opts.clone();
    // resolve_cabinet_owned (feature contract): registry hit clones; a miss falls back to
    // <base_dir>/cabinets/<name>.yaml so cabinets saved after startup are
    // launchable by name. Bound by reference below to keep downstream usage
    // (fan_out, synthesize, cabinet.rounds, …) unchanged.
    let cabinet = config.resolve_cabinet_owned(cabinet_name)?;
    let session_id = Uuid::new_v4().to_string()[..12].to_string();
    req_ctx.council_session_id = Some(session_id.clone());
    let effective_via_gateway = req_ctx
        .via_gateway
        .unwrap_or_else(provider::default_via_gateway);
    let effective_sensitivity = req_ctx
        .sensitivity
        .clone()
        .unwrap_or_else(provider::default_sensitivity);

    if effective_via_gateway {
        let required_models = governed_required_transport_models(&cabinet);
        let alternatives =
            governed_alternative_transport_model_groups(config, frame_check, validate);
        if let Err(error) =
            provider::gateway::preflight_pairs_with_alternatives(&required_models, &alternatives)
                .await
        {
            anyhow::bail!("Governed Gateway preflight failed: {error}");
        }
    }

    let available = provider::check_providers_with_gateway(effective_via_gateway);
    let available_set: HashSet<&str> = available
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(name, _)| *name)
        .collect();
    let unavailable_seats: Vec<&Seat> = cabinet
        .seats
        .iter()
        .filter(|seat| !available_set.contains(seat.provider.as_str()))
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
        anyhow::bail!("provider unavailable: {}", missing.join(", "));
    }

    if verbose {
        eprintln!("\n════════════════════════════════════════════════════════════");
        eprintln!("  COUNCIL: {}", cabinet.name);
        eprintln!("  Topic: {}...", truncate_utf8(topic, 70));
        eprintln!(
            "  Seats: {} | Rounds: {} | Mode: {}",
            cabinet.seats.len(),
            cabinet.rounds,
            mode
        );
        if let Some(budget) = budget_max_usd {
            eprintln!("  Budget: ${:.2}", budget);
        }
        eprintln!("════════════════════════════════════════════════════════════\n");
        print_role_cascades(&config.roles);
    }

    // Session-scoped evidence cache (one per deliberation) for Sheldon --validate
    // dedup across rounds. Passed only to validator path.
    let evidence_cache = sheldon::EvidenceCache::default();

    // BATS Wedge 1: fetch and inject real-time budget signal + per-task log (using session as task_id)
    let (budget_signal, budget_tier) = fetch_budget_signal(
        std::env::var("HERMES_PROFILE").ok().as_deref(),
        Some(&session_id),
    )
    .await;
    if verbose {
        eprintln!(
            "  BATS: {} | Tier: {} (injected to prompts + seats)",
            if budget_signal.is_empty() {
                "no-signal"
            } else {
                "signal"
            },
            budget_tier
        );
    }

    // Precedent injection (unless blind mode). One retrieval receipt:
    // injected text == persisted `session.precedent_ids` by construction.
    // The War Room preview runs the same retrieve() with the same defaults,
    // but re-queries while typing — same ranker, not the same frozen object.
    let (precedent_text, precedent_ids) = if blind {
        if verbose {
            eprintln!("  📚 Precedent: skipped (blind mode)");
        }
        (String::new(), vec![])
    } else {
        // Offload synchronous precedent index loading and retrieval (may also
        // block on embedding-model init) — see server precedent_search wrapper
        // + stream deliberate. On join err: log + empty.
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
                if verbose {
                    eprintln!(
                        "  📚 Precedent: {} prior sessions found (engine={})",
                        receipt.hits.len(),
                        receipt.engine
                    );
                }
                (precedent::format_for_injection(&receipt), receipt.ids())
            }
            Ok(_) => (String::new(), vec![]),
            Err(e) => {
                eprintln!(
                    "ERROR: run_with_cancel precedent retrieve spawn_blocking join failed for topic (len={}): {}",
                    topic.len(),
                    e
                );
                (String::new(), vec![])
            }
        }
    };

    Ok(PreparedDeliberation {
        cabinet,
        session_id,
        req_ctx,
        effective_via_gateway,
        effective_sensitivity,
        budget_signal,
        precedent_text,
        precedent_ids,
        evidence_cache,
    })
}

/// Whether SpecOps auto-escalation is allowed for this origin/capability pair.
///
/// Grok availability is probed separately. API origin requires an explicit
/// `council_auto_escalate` opt-in (POST /api/deliberate default is false).
pub(crate) fn specops_auto_escalate_enabled(
    grok_available: bool,
    council_auto_escalate: bool,
    origin: SessionOrigin,
) -> bool {
    grok_available && (council_auto_escalate || origin != SessionOrigin::Api)
}

/// Phase 3 — SpecOps auto-escalation for non-converging runs (API-default suppressed).
async fn maybe_escalate_specops(
    config: &Config,
    prepared: &PreparedDeliberation,
    mut rounds: RoundExecution,
    opts: &DeliberationOptions<'_>,
) -> RoundExecution {
    let DeliberationOptions {
        topic,
        verbose,
        origin,
        ..
    } = opts.clone();
    // Grok counts as available via OAuth CLI or XAI_API_KEY; an empty
    // XAI_API_KEY= placeholder does not count as configured.
    let grok_available =
        crate::provider::env_nonempty("XAI_API_KEY") || crate::provider::is_grok_cli_available();
    let enable_specops = specops_auto_escalate_enabled(
        grok_available,
        prepared.req_ctx.council_auto_escalate,
        origin,
    );
    let final_converged = rounds.rounds.last().map(|r| r.converged).unwrap_or(false);
    let specops_ready = if enable_specops && !final_converged && prepared.effective_via_gateway {
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
                if verbose {
                    eprintln!("   ⚠️  Skipping unavailable governed SpecOps: {error}");
                }
                false
            }
        }
    } else {
        true
    };

    if enable_specops && !final_converged && specops_ready {
        if verbose {
            eprintln!("   ⚠️  Convergence low. Triggering SpecOps escalation.");
        }
        let sig = crate::engine::direct_fire::run_escalation(
            config,
            topic,
            &rounds.rounds,
            "specops",
            &prepared.req_ctx,
        )
        .await;
        rounds.specops_triggered = true;
        rounds.specops_cost_usd = sig.cost_usd;
        rounds.specops_signal_text = Some(sig.text.clone());
        rounds.total_cost += sig.cost_usd;
        rounds.total_tokens = rounds
            .total_tokens
            .saturating_add(sig.tokens_in + sig.tokens_out);
        rounds.total_latency_ms += sig.latency_ms;

        if verbose {
            eprintln!("   🚨 SPECOPS: {}", sig.text);
        }
    }

    rounds
}

/// Exact transport/model pairs that must be ready before a governed Council
/// can spend. Seats and chair are hard requirements because dropping either
/// makes the proceeding incomplete. Utility alternatives are handled below.
pub(crate) fn governed_required_transport_models(
    cabinet: &Cabinet,
) -> Vec<provider::gateway::TransportModel> {
    let mut pairs = cabinet
        .seats
        .iter()
        .map(|seat| {
            provider::gateway::TransportModel::new(
                provider::canonical_provider_name(&seat.provider),
                seat.model.clone(),
            )
        })
        .collect::<Vec<_>>();
    pairs.push(provider::gateway::TransportModel::new(
        provider::canonical_provider_name(&cabinet.chair.provider),
        cabinet.chair.model.clone(),
    ));
    pairs.sort_by(|a, b| (&a.transport, &a.model).cmp(&(&b.transport, &b.model)));
    pairs.dedup();
    pairs
}

/// Enabled utility roles are cascades: at least one exact candidate must be
/// ready, but requiring every provider would defeat one-key NVIDIA operation.
pub(crate) fn governed_alternative_transport_model_groups(
    config: &Config,
    frame_check: bool,
    validate: bool,
) -> Vec<Vec<provider::gateway::TransportModel>> {
    let mut groups = vec![
        convergence_judge_candidates(&config.roles, &config.models)
            .into_iter()
            .map(|candidate| {
                provider::gateway::TransportModel::new(candidate.provider, candidate.model)
            })
            .collect::<Vec<_>>(),
    ];
    if frame_check {
        groups.push(
            frame_check_candidates(&config.roles, &config.models)
                .into_iter()
                .map(|candidate| {
                    provider::gateway::TransportModel::new(candidate.provider, candidate.model)
                })
                .collect(),
        );
    }
    if validate {
        groups.push(
            config
                .roles
                .claim_validator
                .cascade
                .iter()
                .map(|step| {
                    provider::gateway::TransportModel::new(
                        provider::canonical_provider_name(&step.provider),
                        step.model.clone(),
                    )
                })
                .collect(),
        );
    }
    groups.retain(|group| !group.is_empty());
    groups
}

/// Auth gate aligned with `provider::check_providers` (codex counts for gpt, etc.).
/// For 'grok' we now also accept the OAuth grok CLI binary.
/// API keys use non-empty checks so `KEY=` placeholders do not count as ready.
pub(crate) fn provider_auth_ready(provider: &str) -> bool {
    match provider {
        "grok_api" => crate::provider::env_nonempty("XAI_API_KEY"),
        "grok_build" => crate::provider::is_grok_cli_available(),
        "grok_hermes" => crate::provider::hermes_cli::is_hermes_seat_available(),
        "claude_api" => crate::provider::env_nonempty("ANTHROPIC_API_KEY"),
        "claude_code" => crate::provider::claude::is_claude_cli_available(),
        "openai_api" => crate::provider::env_nonempty("OPENAI_API_KEY"),
        "codex_cli" => crate::provider::agent_cli::is_codex_cli_available(),
        "gemini_agy" => crate::provider::agent_cli::is_agy_cli_available(),
        "gemini_vertex" => crate::provider::gemini::is_vertex_available(),
        "grok" => {
            crate::provider::env_nonempty("XAI_API_KEY") || crate::provider::is_grok_cli_available()
        }
        "grok_cli" => crate::provider::is_grok_cli_available(),
        "gpt" => {
            crate::provider::env_nonempty("OPENAI_API_KEY")
                || Command::new("codex")
                    .arg("--version")
                    .stderr(std::process::Stdio::null())
                    .output()
                    .is_ok()
        }
        "nvidia" => crate::provider::env_nonempty("NVIDIA_API_KEY"),
        "gemini" => Command::new("gcloud")
            .args(["auth", "print-access-token"])
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success()),
        _ => true,
    }
}

#[cfg(test)]
mod specops_enable_tests {
    use super::specops_auto_escalate_enabled;
    use crate::types::SessionOrigin;

    #[test]
    fn api_origin_suppresses_even_when_grok_available() {
        // Capability forced true — this is the contract Copilot correctly
        // noted was untested by an integration run without Grok present.
        assert!(!specops_auto_escalate_enabled(
            true,
            false,
            SessionOrigin::Api
        ));
    }

    #[test]
    fn api_origin_allows_when_auto_escalate_opted_in() {
        assert!(specops_auto_escalate_enabled(
            true,
            true,
            SessionOrigin::Api
        ));
    }

    #[test]
    fn non_api_origin_allows_when_grok_available() {
        assert!(specops_auto_escalate_enabled(
            true,
            false,
            SessionOrigin::Cli
        ));
        assert!(specops_auto_escalate_enabled(
            true,
            false,
            SessionOrigin::Warroom
        ));
    }

    #[test]
    fn no_grok_never_enables() {
        assert!(!specops_auto_escalate_enabled(
            false,
            true,
            SessionOrigin::Cli
        ));
        assert!(!specops_auto_escalate_enabled(
            false,
            true,
            SessionOrigin::Api
        ));
    }
}
