//! council-rs CLI — same interface as council.py, plus new features
//!
//! council "Topic"                              # default: standard + tear-down
//! council --pathfind "Find a way"              # pathfinder mode
//! council --pathfind --then-tear-down "Topic"   # pathfind, then tear it down
//! council --cabinet warroom --pathfind "Topic"  # warroom + pathfinder
//! council --warroom --harden --validate --map . "Topic"  # shortcut: 5-seat war room cabinet
//! council --smoke-provider claude "ACK ping"   # single provider, no session
//! council --serve                               # start WebSocket server
//! council --serve --web-dist warroom/web/out    # ...plus the War Room UI, same origin
//! council --contrarian "Topic"                  # direct-fire contrarian
//! council --munger "Topic"                      # direct-fire Munger
//! council --kiss-review "Topic"                 # direct-fire KISS
//! council --specops "Topic"                     # direct-fire SpecOps
//! council --recall "search"                     # precedent search
//! council --reindex                             # rebuild precedent index
//! council --blind "Topic"                       # skip precedent injection

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use council_rs::cli::{self, DeliberationCliArgs};
use council_rs::config::Config;
use council_rs::precedent;
use council_rs::provider;
use council_rs::registry::ProviderRegistry;
use council_rs::server;
use council_rs::static_web::WebDist;

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

    /// Serve the built War Room static export from DIR on the same loopback
    /// origin as /api and /ws. Omit it to keep the API-only server.
    #[arg(long, value_name = "DIR")]
    web_dist: Option<PathBuf>,

    /// Base directory for config (cabinets/, prompts/, models.yaml)
    #[arg(long, default_value = ".")]
    base_dir: PathBuf,
}

async fn run_discover() -> Result<()> {
    // Discovery performs bounded blocking CLI, TCP, and HTTP probes. Keep
    // reqwest's blocking runtime off the async main thread, matching the
    // War Room `/api/discover` boundary in server.rs.
    let registry = tokio::task::spawn_blocking(ProviderRegistry::discover).await?;
    registry.print_summary();
    Ok(())
}

fn run_reindex() -> Result<()> {
    eprintln!("Rebuilding precedent index from session files...");
    let count = precedent::reindex()?;
    eprintln!("✅ Indexed {} sessions", count);
    Ok(())
}

fn run_list_cabinets(config: &Config) -> Result<()> {
    eprintln!("\nAvailable cabinets:\n");
    for (name, desc) in config.list_cabinets() {
        let short = desc.lines().next().unwrap_or(desc).trim();
        eprintln!("  {:<12} — {}", name, short);
    }
    eprintln!();
    Ok(())
}

async fn run_weekly_drift(
    config: &Arc<Config>,
    drift_window: u32,
    drift_limit: Option<usize>,
) -> Result<()> {
    use council_rs::warroom;
    eprintln!(
        "🔄 Running weekly drift summary (window={}d, limit={:?})...",
        drift_window, drift_limit
    );
    if !warroom::drift::acquire_lock() {
        eprintln!("❌ Drift run already in progress");
        std::process::exit(1);
    }
    let summary = warroom::drift::run_weekly_summary(config, drift_window, drift_limit, true).await;
    warroom::drift::release_lock();
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn run_meta_review(config: &Config) -> Result<()> {
    use council_rs::warroom;
    let result = warroom::meta_review::run(Some(&config.tera));
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn run_utility_eval(config: &Config, cli: &Cli) -> Result<()> {
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
        config,
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
    Ok(())
}

async fn run_sheldon_eval(config: &Config, cli: &Cli) -> Result<()> {
    use council_rs::engine::context::RequestContext;
    use council_rs::engine::sheldon_eval::{SheldonEvalOpts, run_eval};

    let report = run_eval(
        config,
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
    Ok(())
}

async fn run_ws_server(config: Arc<Config>, cli: &Cli) -> Result<()> {
    let addr = match server::resolve_serve_addr(&cli.host, cli.port) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };
    // Resolve --web-dist before binding so a bad path is a startup error,
    // not a server that answers every route with 404.
    let web_dist = match cli.web_dist.as_deref() {
        Some(dir) => match WebDist::new(dir) {
            Ok(dist) => Some(dist),
            Err(err) => {
                eprintln!("ERROR: --web-dist {}: {}", dir.display(), err);
                std::process::exit(1);
            }
        },
        None => None,
    };

    eprintln!("\n┌─────────────────────────────────────────┐");
    eprintln!("│  🏛️  Council Server starting...          │");
    eprintln!("│  WS:   ws://{}/ws/deliberate  │", addr);
    eprintln!("│  REST: http://{}/api/health    │", addr);
    eprintln!("└─────────────────────────────────────────┘\n");
    if let Some(ref dist) = web_dist {
        eprintln!("🌐 War Room static export: {}", dist.root().display());
        eprintln!("   UI:   http://{}/", addr);
    }

    let app = server::router_with_web_dist(config, web_dist);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn run_recall(cli: &Cli) -> Result<()> {
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
    Ok(())
}

fn deliberation_args_from_cli(cli: Cli) -> DeliberationCliArgs {
    DeliberationCliArgs {
        topic: cli.topic,
        context: cli.context,
        map: cli.map,
        quiet: cli.quiet,
        smoke_provider: cli.smoke_provider,
        smoke_model: cli.smoke_model,
        contrarian: cli.contrarian,
        munger: cli.munger,
        kiss_review: cli.kiss_review,
        specops: cli.specops,
        premortem: cli.premortem,
        wargame: cli.wargame,
        quick: cli.quick,
        heritage: cli.heritage,
        warroom: cli.warroom,
        reflection: cli.reflection,
        duo: cli.duo,
        triad: cli.triad,
        cabinet: cli.cabinet,
        harden: cli.harden,
        pathfind: cli.pathfind,
        then_tear_down: cli.then_tear_down,
        blind: cli.blind,
        no_frame_check: cli.no_frame_check,
        budget: cli.budget,
        tier: cli.tier,
        validate: cli.validate,
        validate_provider: cli.validate_provider,
        validate_gate: cli.validate_gate,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Provider discovery
    if cli.discover {
        return run_discover().await;
    }

    // Reindex
    if cli.reindex {
        return run_reindex();
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
        return run_list_cabinets(&config);
    }

    // Weekly drift summary (LaunchAgent / cron)
    if cli.drift_weekly {
        return run_weekly_drift(&config, cli.drift_window, cli.drift_limit).await;
    }

    // Meta-review of the self-audit loop
    if cli.meta_review {
        return run_meta_review(&config);
    }

    // Utility-role live eval harness
    if cli.judge_eval {
        return run_utility_eval(&config, &cli).await;
    }

    // Sheldon claim-validator eval harness
    if cli.sheldon_eval {
        return run_sheldon_eval(&config, &cli).await;
    }

    // Start WebSocket server
    if cli.serve {
        return run_ws_server(config, &cli).await;
    }

    // Precedent recall
    if cli.recall {
        return run_recall(&cli);
    }

    // Topic-required check, context load, direct-fire, full deliberation
    cli::run_deliberation_cli(
        deliberation_args_from_cli(cli),
        config,
        via_gateway,
        loaded_cabinet_key,
    )
    .await
}
