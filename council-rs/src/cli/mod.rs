//! CLI deliberation path — plan resolution + orchestration.
//!
//! `main` owns clap parsing and non-deliberation subcommands. This module owns
//! the topic-required path: context load, smoke/direct-fire, full deliberation,
//! precedent index, flight records, and `--then-tear-down` phase 2.
//!
//! Flag contracts and exit/output behavior are preserved from the former
//! `main::run_deliberation_cli` body (PR8 characterization + phase split).

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::engine::deliberate;
use crate::engine::direct_fire;
use crate::mode::Mode;
use crate::precedent;
use crate::provider;

/// Inputs for the deliberation CLI path (clap-free; binary maps from `Cli`).
#[derive(Debug, Clone)]
pub struct DeliberationCliArgs {
    pub topic: Option<String>,
    pub context: Vec<PathBuf>,
    pub map: Option<PathBuf>,
    pub quiet: bool,
    pub smoke_provider: Option<String>,
    pub smoke_model: Option<String>,
    pub contrarian: bool,
    pub munger: bool,
    pub kiss_review: bool,
    pub specops: bool,
    pub premortem: bool,
    pub wargame: bool,
    pub quick: bool,
    pub heritage: bool,
    pub warroom: bool,
    pub reflection: bool,
    pub duo: bool,
    pub triad: Option<String>,
    pub cabinet: String,
    pub harden: bool,
    pub pathfind: bool,
    pub then_tear_down: bool,
    pub blind: bool,
    pub no_frame_check: bool,
    pub budget: Option<f64>,
    pub tier: String,
    pub validate: bool,
    pub validate_provider: String,
    pub validate_gate: bool,
}

// ── Plan resolution (pure; characterized by unit tests) ──────────────

/// Cabinet shortcut flags → override name, or `None` to use `--cabinet` / external key.
pub fn resolve_cabinet_override(
    wargame: bool,
    quick: bool,
    heritage: bool,
    warroom: bool,
    reflection: bool,
    duo: bool,
    triad: Option<&str>,
) -> Result<Option<String>> {
    if wargame {
        return Ok(Some("wargame".into()));
    }
    if quick {
        return Ok(Some("quick".into()));
    }
    if heritage {
        return Ok(Some("heritage".into()));
    }
    if warroom {
        return Ok(Some("warroom".into()));
    }
    if reflection {
        return Ok(Some("reflection".into()));
    }
    if duo {
        return Ok(Some("duo".into()));
    }
    if let Some(domain) = triad {
        let valid = [
            "strategy",
            "architecture",
            "debugging",
            "product",
            "risk",
            "shipping",
        ];
        if !valid.contains(&domain) {
            anyhow::bail!(
                "Unknown triad domain: '{}'. Valid: {}",
                domain,
                valid.join(", ")
            );
        }
        return Ok(Some(format!("triad-{domain}")));
    }
    Ok(None)
}

/// Final cabinet name: shortcut override > external loaded key > `--cabinet`.
pub fn resolve_cabinet_name(
    override_name: Option<&str>,
    loaded_cabinet_key: Option<&str>,
    cabinet: &str,
) -> String {
    override_name
        .or(loaded_cabinet_key)
        .unwrap_or(cabinet)
        .to_string()
}

/// Mode precedence: `--harden` > `--pathfind`/`--then-tear-down` > tear-down.
/// `--harden` cannot combine with `--then-tear-down`.
pub fn resolve_mode(harden: bool, pathfind: bool, then_tear_down: bool) -> Result<Mode> {
    if harden && then_tear_down {
        anyhow::bail!("--harden cannot be combined with --then-tear-down; harden IS the review");
    }
    let use_pathfind = pathfind || then_tear_down;
    Ok(if harden {
        Mode::Harden
    } else if use_pathfind {
        Mode::Pathfind
    } else {
        Mode::TearDown
    })
}

/// Frame check: on by default; skip with `--no-frame-check`, `--quick`, or
/// local-code-only cabinets.
pub fn should_frame_check(no_frame_check: bool, quick: bool, local_code_only: bool) -> bool {
    !no_frame_check && !quick && !local_code_only
}

/// Direct-fire slug from CLI flags, or `None` when running full council.
pub fn resolve_direct_fire_slug(
    premortem: bool,
    contrarian: bool,
    munger: bool,
    kiss_review: bool,
    specops: bool,
) -> Option<&'static str> {
    if !(premortem || contrarian || munger || kiss_review || specops) {
        return None;
    }
    Some(if premortem {
        "premortem"
    } else if contrarian {
        "contrarian"
    } else if munger {
        "munger"
    } else if kiss_review {
        "kiss"
    } else {
        "specops"
    })
}

pub fn smoke_default_model(provider: &str) -> Option<&'static str> {
    match provider {
        "claude_code" | "claude_api" => Some("claude-opus-4-6"),
        "codex_cli" | "openai_api" => Some("gpt-5.6-sol"),
        "gemini_agy" => Some("agy-default"),
        "gemini_vertex" => Some("gemini-3.1-pro-preview"),
        "grok_api" | "grok_hermes" => Some("grok-4.3"),
        "grok_build" => Some("grok-4.5"),
        // Legacy transport aliases remain accepted during migration.
        "claude" => Some("claude-opus-4-6"),
        "gpt" => Some("gpt-5.6-sol"),
        "gemini" => Some("agy-default"), // agy preferred; falls back in dispatch
        "gemini_cli" => Some("gemini-3.1-pro-preview"),
        "grok" => Some("grok-4.3"),
        "grok_cli" => Some("grok-build"),
        "hermes_cli" => Some("grok-4.3"),
        "nvidia" | "nim" => Some("mistralai/mistral-small-4-119b-2603"),
        "nous" => Some("Hermes-4-70B"),
        _ => None,
    }
}

// ── Orchestration phases ─────────────────────────────────────────────

/// Topic + file/stdin context + optional map scan.
async fn prepare_cli_input(args: &DeliberationCliArgs) -> Result<(String, String)> {
    let topic = match &args.topic {
        Some(t) => t.clone(),
        None => {
            eprintln!("Error: <TOPIC> is required for deliberation.");
            eprintln!("Usage: council [OPTIONS] <TOPIC>");
            eprintln!("       council --discover");
            eprintln!("       council --recall \"search terms\"");
            eprintln!("       council --reindex");
            std::process::exit(1);
        }
    };

    let mut context = String::new();
    for path in &args.context {
        if path.to_str() == Some("-") {
            let mut stdin = String::new();
            std::io::stdin().read_to_string(&mut stdin)?;
            context.push_str(&stdin);
        } else {
            let content = std::fs::read_to_string(path)?;
            context.push_str(&content);
        }
        context.push_str("\n\n");
    }

    if let Some(ref map_dir) = args.map {
        match crate::warroom::safe_map::gather_map_context_for_deliberation(
            &map_dir.to_string_lossy(),
        ) {
            Ok(map_context) => {
                if !context.is_empty() {
                    context.push_str("\n\n---\n\n");
                }
                context.push_str(&map_context);
            }
            Err(e) => {
                if !args.quiet {
                    eprintln!("⚠️  --map: {e}");
                }
            }
        }
    }

    Ok((topic, context))
}

/// Provider smoke — no session, no deliberation.
async fn run_smoke_provider(
    args: &DeliberationCliArgs,
    config: &Config,
    topic: &str,
    provider: &str,
) -> Result<()> {
    let model = args.smoke_model.clone().unwrap_or_else(|| {
        smoke_default_model(provider)
            .unwrap_or_default()
            .to_string()
    });
    if model.is_empty() {
        anyhow::bail!("--smoke-model required for provider '{provider}' (no built-in default)");
    }
    if !args.quiet {
        eprintln!("\n🔬 provider smoke — {provider}/{model} (no session)");
        // T24: scrub secret shapes from the operator-facing topic echo.
        eprintln!("   Prompt: {}\n", crate::scrub::redact(topic));
    }
    let resp = provider::ask(provider, topic, "", &model).await;
    if let Some(err) = &resp.error {
        eprintln!("❌ Error: {err}");
        std::process::exit(1);
    }
    let cost =
        config
            .models
            .estimate_cost(&resp.model, resp.tokens_in, resp.tokens_out, resp.cached_in);
    if !args.quiet {
        eprintln!(
            "   ✅ {}ms | model={} | tok {}→{} | ${:.4}\n",
            resp.latency_ms, resp.model, resp.tokens_in, resp.tokens_out, cost
        );
    }
    println!("{}", resp.text);
    Ok(())
}

/// Direct-fire single-shot path.
async fn run_direct_fire_cli(
    args: &DeliberationCliArgs,
    config: &Config,
    via_gateway: bool,
    topic: &str,
    context: &str,
    slug: &str,
) -> Result<()> {
    let spec = direct_fire::spec(slug).expect("direct-fire spec for CLI flag");

    if via_gateway
        && let Err(error) =
            provider::gateway::preflight_pairs(&[provider::gateway::TransportModel::new(
                spec.provider,
                spec.model,
            )])
            .await
    {
        anyhow::bail!("Governed Gateway preflight failed: {error}");
    }

    if !args.quiet {
        eprintln!("\n⚡ {} — direct-fire mode (no council)", spec.display);
        eprintln!("   Provider: {}/{}", spec.provider, spec.model);
        // T24: scrub secret shapes from the operator-facing topic echo.
        eprintln!("   Topic: {}\n", crate::scrub::redact(topic));
    }

    let prompt = direct_fire::build_prompt(topic, context);

    let resp = provider::ask(spec.provider, &prompt, spec.system, spec.model).await;
    if let Some(err) = &resp.error {
        eprintln!("❌ Error: {}", err);
        std::process::exit(1);
    }

    let cost =
        config
            .models
            .estimate_cost(&resp.model, resp.tokens_in, resp.tokens_out, resp.cached_in);

    if !args.quiet {
        eprintln!(
            "   Latency: {}ms | Tokens: {}→{} | Cost: ${:.4}\n",
            resp.latency_ms, resp.tokens_in, resp.tokens_out, cost
        );
    }

    println!("{}", resp.text);
    Ok(())
}

/// Index + flight recorder for a completed session (shared by phase 1 and 2).
fn persist_session_artifacts(
    session: &crate::types::CouncilSession,
    quiet: bool,
    phase_label: Option<&str>,
) {
    let index_err_prefix = match phase_label {
        Some("phase2") => "Phase 2 indexing failed",
        _ => "Precedent indexing failed",
    };
    if let Err(e) = precedent::index_session(session) {
        eprintln!("⚠️  {index_err_prefix}: {e}");
    }

    match phase_label {
        Some("phase2") => {
            if let Ok(path) = precedent::write_flight_record(session)
                && !quiet
            {
                eprintln!("📋 Phase 2 flight record: {path}");
            }
        }
        _ => match precedent::write_flight_record(session) {
            Ok(path) => {
                if !quiet {
                    eprintln!("📋 Flight record: {path}");
                }
            }
            Err(e) => eprintln!("⚠️  Flight record failed: {e}"),
        },
    }
}

/// Full council deliberation + optional `--then-tear-down` phase 2.
async fn run_council_deliberation(
    args: &DeliberationCliArgs,
    config: &Config,
    loaded_cabinet_key: Option<&str>,
    topic: &str,
    context: &str,
) -> Result<()> {
    let cabinet_override = resolve_cabinet_override(
        args.wargame,
        args.quick,
        args.heritage,
        args.warroom,
        args.reflection,
        args.duo,
        args.triad.as_deref(),
    )?;
    let cabinet_name = resolve_cabinet_name(
        cabinet_override.as_deref(),
        loaded_cabinet_key,
        &args.cabinet,
    );

    let mode = resolve_mode(args.harden, args.pathfind, args.then_tear_down)?;
    let cabinet_policy = config.get_cabinet(&cabinet_name)?;
    let do_frame_check = should_frame_check(
        args.no_frame_check,
        args.quick,
        cabinet_policy.local_code_only,
    );

    let session = deliberate::run(
        config,
        &cabinet_name,
        topic,
        context,
        mode,
        args.blind,
        do_frame_check,
        !args.quiet,
        args.budget,
        &args.tier,
        args.validate,
        &args.validate_provider,
        args.validate_gate,
    )
    .await?;

    if let Some(synthesis) = &session.synthesis {
        println!("{synthesis}");
    }

    persist_session_artifacts(&session, args.quiet, None);

    if args.then_tear_down && mode == Mode::Pathfind {
        if !args.quiet {
            eprintln!("\n\n═══════════════════════════════════════════════════════════════");
            eprintln!("  PHASE 2: TEAR-DOWN — Stress-testing the pathfinder's plan");
            eprintln!("═══════════════════════════════════════════════════════════════\n");
        }

        let teardown_context = format!(
            "## PATHFINDER OUTPUT TO STRESS-TEST\n\n{}\n\n---\n\n{}",
            session.synthesis.as_deref().unwrap_or(""),
            context
        );

        let teardown_topic = format!(
            "STRESS-TEST the following plan produced by a Pathfinder deliberation on: {topic}"
        );

        let session2 = deliberate::run(
            config,
            &cabinet_name,
            &teardown_topic,
            &teardown_context,
            Mode::TearDown,
            args.blind,
            do_frame_check,
            !args.quiet,
            args.budget,
            &args.tier,
            args.validate,
            &args.validate_provider,
            args.validate_gate,
        )
        .await?;

        if let Some(synthesis) = &session2.synthesis {
            println!("\n---\n## TEAR-DOWN ASSESSMENT\n\n{synthesis}");
        }

        persist_session_artifacts(&session2, args.quiet, Some("phase2"));
    }

    Ok(())
}

/// Topic-required CLI path: context → smoke | direct-fire | full deliberation.
pub async fn run_deliberation_cli(
    args: DeliberationCliArgs,
    config: Arc<Config>,
    via_gateway: bool,
    loaded_cabinet_key: Option<String>,
) -> Result<()> {
    let (topic, context) = prepare_cli_input(&args).await?;

    // ── Direct-fire / smoke modes ──
    if let Some(ref prov) = args.smoke_provider {
        let provider = prov.trim();
        if provider.is_empty() {
            anyhow::bail!("--smoke-provider requires a provider name (e.g. claude)");
        }
        return run_smoke_provider(&args, &config, &topic, provider).await;
    }

    if let Some(slug) = resolve_direct_fire_slug(
        args.premortem,
        args.contrarian,
        args.munger,
        args.kiss_review,
        args.specops,
    ) {
        return run_direct_fire_cli(&args, &config, via_gateway, &topic, &context, slug).await;
    }

    // ── Full deliberation ──
    run_council_deliberation(
        &args,
        &config,
        loaded_cabinet_key.as_deref(),
        &topic,
        &context,
    )
    .await
}
