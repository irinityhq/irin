// ==========================================================================
// main.rs — Gateway sidecar HTTP server.
//
// Axum-based replacement for Python FastAPI sidecar.
// Endpoints:
//   POST /guard/input   — prompt injection + encoding attack scanning
//   POST /guard/scan    — DEBUG ONLY (GATEWAY_DEBUG_GUARD_SCAN=1; 404 otherwise):
//                         raw internal decontaminator scan struct
//   POST /guard/tool    — tool call authorization (READ_ONLY allowlist)
//   POST /cache/check   — response cache lookup
//   POST /cache/store   — response cache write
//   POST /route/decide  — smart routing decision
//   POST /route/outcome — record provider response for health tracking
//   POST /budget/check  — pre-flight budget gate
//   POST /budget/record — post-flight spend recording
//   POST /policy/evaluate — sensitivity-based provider filtering
//   GET  /health        — sidecar health check
// ==========================================================================

mod auth;
mod budget;
mod cache;
pub mod comms;
pub mod council;
pub mod council_storage;
mod decontaminator;
mod enforcer;
mod keymgmt;
mod ledger;
mod policy;
mod ratelimit;
mod router;
mod socket;
mod sovereignty_gate;
mod unified_config;
mod vertex_auth;
pub mod watch;

use axum::{
    extract::Json,
    http::StatusCode,
    middleware as axum_mw,
    response::IntoResponse,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

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

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub(crate) struct AppState {
    decon: decontaminator::InputDecontaminator,
    cache: cache::GatewayCache,
    router: router::SmartRouter,
    budget: budget::BudgetEnforcer,
    policy: policy::PolicyFirewall,
    sovereignty: sovereignty_gate::SovereigntyGate,
    ledger: ledger::AuditLedger,
    ledger_signing_key: ed25519_dalek::SigningKey,
    /// Air-gapped root verifying key, loaded from ROOT_PUBKEY_HEX at startup.
    /// When `Some`, ceremony events (key_introduce/key_revoke) signed by the
    /// root are verifiable; when `None`, root verification is skipped with a
    /// warning at startup (backward compatibility).
    #[allow(dead_code)]
    root_pubkey: Option<ed25519_dalek::VerifyingKey>,
    auth: auth::AuthService,
    vertex_token: vertex_auth::VertexTokenProvider,
    /// Council endpoint per-key concurrency + in-memory idempotency cache
    /// (spec §5.8). In-memory only in v0.1 — a sidecar restart loses replay
    /// history (startup WARN emitted). SQLite-backed in v0.1.1.
    pub council: council::CouncilState,
    /// Phase 2 watch.db handle — append-only hash-chained fire log per
    /// tenant. Powers T31 `/watch/verify-chain/:tenant` and the upcoming
    /// `/watch/list` / `/watch/audit` endpoints. Opened at boot; the
    /// chain itself is written via `QuarantineState::write_fire_row` →
    /// `WatchDb::insert_fire` once sentinels start firing.
    pub watch_db: std::sync::Arc<watch::db::WatchDb>,
    /// Phase 2 T30 — (tenant, sentinel_name) → sentinel handle, populated
    /// from `sentinels.yaml` at boot. Powers `POST /watch/force-wake/{sentinel}`.
    pub watch_registry: watch::api::ForceWakeRegistry,
    /// Phase 2 — in-memory quarantine state (hysteresis + hard-kill).
    /// Force-wake gates on this before jumping to escalate().
    pub watch_quarantine: std::sync::Arc<watch::quarantine::QuarantineState>,
    /// Resolved at boot: WATCH_ADMIN_TOKEN || BOOTSTRAP_TOKEN. Empty → all
    /// force-wake requests fail closed with 401 (constant-time compare).
    pub watch_admin_token: String,
    /// Wave-1 single-tenant tripwire: the ONE tenant the outbox surface
    /// accepts. Resolved ONCE at boot from `WATCH_CANARY_TENANT` (default
    /// "sovereign") via `watch::api::resolve_canary_tenant`; the guard compares
    /// every resolved tenant scope against this configured value, not a
    /// hardcoded const. Set only in the CI/phase-3-smoke sidecar; local canary
    /// stays "sovereign".
    pub watch_canary_tenant: String,
    // p0a-four-eyes arm principal registry + stage TTL moved into
    // `watch::api::ArmAdminRouterState` (the arm routes
    // live in the lib crate so the wiring is oneshot-tested).
    /// Librarian upstream url for identity/memory proxy and commits
    pub librarian_base_url: String,
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RecordLedgerRequest {
    source: String,
    target: String,
    payload: serde_json::Value,
    metadata: serde_json::Value,
    #[serde(default)]
    caller_key: Option<String>,
}

#[derive(Serialize)]
struct RecordLedgerResponse {
    recorded: bool,
    event_id: Option<i64>,
    hash: String,
    latency_ms: u64,
}

#[derive(Deserialize)]
struct GuardInputRequest {
    #[serde(default)]
    content: String,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "unknown".to_string()
}

#[derive(Serialize)]
struct GuardInputResponse {
    verdict: decontaminator::ScanVerdict,
    blocked: bool,
    blocked_reason: String,
    original_hash: String,
    cleaned_hash: String,
    threat_count: usize,
    threats: Vec<decontaminator::ThreatDetection>,
    latency_ms: u64,
}

#[derive(Deserialize)]
struct GuardToolRequest {
    #[serde(default)]
    tool: String,
    #[serde(default)]
    args: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize)]
struct GuardToolResponse {
    allowed: bool,
    tool: String,
    violations: Vec<String>,
    latency_ms: u64,
}

#[derive(Serialize)]
struct GuardToolError {
    allowed: bool,
    reason: String,
    tool: String,
    arg: String,
    latency_ms: u64,
}

#[derive(Deserialize)]
struct GuardSovereigntyRequest {
    #[serde(default)]
    action_desc: String,
    #[serde(default)]
    action_type: String,
    #[serde(default = "default_energy")]
    energy: f64,
}

fn default_energy() -> f64 {
    1.0
}

#[derive(Serialize)]
struct GuardSovereigntyResponse {
    allowed: bool,
    score: f64,
    kappa: f64,
    c_alignment: f64,
    d_risk: f64,
    energy: f64,
    question_boost: bool,
    latency_ms: u64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    build_sha: &'static str,
    build_dirty: bool,
}

#[derive(Deserialize)]
struct CacheCheckRequest {
    alias: String,
    raw_body: String,
    /// Lua's translator_version. The check returns hit=false if the cached
    /// entry's version doesn't match — cheap insurance against silent
    /// translator drift between cache writes and reads.
    #[serde(default = "default_translator_version")]
    expected_translator_version: u32,
}

fn default_translator_version() -> u32 {
    0
}

#[derive(Serialize)]
struct CacheCheckResponse {
    hit: bool,
    response: Option<serde_json::Value>,
    /// Only present on hit. The provider that produced `response`; the Lua
    /// caller uses this to drive translate_response on the cached native
    /// shape before emitting to the client.
    provider: Option<String>,
    latency_ms: u64,
}

#[derive(Deserialize)]
struct CacheStoreRequest {
    alias: String,
    raw_body: String,
    /// The NATIVE upstream response shape. The Lua caller MUST pass the
    /// pre-translation body (gw_response_buf_native) — passing a normalized
    /// body would defeat the cache-shape invariant.
    response: serde_json::Value,
    provider: String,
    translator_version: u32,
    ttl_secs: Option<u64>,
}

#[derive(Serialize)]
struct CacheStoreResponse {
    stored: bool,
    latency_ms: u64,
}

#[derive(Deserialize)]
struct RouteDecideRequest {
    #[serde(default)]
    model: Option<String>,
    body: serde_json::Value,
    #[serde(default)]
    strategy: Option<String>,
}

#[derive(Deserialize)]
struct RouteOutcomeRequest {
    /// The actually-routed model ID (not alias). Required for per-family
    /// health tracking — the router derives (provider, family) from this.
    model_id: String,
    success: bool,
    latency_ms: f64,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Serialize)]
struct RouteOutcomeResponse {
    recorded: bool,
}

#[derive(Deserialize)]
struct BudgetCheckRequest {
    budget_key: String,
    estimated_cost: f64,
}

#[derive(Deserialize)]
struct BudgetRecordRequest {
    budget_key: String,
    actual_cost: f64,
}

#[derive(Deserialize)]
struct PolicyEvalRequest {
    provider: String,
    #[serde(default)]
    sensitivity_level: Option<policy::SensitivityLevel>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct AuthCheckRequest {
    raw_key: String,
    ip: String,
}

#[derive(Deserialize)]
struct IpCheckRequest {
    ip: String,
}

#[derive(Deserialize)]
struct ProvisionKeyRequest {
    budget_key: String,
    tier: String,
    rpm: u32,
    #[serde(default)]
    admin_key: String,
    /// Optional immutable role tag (spec §5.6). The gateway uses this in
    /// conjunction with `COUNCIL_GATEWAY_KEY_ID` to gate X-Council-* header
    /// restore. Defaults to None for the common (non-council) provisioning
    /// path — existing automation and admin clients are unaffected.
    #[serde(default)]
    service_role: Option<String>,
}

#[derive(Deserialize)]
struct RevokeKeyRequest {
    key_id: String,
    admin_key: String,
}

#[derive(Deserialize)]
struct RotateKeyRequest {
    admin_key: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn request_id_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum_mw::Next,
) -> impl IntoResponse {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let span = tracing::info_span!("request", request_id = %request_id);
    let _guard = span.enter();
    drop(_guard);

    let resp = {
        let _entered = span.enter();
        next.run(req).await
    };
    resp
}

async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        service: "gateway-sidecar",
        build_sha: watch::attest::build_sha(),
        build_dirty: watch::attest::build_is_dirty(),
    })
}

#[cfg(test)]
mod health_contract_tests {
    use super::*;

    #[tokio::test]
    async fn health_exposes_embedded_build_identity() {
        let response = health().await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(
            payload["build_sha"]
                .as_str()
                .is_some_and(|sha| sha.len() == 40),
            "sidecar health must expose the full embedded commit: {payload}"
        );
        assert_eq!(payload["build_sha"], watch::attest::build_sha());
        assert_eq!(payload["build_dirty"], watch::attest::build_is_dirty());
        assert!(
            payload["build_dirty"].is_boolean(),
            "sidecar health must expose embedded tree cleanliness: {payload}"
        );
    }
}

#[derive(Serialize)]
struct VertexTokenResponse {
    token: String,
    source: vertex_auth::TokenSource,
}

#[derive(Serialize)]
struct VertexTokenError {
    error: String,
}

async fn vertex_token_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.vertex_token.get_token().await {
        Ok((token, source)) => (
            StatusCode::OK,
            Json(serde_json::to_value(VertexTokenResponse { token, source }).unwrap()),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(VertexTokenError { error: e }).unwrap()),
        )
            .into_response(),
    }
}

async fn guard_input(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<GuardInputRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();

    if req.content.is_empty() {
        return Json(GuardInputResponse {
            verdict: decontaminator::ScanVerdict::Clean,
            blocked: false,
            blocked_reason: String::new(),
            original_hash: String::new(),
            cleaned_hash: String::new(),
            threat_count: 0,
            threats: vec![],
            latency_ms: 0,
        });
    }

    let result = state.decon.scan(&req.content);
    let latency_ms = t0.elapsed().as_millis() as u64;

    if result.blocked {
        warn!(
            source = %req.source,
            verdict = ?result.verdict,
            threats = result.threat_count,
            latency_ms,
            "guard/input: blocked"
        );
    }

    Json(GuardInputResponse {
        verdict: result.verdict,
        blocked: result.blocked,
        blocked_reason: result.blocked_reason,
        original_hash: result.original_hash,
        cleaned_hash: result.cleaned_hash,
        threat_count: result.threat_count,
        threats: result.threats,
        latency_ms,
    })
}

async fn guard_scan_debug(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<GuardInputRequest>,
) -> impl IntoResponse {
    // Debug-only. Returns the FULL internal decontaminator scan struct (rule-set
    // fingerprint, per-rule verdicts, original/cleaned hashes) — a UDS caller could
    // use it to probe which obfuscations slip the guard. This handler is REGISTERED
    // ONLY when GATEWAY_DEBUG_GUARD_SCAN=1 (see router construction); when disabled
    // the route is absent, so every method + malformed body uniformly hits the 404
    // fallback and the route's existence is not disclosed. Gating at registration
    // (not inside the handler) is required because the `Json` extractor would
    // otherwise 400/415 a bad body — and a non-404 status leaks existence.
    // Production traffic uses /guard/input — the Lua frontend never calls /guard/scan.
    let result = state.decon.scan(&req.content);
    Json(serde_json::to_value(result).unwrap())
}

async fn guard_tool(Json(req): Json<GuardToolRequest>) -> impl IntoResponse {
    let t0 = Instant::now();

    match enforcer::enforce(&req.tool, &req.args, None, None) {
        Ok(result) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(GuardToolResponse {
                        allowed: result.allowed,
                        tool: result.tool,
                        violations: result.violations,
                        latency_ms,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(violation) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            warn!(
                tool = %req.tool,
                reason = %violation.reason,
                latency_ms,
                "guard/tool: violation"
            );
            (
                StatusCode::FORBIDDEN,
                Json(
                    serde_json::to_value(GuardToolError {
                        allowed: false,
                        reason: violation.reason,
                        tool: violation.tool,
                        arg: violation.arg,
                        latency_ms,
                    })
                    .unwrap(),
                ),
            )
        }
    }
}

async fn guard_sovereignty(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<GuardSovereigntyRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();

    let result = state
        .sovereignty
        .evaluate(&req.action_desc, &req.action_type, req.energy);
    let latency_ms = t0.elapsed().as_millis() as u64;

    if !result.allowed {
        warn!(
            action_type = %req.action_type,
            kappa = result.kappa,
            latency_ms,
            "guard/sovereignty: blocked"
        );
        (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::to_value(GuardSovereigntyResponse {
                    allowed: result.allowed,
                    score: result.score,
                    kappa: result.kappa,
                    c_alignment: result.c_alignment,
                    d_risk: result.d_risk,
                    energy: result.energy,
                    question_boost: result.question_boost,
                    latency_ms,
                })
                .unwrap(),
            ),
        )
    } else {
        (
            StatusCode::OK,
            Json(
                serde_json::to_value(GuardSovereigntyResponse {
                    allowed: result.allowed,
                    score: result.score,
                    kappa: result.kappa,
                    c_alignment: result.c_alignment,
                    d_risk: result.d_risk,
                    energy: result.energy,
                    question_boost: result.question_boost,
                    latency_ms,
                })
                .unwrap(),
            ),
        )
    }
}

async fn cache_check(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<CacheCheckRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();
    let key = cache::GatewayCache::generate_cache_key(&req.alias, &req.raw_body);

    match state.cache.get(&key).await {
        Some(entry) => {
            // Translator-version invalidation: stale entries are reported
            // as misses. The Lua caller will fetch fresh from the upstream
            // and write a current-version entry, naturally aging the cache.
            if entry.translator_version != req.expected_translator_version {
                debug!(
                    cached = entry.translator_version,
                    expected = req.expected_translator_version,
                    "cache entry skipped: translator version mismatch"
                );
                return Json(CacheCheckResponse {
                    hit: false,
                    response: None,
                    provider: None,
                    latency_ms: t0.elapsed().as_millis() as u64,
                });
            }
            Json(CacheCheckResponse {
                hit: true,
                response: Some(entry.response),
                provider: Some(entry.provider),
                latency_ms: t0.elapsed().as_millis() as u64,
            })
        }
        None => Json(CacheCheckResponse {
            hit: false,
            response: None,
            provider: None,
            latency_ms: t0.elapsed().as_millis() as u64,
        }),
    }
}

async fn cache_store(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<CacheStoreRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();
    let key = cache::GatewayCache::generate_cache_key(&req.alias, &req.raw_body);

    // Default TTL is 24 hours if not specified
    let ttl = req.ttl_secs.unwrap_or(86400);

    state
        .cache
        .set(
            &key,
            req.response,
            req.provider,
            req.translator_version,
            ttl,
        )
        .await;

    Json(CacheStoreResponse {
        stored: true,
        latency_ms: t0.elapsed().as_millis() as u64,
    })
}

async fn route_decide(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RouteDecideRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();

    // Strategy resolution: body field > X-Routing-Strategy header > default (Balanced)
    let strategy = req
        .strategy
        .as_deref()
        .and_then(router::RoutingStrategy::from_str_opt)
        .or_else(|| {
            headers
                .get("x-routing-strategy")
                .and_then(|v| v.to_str().ok())
                .and_then(router::RoutingStrategy::from_str_opt)
        })
        .unwrap_or_default();

    // Sensitivity level: header-trusted per COUNCIL_GATEWAY_CONTRACT.md.
    // The gateway has no opinion on payload sensitivity — IRIN or other
    // upstream callers classify and pass the verdict via X-Sensitivity-Level.
    // RED forces routing to a local provider regardless of requested model.
    let sensitivity = headers
        .get("x-sensitivity-level")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| "GREEN".to_string());

    // Sovereign mode: X-Sovereign-Mode header forces all routing to local
    // providers, regardless of sensitivity level. This is the "sovereign switch".

    let base_model = req
        .model
        .as_deref()
        .map(|m| m.split_once('@').map(|(base, _)| base).unwrap_or(m));

    let sovereign_mode = headers
        .get("x-sovereign-mode")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
        .unwrap_or(false);

    match state
        .router
        .route(
            &sensitivity,
            base_model,
            &req.body,
            strategy,
            sovereign_mode,
        )
        .await
    {
        Ok(decision) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            debug!(
                model = %decision.model_id,
                provider = %decision.provider,
                score = decision.score,
                task = ?decision.task_type,
                strategy = ?decision.strategy,
                sensitivity = %sensitivity,
                sovereign_mode,
                latency_ms,
                "route/decide"
            );
            (
                StatusCode::OK,
                Json(serde_json::to_value(&decision).unwrap()),
            )
        }
        Err(e) => {
            warn!(error = %e, "route/decide: failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e})),
            )
        }
    }
}

async fn route_outcome(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<RouteOutcomeRequest>,
) -> impl IntoResponse {
    state
        .router
        .record_outcome(&req.model_id, req.success, req.latency_ms, req.error)
        .await;
    Json(RouteOutcomeResponse { recorded: true })
}

async fn budget_check(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<BudgetCheckRequest>,
) -> impl IntoResponse {
    let result = state
        .budget
        .check(&req.budget_key, req.estimated_cost)
        .await;
    if !result.allowed {
        warn!(
            key = %req.budget_key,
            reason = %result.reason,
            "budget/check: rejected"
        );
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::to_value(&result).unwrap()),
        )
    } else {
        (StatusCode::OK, Json(serde_json::to_value(&result).unwrap()))
    }
}

async fn budget_record(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<BudgetRecordRequest>,
) -> impl IntoResponse {
    let status = state.budget.record(&req.budget_key, req.actual_cost).await;
    Json(serde_json::to_value(&status).unwrap())
}

async fn policy_evaluate(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<PolicyEvalRequest>,
) -> impl IntoResponse {
    let decision =
        state
            .policy
            .evaluate(&req.provider, req.sensitivity_level, req.content.as_deref());
    if !decision.allowed {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::to_value(&decision).unwrap()),
        )
    } else {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&decision).unwrap()),
        )
    }
}

async fn record_ledger(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RecordLedgerRequest>,
) -> impl IntoResponse {
    // W1b (defense-in-depth): writing the hash-chained audit ledger requires an
    // admin-tier key. `caller_key` is stored metadata, not auth. This route is
    // NOT network-exposed (no nginx location block; absent from nginx.conf), so
    // this gate closes UDS-local audit-forgery rather than a network surface —
    // gated under the same admin-key model as the read routes for symmetry (an
    // ungated WRITE path beside gated READs is exactly the asymmetry an auditor
    // flags). HeaderMap is before Json so the body-consuming extractor stays last.
    if let Err(resp) = require_admin_header(&state.auth, &headers).await {
        return resp;
    }

    let t0 = Instant::now();

    let input = ledger::EventInput {
        source: req.source,
        target: req.target,
        payload: req.payload,
        metadata: req.metadata,
        caller_key: req.caller_key,
    };

    match state.ledger.record_event(input).await {
        Ok(event) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(RecordLedgerResponse {
                        recorded: true,
                        event_id: event.id,
                        hash: event.hash,
                        latency_ms,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => {
            warn!(error = %e, "ledger/record: failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
        }
    }
}

/// T31 — `GET /watch/verify-chain/:tenant`. Thin wrapper over
/// `watch::api::verify_chain_json`; the impl lives in the library crate so
/// integration tests can exercise the handler without spinning up AppState.
async fn watch_verify_chain(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::verify_chain_json(state.watch_db.clone(), tenant).await
}

/// T27 — `GET /watch/list/:tenant`. Thin wrapper over `watch::api::list_json`.
async fn watch_list(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::list_json(state.watch_db.clone(), tenant).await
}

/// T28 — `GET /watch/temperature/:tenant`. Thin wrapper over
/// `watch::api::temperature_json`.
async fn watch_get_tenant_policy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::watch_get_tenant_policy(state.watch_db.clone(), tenant, &state.watch_canary_tenant)
        .await
}

async fn watch_set_tenant_policy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    axum::Json(policy): axum::Json<watch::db::TenantPolicy>,
) -> impl IntoResponse {
    // T1: tenant-policy mutation requires the real admin token (constant-time check in lib fn).
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::watch_set_tenant_policy(
        state.watch_db.clone(),
        state.watch_admin_token.clone(),
        bearer,
        tenant,
        policy,
        &state.watch_canary_tenant,
    )
    .await
}

async fn watch_temperature(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::temperature_json(state.watch_db.clone(), tenant).await
}

/// Gate 4 operator snapshot: admin-authenticated, canary-guarded, strict
/// whitelist projection. This is the only general Watch read exposed through
/// nginx; all mutation and arming routes remain UDS-only.
async fn watch_ui_snapshot(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::ui_snapshot_json(
        state.watch_db.clone(),
        state.watch_quarantine.clone(),
        state.watch_admin_token.clone(),
        bearer,
        tenant,
        &state.watch_canary_tenant,
    )
    .await
}

/// T30 — `POST /watch/force-wake/:sentinel`. Admin-authed manual fire
/// trigger. Thin wrapper over `watch::api::force_wake_json`; parses the
/// Bearer header and optional JSON body, then defers to the library crate.
async fn watch_force_wake(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(sentinel): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let body_val = body.map(|axum::Json(v)| v);
    watch::api::force_wake_json(
        state.watch_db.clone(),
        state.watch_registry.clone(),
        state.watch_quarantine.clone(),
        state.watch_admin_token.clone(),
        bearer,
        sentinel,
        body_val,
    )
    .await
}

// p0a-four-eyes arm/disarm routes: the handlers + route table live in the
// LIBRARY crate (`watch::api::arm_admin_router`) so the exact wiring is
// covered by router-level oneshot tests . main.rs merges
// the sub-router below — see the `.merge(...)` in the app Router.

/// T32 — `DELETE /watch/quarantine/:sentinel`. Admin-authed quarantine +
/// hard-kill release. Thin wrapper over `watch::api::clear_quarantine_json`;
/// parses the Bearer header and optional JSON body, then defers to the
/// library crate. Returns the cleared list + (optional) probation_until.
async fn watch_clear_quarantine(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(sentinel): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let body_val = body.map(|axum::Json(v)| v);
    watch::api::clear_quarantine_json(
        state.watch_registry.clone(),
        state.watch_quarantine.clone(),
        state.watch_admin_token.clone(),
        bearer,
        sentinel,
        body_val,
    )
    .await
}

/// P1 — `GET /watch/outbox/{tenant}?status=&cursor=&limit=`. Tenant-scoped
/// list of signed directives; canonical bytes + signature are returned by api.rs.
/// Admin-only (Invariant, Option 3): unauthed -> 401 before any
/// store lookup; the D1/T1 public hash projection was removed (§6 cadence/tenant leak).
async fn watch_list_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let status = params.get("status").cloned();
    let cursor = params.get("cursor").cloned();
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(50);
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let authed = watch::api::admin_token_matches(&state.watch_admin_token, bearer.as_deref());
    watch::api::list_outbox_json(
        state.watch_db.clone(),
        tenant,
        status,
        cursor,
        limit,
        authed,
        &state.watch_canary_tenant,
    )
    .await
}

/// P1 — `GET /watch/outbox/{tenant}/{id}`. A non-canary path tenant is rejected
/// with 403 `single_tenant_violation` (Wave-1 tripwire, fires before the DB
/// lookup); a canary-tenant miss returns 404. Admin-only: unauthed -> 401 before any
/// store lookup (Invariant, Option 3; D1/T1 projection removed).
async fn watch_get_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path((tenant, id)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let authed = watch::api::admin_token_matches(&state.watch_admin_token, bearer.as_deref());
    watch::api::get_outbox_json(
        state.watch_db.clone(),
        tenant,
        id,
        authed,
        &state.watch_canary_tenant,
    )
    .await
}

/// P1 — `GET /watch/outbox/pubkey`. Public verification key for directives.
async fn watch_outbox_pubkey() -> impl IntoResponse {
    watch::api::outbox_pubkey_json().await
}

/// P1 — `POST /watch/outbox/{id}/ack`. Requires `X-Tenant-Scope`.
async fn watch_ack_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let tenant_scope = headers
        .get("x-tenant-scope")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    watch::api::ack_outbox_json(
        state.watch_db.clone(),
        state.watch_admin_token.clone(),
        bearer,
        id,
        tenant_scope,
        &state.watch_canary_tenant,
    )
    .await
}

async fn watch_claim_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    req: axum::Json<watch::api::ClaimRequest>,
) -> impl IntoResponse {
    let tenant_scope = headers
        .get("x-tenant-scope")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // T1: mutations require the real admin token (constant-time check in lib fn).
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::claim_outbox_json(
        state.watch_db.clone(),
        state.watch_admin_token.clone(),
        bearer,
        tenant_scope,
        req.0,
        &state.watch_canary_tenant,
    )
    .await
}

async fn watch_heartbeat_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    req: axum::Json<watch::api::HeartbeatRequest>,
) -> impl IntoResponse {
    let tenant_scope = headers
        .get("x-tenant-scope")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // T1: mutations require the real admin token (constant-time check in lib fn).
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::heartbeat_outbox_json(
        state.watch_db.clone(),
        state.watch_admin_token.clone(),
        bearer,
        tenant_scope,
        id,
        req.0,
        &state.watch_canary_tenant,
    )
    .await
}

async fn watch_worker_ack_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    req: axum::Json<watch::api::WorkerAckRequest>,
) -> impl IntoResponse {
    let tenant_scope = headers
        .get("x-tenant-scope")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // T1: mutations require the real admin token (constant-time check in lib fn).
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::worker_ack_outbox_json(
        state.watch_db.clone(),
        state.watch_admin_token.clone(),
        bearer,
        tenant_scope,
        id,
        req.0,
        &state.watch_canary_tenant,
    )
    .await
}

async fn watch_nack_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    req: axum::Json<watch::api::NackRequest>,
) -> impl IntoResponse {
    let tenant_scope = headers
        .get("x-tenant-scope")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // T1: mutations require the real admin token (constant-time check in lib fn).
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::nack_outbox_json(
        state.watch_db.clone(),
        state.watch_admin_token.clone(),
        bearer,
        tenant_scope,
        id,
        req.0,
        &state.watch_canary_tenant,
    )
    .await
}

/// T33.P1-D — `GET /watch/stats`. Watch-plane counter snapshot scraped by
/// the Lua-side prometheus poller, mirroring the council_stats precedent
/// (council.rs:347 / main.rs:1558 — `/council/stats` JSON → Lua emits
/// `gw_council_*` on /metrics). Returns the two infrastructure counters:
///   - `audit_infra_errors_total` → `gw_watch_audit_infra_errors_total`
///   - `persist_failures_total`   → `gw_watch_persist_failures_total`
///
/// "Not silently unscrapable" is the explicit acceptance bar: the
/// sidecar exposes the values; the Lua poller owns Prometheus formatting.
async fn watch_stats(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::Json<watch::api::WatchStats> {
    // watch telemetry — assembly moved into the shared `build_watch_stats`
    // (api.rs) so the integration tests scrape the SAME code path (no mirror
    // drift). The durable db is passed for the spend-vs-cap gauge pair
    // (telemetry invariant): spend_today_usd reads the spend ledger via the
    // re-pointed get_daily_council_spend; spend_cap_usd is boot-resolved daily_spend_cap().
    axum::Json(
        watch::api::build_watch_stats(&state.watch_quarantine, Some(state.watch_db.as_ref())).await,
    )
}

/// T29 — `GET /watch/audit/:tenant?limit=&before_id=`. Thin wrapper over
/// `watch::api::audit_json`. Limit caps + descending pagination live in
/// the library-crate handler; this just parses the query params.
async fn watch_audit(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = q.get("limit").and_then(|s| s.parse::<i64>().ok());
    let before_id = q.get("before_id").and_then(|s| s.parse::<i64>().ok());
    watch::api::audit_json(state.watch_db.clone(), tenant, limit, before_id).await
}

async fn auth_check(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<AuthCheckRequest>,
) -> impl IntoResponse {
    let decision = state.auth.check(&req.raw_key, &req.ip).await;
    if !decision.allowed {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::to_value(&decision).unwrap()),
        )
    } else {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&decision).unwrap()),
        )
    }
}

async fn auth_ip_check(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<IpCheckRequest>,
) -> impl IntoResponse {
    let result = state.auth.check_ip(&req.ip);
    let status = if result.allowed {
        StatusCode::OK
    } else {
        StatusCode::FORBIDDEN
    };
    (status, Json(serde_json::to_value(&result).unwrap()))
}

async fn admin_provision_key(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<ProvisionKeyRequest>,
) -> impl IntoResponse {
    // Mandatory admin authorization — the previous "empty admin_key = allow" path
    // is closed. Bootstrap (when no admin keys exist yet) is supported only via
    // a deliberate BOOTSTRAP_TOKEN env var that must match req.admin_key.
    let admin_key = if req.admin_key.is_empty() {
        // No client-supplied admin_key — there is no path to provision now.
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "admin_key required. Set BOOTSTRAP_TOKEN env var and pass it as admin_key for initial bootstrap."
            })),
        );
    } else {
        req.admin_key.clone()
    };

    let bootstrap_token = std::env::var("BOOTSTRAP_TOKEN").unwrap_or_default();
    if !bootstrap_token.is_empty() && admin_key == bootstrap_token {
        // Bootstrap path — allowed for initial key creation.
        tracing::info!("Admin provision via BOOTSTRAP_TOKEN");
    } else {
        let auth = state.auth.check(&admin_key, "127.0.0.1").await;
        if !auth.allowed || auth.tier != "admin" {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Admin tier required for key provisioning"})),
            );
        }
    }

    match state
        .auth
        .provision_key(
            &req.budget_key,
            &req.tier,
            req.rpm,
            req.service_role.clone(),
        )
        .await
    {
        Ok(res) => (StatusCode::OK, Json(serde_json::to_value(&res).unwrap())),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

async fn admin_revoke_key(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<RevokeKeyRequest>,
) -> impl IntoResponse {
    // Admin check required
    if req.admin_key.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin_key required"})),
        );
    }
    let auth = state.auth.check(&req.admin_key, "127.0.0.1").await;
    if !auth.allowed || auth.tier != "admin" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin tier required for key revocation"})),
        );
    }

    // Prevent self-revocation
    if auth.key_id == req.key_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Cannot revoke your own key"})),
        );
    }

    match state.auth.revoke_key(&req.key_id).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"revoked": true, "key_id": req.key_id})),
        ),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))),
    }
}

async fn auth_rotate_key(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<RotateKeyRequest>,
) -> impl IntoResponse {
    if req.admin_key.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin_key required"})),
        );
    }
    let auth = state.auth.check(&req.admin_key, "127.0.0.1").await;
    if !auth.allowed || auth.tier != "admin" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin tier required for key rotation"})),
        );
    }

    let (new_signing_key, new_key_bytes) = keymgmt::generate_keypair();
    let new_pubkey_hex = hex::encode(new_signing_key.verifying_key().as_bytes());

    let introduce_payload = keymgmt::sign_introduce(
        &state.ledger_signing_key,
        &new_key_bytes,
        keymgmt::CeremonyPurpose::LedgerSigning,
    );

    let input = ledger::EventInput {
        source: "keymgmt".into(),
        target: ledger::EVENT_KEY_INTRODUCE.into(),
        payload: serde_json::to_value(&introduce_payload).unwrap(),
        metadata: serde_json::json!({
            "admin_key_id": auth.key_id,
            "action": "rotation",
        }),
        caller_key: Some(auth.key_id),
    };

    // Write new key to a staging file so it never appears in logs/responses.
    let staging_path = std::env::var("LEDGER_NEW_KEY_STAGING_PATH")
        .unwrap_or_else(|_| "/run/sidecar/new_ledger_key.bin".to_string());
    if let Some(parent) = std::path::Path::new(&staging_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&staging_path, new_key_bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to stage new key: {}", e)})),
        );
    }
    let _ = std::fs::set_permissions(
        &staging_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    );

    match state.ledger.record_event(input).await {
        Ok(event) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "new_pubkey_hex": new_pubkey_hex,
                "new_key_staging_path": staging_path,
                "introduce_event_id": event.id,
                "introduce_event_hash": event.hash,
                "deploy_instructions": [
                    format!("1. Inspect staged key at {}", staging_path),
                    "2. Move to LEDGER_SIGNING_KEY_PATH (ensure chmod 600)",
                    "3. Set LEDGER_OLD_SIGNING_KEY_PATH to the current key path",
                    "4. Restart sidecar",
                    "5. After grace period, revoke old key",
                ]
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to record introduce event: {}", e)})),
        ),
    }
}

/// W1b — admin gate for the `/ledger/*` routes.
///
/// The ledger lives on the `admin_proxy` surface (nginx.conf:382/387 →
/// `lua/sidecar.lua::admin_proxy`), whose sibling admin routes
/// (`admin_provision_key`/`admin_revoke_key`/`auth_rotate_key`, main.rs:1238/
/// 1276/1311) authorize via `state.auth.check(...) → tier == "admin"`. We mirror
/// that idiom, NOT the watch/outbox bearer model (`admin_token_matches`) — the
/// outbox is a different proxy (`watch_outbox_proxy`, which forwards
/// `Authorization`); `admin_proxy` strips `Authorization` and (post-fix)
/// forwards `X-Admin-Key`, so the ledger key arrives as that header.
///
/// Fail-closed semantics:
/// * `X-Admin-Key` missing/empty            → 401
/// * key present but `auth.check` rejects it → 401
/// * key valid but `tier != "admin"`        → 403
async fn require_admin_header(
    auth: &auth::AuthService,
    headers: &axum::http::HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let admin_key = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if admin_key.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "X-Admin-Key required"})),
        ));
    }
    let decision = auth.check(admin_key, "127.0.0.1").await;
    if !decision.allowed {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        ));
    }
    if decision.tier != "admin" {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin tier required"})),
        ));
    }
    Ok(())
}

async fn ledger_verify(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // W1b: chain-validity readout is admin-gated (admin_proxy surface). Whether
    // it should be publicly verifiable is a deferred design decision (#25) —
    // gate it now, fail-closed.
    if let Err((code, body)) = require_admin_header(&state.auth, &headers).await {
        return (code, body).into_response();
    }
    match state.ledger.verify_chain().await {
        Ok(valid) => (StatusCode::OK, Json(serde_json::json!({"valid": valid}))).into_response(),
        Err(e) => {
            warn!(error = %e, "ledger/verify: failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct LedgerExportQuery {
    #[serde(default = "default_export_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}
fn default_export_limit() -> u32 {
    1000
}

async fn ledger_export(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<LedgerExportQuery>,
) -> impl IntoResponse {
    // W1b: full audit-row dump (payload, metadata, caller_key, signatures) is
    // admin-gated — this is the ledger-exfil surface (admin_proxy). Fail-closed.
    if let Err((code, body)) = require_admin_header(&state.auth, &headers).await {
        return (code, body).into_response();
    }
    let limit = query.limit.min(10_000); // Max 10k per page
    match state.ledger.export_events(limit, query.offset).await {
        Ok(events) => (StatusCode::OK, Json(events)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Ledger signing key loader
//
// Loads the Ed25519 signing key seed from disk. The file is exactly 32 raw
// bytes (despite the historical `.pem` extension on the default path — the
// extension is misleading; the contents are a raw seed, accidentally
// compatible with `ed25519-dalek::SigningKey::from_bytes`).
//
// Fails closed: missing file, wrong size, or non-0600 permissions panic
// at startup. No ephemeral key generation — that would silently break
// chain verification across restarts.
//
// See COUNCIL_GATEWAY_CONTRACT.md for the trust root section.
// ---------------------------------------------------------------------------
fn load_ledger_signing_key() -> Vec<u8> {
    let key_path = std::env::var("LEDGER_SIGNING_KEY_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME")
                .expect("FATAL: HOME env var must be set to locate the ledger signing key");
            std::path::PathBuf::from(home)
                .join(".irin")
                .join("ledger_key.pem")
        });

    let metadata = std::fs::metadata(&key_path).unwrap_or_else(|e| {
        panic!(
            "FATAL: cannot stat ledger signing key at {:?}: {}. \
             Set LEDGER_SIGNING_KEY_PATH or place a 32-byte seed file at the default path.",
            key_path, e
        )
    });

    let perms = metadata.permissions().mode() & 0o777;
    if perms != 0o600 {
        panic!(
            "FATAL: ledger signing key at {:?} must be chmod 0600 (got {:o}). \
             Run: chmod 600 {:?}",
            key_path, perms, key_path
        );
    }

    let bytes = std::fs::read(&key_path).unwrap_or_else(|e| {
        panic!(
            "FATAL: cannot read ledger signing key at {:?}: {}",
            key_path, e
        )
    });

    if bytes.len() != 32 {
        panic!(
            "FATAL: ledger signing key at {:?} must be exactly 32 bytes (got {}). \
             Generate with: openssl rand -out {:?} 32 && chmod 600 {:?}",
            key_path,
            bytes.len(),
            key_path,
            key_path
        );
    }

    info!(path = %key_path.display(), "ledger signing key loaded");
    bytes
}

// ---------------------------------------------------------------------------
// Root verifying key loader (ROOT_PUBKEY_HEX)
//
// The air-gapped root signing key never reaches a running sidecar — only its
// public counterpart is needed for verification. When `ROOT_PUBKEY_HEX` is
// set to a 64-character hex string (32 raw Ed25519 public-key bytes), it is
// parsed into a `VerifyingKey` and held on AppState. This enables ceremony
// (key_introduce / key_revoke) envelope verification at fsck/verify time.
//
// When unset or unparseable, the function returns `None` and we log a warning
// — the sidecar continues to start so existing deployments keep working.
// Operators graduating to PKI provide the hex; everyone else stays on the
// implicit-root model with the active signing key as the de-facto root.
// ---------------------------------------------------------------------------
fn load_root_pubkey() -> Option<ed25519_dalek::VerifyingKey> {
    let hex_str = match std::env::var("ROOT_PUBKEY_HEX") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            warn!("ROOT_PUBKEY_HEX not set — ceremony envelope root-verification disabled");
            return None;
        }
    };
    if hex_str.len() != 64 {
        warn!(
            len = hex_str.len(),
            "ROOT_PUBKEY_HEX must be exactly 64 hex chars (32 bytes) — root verification disabled"
        );
        return None;
    }
    let bytes = match hex::decode(&hex_str) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                "ROOT_PUBKEY_HEX is not valid hex ({}) — root verification disabled",
                e
            );
            return None;
        }
    };
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    match ed25519_dalek::VerifyingKey::from_bytes(&arr) {
        Ok(vk) => {
            info!(pubkey = %hex_str, "ROOT_PUBKEY_HEX loaded — ceremony root verification enabled");
            Some(vk)
        }
        Err(e) => {
            warn!("ROOT_PUBKEY_HEX bytes do not form a valid Ed25519 point ({}) — root verification disabled", e);
            None
        }
    }
}

fn load_old_ledger_key() -> Option<Vec<u8>> {
    // Apply the same strict file checks to the primary and previous keys.
    // to the old-key path used in dual-signing window / ceremony. Previously silent-ignore on bad file.
    // If the env var is set, the file MUST be valid — fail closed for provenance hygiene.
    if let Ok(path) = std::env::var("LEDGER_OLD_SIGNING_KEY_PATH") {
        let key_path = std::path::PathBuf::from(path);
        let metadata = std::fs::metadata(&key_path).unwrap_or_else(|e| {
            panic!(
                "FATAL: LEDGER_OLD_SIGNING_KEY_PATH set but cannot stat {:?}: {}. \
                 Must be 32-byte 0600 seed during rotation window. \
                 Set LEDGER_OLD_SIGNING_KEY_PATH or place a 32-byte seed file at the path.",
                key_path, e
            )
        });
        let perms = metadata.permissions().mode() & 0o777;
        if perms != 0o600 {
            panic!(
                "FATAL: LEDGER_OLD_SIGNING_KEY_PATH at {:?} must be chmod 0600 (got {:o}). \
                 Run: chmod 600 {:?}",
                key_path, perms, key_path
            );
        }
        let bytes = std::fs::read(&key_path).unwrap_or_else(|e| {
            panic!(
                "FATAL: cannot read LEDGER_OLD_SIGNING_KEY_PATH at {:?}: {}",
                key_path, e
            )
        });
        if bytes.len() != 32 {
            panic!(
                "FATAL: LEDGER_OLD_SIGNING_KEY_PATH at {:?} must be exactly 32 bytes (got {}). \
                 Generate with: openssl rand -out {:?} 32 && chmod 600 {:?}",
                key_path,
                bytes.len(),
                key_path,
                key_path
            );
        }
        info!(path = %key_path.display(), "old ledger signing key loaded for rotation window");
        return Some(bytes);
    }
    None
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured JSON logging + optional OTEL (P2/Phase 4.5)
    // OTEL opt-in: set OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318 (or :4317 for grpc)
    // Traces will be exported; falls back to no-op if not configured or init fails.
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

    // Initialize Cryptographic Ledger with persistent Ed25519 signing key.
    // Fails closed (panics) if the key file is missing, wrong size, or has
    // wrong permissions. See load_ledger_signing_key() for details.
    let ledger_path = std::env::var("LEDGER_DB_PATH").unwrap_or_else(|_| "ledger.db".to_string());
    let signing_key_bytes = load_ledger_signing_key();
    let old_key_bytes = load_old_ledger_key();
    let audit_ledger = ledger::AuditLedger::new(
        &ledger_path,
        Some(&signing_key_bytes),
        old_key_bytes.as_deref(),
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

    // Initialize Vertex ADC token provider
    let vertex_token = vertex_auth::VertexTokenProvider::new().await;

    let mut sk_bytes = [0u8; 32];
    sk_bytes.copy_from_slice(&signing_key_bytes);
    let ledger_sk = ed25519_dalek::SigningKey::from_bytes(&sk_bytes);

    let root_pubkey = load_root_pubkey();

    // Spec P1 #14: in-memory idempotency means replays initiated before this
    // PID started cannot be observed. Surface that explicitly at boot rather
    // than discovering it during a billing-reconciliation investigation.
    warn!(
        "council idempotency: in-memory only — replays before this PID may bill twice. \
         SQLite-backed in v0.1.1."
    );

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
    //   - SENTINELS_CONFIG_PATH explicitly set → file must exist and parse;
    //     any failure is fatal (tracing::error! + exit(1)).
    //   - Unset → fall back to /etc/gateway/sentinels.yaml; if absent, boot
    //     with an empty Vec (the runtime is still up, ready for hot-reload).
    let sentinels_yaml_explicit = std::env::var("SENTINELS_CONFIG_PATH").ok();
    let sentinels_yaml_path = sentinels_yaml_explicit
        .clone()
        .unwrap_or_else(|| "/etc/gateway/sentinels.yaml".to_string());
    let loaded_sentinels: Vec<watch::registry::LoadedSentinel> = {
        let path = std::path::PathBuf::from(&sentinels_yaml_path);
        if path.exists() {
            match watch::registry::SentinelRegistry::load_from_yaml(&path) {
                Ok(v) => {
                    info!(
                        "sentinels.yaml: loaded {} sentinel(s) from {}",
                        v.len(),
                        path.display()
                    );
                    v
                }
                Err(e) => {
                    tracing::error!(
                        "sentinels.yaml at {} failed to load: {:#}. \
                         Cold-boot Phase 2 cannot start with an invalid sentinel config.",
                        path.display(),
                        e
                    );
                    std::process::exit(1);
                }
            }
        } else if sentinels_yaml_explicit.is_some() {
            tracing::error!(
                "SENTINELS_CONFIG_PATH={} but the file does not exist. \
                 Set the variable to an existing yaml or unset it for default lookup.",
                path.display()
            );
            std::process::exit(1);
        } else {
            tracing::warn!(
                "no sentinels.yaml at {} — WatchRunner starting with 0 sentinels",
                path.display()
            );
            Vec::new()
        }
    };

    // T27 — boot-time registry upsert: write each loaded sentinel into
    // watch_sentinels so `/watch/list/{tenant}` has something to return.
    // ON CONFLICT preserves hard_killed_at / probation_until / enabled
    // (see WatchDb::upsert_sentinel_registration), so restart-safe.
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

    let mut app = Router::new()
        .route("/health", get(health))
        // Guard endpoints
        .route("/guard/input", post(guard_input))
        .route("/guard/tool", post(guard_tool))
        .route("/guard/sovereignty", post(guard_sovereignty))
        // Ledger endpoint
        .route("/ledger/record", post(record_ledger))
        .route("/ledger/verify", get(ledger_verify))
        .route("/ledger/export", get(ledger_export))
        // Cache endpoints
        .route("/cache/check", post(cache_check))
        .route("/cache/store", post(cache_store))
        // Routing endpoints
        .route("/route/decide", post(route_decide))
        .route("/route/outcome", post(route_outcome))
        // Budget endpoints
        .route("/budget/check", post(budget_check))
        .route("/budget/record", post(budget_record))
        // Policy endpoint
        .route("/policy/evaluate", post(policy_evaluate))
        // Auth / Admin endpoints
        .route("/auth/check", post(auth_check))
        .route("/auth/ip-check", post(auth_ip_check))
        .route("/admin/keys", post(admin_provision_key))
        .route("/admin/keys/revoke", post(admin_revoke_key))
        .route("/auth/rotate", post(auth_rotate_key))
        // Vertex ADC token endpoint
        .route("/vertex/token", get(vertex_token_handler))
        // Council endpoint (spec §5.8): per-caller concurrency + idempotency
        // for Phase 0.5 council-* models. The Lua router calls these UDS
        // endpoints in a peek → lock → claim sequence and a paired
        // unlock + store-or-fail cleanup from cost.lua's log phase.
        .route("/council/lock", post(council::council_lock))
        .route("/council/unlock", post(council::council_unlock))
        .route(
            "/council/idempotency/peek",
            post(council::council_idem_peek),
        )
        .route(
            "/council/idempotency/claim",
            post(council::council_idem_claim),
        )
        .route(
            "/council/idempotency/store",
            post(council::council_idem_store),
        )
        .route(
            "/council/idempotency/fail",
            post(council::council_idem_fail),
        )
        // P1-C: scrape target for the Lua poller that surfaces council
        // counters (active_swept_total, unlock_missing_grant_total) +
        // gauges (active_locks, active_caller_keys) on /metrics.
        .route("/council/stats", get(council::council_stats))
        // T31 — P0-5 closure: walk per-tenant hash chain.
        .route("/watch/verify-chain/{tenant}", get(watch_verify_chain))
        // T27 — registered sentinels + per-sentinel stats.
        .route("/watch/list/{tenant}", get(watch_list))
        // T28 — single-scalar liveness gauge.
        .route("/watch/temperature/{tenant}", get(watch_temperature))
        // Gate 4 — exact human-facing Watch snapshot. Admin auth + canary
        // guard are enforced in the sidecar handler.
        .route("/watch/ui-snapshot/{tenant}", get(watch_ui_snapshot))
        // T29 — descending fire log with cursor pagination.
        .route("/watch/audit/{tenant}", get(watch_audit))
        // T30 — admin-authed manual fire trigger (constant-time Bearer compare).
        .route("/watch/force-wake/{sentinel}", post(watch_force_wake))
        // T32 — admin-authed quarantine + hard-kill release.
        .route(
            "/watch/quarantine/{sentinel}",
            delete(watch_clear_quarantine),
        )
        // T33.P1-D — JSON scrape target for the Lua poller that surfaces
        // `gw_watch_audit_infra_errors_total` + `gw_watch_persist_failures_total`
        // on /metrics. Matches council_stats precedent — sidecar exposes
        // JSON state, Lua owns Prometheus formatting.
        .route("/watch/stats", get(watch_stats))
        .route(
            "/watch/tenant-policy/{tenant}",
            get(watch_get_tenant_policy),
        )
        .route(
            "/watch/tenant-policy/{tenant}",
            post(watch_set_tenant_policy),
        )
        // P1 — Directive outbox surface (read/list, verification pubkey, ack).
        .route("/watch/outbox/pubkey", get(watch_outbox_pubkey))
        .route("/watch/outbox/{tenant}", get(watch_list_outbox))
        .route("/watch/outbox/{tenant}/{id}", get(watch_get_outbox))
        .route("/watch/outbox/{id}/ack", post(watch_ack_outbox))
        .route("/watch/outbox/claim", post(watch_claim_outbox))
        .route("/watch/outbox/{id}/heartbeat", post(watch_heartbeat_outbox))
        .route(
            "/watch/outbox/{id}/worker_ack",
            post(watch_worker_ack_outbox),
        )
        .route("/watch/outbox/{id}/nack", post(watch_nack_outbox))
        // p0a-four-eyes (the dual-custody invariant): legacy single-shot arm is 410
        // Gone; arming requires stage (principal A) + confirm (principal B).
        // The four arm/disarm routes live in the lib crate so the exact
        // wiring is oneshot-tested .
        .merge(watch::api::arm_admin_router(
            watch::api::ArmAdminRouterState {
                quarantine: watch_quarantine.clone(),
                principals: arm_principals.clone(),
                stage_ttl: arm_stage_ttl,
                admin_token: watch_admin_token.clone(),
                notifier: arm_notifier.clone(),
                deviation: arm_deviation.clone(),
                attest_keys: attest_keys.clone(),
                // B6 (T1 MF-1): derive the real-arm permission ONCE from the
                // EMBEDDED build identity — a `-dirty` build can only ever run
                // DARK/rehearsal ceremonies (the producer never starts).
                allow_real_arm: !watch::attest::build_is_dirty(),
            },
        ))
        // Librarian Proxy endpoints (v0.3)
        .route("/librarian/commit", post(librarian_commit))
        .route("/librarian/context/{tenant}", get(librarian_context));

    // /guard/scan — debug-only decontaminator introspection. Registered ONLY when
    // GATEWAY_DEBUG_GUARD_SCAN=1 so that when disabled the route is entirely absent:
    // every method (GET/POST/...) and every malformed/wrong-content-type body
    // uniformly hits the 404 fallback, disclosing nothing about its existence.
    // Registration-time gating avoids an in-handler `Json` extractor returning
    // a distinguishable 400/415. Production never sets it.
    if std::env::var("GATEWAY_DEBUG_GUARD_SCAN")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        app = app.route("/guard/scan", post(guard_scan_debug));
    }

    // Audit F-3: global flood backstop over the whole UDS router. Added as the
    // outermost layer (last `.layer` = runs first) so excess local traffic is
    // shed with a 429 before any per-route work; `/health` is exempted inside
    // the middleware so liveness probes never trip it.
    let global_limiter = ratelimit::GlobalRateLimiter::from_env();
    let app = app
        .layer(axum_mw::from_fn(request_id_layer))
        .layer(axum_mw::from_fn_with_state(
            global_limiter,
            ratelimit::global_rate_limit,
        ))
        .with_state(state.clone());

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

    // Start background tasks
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

    // Solve server-spawn timing for self-probe (P0-eta residual):
    // Spawn axum UDS router first so it is accepting (for C11 /council/* back-calls
    // that the gateway lua will make when we POST the probe to /v1/chat/completions).
    // Healthcheck only waits for socket file (post-bind), gateway container starts,
    // then we probe; a short retry loop tolerates nginx spin-up.
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
        match spawn_live_worker_loop(db_for_worker, worker_config) {
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

    // Graceful shutdown (restart regression): a Docker
    // `compose recreate` sends SIGTERM. Previously the process only handled
    // SIGHUP, so SIGTERM killed it WITHOUT firing the runner shutdown channel —
    // the writer_claim_heartbeat_loop never reached its graceful-exit branch,
    // so the writer claim was never RELEASED and the row persisted on the
    // compose volume, bricking the next instance's producer for the stale
    // window. Now: signal → runner.shutdown() (fires the watch channel every
    // loop selects on, incl. the heartbeat loop's release branch) → bounded
    // grace for the release + drain → exit. Minimal by design — not a full
    // graceful-shutdown framework; integrated via select! without restructuring
    // the axum::serve future.
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

// ---------------------------------------------------------------------------
// Librarian v0.3 Proxy Handlers
// ---------------------------------------------------------------------------

async fn librarian_commit(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(payload): axum::extract::Json<serde_json::Value>,
) -> axum::response::Response {
    let client = reqwest::Client::new();
    let url = format!("{}/api/librarian/commits", state.librarian_base_url);
    match client.post(&url).json(&payload).send().await {
        Ok(resp) => {
            let status = resp.status();
            match resp.bytes().await {
                Ok(b) => (status, b).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("librarian upstream error: {}", e),
        )
            .into_response(),
    }
}

async fn librarian_context(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> axum::response::Response {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/librarian/context/{}",
        state.librarian_base_url, tenant
    );
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            match resp.bytes().await {
                Ok(b) => (status, b).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("librarian upstream error: {}", e),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// W1b drift-lock (Council P1, session 2b3183af-12c).
//
// `tests/ledger_export_auth.rs` exercises the full per-handler matrix but
// against a *copy* of the gate logic (the real handlers are private to this
// binary crate). That copy can silently diverge — e.g. a future edit dropping
// the tier check would leave that integration test green. This same-crate test
// calls the REAL `require_admin_header` so the actual gate semantics are
// pinned: drop the tier check and THIS test goes red.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod require_admin_header_tests {
    use super::*;

    /// Real AuthService over a temp config, with a provisioned admin-tier key
    /// and a default-tier key. Dev env so `AuthService::new` does not panic
    /// (it requires AUTH_PEPPER set or GATEWAY_AUTH_FAIL_CLOSED=false).
    async fn fixture() -> (tempfile::TempDir, auth::AuthService, String, String) {
        std::env::set_var("GATEWAY_AUTH_FAIL_CLOSED", "false");
        std::env::set_var("AUTH_PEPPER", "w1b-driftlock-pepper");
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("auth_keys.json");
        let svc = auth::AuthService::new(Some(cfg));
        let admin = svc
            .provision_key("ledger_admin", "admin", 600, None)
            .await
            .unwrap();
        let user = svc
            .provision_key("ledger_user", "default", 600, None)
            .await
            .unwrap();
        (tmp, svc, admin.raw_key, user.raw_key)
    }

    fn headers_with_key(key: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-admin-key", key.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn no_key_is_401() {
        let (_tmp, svc, _admin, _user) = fixture().await;
        let empty = axum::http::HeaderMap::new();
        let err = require_admin_header(&svc, &empty).await.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn junk_key_is_401() {
        let (_tmp, svc, _admin, _user) = fixture().await;
        let err = require_admin_header(&svc, &headers_with_key("gw_not_a_real_key"))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_non_admin_tier_is_403() {
        // The load-bearing case: this is what goes red if the tier check is
        // dropped. A valid, allowed key whose tier != "admin" must be 403.
        let (_tmp, svc, _admin, user) = fixture().await;
        let err = require_admin_header(&svc, &headers_with_key(&user))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn valid_admin_key_is_ok() {
        let (_tmp, svc, admin, _user) = fixture().await;
        let res = require_admin_header(&svc, &headers_with_key(&admin)).await;
        assert!(res.is_ok(), "admin-tier key must pass the gate");
    }
}
