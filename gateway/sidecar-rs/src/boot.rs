// ==========================================================================
// boot — sidecar startup stages.
// ==========================================================================

use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

use crate::keymgmt::DirectiveSigningKey;
use crate::watch::dispatcher::{
    live_dispatcher_config_from_env, run_boot_hydration_sweep, should_spawn_live_dispatcher,
    ReqwestCouncilClient,
};
use crate::watch::startup_probe::{
    probe_phase3_dispatcher_activation, Phase3DispatcherActivation, ReqwestTriageProbeClient,
};
use crate::watch::worker::{
    live_worker_config_from_env, should_spawn_live_worker, spawn_live_worker_loop,
};
use crate::{
    auth, budget, cache, council, council_storage, decontaminator, ledger, load_ledger_signing_key,
    load_old_ledger_key, load_root_pubkey, policy, router, routes, socket, sovereignty_gate,
    unified_config, vertex_auth, watch, AppState,
};

/// Structured JSON logging + optional OTEL. Returns provider for shutdown flush.
pub(crate) fn init_telemetry() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // Provider handle kept for explicit flush+shutdown at exit — 0.31+ removed
    // global::shutdown_tracer_provider().
    let mut otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider> = None;
    let otel_layer = match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(_) => {
            use opentelemetry_otlp::WithExportConfig;
            match opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
                .build()
            {
                Ok(exporter) => {
                    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                        .with_batch_exporter(exporter)
                        .build();
                    use opentelemetry::trace::TracerProvider as _;
                    let tracer = provider.tracer("gateway-sidecar");
                    otel_provider = Some(provider);
                    Some(tracing_opentelemetry::layer().with_tracer(tracer))
                }
                Err(e) => {
                    eprintln!("OTEL exporter init failed (non-fatal): {e}");
                    None
                }
            }
        }
        Err(_) => None,
    };

    use tracing_subscriber::prelude::*;
    let fmt_layer = tracing_subscriber::fmt::layer().json().with_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    );

    let subscriber = tracing_subscriber::registry().with(fmt_layer);

    if let Some(otel) = otel_layer {
        subscriber.with(otel).init();
    } else {
        subscriber.init();
    }

    otel_provider
}

/// Phase 1 product: models/router configuration resolved from env/YAML.
struct BootConfig {
    redis_url: Option<String>,
    smart_router: router::SmartRouter,
}

/// Phase 2 product: ledger keys, durable cache/budget, auth, provider tokens.
struct BootAuthority {
    smart_router: router::SmartRouter,
    audit_ledger: ledger::AuditLedger,
    gw_cache: cache::GatewayCache,
    budget_enforcer: budget::BudgetEnforcer,
    auth_service: auth::AuthService,
    vertex_token: vertex_auth::VertexTokenProvider,
    ledger_sk: ed25519_dalek::SigningKey,
    root_pubkey: Option<ed25519_dalek::VerifyingKey>,
}

/// Phase 3 product: watch DBs hydrated, AppState built, background sweeper live.
struct BootHydrated {
    state: Arc<AppState>,
    watch_db: Arc<watch::db::WatchDb>,
    watch_quarantine: Arc<watch::quarantine::QuarantineState>,
    watch_runtime: tokio::runtime::Runtime,
    sentinels: Vec<Arc<dyn watch::Sentinel>>,
    arm_principals: Arc<watch::api::ArmPrincipals>,
    arm_stage_ttl: Duration,
    arm_notifier: Arc<watch::api::ArmNotifier>,
    arm_deviation: Arc<watch::api::ArmDeviationTags>,
    attest_keys: Arc<watch::attest::AttestKeyRegistry>,
    watch_admin_token: String,
}

/// Phase 4 product: UDS serving, watch runner, optional dispatcher/worker.
struct BootServing {
    server_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    watch_runner_handles: watch::runner::WatchRunnerHandles,
    _watch_runtime_keepalive: tokio::runtime::Runtime,
    _dispatcher_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    _worker_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Phase 1 — configuration load (unified YAML, models, smart router).
fn load_configuration() -> BootConfig {
    let redis_url = std::env::var("REDIS_URL").ok();

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // 1. Unified Configuration (Phase 0) — opt-in via GATEWAY_CONFIG_PATH.
    let unified_cfg: Option<unified_config::UnifiedConfig> =
        match unified_config::UnifiedConfig::configured_path() {
            Some(path) => match unified_config::UnifiedConfig::from_path(&path) {
                Ok(c) => Some(c),
                Err(e) => panic!("FATAL: GATEWAY_CONFIG_PATH set but failed to load: {}", e),
            },
            None => None,
        };
    unified_config::log_section_sources(&unified_cfg);

    if let Some(cfg) = &unified_cfg {
        match cfg.materialize_lua_derived() {
            Ok(dir) => {
                info!(dir = %dir.display(), "unified_config: derived JSON ready for Lua side")
            }
            Err(e) => warn!("unified_config: failed to materialize derived JSON: {}", e),
        }
    }

    // Load models — YAML section takes priority over MODELS_JSON_PATH
    let models_json = if let Some(v) = unified_cfg.as_ref().and_then(|c| c.models.clone()) {
        info!("models: sourced from unified YAML config");
        v
    } else {
        match std::env::var("MODELS_JSON_PATH") {
            Ok(path) => {
                let content = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("failed to read {}: {}", path, e));
                serde_json::from_str(&content)
                    .unwrap_or_else(|e| panic!("failed to parse {}: {}", path, e))
            }
            Err(_) => {
                warn!("MODELS_JSON_PATH not set — using empty model registry");
                serde_json::json!({"models": []})
            }
        }
    };

    let smart_router = router::SmartRouter::from_models_json(&models_json)
        .expect("failed to initialize smart router");

    BootConfig {
        redis_url,
        smart_router,
    }
}

/// Phase 2 — authority initialization (ledger keys, durable state, auth, tokens).
async fn initialize_authority(config: BootConfig) -> BootAuthority {
    let BootConfig {
        redis_url,
        smart_router,
    } = config;

    // Initialize Cryptographic Ledger with persistent Ed25519 signing key.
    // Fails closed (panics) if the key file is missing, wrong size, or has
    // wrong permissions. See load_ledger_signing_key() for details.
    // Ceremony root is loaded first so present-but-invalid ROOT_PUBKEY_HEX
    // refuses boot before the ledger opens.
    let ledger_path = std::env::var("LEDGER_DB_PATH").unwrap_or_else(|_| "ledger.db".to_string());
    let signing_key_bytes = load_ledger_signing_key();
    let old_key_bytes = load_old_ledger_key();
    let root_pubkey =
        load_root_pubkey().expect("FATAL: ROOT_PUBKEY_HEX is set but invalid (refuse boot)");
    let audit_ledger = ledger::AuditLedger::new(
        &ledger_path,
        Some(&signing_key_bytes),
        old_key_bytes.as_deref(),
        root_pubkey, // VerifyingKey is Copy; same value stays in BootAuthority
    )
    .await
    .expect("FATAL: failed to initialize audit ledger");

    let durable = std::env::var("GATEWAY_DURABLE")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false);
    let state_db_path = std::env::var("GATEWAY_STATE_DB_PATH")
        .unwrap_or_else(|_| "/var/lib/sidecar/gateway.db".to_string());

    let mut gw_cache = cache::GatewayCache::new(redis_url.clone());
    let mut budget_enforcer =
        budget::BudgetEnforcer::new(budget::BudgetConfig::default(), redis_url.as_deref());

    if durable {
        info!(db = %state_db_path, "durable state enabled (SQLite WAL)");

        let sqlite_cache = cache::SqliteCache::new(&state_db_path)
            .await
            .expect("FATAL: failed to initialize SQLite cache");
        gw_cache = gw_cache.with_sqlite(std::sync::Arc::new(sqlite_cache));

        let budget_conn = tokio_rusqlite::Connection::open(&state_db_path)
            .await
            .expect("FATAL: failed to open SQLite for budget");
        budget_conn
            .call(|c| {
                c.execute_batch(
                    "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA busy_timeout=5000;
                 CREATE TABLE IF NOT EXISTS budget_state (
                     key TEXT PRIMARY KEY,
                     spent_usd REAL NOT NULL DEFAULT 0.0,
                     request_count INTEGER NOT NULL DEFAULT 0,
                     updated_at INTEGER NOT NULL
                 );",
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await
            .expect("FATAL: failed to initialize budget schema");
        budget_enforcer = budget_enforcer.with_sqlite(budget_conn);
    } else {
        info!("durable state disabled (in-memory only). Set GATEWAY_DURABLE=1 to persist.");
    }

    let auth_config_path = std::env::var("AUTH_CONFIG_PATH")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| Some(std::path::PathBuf::from("conf/auth_keys.json")));

    let auth_service = auth::AuthService::new(auth_config_path);

    let vertex_token = vertex_auth::VertexTokenProvider::new().await;

    let mut sk_bytes = [0u8; 32];
    sk_bytes.copy_from_slice(&signing_key_bytes);
    let ledger_sk = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);

    // Spec P1 #14: in-memory idempotency means replays initiated before this
    // PID started cannot be observed. Surface that explicitly at boot rather
    // than discovering it during a billing-reconciliation investigation.
    warn!(
        "council idempotency: in-memory only — replays before this PID may bill twice. \
         SQLite-backed in v0.1.1."
    );

    BootAuthority {
        smart_router,
        audit_ledger,
        gw_cache,
        budget_enforcer,
        auth_service,
        vertex_token,
        ledger_sk,
        root_pubkey,
    }
}

/// Phase 3 — state hydration (watch.db, sentinels, arm registry, AppState).
async fn hydrate_runtime_state(authority: BootAuthority) -> anyhow::Result<BootHydrated> {
    let BootAuthority {
        smart_router,
        audit_ledger,
        gw_cache,
        budget_enforcer,
        auth_service,
        vertex_token,
        ledger_sk,
        root_pubkey,
    } = authority;

    // Phase 2 §4 — open the append-only watch.db (hash-chained per tenant).
    // Fatal at boot if it can't open; the chain MUST persist so verify-chain
    // (T31) and the upcoming list/audit endpoints have something to walk.
    let watch_db_path = std::env::var("WATCH_DB_PATH").unwrap_or_else(|_| "watch.db".to_string());
    let watch_db = std::sync::Arc::new(
        watch::db::WatchDb::open(std::path::Path::new(&watch_db_path))
            .await
            .expect("FATAL: failed to open watch.db (Phase 2 §4)"),
    );
    watch_db
        .run_migrations()
        .await
        .expect("FATAL: watch.db migration failed");
    info!(path = %watch_db_path, "watch.db: opened (hash-chained fire log online)");

    // Phase 2 §8 — dedicated watch_runtime (2 workers + 8 blocking threads),
    // isolated from this main runtime's hot path. Holds for process lifetime;
    // dropping it would stop all sentinel loops.
    let watch_runtime = watch::runtime::build_watch_runtime();
    let watch_quarantine = std::sync::Arc::new(watch::quarantine::QuarantineState::new_with_db(
        watch::quarantine::QuarantineConfig::default(),
        watch_db.clone(),
    ));

    // dual-custody-local-attest B2 (spec §5, challenge-format invariant): boot
    // self-test of the pinned challenge format vector — serialization drift
    // fails HERE, loudly, never at arm time.
    watch::attest::challenge_format_self_test()
        .expect("FATAL: arm-confirm challenge format self-test failed (serialization drift — challenge-format invariant)");

    // dual-custody-local-attest B1 (spec §4.3, restart-recovery invariant): rehydrate
    // a persisted unexpired pending arm stage — a sidecar restart
    // mid-ceremony no longer drops the stage (bin/arm resumes via
    // GET /arm/pending). Expired rows are never rehydrated; armed state
    // itself still does not persist (env-gate unchanged, fail-closed).
    let _rehydrated = watch_quarantine.rehydrate_arm_pending().await;

    // Load sentinels.yaml:
    //   - SENTINELS_CONFIG_PATH explicitly set (non-empty) → file must exist
    //     and parse; any failure is fatal (tracing::error! + exit(1)).
    //   - Unset OR empty string → fall back to /etc/gateway/sentinels.yaml;
    //     if absent, boot with an empty Vec (runtime healthy, 0 sentinels).
    // Empty-string-as-unset is load-bearing for pack compose:
    // SENTINELS_CONFIG_PATH=${IRIN_WATCH_PROFILE_PATH:-} expands to "" when
    // no profile is installed; treating that as explicit would brick boot.
    let policy = sentinels_config_policy(std::env::var("SENTINELS_CONFIG_PATH").ok().as_deref());
    let disposition = sentinels_boot_disposition(
        &policy,
        std::path::Path::new(match &policy {
            SentinelsConfigPolicy::Require { path }
            | SentinelsConfigPolicy::OptionalDefault { path } => path.as_str(),
        })
        .exists(),
    );
    let loaded_sentinels: Vec<watch::registry::LoadedSentinel> = match disposition {
        SentinelsBootDisposition::FatalMissing { path } => {
            tracing::error!(
                "SENTINELS_CONFIG_PATH={} but the file does not exist. \
                 Set the variable to an existing yaml or unset it for default lookup.",
                path
            );
            std::process::exit(1);
        }
        SentinelsBootDisposition::EmptyWarn { path } => {
            tracing::warn!(
                "no sentinels.yaml at {} — WatchRunner starting with 0 sentinels",
                path
            );
            Vec::new()
        }
        SentinelsBootDisposition::Load { path } => {
            let p = std::path::Path::new(&path);
            match watch::registry::SentinelRegistry::load_from_yaml(p) {
                Ok(v) => {
                    info!(
                        "sentinels.yaml: loaded {} sentinel(s) from {}",
                        v.len(),
                        p.display()
                    );
                    v
                }
                Err(e) => {
                    tracing::error!(
                        "sentinels.yaml at {} failed to load: {:#}. \
                         Cold-boot Phase 2 cannot start with an invalid sentinel config.",
                        p.display(),
                        e
                    );
                    std::process::exit(1);
                }
            }
        }
    };

    // T27 — boot-time registry upsert: write each loaded sentinel into
    // watch_sentinels so `/watch/list/{tenant}` has something to return.
    // ON CONFLICT preserves hard_killed_at / probation_until / enabled;
    // sync_registration_enabled then sets enabled from the loaded profile
    // so Watch Off / rename cannot leave ready-looking leftover rows.
    let mut sentinels: Vec<std::sync::Arc<dyn watch::Sentinel>> =
        Vec::with_capacity(loaded_sentinels.len());
    let mut force_wake_map: std::collections::HashMap<
        (String, String),
        std::sync::Arc<dyn watch::Sentinel>,
    > = std::collections::HashMap::with_capacity(loaded_sentinels.len());
    for loaded in loaded_sentinels {
        let s = &loaded.sentinel;
        let tier_str = match s.tier() {
            watch::Tier::Fast => "fast",
            watch::Tier::Polling => "polling",
            watch::Tier::Deep => "deep",
        };
        let cooldown_ms = s.cooldown().as_millis() as i64;
        let config_json_str =
            serde_json::to_string(&loaded.config_json).unwrap_or_else(|_| "{}".to_string());
        if let Err(e) = watch_db
            .upsert_sentinel_registration(
                s.tenant(),
                s.name(),
                tier_str,
                cooldown_ms,
                &config_json_str,
            )
            .await
        {
            tracing::error!(
                "watch_sentinels upsert failed for {}/{}: {:#}",
                s.tenant(),
                s.name(),
                e
            );
            std::process::exit(1);
        }
        // T30 — index by (tenant, name) for force-wake lookup. Both the
        // runner (Vec) and the registry (HashMap) share the same Arc, so
        // there is one set of sentinel instances behind two views.
        force_wake_map.insert(
            (s.tenant().to_string(), s.name().to_string()),
            loaded.sentinel.clone(),
        );
        sentinels.push(loaded.sentinel);
    }
    let loaded_keys: Vec<(String, String)> = sentinels
        .iter()
        .map(|s| (s.tenant().to_string(), s.name().to_string()))
        .collect();
    if let Err(e) = watch_db.sync_registration_enabled(&loaded_keys).await {
        tracing::error!("watch_sentinels enabled-sync failed: {:#}", e);
        std::process::exit(1);
    }
    let watch_registry: watch::api::ForceWakeRegistry = std::sync::Arc::new(force_wake_map);

    // T33.7 P1-5 — hydrate active probation windows from watch.db into the
    // in-memory QuarantineState. Without this, a sidecar restart during the
    // 10-min log-only window post-admin-clear silently drops the
    // [PROBATION] reason prefix on every scheduled fire — audit rows for a
    // recovering sentinel mix with normal traffic until the wall-clock
    // window expires. Hard-killed rows are skipped intentionally; the OCC
    // in insert_fire owns hard-kill gating across restart.
    match watch_quarantine.hydrate_probation_from_db().await {
        Ok(0) => {
            info!("watch.db: hydrate_probation_from_db — 0 active probation rows");
        }
        Ok(n) => {
            info!(
                hydrated = n,
                "watch.db: hydrate_probation_from_db — restored {} active probation row(s) into QuarantineState",
                n
            );
        }
        Err(e) => {
            // Non-fatal: a hydrate miss only suppresses the [PROBATION]
            // prefix for the residual window. Logged loud; do not exit.
            tracing::warn!(error = %e, "watch.db: hydrate_probation_from_db failed; scheduled fires during residual probation windows will NOT carry the [PROBATION] prefix until the wall-clock deadline expires");
        }
    }

    // T33.P0-B (review) — hydrate durable hard-kills from
    // watch_sentinels.hard_killed_at into the in-memory QuarantineState
    // BEFORE the runner spawns. Without this, post-restart `is_blocked`
    // returns None for known-bad sentinels; runner_loop drives
    // observe/interesting/escalate and only the OCC in `insert_fire`
    // rejects the write — gate and OCC layers disagree.
    //
    // Bifurcated hydration policy (council durability invariant): hard-kill hydrate is
    // fail-closed. The wall-line "Action is final" hinges on the hard-kill
    // ladder surviving restart; a hydrate failure here propagates via `?`
    // and blocks runner spawn (process exits 1 after tokio::main unwinds).
    // Probation hydrate stays log-and-continue (above).
    match watch_quarantine.hydrate_hard_kill_from_db().await {
        Ok(0) => {
            info!("watch.db: hydrate_hard_kill_from_db — 0 active hard-killed rows");
        }
        Ok(n) => {
            info!(
                hydrated = n,
                "watch.db: hydrate_hard_kill_from_db — restored {} hard-killed row(s) into QuarantineState",
                n
            );
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "watch.db: hydrate_hard_kill_from_db failed ({e}); refusing to spawn runner — hard-kill safety rail cannot silently degrade across restart"
            ));
        }
    }

    // T30 — admin token: WATCH_ADMIN_TOKEN takes precedence; fall back to
    // BOOTSTRAP_TOKEN for ops continuity (matches /admin/keys bootstrap).
    // Empty → force-wake fails closed (constant-time compare rejects).
    let watch_admin_token = std::env::var("WATCH_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("BOOTSTRAP_TOKEN").ok())
        .unwrap_or_default();
    if watch_admin_token.is_empty() {
        warn!(
            "watch force-wake: no WATCH_ADMIN_TOKEN or BOOTSTRAP_TOKEN set — all requests will 401"
        );
    }

    // Wave-1 single-tenant tripwire: resolve the configured canary tenant ONCE
    // at boot (WATCH_CANARY_TENANT, default "sovereign"). Stored on AppState so
    // the outbox guard does not re-read env per request. Default-preserving:
    // UNSET keeps the historical hard-coded "sovereign". An EXPLICIT-but-malformed
    // var (empty/whitespace/non-unicode) ABORTS boot — mirrors the empty-token
    // fail-closed precedent above; a deploy in that state would 403 every outbox
    // request, so failing loud at boot is strictly better than failing silently
    // (W1 re-gate P0, review).
    let watch_canary_tenant =
        watch::api::resolve_canary_tenant().expect("WATCH_CANARY_TENANT set but empty/invalid");
    if watch_canary_tenant != watch::api::CANARY_TENANT_DEFAULT {
        warn!(
            canary_tenant = %watch_canary_tenant,
            "watch outbox: single-tenant tripwire pinned to a NON-DEFAULT tenant via WATCH_CANARY_TENANT \
             (expected only in CI/phase-3-smoke; local canary should be 'sovereign')"
        );
    }

    // p0a-four-eyes (the dual-custody invariant) — arming principal registry.
    // GW_ARM_PRINCIPALS='alice:tok_aaaa,bob:tok_bbbb'. Fewer than 2 distinct
    // principals → arm-capable mode is refused at the stage/confirm handlers
    // (fail-closed; a four-eyes gate with one principal is theater). The
    // process still boots — arming is default-OFF and /disarm must stay
    // reachable regardless.
    let arm_principals = std::sync::Arc::new(watch::api::ArmPrincipals::from_env());
    if !arm_principals.is_arm_capable() {
        warn!(
            "watch arm: GW_ARM_PRINCIPALS has fewer than 2 distinct principals — \
             arm-capable mode disabled (stage/confirm fail closed); disarm unaffected"
        );
    }
    let arm_stage_ttl = watch::api::arm_stage_ttl();

    // Dual-custody single-operator riders (the invariant):
    // RIDER C — out-of-band ceremony alerting (ARM_NOTIFY_URL; warns when off).
    // RIDER D — deviation/domain tags (GW_ARM_DEVIATION_FLAG + GW_ARM_PRINCIPAL_DOMAINS).
    let arm_notifier = std::sync::Arc::new(watch::api::ArmNotifier::from_env());
    let arm_deviation = std::sync::Arc::new(watch::api::ArmDeviationTags::from_env());

    // dual-custody-local-attest B3 (spec §7.2): boot-ONLY load of the
    // enrolled-credential registry (GW_ARM_ATTEST_KEYS_PATH), fail-closed
    // like ArmPrincipals — any violation unloads the registry and confirm
    // rejects (`registry_unloaded`). The keyset hash is published for the
    // boot_env_arm audit row and announced over ntfy at EVERY boot: an
    // unexplained keyset change is the alarm (keyset-change detection invariant
    // — detection, not file modes).
    let attest_keys = std::sync::Arc::new(watch::attest::AttestKeyRegistry::from_env());
    watch::attest::publish_boot_keyset_hash(&attest_keys);
    // Publish the boot registry so the reserve can
    // re-verify a persisted arm's ES256 signature at spend time (the SQLite
    // thread has no handle to this Arc otherwise).
    watch::attest::publish_boot_registry(attest_keys.clone());
    arm_notifier.notify(
        "boot_keyset",
        "boot",
        &format!(
            "keyset_hash={} credentials={}",
            watch::attest::boot_keyset_hash(),
            attest_keys.len()
        ),
    );

    // dual-custody-local-attest B6 (spec §9): the OTC mechanism is RETIRED —
    // codes are never loaded, the arm_otc table is archived in place (rows
    // are history, never read or written). A leftover env var is stale
    // config worth a loud warning; the '@otc' principal guard in
    // ArmPrincipals::parse fail-closes the registry itself.
    if std::env::var("GW_ARM_OTC_HASHES_PATH").is_ok_and(|p| !p.trim().is_empty()) {
        warn!(
            "GW_ARM_OTC_HASHES_PATH is set but OTC is RETIRED (dual-custody-local-attest §9) — ignored; remove it from the environment"
        );
    }

    // single-writer (single-writer invariant) — this process's writer identity, logged
    // once at boot so operators can match the writer_claim row in watch.db
    // to a concrete sidecar instance during an incident. Claim acquisition
    // itself happens at arm/producer-spawn time (refuse-to-arm on a second
    // writer); single-writer assumes a single SHARED watch.db (see
    // docs/runbooks/arming-authorization.md).
    info!(
        instance_uuid = watch::db::process_instance_uuid(),
        stale_ms = watch::db::writer_claim_stale_ms(),
        heartbeat_ms = watch::db::writer_claim_heartbeat_ms(),
        "watch single-writer identity"
    );

    let state = Arc::new(AppState {
        decon: decontaminator::InputDecontaminator::default(),
        cache: gw_cache,
        router: smart_router,
        budget: budget_enforcer,
        policy: policy::PolicyFirewall::new(policy::PolicyConfig::default()),
        sovereignty: sovereignty_gate::SovereigntyGate::default(),
        ledger: audit_ledger,
        ledger_signing_key: ledger_sk,
        root_pubkey,
        auth: auth_service,
        vertex_token,
        council: {
            // Phase 2 §7 — write-ahead durable mirror for council idempotency.
            // The mirror MUST be open before the HTTP handlers can serve
            // /council/idempotency/claim, because the handler returns 503
            // when the mirror is unavailable. A boot failure here is fatal.
            let council_idem_path = std::env::var("COUNCIL_IDEM_DB_PATH")
                .unwrap_or_else(|_| "council_idem.db".to_string());
            // D5 durability: in container (docker-compose.yml) this is pinned to
            // /var/lib/sidecar/council_idem.db via the sidecar_data volume (mirrors
            // WATCH_DB_PATH). Local binary dev uses relative default. The write-ahead
            // mirror + get_stored_row read-through must survive restarts.
            let db = council_storage::CouncilIdemDb::open(std::path::Path::new(&council_idem_path))
                .await
                .expect("FATAL: failed to open council_idem.db (P0-2 write-ahead mirror)");
            db.run_migrations()
                .await
                .expect("FATAL: council_idem.db migration failed");
            let recovery = db
                .recover_on_startup()
                .await
                .expect("FATAL: council_idem.db startup recovery failed");
            // Load surviving Stored rows before moving `db` into
            // the state, then rehydrate the in-memory LRU so causal-keyed
            // council dedup survives a sidecar restart (no re-deliberate /
            // re-bill on replay). Without this the LRU boots EMPTY and the
            // durable mirror would be write-only.
            let stored_rows = db
                .load_stored_rows()
                .await
                .expect("FATAL: council_idem.db load of Stored rows failed");
            let council_state = council::CouncilState::with_db(std::sync::Arc::new(db));
            let rehydrated = council_state.rehydrate_stored(stored_rows);
            info!(
                loaded_stored = recovery.loaded_stored,
                rehydrated = rehydrated.rehydrated,
                skipped_expired = rehydrated.skipped_expired,
                skipped_malformed = rehydrated.skipped_malformed,
                dropped_pending = recovery.dropped_pending,
                stale_grants = recovery.stale_grants,
                path = %council_idem_path,
                "council_idem: write-ahead durable mirror open; Stored LRU rehydrated from durable mirror"
            );
            if rehydrated.rehydrated != recovery.loaded_stored {
                // Drift = rows lost to TTL race, malformed JSON, or LRU
                // overflow (> IDEM_CAPACITY). Observable, not silent.
                tracing::warn!(
                    loaded_stored = recovery.loaded_stored,
                    rehydrated = rehydrated.rehydrated,
                    skipped_expired = rehydrated.skipped_expired,
                    skipped_malformed = rehydrated.skipped_malformed,
                    "council_idem: rehydrated count differs from recovered Stored count"
                );
            }
            if recovery.loaded_stored > council::IDEM_CAPACITY {
                // D5 read-through now active (council_idem_peek + get_stored_row
                // durable fallback). First re-observation of a cold Stored row
                // will hit SQLite, warm the LRU, and prevent re-bill. The cap
                // can still cause LRU thrashing / extra DB hits on very high
                // cardinality tenants; this WARN is now an observability signal
                // for that pressure rather than an admission of a re-bill window.
                tracing::warn!(
                    loaded_stored = recovery.loaded_stored,
                    idem_capacity = council::IDEM_CAPACITY,
                    cold_tail = recovery.loaded_stored.saturating_sub(council::IDEM_CAPACITY),
                    "council_idem: durable Stored rows exceed in-memory LRU cap — read-through active on peek; cold tail only increases DB fallback rate until re-observed"
                );
            }
            council_state
        },
        watch_db: watch_db.clone(),
        watch_registry: watch_registry.clone(),
        watch_quarantine: watch_quarantine.clone(),
        watch_admin_token: watch_admin_token.clone(),
        watch_canary_tenant: watch_canary_tenant.clone(),
        librarian_base_url: std::env::var("LIBRARIAN_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11435".to_string()),
    });

    // P0-4: self-healing sweeper for leaked council concurrency slots.
    // Runs every 30s; reclaims any granted_at older than PENDING_TTL + 30s.
    // Spawned ONCE at startup; cancelled when the process exits.
    council::spawn_active_sweeper(state.clone());

    Ok(BootHydrated {
        state,
        watch_db,
        watch_quarantine,
        watch_runtime,
        sentinels,
        arm_principals,
        arm_stage_ttl,
        arm_notifier,
        arm_deviation,
        attest_keys,
        watch_admin_token,
    })
}

/// Phase 4 — listener startup (router, UDS, probe, hydration sweep, dispatcher).
async fn start_listener_and_background(hydrated: BootHydrated) -> BootServing {
    let BootHydrated {
        state,
        watch_db,
        watch_quarantine,
        watch_runtime,
        sentinels,
        arm_principals,
        arm_stage_ttl,
        arm_notifier,
        arm_deviation,
        attest_keys,
        watch_admin_token,
    } = hydrated;

    let app = routes::build_router(routes::BuildRouterParts {
        state: state.clone(),
        watch_quarantine: watch_quarantine.clone(),
        arm_principals: arm_principals.clone(),
        arm_stage_ttl,
        watch_admin_token: watch_admin_token.clone(),
        arm_notifier: arm_notifier.clone(),
        arm_deviation: arm_deviation.clone(),
        attest_keys: attest_keys.clone(),
    });

    // DirectiveSigningKey load + publish (yields HydrationToken for later sweep).
    // Placed after router construction but before UDS bind; the token is consumed
    // only after router is serving (see boot step 4.5 below).
    let directive_identity_path: std::path::PathBuf = std::env::var("DIRECTIVE_IDENTITY_PATH")
        .map(Into::into)
        .unwrap_or_else(|_| "/var/lib/sidecar/directive_identity.json".into());
    let (directive_key, hydration_token) = match DirectiveSigningKey::load_or_initialize(
        &directive_identity_path,
        &watch_db,
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "FATAL: DirectiveSigningKey::load_or_initialize failed");
            std::process::exit(1);
        }
    };

    let socket_path = std::env::var("SIDECAR_SOCKET_PATH")
        .unwrap_or_else(|_| "/tmp/gateway-sidecar.sock".to_string());

    // Remove existing socket file if it exists
    if std::path::Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path).expect("failed to remove existing socket file");
    }

    info!(socket_path, "gateway-sidecar starting on UDS");

    let listener = UnixListener::bind(&socket_path).expect("failed to bind to UDS");

    // Lock down the management UDS before exposing administrative routes.
    //
    // SECURITY NOTE (honest attacker model, post-tightening): the arm/admin
    // routes on this UDS are NOT fronted by nginx auth (nginx.conf has no
    // /watch/admin/ location), so the file mode is the FIRST and (for non-arm
    // callers) ONLY isolation boundary against other local processes. The
    // tightened default is now 0o660 (owner+group rw, WORLD NONE) — the prior
    // 0o777 (world-rwx) gave NO isolation and is gone.
    //   - Host mode (nginx + sidecar same uid, the developer Mac): owner bit
    //     suffices; the group bits are inert. Override SIDECAR_SOCKET_MODE=0600
    //     for a strict same-uid lockdown.
    //   - Compose mode (nginx worker uid != sidecar uid): set
    //     SIDECAR_SOCKET_GID to the nginx worker's gid so the socket is
    //     chowned to root:<nginx-gid> with mode 0o660 — group grants connect,
    //     world is denied. See docker-compose.yml for the wiring.
    // Residual risk: any process running as the socket owner OR in the
    // configured group can still reach the full management surface; the arm
    // ceremony's defense-in-depth is the GW_ARM_PRINCIPALS bearer + four-eyes
    // split (see watch/api.rs §MANAGEMENT-SURFACE / HONEST ATTACKER MODEL).
    // The mode reduces the blast radius from "any local process" to
    // "owner + configured group" — it does NOT replace the bearer/four-eyes.
    //
    // FAIL CLOSED: a malformed SIDECAR_SOCKET_MODE / SIDECAR_SOCKET_GID refuses
    // startup (never a fallback to a looser mode). See socket.rs.
    let socket_mode = match socket::socket_mode_from_env(
        std::env::var(socket::SIDECAR_SOCKET_MODE_VAR)
            .ok()
            .as_deref(),
    ) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "FATAL: invalid socket mode config");
            std::process::exit(1);
        }
    };
    let socket_gid = match socket::socket_gid_from_env(
        std::env::var(socket::SIDECAR_SOCKET_GID_VAR)
            .ok()
            .as_deref(),
    ) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(error = %e, "FATAL: invalid socket gid config");
            std::process::exit(1);
        }
    };
    if let Err(e) =
        socket::apply_socket_perms(std::path::Path::new(&socket_path), socket_mode, socket_gid)
    {
        tracing::error!(
            error = %e,
            mode = format_args!("{socket_mode:#o}"),
            gid = ?socket_gid,
            "FATAL: failed to lock down socket permissions"
        );
        std::process::exit(1);
    }
    info!(
        socket_path,
        mode = format_args!("{socket_mode:#o}"),
        gid = ?socket_gid,
        "management UDS locked down"
    );

    // P0-1 (Review): fail-closed day-cap resolve at boot. Env may
    // only LOWER DAILY_SPEND_CAP; garbage/above-ceiling refuses startup.
    if let Err(e) = watch::db::init_daily_spend_cap_at_boot(
        std::env::var(watch::db::DAILY_SPEND_CAP_ENV_VAR)
            .ok()
            .as_deref(),
    ) {
        tracing::error!(error = %e, "FATAL: invalid daily spend cap config");
        std::process::exit(1);
    }
    info!(
        daily_spend_cap_usd = watch::db::daily_spend_cap(),
        env_var = watch::db::DAILY_SPEND_CAP_ENV_VAR,
        "watch UTC-day spend cap resolved at boot"
    );

    // Attested-arm (HIGH spend-window split-brain): resolve the attested-arm
    // SPEND-WINDOW ONCE at boot. A live env read on the spend path would let a
    // box-owning attacker set GW_ARM_WINDOW_MS=<huge> and extend a live window
    // indefinitely — the same bypass class as the removed GW_REQUIRE_ATTESTED_ARM
    // flag. Boot-locking it means changing the window needs a restart.
    watch::db::init_arm_window_ms_at_boot();
    // Attested-arm (invariant): resolve the named rollback flag
    // GW_ARM_SIGNED_WINDOW once (default-on). When on, the spend gate reads the
    // SIGNED window so a post-tap GW_ARM_WINDOW_MS restart cannot extend a
    // genuine tap's horizon; =false reverts to the boot-locked window WITHOUT a
    // redeploy (the rollback for a JCS/signing regression).
    watch::db::init_signed_spend_window_at_boot();
    info!(
        arm_window_ms = watch::db::arm_window_ms_bootlocked(),
        signed_spend_window = watch::db::signed_spend_window_enabled(),
        "attested-arm spend window resolved at boot (measured from signed iat; restart to change)"
    );

    // Attested-arm: attested-arm enforcement is UNCONDITIONAL (no runtime bypass).
    // The reserve always requires a signature-re-verified active_arm; the only
    // revert is redeploying the prior binary.
    info!("attested-arm enforcement ON (reserve re-verifies the ES256 signature before real spend; no runtime bypass)");

    let state_clone = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600)); // Every hour
        loop {
            interval.tick().await;
            if let Err(e) = state_clone.ledger.run_vacuum_if_needed(50.0).await {
                warn!("Background ledger vacuum failed: {}", e);
            }
        }
    });

    let state_sighup = state.clone();
    tokio::spawn(async move {
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to set up SIGHUP listener: {}", e);
                return;
            }
        };
        loop {
            sighup.recv().await;
            info!("Received SIGHUP, reloading auth config...");
            state_sighup.auth.reload().await;
        }
    });

    let watch_runner_handles = watch::runner::WatchRunner::start(
        watch_runtime.handle().clone(),
        sentinels,
        watch_quarantine.clone(),
    );
    info!("watch_runtime: dedicated 2-worker + 8-blocking pool online");
    // Bind to locals so the runtime + handles outlive axum::serve.
    let _watch_runtime_keepalive = watch_runtime;
    // `watch_runner_handles` is held for the lifetime of main; on SIGTERM/SIGINT
    // we call `.shutdown()` on it to fire the watch-channel signal that reaches
    // the writer-claim heartbeat loop's graceful-exit branch (which RELEASES the
    // claim). See the shutdown select! at the end of main. (No `_` prefix — it
    // is now used.)

    // The axum UDS router must be accepting before the self-probe runs: the probe
    // POSTs /v1/chat/completions and the gateway lua back-calls /council/* on this
    // socket. The healthcheck only waits for the socket file (post-bind), so the
    // probe retries briefly to tolerate nginx spin-up.
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow::anyhow!("axum::serve error: {e}"))
    });

    // Phase 3b live dispatcher config is read before the probe so the default
    // disabled stack does not require Phase 3 caller auth merely to boot.
    let disp_config = live_dispatcher_config_from_env();

    // Boot step 4.5 (after load/publish + after router serving, before sweep):
    // Use ReqwestTriageProbeClient against the gateway (default localhost:18080;
    // override via GATEWAY_BASE_URL / GW_URL for compose "http://gateway:8080").
    //
    // The same WATCH_DISPATCHER_GATEWAY_KEY (caller credential into the gateway)
    // is used for both the P0-eta probe and the live dispatcher path.
    // IRIN Comms Contract v0.2: the probe is mandatory before dispatcher
    // activation, but failed optional-peer readiness degrades this feature by
    // default instead of killing sidecar base health. Strict boot preserves the
    // old exit-88 behavior for hardened deployments.
    let gateway_base_url = std::env::var("GATEWAY_BASE_URL")
        .or_else(|_| std::env::var("GW_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:18080".to_string());
    let gateway_key = std::env::var("WATCH_DISPATCHER_GATEWAY_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let dispatcher_strict_boot = std::env::var("WATCH_DISPATCHER_STRICT_BOOT")
        .ok()
        .map(|v| {
            let v = v.trim().to_lowercase();
            v == "true" || v == "1" || v == "yes"
        })
        .unwrap_or(false);
    let phase3_probe_required = disp_config.enabled || gateway_key.is_some();
    let mut phase3_feature_ready = false;
    if disp_config.enabled && gateway_key.is_none() {
        if dispatcher_strict_boot {
            tracing::error!(
                exit_code = watch::startup_probe::CABINET_PROBE_FAILURE_EXIT_CODE,
                "FATAL: WATCH_DISPATCHER_ENABLED=true but WATCH_DISPATCHER_GATEWAY_KEY is not set; refusing strict Phase 3 dispatcher boot unauthenticated"
            );
            std::process::exit(watch::startup_probe::CABINET_PROBE_FAILURE_EXIT_CODE);
        }
        tracing::warn!(
            "WATCH_DISPATCHER_ENABLED=true but WATCH_DISPATCHER_GATEWAY_KEY is not set; Phase 3 dispatcher will remain inactive (set WATCH_DISPATCHER_STRICT_BOOT=true to fail startup instead)"
        );
    }

    if phase3_probe_required && !(disp_config.enabled && gateway_key.is_none()) {
        let probe_client =
            ReqwestTriageProbeClient::new_with_key(gateway_base_url, gateway_key.clone());
        let probe_tenant =
            std::env::var("BOOT_PROBE_TENANT").unwrap_or_else(|_| "default".to_string());

        let probe_max_attempts = std::env::var("WATCH_DISPATCHER_PROBE_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(30);
        let probe_retry_delay = std::env::var("WATCH_DISPATCHER_PROBE_RETRY_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(1));

        match probe_phase3_dispatcher_activation(
            &probe_client,
            &probe_tenant,
            dispatcher_strict_boot,
            probe_max_attempts,
            probe_retry_delay,
        )
        .await
        {
            Phase3DispatcherActivation::Ready => phase3_feature_ready = true,
            Phase3DispatcherActivation::Degraded { error } => {
                tracing::warn!(
                    error = %error,
                    "council-triage cabinet probe failed; Phase 3 dispatcher/hydration will remain inactive"
                );
            }
            Phase3DispatcherActivation::Fatal { exit_code, error } => {
                tracing::error!(
                    exit_code,
                    error = %error,
                    "FATAL: council-triage cabinet probe failed (P0-eta); aborting strict boot before hydration"
                );
                std::process::exit(exit_code);
            }
        }
    }

    if phase3_feature_ready {
        info!("cabinet probe passed (boot step 4.5); running boot hydration sweep");
        match run_boot_hydration_sweep(&watch_db, hydration_token, &directive_key).await {
            Ok(report) => {
                info!(
                    rows_examined = report.rows_examined,
                    recovered = report.staged_rows_recovered,
                    arm_held = report.arm_held,
                    skew_held = report.skew_held,
                    parse_failures = report.parse_failures,
                    deadline_hit = report.deadline_hit,
                    "boot hydration sweep completed"
                );
                if report.arm_held > 0 {
                    tracing::warn!(
                        arm_held = report.arm_held,
                        "hydration parked staged rows: no valid attested arm at sign time; \
                         rows stay council_response_staged and recover on the next armed sweep"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "boot hydration sweep had error (non-fatal)");
            }
        }
    } else if phase3_probe_required {
        tracing::warn!("Phase 3 hydration skipped: council-triage probe did not pass");
    } else {
        info!("Phase 3 probe/hydration skipped: live dispatcher disabled and WATCH_DISPATCHER_GATEWAY_KEY not configured");
    }

    // Phase 3b.5 — Live dispatcher loop (explicit opt-in via env, after hydration)
    // Boot order preserved: migrations → key load → router + probe → hydration → (optional) live dispatcher
    let _dispatcher_shutdown = if should_spawn_live_dispatcher(&disp_config) && phase3_feature_ready
    {
        info!(
            enabled = disp_config.enabled,
            interval_ms = disp_config.tick_interval_ms,
            max_claims = disp_config.max_claims_per_tick,
            base_url = %disp_config.gateway_base_url,
            council_timeout_secs = disp_config.council_call_timeout_secs,
            "starting live dispatcher loop (Phase 3b.5)"
        );

        // Same gateway caller credential path as the probe client (source assertion).
        // The gateway_key was read earlier (from WATCH_DISPATCHER_GATEWAY_KEY)
        // before the P0-eta probe.
        let client = ReqwestCouncilClient::new_with_timeout(
            disp_config.gateway_base_url.clone(),
            gateway_key.clone(),
            Duration::from_secs(disp_config.council_call_timeout_secs),
        );
        // WatchDb is Clone (cheap handle). We clone it so the spawned task owns its copy.
        let db_for_dispatch = (*watch_db).clone();
        // Clone the signing key for the dispatcher (original only needed for hydration here).
        let key_for_dispatch = directive_key.clone();

        // lease liveness: thread the quarantine handle so mid-flight lease
        // losses bump lease_expired_during_deliberation (telemetry invariant).
        match watch::dispatcher::spawn_live_dispatcher_loop_with_quarantine(
            db_for_dispatch,
            client,
            key_for_dispatch,
            disp_config,
            Some(watch_quarantine.clone()),
        ) {
            Some((_handle, shutdown_tx)) => {
                // Keep the shutdown sender alive until the end of main (prevents early drop).
                // Real graceful shutdown integration (SIGTERM etc.) can be added later.
                info!("live dispatcher loop spawned successfully");
                Some(shutdown_tx)
            }
            None => {
                warn!("live dispatcher spawn returned None despite enabled=true");
                None
            }
        }
    } else {
        if disp_config.enabled && !phase3_feature_ready {
            warn!("live dispatcher inactive: Phase 3 startup readiness did not pass");
        } else {
            info!("live dispatcher disabled (WATCH_DISPATCHER_ENABLED != true)");
        }
        None
    };

    let worker_config = live_worker_config_from_env();
    let _worker_shutdown = if should_spawn_live_worker(&worker_config) {
        info!(
            enabled = worker_config.enabled,
            interval_ms = worker_config.tick_interval_ms,
            max_claims = worker_config.max_claims_per_tick,
            lease_duration_ms = worker_config.lease_duration_ms,
            tenant_scope = %worker_config.tenant_scope,
            "starting live worker loop"
        );
        let db_for_worker = (*watch_db).clone();
        match spawn_live_worker_loop(
            db_for_worker,
            worker_config,
            watch_quarantine.clone(),
            arm_notifier.clone(),
        ) {
            Some((_handle, shutdown_tx)) => {
                info!("live worker loop spawned successfully");
                Some(shutdown_tx)
            }
            None => {
                warn!("live worker spawn returned None despite enabled=true");
                None
            }
        }
    } else {
        info!("live worker disabled (WATCH_WORKER_ENABLED != true)");
        None
    };

    BootServing {
        server_handle,
        watch_runner_handles,
        _watch_runtime_keepalive,
        _dispatcher_shutdown,
        _worker_shutdown,
    }
}

/// Phase 5 — shutdown (SIGTERM/SIGINT runner release, OTEL flush).
async fn await_shutdown(
    serving: BootServing,
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
) -> anyhow::Result<()> {
    let BootServing {
        server_handle,
        watch_runner_handles,
        _watch_runtime_keepalive,
        _dispatcher_shutdown,
        _worker_shutdown,
    } = serving;

    // SIGTERM (a Docker `compose recreate`) must reach the runner shutdown
    // channel, not just SIGHUP: signal → runner.shutdown() (fires the watch
    // channel every loop selects on, including the heartbeat loop's release
    // branch) → bounded grace for the release + drain → exit. Without it the
    // writer claim is never RELEASED and the stale row bricks the next
    // instance's producer. Integrated via select!, not a full graceful-shutdown
    // framework.
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("failed to install SIGTERM handler: {e}"))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| anyhow::anyhow!("failed to install SIGINT handler: {e}"))?;

    let server_result = tokio::select! {
        joined = server_handle => {
            // Server task ended on its own (error or clean exit).
            match joined {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("server task join error: {e}")),
            }
        }
        _ = sigterm.recv() => {
            info!("received SIGTERM — signalling watch runner shutdown (releases writer claim) before exit");
            watch_runner_handles.shutdown();
            // Bounded grace for the heartbeat loop to release the claim and the
            // producer to drain its in-flight tick. Best-effort; we exit even if
            // it overruns (the stale predicate is the fallback).
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Ok(())
        }
        _ = sigint.recv() => {
            info!("received SIGINT — signalling watch runner shutdown (releases writer claim) before exit");
            watch_runner_handles.shutdown();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Ok(())
        }
    };

    // Best-effort flush + shutdown for any OTEL exporter (no-op when not initialized).
    if let Some(provider) = otel_provider {
        if let Err(e) = provider.shutdown() {
            eprintln!("OTEL provider shutdown error (non-fatal): {e}");
        }
    }

    server_result
}

/// Load config, open DBs, construct AppState, assemble router, bind UDS, serve.
///
/// Implemented as five named phases (configuration → authority → hydration →
/// listener → shutdown). Inputs, outputs, ordering, and authority checks are
/// unchanged from the pre-split monolithic boot path.
pub(crate) async fn load_config_build_state_and_serve(
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
) -> anyhow::Result<()> {
    let config = load_configuration();
    let authority = initialize_authority(config).await;
    let hydrated = hydrate_runtime_state(authority).await?;
    let serving = start_listener_and_background(hydrated).await;
    await_shutdown(serving, otel_provider).await
}

/// Sentinel config path policy from `SENTINELS_CONFIG_PATH`.
///
/// Empty string is treated as **unset** so pack compose can pin
/// `SENTINELS_CONFIG_PATH=${IRIN_WATCH_PROFILE_PATH:-}` without fatally
/// requiring a profile on every no-profile boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SentinelsConfigPolicy {
    /// Non-empty explicit path: missing or invalid → fatal exit.
    Require { path: String },
    /// Unset/empty: default path; missing file → 0 sentinels (healthy).
    OptionalDefault { path: String },
}

const DEFAULT_SENTINELS_YAML: &str = "/etc/gateway/sentinels.yaml";

/// Resolve env value → load policy. Pure: no I/O, no process exit.
pub(crate) fn sentinels_config_policy(env: Option<&str>) -> SentinelsConfigPolicy {
    match env.map(str::trim).filter(|s| !s.is_empty()) {
        Some(path) => SentinelsConfigPolicy::Require {
            path: path.to_string(),
        },
        None => SentinelsConfigPolicy::OptionalDefault {
            path: DEFAULT_SENTINELS_YAML.to_string(),
        },
    }
}

/// Boot disposition after policy + filesystem existence (still pure of load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SentinelsBootDisposition {
    /// Path exists: load YAML; parse failure is always fatal.
    Load { path: String },
    /// Explicit path missing: fatal (never silent 0 sentinels).
    FatalMissing { path: String },
    /// Default path missing: warn and start with 0 sentinels.
    EmptyWarn { path: String },
}

pub(crate) fn sentinels_boot_disposition(
    policy: &SentinelsConfigPolicy,
    path_exists: bool,
) -> SentinelsBootDisposition {
    match (policy, path_exists) {
        (SentinelsConfigPolicy::Require { path }, true)
        | (SentinelsConfigPolicy::OptionalDefault { path }, true) => {
            SentinelsBootDisposition::Load { path: path.clone() }
        }
        (SentinelsConfigPolicy::Require { path }, false) => {
            SentinelsBootDisposition::FatalMissing { path: path.clone() }
        }
        (SentinelsConfigPolicy::OptionalDefault { path }, false) => {
            SentinelsBootDisposition::EmptyWarn { path: path.clone() }
        }
    }
}

#[cfg(test)]
mod sentinels_config_policy_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_env_string_is_unset_default_lookup() {
        assert_eq!(
            sentinels_config_policy(Some("")),
            SentinelsConfigPolicy::OptionalDefault {
                path: DEFAULT_SENTINELS_YAML.into()
            }
        );
        assert_eq!(
            sentinels_config_policy(Some("   ")),
            SentinelsConfigPolicy::OptionalDefault {
                path: DEFAULT_SENTINELS_YAML.into()
            }
        );
        assert_eq!(
            sentinels_boot_disposition(&sentinels_config_policy(Some("")), false),
            SentinelsBootDisposition::EmptyWarn {
                path: DEFAULT_SENTINELS_YAML.into()
            }
        );
    }

    #[test]
    fn unset_env_is_default_lookup() {
        assert_eq!(
            sentinels_config_policy(None),
            SentinelsConfigPolicy::OptionalDefault {
                path: DEFAULT_SENTINELS_YAML.into()
            }
        );
    }

    #[test]
    fn explicit_missing_is_fatal_disposition() {
        let policy = sentinels_config_policy(Some("/no/such/sentinels.yaml"));
        assert_eq!(
            policy,
            SentinelsConfigPolicy::Require {
                path: "/no/such/sentinels.yaml".into()
            }
        );
        assert_eq!(
            sentinels_boot_disposition(&policy, false),
            SentinelsBootDisposition::FatalMissing {
                path: "/no/such/sentinels.yaml".into()
            }
        );
    }

    #[test]
    fn valid_explicit_path_loads() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let path = dir.path().join("sentinels.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "- name: file-inbox-watch\n  tenant: canary\n  tier: polling\n  cooldown_ms: 30000\n  config:\n    path: {}\n    patterns: [\"*.txt\"]\n    debounce_ms: 500\n",
            inbox.display()
        )
        .unwrap();
        let policy = sentinels_config_policy(Some(path.to_str().unwrap()));
        assert!(matches!(policy, SentinelsConfigPolicy::Require { .. }));
        assert_eq!(
            sentinels_boot_disposition(&policy, path.exists()),
            SentinelsBootDisposition::Load {
                path: path.to_string_lossy().into_owned()
            }
        );
        let loaded = watch::registry::SentinelRegistry::load_from_yaml(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].sentinel.name(), "file-inbox-watch");
        assert_eq!(loaded[0].sentinel.tenant(), "canary");
    }
}
