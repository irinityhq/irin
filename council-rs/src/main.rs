//! council-rs CLI — same interface as council.py, plus new features
//!
//! council "Topic"                              # default: standard + tear-down
//! council --pathfind "Find a way"              # pathfinder mode
//! council --pathfind --then-tear-down "Topic"   # pathfind, then tear it down
//! council --cabinet warroom --pathfind "Topic"  # warroom + pathfinder
//! council --warroom --harden --validate --map . "Topic"  # shortcut: 5-seat war room cabinet
//! council --smoke-provider claude "ACK ping"   # single provider, no session
//! council --serve                               # start WebSocket server
//! council --contrarian "Topic"                  # direct-fire contrarian
//! council --munger "Topic"                      # direct-fire Munger
//! council --kiss-review "Topic"                 # direct-fire KISS
//! council --specops "Topic"                     # direct-fire SpecOps
//! council --recall "search"                     # precedent search
//! council --reindex                             # rebuild precedent index
//! council --blind "Topic"                       # skip precedent injection

use anyhow::Result;
use clap::Parser;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use council_rs::config::Config;
use council_rs::engine::deliberate;
use council_rs::engine::direct_fire;

use council_rs::mode::Mode;
use council_rs::precedent;
use council_rs::provider;
use council_rs::registry::ProviderRegistry;
use council_rs::server;

#[derive(Parser, Debug)]
#[command(
    name = "council",
    version,
    about = "Sovereign Intelligence Council — multi-model deliberation engine"
)]
struct Cli {
    /// Topic to deliberate on
    topic: Option<String>,

    /// Cabinet to use (name like standard/warroom/heritage, or path to YAML file)
    #[arg(long, short = 'C', default_value = "standard")]
    cabinet: String,

    /// Quick mode — alias for --cabinet quick
    #[arg(long)]
    quick: bool,

    // ── Deliberation Mode Toggle ──────────────────────────────
    /// PATHFINDER mode: don't stop til you find a way.
    /// Dead-end output forbidden. Every objection must include a solution.
    #[arg(long)]
    pathfind: bool,

    /// TEAR-DOWN mode (default): find every flaw, kill it if it deserves killing.
    #[arg(long)]
    tear_down: bool,

    /// HARDEN mode: stress like a redteam, build like a craftsman.
    /// Every flaw must come paired with the better way (cited prior art or
    /// concrete first-principles replacement). No bare "this is broken"
    /// verdicts. Outputs ratify / ratify-with-changes / replace-with-design.
    #[arg(long)]
    harden: bool,

    /// Run PATHFIND first, then TEAR-DOWN on the result.
    /// The recommended production usage for serious decisions.
    #[arg(long)]
    then_tear_down: bool,

    /// Blind mode — skip precedent injection
    #[arg(long)]
    blind: bool,

    /// Skip pre-dispatch frame check (v9.10.0 anti-prompt-poisoning).
    /// Saves ~500 tokens + 1 LLM call. Auto-skipped for --quick and direct-fire.
    #[arg(long)]
    no_frame_check: bool,

    /// Budget cap in USD. Pauses deliberation at round boundary when exceeded.
    #[arg(long)]
    budget: Option<f64>,

    /// Routing tier: best (default), sovereign, strict_sovereign
    #[arg(long, default_value = "best")]
    tier: String,

    /// Route all provider calls through Gateway (localhost:18080) for audit/decon/cost
    #[arg(long)]
    via_gateway: bool,

    /// Sensitivity level for Gateway routing: GREEN (default), YELLOW, RED
    #[arg(long, default_value = "GREEN", value_parser = clap::builder::PossibleValuesParser::new(["GREEN", "YELLOW", "RED"]))]
    sensitivity: String,

    // ── Direct-Fire Modes ─────────────────────────────────────
    /// Contrarian: first-principles teardown, no appeals to authority
    #[arg(long)]
    contrarian: bool,

    /// Munger Mind: Charlie Munger's latticework — inversion, incentives, models
    #[arg(long)]
    munger: bool,

    /// KISS Review: direct, comprehensive single-pass analysis
    #[arg(long)]
    kiss_review: bool,

    /// SpecOps: Grok multi-agent swarm analysis
    #[arg(long)]
    specops: bool,

    /// [EXPERIMENTAL] Wargame: MDMP-style adversarial COA wargaming via cabinets/wargame.yaml
    #[arg(long)]
    wargame: bool,

    /// [EXPERIMENTAL] Premortem: temporal-flip failure analysis ("it failed, write the AAR")
    #[arg(long)]
    premortem: bool,

    // ── Cabinet Shortcuts ────────────────────────────────────
    /// Heritage Cabinet — the 4 original archetypes (Skeptic + Mirror + Strategist + Tao)
    #[arg(long)]
    heritage: bool,

    /// War Room cabinet — 5 seats, 3 rounds, maximum depth (CEO/Mirror/Red Team/Constraint/Operator)
    #[arg(long)]
    warroom: bool,

    /// Reflection Cabinet (Munger + Socrates + Advocate + Tao)
    #[arg(long)]
    reflection: bool,

    /// Dialectic duo (for/against)
    #[arg(long)]
    duo: bool,

    /// Domain triad: strategy, architecture, debugging, product, risk, shipping
    #[arg(long)]
    triad: Option<String>,

    // ── Context & Output ──────────────────────────────────────
    /// Context files to inject (use - for stdin)
    #[arg(long, short = 'c')]
    context: Vec<PathBuf>,

    /// Quiet mode — only print the synthesis
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Auto-scan directory into context (Mapmaker)
    #[arg(long, short = 'm')]
    map: Option<PathBuf>,

    // ── Precedent & Admin ─────────────────────────────────────
    /// Search prior rulings (precedent recall)
    #[arg(long)]
    recall: bool,

    /// Rebuild precedent index from session JSONs
    #[arg(long)]
    reindex: bool,

    /// Show detected providers and exit
    #[arg(long)]
    discover: bool,

    /// List available cabinets and exit
    #[arg(long)]
    list_cabinets: bool,

    /// Single provider ping — no session, no deliberation (provider smoke)
    #[arg(long, value_name = "PROVIDER")]
    smoke_provider: Option<String>,

    /// Model for --smoke-provider (default: provider-specific opus/gpt/gemini/grok)
    #[arg(long)]
    smoke_model: Option<String>,

    /// Run weekly drift summary and exit (for LaunchAgent / cron)
    #[arg(long)]
    drift_weekly: bool,

    /// Window in days for drift analysis (default: 7)
    #[arg(long, default_value = "7")]
    drift_window: u32,

    /// Max sessions to analyze in drift run
    #[arg(long)]
    drift_limit: Option<usize>,

    // ── Sheldon Validator (v9.13) ───────────────────────────────
    /// Enable between-round claim validation (Sheldon)
    #[arg(long)]
    validate: bool,

    /// Legacy validator transport hint; roles.yaml owns the runtime cascade
    #[arg(long, default_value = "grok_hermes")]
    validate_provider: String,

    /// Gate mode: redact CONTRADICTED claims before cross-pollination
    #[arg(long)]
    validate_gate: bool,

    /// Run meta-review of the self-audit loop and exit
    #[arg(long)]
    meta_review: bool,

    /// Run utility-role eval harness (judge + frame-check fixtures) and exit
    #[arg(long)]
    judge_eval: bool,

    /// Eval role filter: judge, frame, or both
    #[arg(long, default_value = "both")]
    judge_eval_role: String,

    /// Run a single fixture by id (e.g. high_agreement, poisoned_frame)
    #[arg(long)]
    judge_eval_fixture: Option<String>,

    /// Pin eval to one provider (sets COUNCIL_JUDGE_* / COUNCIL_FRAME_CHECK_*)
    #[arg(long)]
    eval_provider: Option<String>,

    /// Pin eval to one model
    #[arg(long)]
    eval_model: Option<String>,

    /// Run Sheldon claim-validator eval harness and exit
    #[arg(long)]
    sheldon_eval: bool,

    /// Run live validator fixtures (spends API $); scoped fixtures always run
    #[arg(long)]
    sheldon_eval_live: bool,

    /// Run only deterministic skip_scoped fixtures (no API $)
    #[arg(long)]
    sheldon_eval_scoped_only: bool,

    /// Run a single Sheldon fixture by id (e.g. local_no_map, public_fact)
    #[arg(long)]
    sheldon_eval_fixture: Option<String>,

    // ── Server ───────────────────────────────────────────────────
    /// Start WebSocket server (warroom backend replacement)
    #[arg(long)]
    serve: bool,

    /// Server port (default: 8765, matches Python backend)
    #[arg(long, default_value = "8765")]
    port: u16,

    /// Server bind address (default: 127.0.0.1).
    /// Non-loopback (e.g. 0.0.0.0) is refused at startup unless COUNCIL_AUTH_TOKEN
    /// is also set. COUNCIL_DEV_NO_AUTH=1 does not permit non-loopback binds.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Base directory for config (cabinets/, prompts/, models.yaml)
    #[arg(long, default_value = ".")]
    base_dir: PathBuf,
}

fn smoke_default_model(provider: &str) -> Option<&'static str> {
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
        "nvidia" | "nim" => Some("mistralai/mistral-large-3-675b-instruct-2512"),
        "nous" => Some("Hermes-4-70B"),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Provider discovery
    if cli.discover {
        // Discovery performs bounded blocking CLI, TCP, and HTTP probes. Keep
        // reqwest's blocking runtime off the async main thread, matching the
        // War Room `/api/discover` boundary in server.rs.
        let registry = tokio::task::spawn_blocking(ProviderRegistry::discover).await?;
        registry.print_summary();
        return Ok(());
    }

    // Reindex
    if cli.reindex {
        eprintln!("Rebuilding precedent index from session files...");
        let count = precedent::reindex()?;
        eprintln!("✅ Indexed {} sessions", count);
        return Ok(());
    }

    // Gateway routing
    let via_gateway = cli.via_gateway
        || std::env::var("COUNCIL_VIA_GATEWAY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if via_gateway {
        let gw_key = match std::env::var("GW_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("❌ GW_API_KEY not set (required for --via-gateway)");
                std::process::exit(1);
            }
        };
        eprintln!(
            "🔌 Gateway mode: routing all calls through {}",
            std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:18080".into())
        );
        if let Err(e) = provider::gateway::health_check(&gw_key).await {
            eprintln!("❌ Gateway health check failed: {}", e);
            std::process::exit(1);
        }
        eprintln!(
            "  ✅ Gateway health check passed (sensitivity: {})",
            cli.sensitivity
        );
        let verbose = !cli.quiet;
        provider::gateway::init(gw_key, verbose);
        provider::init_gateway(true, cli.sensitivity.clone());
    }

    // Load configuration
    let mut config = Config::load(&cli.base_dir)?;

    // If --cabinet looks like a file path, load it as an external cabinet.
    // The registry key is the file stem, so later lookup must use that key
    // instead of the original path string.
    let mut loaded_cabinet_key: Option<String> = None;
    let cabinet_path = std::path::Path::new(&cli.cabinet);
    if cabinet_path
        .extension()
        .is_some_and(|e| e == "yaml" || e == "yml")
        && cabinet_path.exists()
    {
        let key = config.load_external_cabinet(cabinet_path)?;
        eprintln!(
            "Loaded external cabinet: {} (from {})",
            key,
            cabinet_path.display()
        );
        loaded_cabinet_key = Some(key);
    }

    let config = Arc::new(config);

    // List cabinets
    if cli.list_cabinets {
        eprintln!("\nAvailable cabinets:\n");
        for (name, desc) in config.list_cabinets() {
            let short = desc.lines().next().unwrap_or(desc).trim();
            eprintln!("  {:<12} — {}", name, short);
        }
        eprintln!();
        return Ok(());
    }

    // Weekly drift summary (LaunchAgent / cron)
    if cli.drift_weekly {
        use council_rs::warroom;
        eprintln!(
            "🔄 Running weekly drift summary (window={}d, limit={:?})...",
            cli.drift_window, cli.drift_limit
        );
        if !warroom::drift::acquire_lock() {
            eprintln!("❌ Drift run already in progress");
            std::process::exit(1);
        }
        let summary =
            warroom::drift::run_weekly_summary(&config, cli.drift_window, cli.drift_limit, true)
                .await;
        warroom::drift::release_lock();
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // Meta-review of the self-audit loop
    if cli.meta_review {
        use council_rs::warroom;
        let result = warroom::meta_review::run(Some(&config.tera));
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Utility-role live eval harness
    if cli.judge_eval {
        use council_rs::engine::context::RequestContext;
        use council_rs::engine::judge_eval::{EvalOpts, EvalRole};

        let role = EvalRole::parse(&cli.judge_eval_role)?;
        let (judge_model, judge_provider, frame_model, frame_provider) = match role {
            EvalRole::Judge => (
                cli.eval_model.clone(),
                cli.eval_provider.clone(),
                None,
                None,
            ),
            EvalRole::Frame => (
                None,
                None,
                cli.eval_model.clone(),
                cli.eval_provider.clone(),
            ),
            EvalRole::Both => (
                cli.eval_model.clone(),
                cli.eval_provider.clone(),
                cli.eval_model.clone(),
                cli.eval_provider.clone(),
            ),
        };

        let report = council_rs::engine::judge_eval::run_eval(
            &config,
            EvalOpts {
                role,
                fixture_id: cli.judge_eval_fixture.clone(),
                judge_provider,
                judge_model,
                frame_provider,
                frame_model,
            },
            &RequestContext::default(),
        )
        .await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Sheldon claim-validator eval harness
    if cli.sheldon_eval {
        use council_rs::engine::context::RequestContext;
        use council_rs::engine::sheldon_eval::{SheldonEvalOpts, run_eval};

        let report = run_eval(
            &config,
            SheldonEvalOpts {
                fixture_id: cli.sheldon_eval_fixture.clone(),
                provider: cli.eval_provider.clone(),
                model: cli.eval_model.clone(),
                live: cli.sheldon_eval_live,
                scoped_only: cli.sheldon_eval_scoped_only,
            },
            &RequestContext::default(),
        )
        .await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Start WebSocket server
    if cli.serve {
        let addr = match server::resolve_serve_addr(&cli.host, cli.port) {
            Ok(a) => a,
            Err(msg) => {
                eprintln!("{}", msg);
                std::process::exit(1);
            }
        };
        eprintln!("\n┌─────────────────────────────────────────┐");
        eprintln!("│  🏛️  Council Server starting...          │");
        eprintln!("│  WS:   ws://{}/ws/deliberate  │", addr);
        eprintln!("│  REST: http://{}/api/health    │", addr);
        eprintln!("└─────────────────────────────────────────┘\n");

        let app = server::router(config);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;
        return Ok(());
    }

    // Precedent recall
    if cli.recall {
        let query = cli.topic.as_deref().unwrap_or("");
        if query.is_empty() {
            eprintln!("Usage: council --recall \"search terms\"");
            std::process::exit(1);
        }
        let receipt = precedent::retrieve(query, 20, precedent::RETRIEVE_THRESHOLD, false);
        if receipt.hits.is_empty() {
            eprintln!("No prior sessions match: \"{}\"", query);
        } else {
            eprintln!(
                "\n📚 Precedent search ({}): {} results for \"{}\"\n",
                receipt.engine,
                receipt.hits.len(),
                query
            );
            for (i, hit) in receipt.hits.iter().enumerate() {
                let entry = &hit.entry;
                let date = entry
                    .timestamp
                    .split('T')
                    .next()
                    .unwrap_or(&entry.timestamp);
                eprintln!(
                    "  {}. [{}] {} ({}) — score {:.2} ({})",
                    i + 1,
                    date,
                    entry.topic,
                    entry.cabinet,
                    hit.score,
                    hit.why
                );
                eprintln!("     ID: {}", entry.session_id);
                eprintln!("     {}", entry.digest);
                if !entry.keywords.is_empty() {
                    eprintln!("     keywords: {}", entry.keywords.join(", "));
                }
                eprintln!();
            }
        }
        return Ok(());
    }

    // Topic is required for deliberation and direct-fire
    let topic = match cli.topic {
        Some(t) => t,
        None => {
            eprintln!("Error: <TOPIC> is required for deliberation.");
            eprintln!("Usage: council [OPTIONS] <TOPIC>");
            eprintln!("       council --discover");
            eprintln!("       council --recall \"search terms\"");
            eprintln!("       council --reindex");
            std::process::exit(1);
        }
    };

    // Load context from files (supports - for stdin)
    let mut context = String::new();
    for path in &cli.context {
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

    // Mapmaker — allowlisted scan (same helper as War Room WS map_dir)
    if let Some(ref map_dir) = cli.map {
        match council_rs::warroom::safe_map::gather_map_context_for_deliberation(
            &map_dir.to_string_lossy(),
        ) {
            Ok(map_context) => {
                if !context.is_empty() {
                    context.push_str("\n\n---\n\n");
                }
                context.push_str(&map_context);
            }
            Err(e) => {
                if !cli.quiet {
                    eprintln!("⚠️  --map: {e}");
                }
            }
        }
    }

    // ── Direct-fire modes ──
    // Personas live in engine::direct_fire — shared with the WS direct_fire
    // path and streaming escalations (feature contract).
    if let Some(ref prov) = cli.smoke_provider {
        let provider = prov.trim();
        if provider.is_empty() {
            anyhow::bail!("--smoke-provider requires a provider name (e.g. claude)");
        }
        let model = cli.smoke_model.clone().unwrap_or_else(|| {
            smoke_default_model(provider)
                .unwrap_or_default()
                .to_string()
        });
        if model.is_empty() {
            anyhow::bail!("--smoke-model required for provider '{provider}' (no built-in default)");
        }
        if !cli.quiet {
            eprintln!("\n🔬 provider smoke — {provider}/{model} (no session)");
            // T24: scrub secret shapes from the operator-facing topic echo.
            eprintln!("   Prompt: {}\n", council_rs::scrub::redact(&topic));
        }
        let resp = provider::ask(provider, &topic, "", &model).await;
        if let Some(err) = &resp.error {
            eprintln!("❌ Error: {err}");
            std::process::exit(1);
        }
        let cost = config.models.estimate_cost(
            &resp.model,
            resp.tokens_in,
            resp.tokens_out,
            resp.cached_in,
        );
        if !cli.quiet {
            eprintln!(
                "   ✅ {}ms | model={} | tok {}→{} | ${:.4}\n",
                resp.latency_ms, resp.model, resp.tokens_in, resp.tokens_out, cost
            );
        }
        println!("{}", resp.text);
        return Ok(());
    }

    let direct_fire =
        cli.contrarian || cli.munger || cli.kiss_review || cli.specops || cli.premortem;
    if direct_fire {
        let slug = if cli.premortem {
            "premortem"
        } else if cli.contrarian {
            "contrarian"
        } else if cli.munger {
            "munger"
        } else if cli.kiss_review {
            "kiss"
        } else {
            "specops"
        };
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

        if !cli.quiet {
            eprintln!("\n⚡ {} — direct-fire mode (no council)", spec.display);
            eprintln!("   Provider: {}/{}", spec.provider, spec.model);
            // T24: scrub secret shapes from the operator-facing topic echo.
            eprintln!("   Topic: {}\n", council_rs::scrub::redact(&topic));
        }

        let prompt = direct_fire::build_prompt(&topic, &context);

        let resp = provider::ask(spec.provider, &prompt, spec.system, spec.model).await;
        if let Some(err) = &resp.error {
            eprintln!("❌ Error: {}", err);
            std::process::exit(1);
        }

        let cost = config.models.estimate_cost(
            &resp.model,
            resp.tokens_in,
            resp.tokens_out,
            resp.cached_in,
        );

        if !cli.quiet {
            eprintln!(
                "   Latency: {}ms | Tokens: {}→{} | Cost: ${:.4}\n",
                resp.latency_ms, resp.tokens_in, resp.tokens_out, cost
            );
        }

        println!("{}", resp.text);
        return Ok(());
    }

    // ── Full deliberation ──

    // Determine cabinet from shortcut flags or --cabinet
    let cabinet_override: Option<String> = if cli.wargame {
        Some("wargame".into())
    } else if cli.quick {
        Some("quick".into())
    } else if cli.heritage {
        Some("heritage".into())
    } else if cli.warroom {
        Some("warroom".into())
    } else if cli.reflection {
        Some("reflection".into())
    } else if cli.duo {
        Some("duo".into())
    } else if let Some(ref domain) = cli.triad {
        let valid = [
            "strategy",
            "architecture",
            "debugging",
            "product",
            "risk",
            "shipping",
        ];
        if !valid.contains(&domain.as_str()) {
            anyhow::bail!(
                "Unknown triad domain: '{}'. Valid: {}",
                domain,
                valid.join(", ")
            );
        }
        Some(format!("triad-{}", domain))
    } else {
        None
    };
    let cabinet_name = cabinet_override
        .as_deref()
        .or(loaded_cabinet_key.as_deref())
        .unwrap_or(&cli.cabinet);

    // Determine mode. Precedence: --harden > --pathfind/--then-tear-down > default tear-down.
    // --harden is incompatible with --then-tear-down (the constructive phase IS the review,
    // not a precursor to a kill review).
    if cli.harden && cli.then_tear_down {
        anyhow::bail!("--harden cannot be combined with --then-tear-down; harden IS the review");
    }
    let use_pathfind = cli.pathfind || cli.then_tear_down;
    let mode = if cli.harden {
        Mode::Harden
    } else if use_pathfind {
        Mode::Pathfind
    } else {
        Mode::TearDown
    };

    let cabinet_policy = config.get_cabinet(cabinet_name)?;

    // Frame check: on by default, skip with --no-frame-check, --quick, or
    // local-code-only cabinets where global provider preflights violate policy.
    let do_frame_check = !cli.no_frame_check && !cli.quick && !cabinet_policy.local_code_only;

    // Run deliberation (Phase 1)
    let session = deliberate::run(
        &config,
        cabinet_name,
        &topic,
        &context,
        mode,
        cli.blind,
        do_frame_check,
        !cli.quiet,
        cli.budget,
        &cli.tier,
        cli.validate,
        &cli.validate_provider,
        cli.validate_gate,
    )
    .await?;

    // Print synthesis
    if let Some(synthesis) = &session.synthesis {
        println!("{}", synthesis);
    }

    // Index for precedent engine
    if let Err(e) = precedent::index_session(&session) {
        eprintln!("⚠️  Precedent indexing failed: {}", e);
    }

    // Flight recorder
    match precedent::write_flight_record(&session) {
        Ok(path) => {
            if !cli.quiet {
                eprintln!("📋 Flight record: {}", path);
            }
        }
        Err(e) => eprintln!("⚠️  Flight record failed: {}", e),
    }

    // Phase 2: --then-tear-down
    if cli.then_tear_down && mode == Mode::Pathfind {
        if !cli.quiet {
            eprintln!("\n\n═══════════════════════════════════════════════════════════════");
            eprintln!("  PHASE 2: TEAR-DOWN — Stress-testing the pathfinder's plan");
            eprintln!("═══════════════════════════════════════════════════════════════\n");
        }

        // Use the synthesis as context for the tear-down pass
        let teardown_context = format!(
            "## PATHFINDER OUTPUT TO STRESS-TEST\n\n{}\n\n---\n\n{}",
            session.synthesis.as_deref().unwrap_or(""),
            context
        );

        let teardown_topic = format!(
            "STRESS-TEST the following plan produced by a Pathfinder deliberation on: {}",
            topic
        );

        let session2 = deliberate::run(
            &config,
            cabinet_name,
            &teardown_topic,
            &teardown_context,
            Mode::TearDown,
            cli.blind,
            do_frame_check,
            !cli.quiet,
            cli.budget,
            &cli.tier,
            cli.validate,
            &cli.validate_provider,
            cli.validate_gate,
        )
        .await?;

        if let Some(synthesis) = &session2.synthesis {
            println!("\n---\n## TEAR-DOWN ASSESSMENT\n\n{}", synthesis);
        }

        // Index phase 2 too
        if let Err(e) = precedent::index_session(&session2) {
            eprintln!("⚠️  Phase 2 indexing failed: {}", e);
        }
        if let Ok(path) = precedent::write_flight_record(&session2)
            && !cli.quiet
        {
            eprintln!("📋 Phase 2 flight record: {}", path);
        }
    }

    Ok(())
}
