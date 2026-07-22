//! HTTP + WebSocket server — axum equivalent of warroom/backend/app.py.
//!
//! Core deliberation:
//!     GET  /api/health              — credentials available
//!     GET  /api/cabinets            — live cabinet listing (full shape, re-scans disk)
//!     POST /api/cabinets/save       — persist a War Room cabinet draft (feature contract)
//!     GET  /api/sessions            — session history
//!     GET  /api/sessions/:id        — single session detail
//!     GET  /api/precedent           — precedent search
//!     WS   /ws/deliberate           — live streaming deliberation
//!
//! Lineage / fork:
//!     POST /api/sessions/:id/fork
//!     GET  /api/sessions/:id/lineage
//!     GET  /api/sessions/:a/diff/:b
//!
//! Operator intelligence:
//!     GET  /api/interventions       — operator pause log
//!     GET  /api/patterns            — aggregated decision style
//!
//! Drift self-audit (read-only — runs are Phase 2):
//!     GET  /api/drift/reports
//!     GET  /api/drift/reports/:name
//!     GET  /api/drift/weekly
//!     GET  /api/drift/weekly/history
//!
//! Mapmaker briefs:
//!     GET  /api/mapmaker/briefs
//!     GET  /api/mapmaker/briefs/:name
//!     POST /api/map/preview
//!
//! Embeddings:
//!     GET  /api/embeddings/stats    — semantic via fastembed-rs MiniLM-L6-v2

use sovereign_protocol::types::ProblemDetails;

fn problem(status: axum::http::StatusCode, title: &str, detail: &str) -> axum::response::Response {
    let mut details = ProblemDetails::new(title, detail);
    details.status = Some(status.as_u16());
    let mut resp = (status, axum::Json(details)).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    resp
}

use axum::{
    Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::Config;
use crate::engine::context::RequestContext;
use crate::engine::deliberate as engine_deliberate;
use crate::governance;
use crate::librarian;
use crate::mode::Mode;
use crate::precedent;
use crate::provider;
use crate::stream::deliberate::{self, StreamConfig};
use crate::stream::events::StreamEvent;
use crate::stream::intervention::{Intervention, InterventionQueue};
use crate::types::{SessionOrigin, SynthesisMode};
use crate::warroom;

/// Auth configuration — mirrors Python warroom/backend/security.py.
///
/// Three credentials surfaces:
/// 1. COUNCIL_AUTH_TOKEN  → bearer auth required on warroom + admin endpoints.
///    ALSO gates non-loopback bind: non-loopback (e.g. --host 0.0.0.0) is
///    refused at startup unless this token is set (hard error, not warning).
/// 2. COUNCIL_GATEWAY_TOKEN → service-identity token accepted on
///    `/api/deliberate` only via `X-Gateway-Auth`. FAIL-CLOSED on mismatch
///    (no bearer fallback) per §12.4.
/// 3. No token (dev only) → server binds loopback-only (127.0.0.1).
///    COUNCIL_DEV_NO_AUTH=1 is a documentation signal ONLY for auth_middleware
///    bypass on loopback; it NEVER unlocks non-loopback binds.
struct AuthConfig {
    token: Option<String>,
    gateway_token: Option<String>,
    dev_no_auth: bool,
}

impl AuthConfig {
    fn from_env() -> Self {
        let token = std::env::var("COUNCIL_AUTH_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        let gateway_token = std::env::var("COUNCIL_GATEWAY_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        let dev_no_auth = std::env::var("COUNCIL_DEV_NO_AUTH")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
        Self {
            token,
            gateway_token,
            dev_no_auth,
        }
    }

    fn announce(&self) {
        if self.token.is_some() {
            eprintln!("🔒 COUNCIL_AUTH_TOKEN set — bearer auth required");
        } else if self.dev_no_auth {
            eprintln!("⚠️  COUNCIL_DEV_NO_AUTH=1 — auth bypassed for loopback dev");
        } else {
            eprintln!("🔒 No auth token — loopback-only, network-restricted");
        }
        if self.gateway_token.is_some() {
            eprintln!("🔒 COUNCIL_GATEWAY_TOKEN set — X-Gateway-Auth accepted on /api/deliberate");
        }
    }
}

/// Returns true for loopback hosts that are safe to bind without a token.
/// Treats "localhost", 127.0.0.1/8, and ::1 (with optional []) as loopback.
/// Resolved addrs for IPv6 use bracket form e.g. "[::1]:port" (see resolve fn).
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim();
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// Resolve the addr for --serve, enforcing the fail-safe bind policy.
///
/// Default (loopback) is always allowed.
/// A non-loopback bind is allowed ONLY if COUNCIL_AUTH_TOKEN is set.
/// COUNCIL_DEV_NO_AUTH=1 does not permit non-loopback binds.
/// On violation returns Err with a loud multi-line error message.
#[allow(dead_code)]
pub fn resolve_serve_addr(host: &str, port: u16) -> Result<String, String> {
    let has_auth_token = std::env::var("COUNCIL_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .is_some();
    resolve_serve_addr_with_token(host, port, has_auth_token)
}

/// Testable variant that takes the has_auth_token decision explicitly.
#[allow(dead_code)]
pub(crate) fn resolve_serve_addr_with_token(
    host: &str,
    port: u16,
    has_auth_token: bool,
) -> Result<String, String> {
    if !is_loopback_host(host) && !has_auth_token {
        let msg = format!(
            "ERROR: Non-loopback bind to '{}' requested without COUNCIL_AUTH_TOKEN.\n\
             Council refuses to bind non-loopback addresses unless BOTH an explicit\n\
             non-loopback --host is given AND COUNCIL_AUTH_TOKEN is set.\n\
             COUNCIL_DEV_NO_AUTH=1 does NOT unlock non-loopback binding.\n\
             Set COUNCIL_AUTH_TOKEN=... or use --host 127.0.0.1 (default).",
            host
        );
        return Err(msg);
    }
    // Format as usable host:port. IPv6 literals must be bracketed (e.g. [::1]:8765)
    // for TcpListener::bind and URL display to be valid. Chosen: bracket in output.
    let addr = if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    };
    Ok(addr)
}

/// Constant-time string comparison via `subtle::ConstantTimeEq`.
fn subtle_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Auth middleware — bearer auth on most endpoints, service-identity
/// (X-Gateway-Auth) on `/api/deliberate` per §4.6.
async fn auth_middleware(
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let auth_config = AUTH_CONFIG.get().unwrap();

    // Dev bypass: only when NO auth secret of either kind is configured.
    if auth_config.dev_no_auth && auth_config.token.is_none() && auth_config.gateway_token.is_none()
    {
        return Ok(next.run(request).await);
    }

    let raw_path = request.uri().path();
    let norm_path = raw_path.trim_end_matches('/');
    let is_gateway_path = norm_path == "/api/deliberate";

    // Service-identity path: only on /api/deliberate, only when
    // COUNCIL_GATEWAY_TOKEN is configured. Per §12.4 / §4.6: FAIL-CLOSED on
    // wrong X-Gateway-Auth — no fallback to bearer if the header is present
    // but doesn't match.
    if is_gateway_path
        && let Some(ref t) = auth_config.gateway_token
        && let Some(provided) = headers.get("X-Gateway-Auth").and_then(|v| v.to_str().ok())
    {
        return if subtle_eq(provided, t) {
            Ok(next.run(request).await)
        } else {
            Err(StatusCode::UNAUTHORIZED)
        };
    }
    // No X-Gateway-Auth header at all → fall through to bearer (covers
    // warroom UI hitting /api/deliberate via bearer if that ever happens).

    // WebSocket upgrades cannot send Authorization; subprotocol auth is checked
    // in `ws_deliberate` / `ws_librarian` via `validate_ws_upgrade`. Both WS
    // paths are exempted from bearer here (R20 reuses the deliberate posture).
    let is_ws_subprotocol_auth = is_ws_subprotocol_path(norm_path);

    if !is_ws_subprotocol_auth && let Some(ref token) = auth_config.token {
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(|t| t.trim().to_string());

        match provided {
            Some(ref t) if subtle_eq(t, token.as_str()) => {}
            _ => return Err(StatusCode::UNAUTHORIZED),
        }
    }
    // No bearer token set — server is loopback-only by bind address (127.0.0.1).
    // Non-loopback binds are rejected at startup (resolve_serve_addr) unless
    // COUNCIL_AUTH_TOKEN was set; DEV_NO_AUTH does not relax bind policy.

    Ok(next.run(request).await)
}

static AUTH_CONFIG: std::sync::OnceLock<AuthConfig> = std::sync::OnceLock::new();

/// Shared state for all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Librarian state (R20). Shared with the nested `/api/librarian` router so
    /// the `/ws/librarian/{chat_id}` WS handler on the main router reuses the
    /// exact same Store / upstream client / idempotency cache / semaphore as
    /// `POST /ask`.
    pub librarian: librarian::routes::LibrarianState,
    /// Cap on concurrent `POST /api/deliberate` jobs. Each deliberation fans out
    /// to 4+ frontier LLMs with real spend, so an authed caller spawning unbounded
    /// PARALLEL jobs is a cost-exhaustion vector (audit #6). A non-blocking
    /// `try_acquire` fails fast with 429 instead of queueing, bounding the blast
    /// radius to N concurrent deliberations. Size via
    /// `COUNCIL_MAX_CONCURRENT_DELIBERATIONS` (default 4).
    ///
    /// SCOPE (honest framing — do not read this as a spend cap): this bounds PEAK
    /// CONCURRENCY, not CUMULATIVE spend. Serial N-at-a-time abuse still walks
    /// through; a true cost ceiling is a budget governor (per-subject $/day token
    /// bucket) — deferred to multi-tenant. So audit #6 is BURST-CLOSED /
    /// cumulative-deferred. The cap also counts jobs, not seats×rounds, so a
    /// `warroom` run weighs far more than a `--quick` one.
    ///
    /// NOT-DONE (revisit triggers): the cap is GLOBAL, not per-caller — fine for
    /// the single-tenant canary; the moment a second auth subject exists it must
    /// become per-subject. `/ws/deliberate` is intentionally UNCAPPED (single-
    /// sovereign warroom UI, not gateway-reachable) — the moment WS is gateway-
    /// reachable or multi-subject, it inherits this cap.
    pub deliberate_semaphore: Arc<Semaphore>,
}

/// Default concurrent-deliberation cap when `COUNCIL_MAX_CONCURRENT_DELIBERATIONS`
/// is unset or invalid.
const DEFAULT_MAX_DELIBERATIONS: usize = 4;

/// Resolve the concurrent-deliberation cap (audit #6) from a raw env value. Pure
/// (takes the already-read `Option<String>`, never reads env itself) so it is
/// unit-testable without the process-global env race. Any missing / unparseable /
/// zero value falls back to [`DEFAULT_MAX_DELIBERATIONS`] — it never returns 0,
/// because a 0-permit `Semaphore` would deadlock every deliberation (fail-closed
/// to a safe cap, not to "no service").
fn resolve_max_deliberations(raw: Option<String>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_DELIBERATIONS)
}

/// Build the axum router.
pub fn router(config: Arc<Config>) -> Router {
    let auth = AuthConfig::from_env();
    auth.announce();
    AUTH_CONFIG.get_or_init(|| auth);

    // One LibrarianState shared by the nested REST router and the main-router
    // WS handler (R20) so both speak to the same store + idempotency cache.
    let librarian_state = librarian::routes::LibrarianState::from_env();
    // Cost-exhaustion guard for /api/deliberate (audit #6).
    let max_deliberations =
        resolve_max_deliberations(std::env::var("COUNCIL_MAX_CONCURRENT_DELIBERATIONS").ok());
    let state = AppState {
        config,
        librarian: librarian_state.clone(),
        deliberate_semaphore: Arc::new(Semaphore::new(max_deliberations)),
    };

    let configured_origins = configured_cors_origins();
    let allowed_origins = AllowOrigin::predicate(move |origin, _| {
        origin_is_loopback(origin) || configured_origins.iter().any(|allowed| allowed == origin)
    });

    let cors = CorsLayer::new()
        .allow_origin(allowed_origins)
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any);

    Router::new()
        .route("/api/health", get(health))
        .route("/api/discover", get(discover_providers))
        .route("/api/cabinets", get(cabinets))
        .route("/api/cabinets/save", post(cabinets_save_handler))
        .route("/api/sessions", get(sessions_list))
        .route("/api/sessions/{id}", get(session_detail))
        .route("/api/sessions/{id}/fork", post(session_fork))
        .route("/api/sessions/{id}/lineage", get(session_lineage))
        .route("/api/sessions/{id}/export/pdf", post(session_export_pdf))
        .route("/api/sessions/{a}/diff/{b}", get(session_diff))
        .route("/api/precedent", get(precedent_search))
        .route("/api/precedent/reindex", post(precedent_reindex))
        .route("/api/interventions", get(interventions_list))
        .route("/api/interventions/predict", get(interventions_predict))
        .route("/api/patterns", get(patterns_aggregate))
        .route("/api/clusters", get(clusters_get))
        .route("/api/drift/reports", get(drift_reports_list))
        .route("/api/drift/reports/{name}", get(drift_report_get))
        .route("/api/drift/run", post(drift_run))
        .route("/api/drift/weekly", get(drift_weekly_latest))
        .route("/api/drift/weekly/history", get(drift_weekly_history))
        .route("/api/drift/weekly/run", post(drift_weekly_run))
        .route("/api/mapmaker/run", post(mapmaker_run))
        .route("/api/mapmaker/briefs", get(mapmaker_briefs_list))
        .route("/api/mapmaker/briefs/{name}", get(mapmaker_brief_get))
        .route("/api/map/preview", post(map_preview))
        .route("/api/embeddings/stats", get(embeddings_stats))
        .route("/api/embeddings/rebuild", post(embeddings_rebuild))
        .route("/api/meta-review/run", post(meta_review_run))
        .route("/api/meta-review/latest", get(meta_review_latest))
        .route("/api/deliberate", post(deliberate_handler))
        // TODO(multi-tenant): /ws/deliberate is intentionally NOT behind
        // deliberate_semaphore (audit #6) — single-sovereign warroom UI, not
        // gateway-reachable. The moment it becomes gateway-reachable or serves a
        // second auth subject, it must inherit the same concurrency cap.
        .route("/ws/deliberate", get(ws_deliberate))
        .route("/ws/librarian/{chat_id}", get(ws_librarian))
        .with_state(state)
        .nest("/api/librarian", librarian::routes::router(librarian_state))
        .nest("/api/governance", governance::router())
        .layer(middleware::from_fn(auth_middleware))
        .layer(cors)
}

fn default_cors_origins() -> Vec<HeaderValue> {
    vec![
        HeaderValue::from_static("http://localhost:3000"),
        HeaderValue::from_static("http://127.0.0.1:3000"),
        HeaderValue::from_static("http://localhost:3010"),
        HeaderValue::from_static("http://127.0.0.1:3010"),
        // Packaged Tauri webview origin (macOS/Linux).
        HeaderValue::from_static("tauri://localhost"),
    ]
}

/// Any loopback origin (any port on localhost / 127.0.0.1/8 / [::1]) is
/// always allowed. A page sending a loopback Origin is served from this
/// machine, which is already the trust boundary for a token-less loopback
/// bind; a hostile external page carries its own non-loopback Origin and is
/// rejected. Non-loopback origins (e.g. a tailnet address serving the UI to
/// a phone) must be listed in COUNCIL_CORS_ORIGINS.
fn origin_is_loopback(origin: &HeaderValue) -> bool {
    let Ok(s) = origin.to_str() else {
        return false;
    };
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    // Origin never carries userinfo — an '@' means someone is smuggling a
    // loopback-looking prefix in front of the real host. Reject outright.
    if authority.contains('@') {
        return false;
    }
    // "[::1]:3010" → "[::1]" (is_loopback_host strips the brackets); anything
    // after the bracket must be exactly a numeric ":port" or nothing.
    // "127.0.0.1:3011" / "localhost" → host before the port, if any.
    let host = if let Some(end) = authority.find(']') {
        let (bracketed, rest) = authority.split_at(end + 1);
        let port_ok = rest.is_empty()
            || (rest.len() > 1
                && rest.starts_with(':')
                && rest[1..].chars().all(|c| c.is_ascii_digit()));
        if !port_ok {
            return false;
        }
        bracketed
    } else {
        authority
            .split_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    is_loopback_host(host)
}

fn configured_cors_origins() -> Vec<HeaderValue> {
    let origins = std::env::var("COUNCIL_CORS_ORIGINS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|origins| !origins.is_empty())
        .unwrap_or_else(|| {
            default_cors_origins()
                .into_iter()
                .filter_map(|origin| origin.to_str().ok().map(str::to_string))
                .collect()
        });

    let parsed = origins
        .iter()
        .filter_map(|origin| match HeaderValue::from_str(origin) {
            Ok(value) => Some(value),
            Err(_) => {
                eprintln!("⚠️  ignoring invalid COUNCIL_CORS_ORIGINS entry: {origin}");
                None
            }
        })
        .collect::<Vec<_>>();

    if parsed.is_empty() {
        default_cors_origins()
    } else {
        parsed
    }
}

fn ws_smoke_only_enabled() -> bool {
    std::env::var("COUNCIL_WS_SMOKE_ONLY").as_deref() == Ok("1")
}

/// GET /api/health
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // The governed runtime starts Council before the Gateway containers, but
    // GW_API_KEY already establishes the configured route. Use the existing
    // gateway-mode provider semantics so liveness never shells out to every
    // optional CLI. Exact model readiness is enforced by Gateway /v1/models
    // immediately before each governed dispatch.
    let providers = provider::check_providers_with_gateway(provider::env_nonempty("GW_API_KEY"));
    let available: Vec<&str> = providers
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(name, _)| *name)
        .collect();
    let missing: Vec<&str> = providers
        .iter()
        .filter(|(_, ok)| !*ok)
        .map(|(name, _)| *name)
        .collect();

    let sessions_dir =
        std::env::var("COUNCIL_SESSIONS_DIR").unwrap_or_else(|_| "sessions".to_string());
    let index_path = format!("{}/index.jsonl", sessions_dir);
    let index_exists = std::path::Path::new(&index_path).exists();

    axum::Json(json!({
        "council_version": env!("CARGO_PKG_VERSION"),
        "build_sha": option_env!("COUNCIL_BUILD_GIT_SHA").unwrap_or("unknown"),
        "build_dirty": option_env!("COUNCIL_BUILD_DIRTY") != Some("false"),
        "stream_version": "rs-1.0.0",
        "providers_available": available,
        "providers_missing": missing,
        "sessions_dir": sessions_dir,
        "index_path": index_path,
        "index_exists": index_exists,
        "ws_smoke_only": ws_smoke_only_enabled(),
        // H1 (audit #6): live free slots in the /api/deliberate concurrency cap.
        // 0 = saturated (further deliberations get 429 until one completes).
        "deliberate_permits_available": state.deliberate_semaphore.available_permits(),
    }))
}

/// GET /api/discover — JSON mirror of `council --discover` (feature contract).
///
/// Same bearer posture as sibling GET routes (router-wide auth middleware).
/// Wire shape + env-hint privacy live in `ProviderRegistry::to_discover_json`.
async fn discover_providers() -> Response {
    // ProviderRegistry::discover() shells out (gcloud) and TCP-probes
    // localhost — offload per the spawn_blocking convention used by
    // precedent_search and embeddings_rebuild.
    match tokio::task::spawn_blocking(crate::registry::ProviderRegistry::discover).await {
        Ok(registry) => axum::Json(registry.to_discover_json()).into_response(),
        Err(e) => {
            eprintln!("ERROR: /api/discover spawn_blocking join failed: {e}");
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Discovery failed",
                &format!("spawn_blocking join failed: {e}"),
            )
        }
    }
}

/// GET /api/cabinets — full shape matching frontend CabinetEditor expectations.
///
/// feature contract: re-scans `<base_dir>/cabinets/` per request so cabinets saved via
/// POST /api/cabinets/save appear without a restart (the startup `Arc<Config>`
/// stays immutable). Saved cabinets launch by registry name: every launch path
/// resolves through `Config::resolve_cabinet_owned`, which falls back to the
/// saved YAML on a registry miss and runs the same per-run validation gate.
/// Falls back to the startup snapshot when the scan comes back empty (e.g. dir
/// unreadable).
async fn cabinets(State(state): State<AppState>) -> impl IntoResponse {
    let scanned = crate::config::scan_cabinets_dir(&state.config.base_dir);
    let live = if scanned.is_empty() {
        &state.config.cabinets
    } else {
        &scanned
    };
    let cabs: Vec<serde_json::Value> = live
        .iter()
        .map(|(key, cab)| {
            // Chair: always name/provider/model; include the optional system and
            // thinking_effort only when set so the GET shape stays wire-minimal
            // and round-trips Chair's `#[serde(default)] Option<String>` fields.
            let mut chair = serde_json::Map::new();
            chair.insert("name".into(), json!(cab.chair.name));
            chair.insert("provider".into(), json!(cab.chair.provider));
            chair.insert("model".into(), json!(cab.chair.model));
            if let Some(system) = &cab.chair.system {
                chair.insert("system".into(), json!(system));
                let p = state
                    .config
                    .base_dir
                    .join("prompts")
                    .join(format!("{}.tera", system));
                if let Ok(src) = std::fs::read_to_string(&p) {
                    chair.insert("system_source".into(), json!(src));
                }
            }
            if let Some(effort) = &cab.chair.thinking_effort {
                chair.insert("thinking_effort".into(), json!(effort));
            }

            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), json!(key));
            obj.insert("label".into(), json!(cab.name));
            // Truncate to first line for picker/UI display (full spec is long for triage etc.)
            let short_desc = cab
                .description
                .lines()
                .next()
                .unwrap_or(&cab.description)
                .trim()
                .to_string();
            obj.insert("description".into(), json!(short_desc));
            obj.insert("rounds".into(), json!(cab.rounds));
            obj.insert(
                "seats".into(),
                json!(
                    cab.seats
                        .iter()
                        .map(|s| {
                            let mut seat = serde_json::Map::new();
                            seat.insert("name".into(), json!(s.name));
                            seat.insert("provider".into(), json!(s.provider));
                            seat.insert("model".into(), json!(s.model));
                            let sys = &s.system;
                            seat.insert("system".into(), json!(sys));
                            if !sys.trim().is_empty() {
                                let p = state
                                    .config
                                    .base_dir
                                    .join("prompts")
                                    .join(format!("{}.tera", sys));
                                if let Ok(src) = std::fs::read_to_string(&p) {
                                    seat.insert("system_source".into(), json!(src));
                                }
                            }
                            serde_json::Value::Object(seat)
                        })
                        .collect::<Vec<_>>()
                ),
            );
            obj.insert("chair".into(), serde_json::Value::Object(chair));
            obj.insert(
                "is_triad".into(),
                json!(warroom::fork::is_triad_registry_key(key)),
            );
            obj.insert("local_code_only".into(), json!(cab.local_code_only));
            // Skip the serde default (Generic) so the wire stays back-compatible
            // with older clients that never saw the field.
            if cab.synthesis_mode != crate::types::SynthesisMode::default() {
                obj.insert(
                    "synthesis_mode".into(),
                    serde_json::to_value(&cab.synthesis_mode).unwrap_or(json!("generic")),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    axum::Json(json!({ "cabinets": cabs }))
}

/// POST /api/cabinets/save — persist a War Room cabinet draft to
/// `<base_dir>/cabinets/<name>.yaml` (feature contract).
///
/// Body: `{"name": string, "yaml": string}`. Auth: covered by the router-wide
/// `auth_middleware` layer — same posture as the other mutating routes
/// (embeddings/rebuild, drift/run, precedent/reindex).
/// Responses: 200 `{"ok": true, "name", "path"}` | 4xx/5xx `{"error": ...}`.
async fn cabinets_save_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CabinetSaveRequest>,
) -> Response {
    use warroom::cabinets_save::{self, SaveError};

    let cabinet = match cabinets_save::validate_save_request(&req.name, &req.yaml) {
        Ok(c) => c,
        Err(e) => {
            let status = match e {
                SaveError::EmbeddedKey(_) => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            return (status, axum::Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    // Full execution validation (structural + xmcp vault) before the write —
    // the same gate the WS custom_cabinet path runs per-run.
    // `model_check_blocking` is a synchronous network call, so offload per the
    // spawn_blocking convention used by precedent_reindex / embeddings_rebuild.
    let config = state.config.clone();
    let name_for_validation = req.name.clone();
    let validated = tokio::task::spawn_blocking(move || {
        config.validate_cabinet_for_save(&name_for_validation, &cabinet)
    })
    .await;
    match validated {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": format!("{e:#}") })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("validation join failed: {e}") })),
            )
                .into_response();
        }
    }

    match cabinets_save::write_cabinet_yaml(&state.config.base_dir, &req.name, &req.yaml) {
        Ok(path) => axum::Json(json!({
            "ok": true,
            "name": req.name,
            "path": path.display().to_string(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("write failed: {e}") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct CabinetSaveRequest {
    name: String,
    yaml: String,
}

/// GET /api/sessions?limit=100
///
/// Reads sessions/index.jsonl directly so the wire shape matches the Python
/// SessionIndexEntry the React frontend already consumes. Missing fields are
/// filled in with sensible defaults so older entries don't crash the UI.
#[derive(Deserialize)]
struct SessionsQuery {
    limit: Option<usize>,
}

/// Normalize one sessions/index.jsonl line into the wire shape the React
/// SessionIndexEntry interface reads. Returns None for blank, malformed, or
/// non-object lines so the caller can skip them.
pub(crate) fn normalize_index_entry(line: &str) -> Option<serde_json::Value> {
    if line.trim().is_empty() {
        return None;
    }
    let mut v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    {
        // Ensure every field the React SessionIndexEntry interface reads is present.
        let obj = v.as_object_mut()?;
        // Aliases: Rust-era writers use {session_id, timestamp, digest};
        // Python-era writers use {id, ts, ruling_digest}.
        if !obj.contains_key("id")
            && let Some(sid) = obj.get("session_id").cloned()
        {
            obj.insert("id".to_string(), sid);
        }
        if !obj.contains_key("ts")
            && let Some(t) = obj.get("timestamp").cloned()
        {
            obj.insert("ts".to_string(), t);
        }
        if !obj.contains_key("ruling_digest")
            && let Some(d) = obj.get("digest").cloned()
        {
            obj.insert("ruling_digest".to_string(), d);
        }
        // Fill in defaults the UI reads. mode defaults to "normal" — legacy
        // entries lacking the key are Python-era normal sessions, matching
        // the lenient CouncilSession::mode deserialization default.
        obj.entry("topic".to_string()).or_insert(json!(""));
        obj.entry("keywords".to_string()).or_insert(json!([]));
        obj.entry("ruling_digest".to_string()).or_insert(json!(""));
        obj.entry("confidence".to_string()).or_insert(json!(""));
        obj.entry("cabinet".to_string()).or_insert(json!(""));
        obj.entry("convergence".to_string()).or_insert(json!(0.0));
        obj.entry("mode".to_string()).or_insert(json!("normal"));
        obj.entry("seat_count".to_string()).or_insert(json!(0));
        obj.entry("rounds".to_string()).or_insert(json!(0));
        obj.entry("synthesis_model".to_string())
            .or_insert(json!(""));
        obj.entry("version".to_string()).or_insert(json!(""));
    }
    Some(v)
}

async fn sessions_list(Query(q): Query<SessionsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(500);

    let path = std::path::PathBuf::from(
        std::env::var("COUNCIL_SESSIONS_DIR").unwrap_or_else(|_| "sessions".to_string()),
    )
    .join("index.jsonl");

    let mut entries: Vec<serde_json::Value> = Vec::new();
    if let Ok(file) = std::fs::File::open(&path) {
        use std::io::BufRead;
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(v) = normalize_index_entry(&line) {
                entries.push(v);
            }
        }
    }

    // Newest first by ts (ISO-8601 sorts lexicographically).
    entries.sort_by(|a, b| {
        b.get("ts")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .cmp(a.get("ts").and_then(|x| x.as_str()).unwrap_or(""))
    });
    entries.truncate(limit);

    axum::Json(json!({ "sessions": entries }))
}

/// GET /api/sessions/:id
async fn session_detail(Path(id): Path<String>) -> impl IntoResponse {
    match precedent::load_session(&id) {
        Some(session) => axum::Json(json!(session)).into_response(),
        None => problem(
            axum::http::StatusCode::NOT_FOUND,
            "error",
            &format!("Session not found: {}", id),
        ),
    }
}

/// GET /api/precedent?q=...&limit=20
#[derive(Deserialize)]
struct PrecedentQuery {
    q: String,
    limit: Option<usize>,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    mode: Option<String>,
}

async fn precedent_search(Query(q): Query<PrecedentQuery>) -> impl IntoResponse {
    // Defaults mirror the engine's injection parameters so a bare query
    // previews exactly what a convene would inject.
    let limit = q.limit.unwrap_or(precedent::RETRIEVE_LIMIT).min(100);
    let threshold = q.threshold.unwrap_or(precedent::RETRIEVE_THRESHOLD);
    let mode = q.mode.as_deref().unwrap_or("auto");

    // Same retrieve() the deliberation engine uses — the preview IS the
    // injection set when queried with the engine's limit + threshold.
    // Keep synchronous precedent lookup off the async request worker:
    // retrieve() does FS load_index + (lazy fastembed model + embed) and can
    // block the axum worker (WarRoom / --serve responsiveness). Follows the
    // spawn_blocking pattern at embeddings_rebuild. On join error: log + 500.
    let q_clone = q.q.clone();
    let force_keyword = mode == "keyword";
    let join_res = tokio::task::spawn_blocking(move || {
        precedent::retrieve_with_mode(&q_clone, limit, threshold, false, force_keyword)
    })
    .await;

    let (matches, actual_mode, engine) = match join_res {
        Ok(receipt) => (
            precedent::receipt_to_match_values(&receipt),
            // UI contract: "semantic" | "keyword". hybrid-v1 carries the dense
            // layer, so it reports as semantic; `engine` holds the exact ranker.
            if receipt.engine == "hybrid-v1" {
                "semantic"
            } else {
                "keyword"
            },
            receipt.engine,
        ),
        Err(e) => {
            eprintln!(
                "ERROR: precedent_search spawn_blocking join failed for q (len={}): {}",
                q.q.len(),
                e
            );
            (vec![], "error", "error")
        }
    };

    let body = json!({
        "matches": matches,
        "query": q.q,
        "mode": actual_mode,
        "engine": engine,
        "threshold": threshold,
    });
    if actual_mode == "error" {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(body),
        )
            .into_response();
    }
    axum::Json(body).into_response()
}

// ───── Session lineage / fork ─────────────────────────────────

#[derive(Deserialize)]
struct ForkBody {
    #[serde(default)]
    swaps: Vec<serde_json::Value>,
}

async fn session_fork(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<axum::Json<ForkBody>>,
) -> impl IntoResponse {
    let swaps = body.map(|b| b.0.swaps).unwrap_or_default();
    let result = warroom::fork::fork_session(&state.config, &id, &swaps);
    if result.get("error").is_some() {
        return (axum::http::StatusCode::NOT_FOUND, axum::Json(result)).into_response();
    }
    axum::Json(result).into_response()
}

async fn session_lineage(Path(id): Path<String>) -> impl IntoResponse {
    let parent = warroom::lineage::parent_of(&id);
    let children = warroom::lineage::children_of(&id);
    axum::Json(json!({
        "session_id": id,
        "parent": parent,
        "children": children,
    }))
}

/// POST /api/sessions/{id}/export/pdf (N06) — render the session's ruling to a
/// downloadable PDF. 404 when the session is unknown. The PDF is a hand-rolled
/// paginated text document (no new crate); the browser receives it as an
/// attachment `council_<id>.pdf` (works in the Tauri webview too).
async fn session_export_pdf(Path(id): Path<String>) -> Response {
    // load_session does sync FS reads — offload per the spawn_blocking
    // convention used by precedent_search / embeddings_rebuild.
    let id_for_load = id.clone();
    let join = tokio::task::spawn_blocking(move || {
        precedent::load_session(&id_for_load).map(|session| warroom::pdf::render_session(&session))
    })
    .await;

    match join {
        Ok(Some(bytes)) => {
            let disposition = format!("attachment; filename=\"council_{}.pdf\"", id);
            let headers = [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/pdf"),
                ),
                (
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_str(&disposition).unwrap_or_else(|_| {
                        HeaderValue::from_static("attachment; filename=\"council.pdf\"")
                    }),
                ),
            ];
            (StatusCode::OK, headers, bytes).into_response()
        }
        Ok(None) => problem(
            StatusCode::NOT_FOUND,
            "error",
            &format!("Session not found: {}", id),
        ),
        Err(e) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "error",
            &format!("PDF render join failed: {e}"),
        ),
    }
}

async fn session_diff(Path((a, b)): Path<(String, String)>) -> impl IntoResponse {
    let parent = precedent::load_session(&a);
    let child = precedent::load_session(&b);
    let (Some(parent), Some(child)) = (parent, child) else {
        return problem(
            axum::http::StatusCode::NOT_FOUND,
            "error",
            "one or both sessions not found",
        );
    };
    let parent_v = serde_json::to_value(&parent).unwrap_or(json!({}));
    let child_v = serde_json::to_value(&child).unwrap_or(json!({}));
    axum::Json(warroom::lineage::diff_synthesis(&parent_v, &child_v)).into_response()
}

// ───── Interventions / patterns ───────────────────────────────

#[derive(Deserialize)]
struct InterventionsQuery {
    days: Option<i64>,
    limit: Option<usize>,
}

async fn interventions_list(Query(q): Query<InterventionsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(200).min(1000);
    let entries = warroom::intervention_log::load_all(q.days);
    let total = entries.len();
    // Tail of `limit`, reversed (newest first)
    let mut tail: Vec<_> = entries.into_iter().rev().take(limit).collect();
    tail.shrink_to_fit();
    axum::Json(json!({
        "entries": tail,
        "total": total,
    }))
}

#[derive(Deserialize)]
struct PatternsQuery {
    days: Option<i64>,
}

async fn patterns_aggregate(Query(q): Query<PatternsQuery>) -> impl IntoResponse {
    axum::Json(warroom::intervention_log::patterns(q.days))
}

/// GET /api/interventions/predict?convergence=<f64>&round=<u32> (N04).
#[derive(Deserialize)]
struct PredictQuery {
    convergence: f64,
    round: u32,
}

/// N04: probability that the operator escalates at the given pause point.
/// Trains a tiny logistic regression at request time from the intervention log;
/// < 30 usable samples falls back to overall escalation frequency. Cheap — runs
/// inline (a few thousand gradient steps over a handful of rows).
async fn interventions_predict(Query(q): Query<PredictQuery>) -> impl IntoResponse {
    axum::Json(warroom::predict::predict(q.convergence, q.round))
}

/// GET /api/clusters (N03) — topic clusters over the session embedding index.
async fn clusters_get() -> impl IntoResponse {
    // Reads + parses the embeddings/index JSONL and runs k-means; offload off
    // the request thread per the spawn_blocking convention.
    let result = tokio::task::spawn_blocking(warroom::clusters::build)
        .await
        .unwrap_or_else(|e| {
            json!({
                "clusters": [],
                "method": "kmeans",
                "k": 0,
                "n_sessions": 0,
                "error": format!("join: {e}"),
            })
        });
    axum::Json(result)
}

// ───── Drift reports ──────────────────────────────────────────

async fn drift_reports_list() -> impl IntoResponse {
    axum::Json(json!({
        "reports": warroom::drift::list_reports(),
        "running": warroom::drift::is_running(),
    }))
}

async fn drift_report_get(Path(name): Path<String>) -> impl IntoResponse {
    if !name.starts_with("drift_") || !name.ends_with(".md") {
        return problem(
            axum::http::StatusCode::NOT_FOUND,
            "error",
            &format!("report {} not found", name),
        );
    }
    match warroom::drift::get_report(&name) {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": format!("report {} not found", name)})),
        )
            .into_response(),
    }
}

async fn drift_weekly_latest() -> impl IntoResponse {
    match warroom::drift::latest_weekly_summary() {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({
                "detail": "no weekly summary yet — POST /api/drift/weekly/run to generate one"
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct DriftRunBody {
    #[serde(default = "default_window")]
    window: u32,
    #[serde(default = "default_drift_limit")]
    limit: Option<usize>,
}
fn default_window() -> u32 {
    7
}
fn default_drift_limit() -> Option<usize> {
    Some(8)
}

async fn drift_run(
    State(state): State<AppState>,
    body: Option<axum::Json<DriftRunBody>>,
) -> impl IntoResponse {
    if warroom::drift::is_running() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "drift run already in progress"})),
        )
            .into_response();
    }
    if !warroom::drift::acquire_lock() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "could not acquire drift lock"})),
        )
            .into_response();
    }
    let body = body.map(|b| b.0).unwrap_or(DriftRunBody {
        window: 7,
        limit: Some(8),
    });
    let window = body.window.clamp(1, 90);
    let limit = body.limit.map(|l| l.clamp(1, 50));

    let cfg = state.config.clone();
    tokio::spawn(async move {
        let _ = warroom::drift::run_drift_report(&cfg, window, limit).await;
        warroom::drift::release_lock();
    });

    axum::Json(json!({"status": "started", "window": window, "limit": limit})).into_response()
}

#[derive(Deserialize)]
struct WeeklyRunBody {
    #[serde(default = "default_window")]
    window: u32,
    #[serde(default = "default_drift_limit")]
    limit: Option<usize>,
    #[serde(default)]
    post_webhooks: bool,
}

async fn drift_weekly_run(
    State(state): State<AppState>,
    body: Option<axum::Json<WeeklyRunBody>>,
) -> impl IntoResponse {
    if warroom::drift::is_running() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "drift run already in progress"})),
        )
            .into_response();
    }
    if !warroom::drift::acquire_lock() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "could not acquire drift lock"})),
        )
            .into_response();
    }
    let body = body.map(|b| b.0).unwrap_or(WeeklyRunBody {
        window: 7,
        limit: Some(8),
        post_webhooks: false,
    });
    let window = body.window.clamp(1, 90);
    let limit = body.limit.map(|l| l.clamp(1, 50));
    let post = body.post_webhooks;

    let cfg = state.config.clone();
    tokio::spawn(async move {
        let _ = warroom::drift::run_weekly_summary(&cfg, window, limit, post).await;
        warroom::drift::release_lock();
    });

    axum::Json(json!({"status": "started", "window": window, "limit": limit})).into_response()
}

#[derive(Deserialize)]
struct WeeklyHistoryQuery {
    limit: Option<usize>,
}

async fn drift_weekly_history(Query(q): Query<WeeklyHistoryQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(12).min(52);
    axum::Json(json!({
        "summaries": warroom::drift::weekly_history(limit),
    }))
}

// ───── Mapmaker briefs / map preview ──────────────────────────

#[derive(Deserialize)]
struct BriefsQuery {
    limit: Option<usize>,
}

async fn mapmaker_briefs_list(Query(q): Query<BriefsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).min(200);
    axum::Json(json!({
        "briefs": warroom::mapmaker::list_briefs(limit),
    }))
}

async fn mapmaker_brief_get(Path(name): Path<String>) -> impl IntoResponse {
    match warroom::mapmaker::get_brief(&name) {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": format!("brief {} not found", name)})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct MapPreviewBody {
    dir_path: String,
}

async fn map_preview(axum::Json(body): axum::Json<MapPreviewBody>) -> impl IntoResponse {
    let result = warroom::safe_map::gather_map_preview(&body.dir_path);
    if result.get("error").is_some() {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(result)).into_response();
    }
    axum::Json(result).into_response()
}

#[derive(Deserialize)]
struct MapmakerRunBody {
    dir_path: String,
    task: String,
    #[serde(default = "default_auto")]
    model: String,
}
fn default_auto() -> String {
    "auto".into()
}

async fn mapmaker_run(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<MapmakerRunBody>,
) -> impl IntoResponse {
    let model = match warroom::mapmaker::MapmakerModel::parse(&body.model) {
        Some(m) => m,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"detail": format!("unknown model: {}", body.model)})),
            )
                .into_response();
        }
    };
    let result =
        warroom::mapmaker::run_mapmaker(&state.config, &body.dir_path, &body.task, model).await;
    if result.get("error").is_some() {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(result)).into_response();
    }
    axum::Json(result).into_response()
}

// ───── Embeddings ─────────────────────────────────────────────

async fn embeddings_stats() -> impl IntoResponse {
    axum::Json(warroom::embeddings::stats())
}

#[derive(Deserialize)]
struct RebuildQuery {
    #[serde(default)]
    force: bool,
}

/// JSON body for successful `POST /api/precedent/reindex`.
pub(crate) fn precedent_reindex_success_json(count: usize) -> serde_json::Value {
    json!({ "reindexed": count })
}

async fn precedent_reindex() -> impl IntoResponse {
    let join = tokio::task::spawn_blocking(precedent::reindex).await;
    match join {
        Ok(Ok(count)) => axum::Json(precedent_reindex_success_json(count)).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("join: {}", e) })),
        )
            .into_response(),
    }
}

async fn embeddings_rebuild(Query(q): Query<RebuildQuery>) -> impl IntoResponse {
    // First pass through the model can take ~30s for download. Offload off
    // the request thread so axum keeps responding.
    let force = q.force;
    let result = tokio::task::spawn_blocking(move || warroom::embeddings::build_index(force))
        .await
        .unwrap_or_else(|e| json!({"built": false, "error": format!("join: {}", e)}));
    if result.get("error").is_some() && result.get("built").and_then(|x| x.as_bool()) != Some(true)
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(result),
        )
            .into_response();
    }
    axum::Json(result).into_response()
}

// ───── Meta-review ───────────────────────────────────────────

async fn meta_review_run(State(state): State<AppState>) -> impl IntoResponse {
    if warroom::meta_review::is_running() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "meta-review already in progress"})),
        )
            .into_response();
    }
    if !warroom::meta_review::acquire_lock() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "could not acquire meta-review lock"})),
        )
            .into_response();
    }
    let tera = state.config.tera.clone();
    let result = tokio::task::spawn_blocking(move || {
        let r = warroom::meta_review::run(Some(&tera));
        warroom::meta_review::release_lock();
        r
    })
    .await
    .unwrap_or_else(|e| json!({"status": "error", "error": format!("join: {}", e)}));
    axum::Json(result).into_response()
}

async fn meta_review_latest() -> impl IntoResponse {
    match warroom::meta_review::latest() {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": "no meta-review report found"})),
        )
            .into_response(),
    }
}

// ───── Phase 0.5 — /api/deliberate (council endpoint) ────────────

const STATUS_CLIENT_CLOSED: u16 = 499;

fn handler_timeout() -> Duration {
    std::env::var("COUNCIL_HANDLER_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(crate::provider::request_timeout)
}

#[derive(Deserialize)]
struct DeliberateRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    max_tokens: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    temperature: Option<f64>,
    #[serde(default)]
    council_auto_escalate: Option<bool>,
    #[serde(default)]
    worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
    /// feature contract engine knobs (mode, tier, budget_max_usd, validate,
    /// validate_gate, blind, cabinet_name) — captured raw so
    /// `parse_deliberate_knobs` can share the WS field parsers and return the
    /// WS-style parse errors as 4xx. Unrecognized keys are ignored, matching
    /// the WS payload posture.
    #[serde(flatten)]
    knobs: serde_json::Map<String, serde_json::Value>,
}

fn openai_error(status: StatusCode, err_type: &str, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": { "type": err_type, "code": code, "message": message }
    });
    (status, axum::Json(body)).into_response()
}

#[derive(Debug)]
enum HandlerError {
    Cancelled,
    QuorumFailed(anyhow::Error),
    Unavailable(anyhow::Error),
    Internal(anyhow::Error),
}

fn classify_engine_error(e: anyhow::Error) -> HandlerError {
    let msg = format!("{:#}", e).to_lowercase();
    if msg.contains("cancelled") {
        HandlerError::Cancelled
    } else if msg.contains("quorum") || msg.contains("all seats failed") {
        HandlerError::QuorumFailed(e)
    } else if msg.contains("provider unavailable") || msg.contains("connection refused") {
        HandlerError::Unavailable(e)
    } else {
        HandlerError::Internal(e)
    }
}

/// Drop guard — fires `CancellationToken::cancel()` when the handler future
/// is dropped (client disconnect, response stream cancel, etc.).
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// `POST /api/deliberate` — Phase 0.5 council endpoint.
///
/// Body shape mirrors `/v1/chat/completions`. The Gateway adapter (see spec
/// §5.2) is responsible for wrapping the upstream user messages in the
/// injection-isolation envelope before forwarding here; this handler treats
/// the body verbatim.
///
/// feature contract: the body also accepts optional engine knobs — `mode`, `tier`,
/// `budget_max_usd`, `validate`, `validate_gate`, `blind`, `cabinet_name` —
/// with the same value rules as the WS start payload (shared parsers).
/// Invalid values 4xx in this handler's openai_error shape.
async fn deliberate_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<DeliberateRequest>,
) -> Response {
    // feature contract: engine-knob parity with the WS start payload — shared parsers,
    // Strict mode (invalid values 4xx instead of the WS silent coercion).
    let knobs = match parse_deliberate_knobs(&serde_json::Value::Object(req.knobs.clone())) {
        Ok(k) => k,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_parameter",
                &e,
            );
        }
    };

    let (base_model, requested_hash) = match req.model.split_once('@') {
        Some((m, h)) => (m, Some(h)),
        None => (req.model.as_str(), None),
    };

    if base_model == "council-audit" {
        let text = req
            .messages
            .iter()
            .filter_map(|m| {
                if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                    m.get("content").and_then(|c| c.as_str())
                } else {
                    None
                }
            })
            .next_back()
            .unwrap_or("");

        let id = text.trim();
        let id = id.strip_prefix("session_id:").unwrap_or(id).trim();
        let id = id.strip_prefix("trace_id:").unwrap_or(id).trim();

        match crate::precedent::load_session(id) {
            Some(session) => {
                let body = serde_json::json!({
                    "id":      format!("chatcmpl-{}", session.session_id),
                    "object":  "chat.completion",
                    "created": session.timestamp.timestamp(),
                    "model":   req.model,
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": serde_json::to_string_pretty(&session).unwrap_or_default() },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens":     0,
                        "completion_tokens": 0,
                        "total_tokens":      0
                    }
                });
                return axum::Json(body).into_response();
            }
            None => {
                return openai_error(
                    StatusCode::NOT_FOUND,
                    "invalid_request_error",
                    "session_not_found",
                    &format!("Session not found: {}", id),
                );
            }
        }
    }

    // feature contract: body cabinet_name (WS rule: existence checked at load) overrides
    // the model→cabinet mapping; otherwise the Phase 0.5 mapping holds.
    let (cabinet_name, cabinet_from_body) = match knobs.cabinet_name.as_deref() {
        Some(name) => (name.to_string(), true),
        None => match base_model {
            "council-triage" => ("triage".to_string(), false),
            "council-warroom" => ("warroom".to_string(), false),
            _ => {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "unknown_council_model",
                    &format!("Unknown council model: {}", base_model),
                );
            }
        },
    };

    // resolve_cabinet_owned (feature contract): registry hit clones; a miss falls
    // back to <base_dir>/cabinets/<name>.yaml so a cabinet saved after startup
    // (named in the request body) is launchable and hash-pinnable here exactly
    // as the engine resolves it. Built-in triage/warroom always hit the
    // registry, so a miss on those is a genuine server-side load failure.
    let cabinet = match state.config.resolve_cabinet_owned(&cabinet_name) {
        Ok(c) => c,
        // A cabinet the client named is a client error; a failure to load a
        // model-derived built-in (triage/warroom) is a server error.
        Err(e) if cabinet_from_body => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unknown_cabinet",
                &format!("{e:#}"),
            );
        }
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "cabinet_load_error",
                &format!("Failed to load cabinet: {e}"),
            );
        }
    };

    if let Some(h) = requested_hash
        && h != cabinet.hash
    {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "model_hash_mismatch",
            &format!(
                "Requested model hash {} does not match current cabinet configuration.",
                h
            ),
        );
    }

    // Sticky-pin: if unpinned, pin it for the session
    let model_in = if requested_hash.is_some() {
        req.model.clone()
    } else {
        format!("{}@{}", base_model, cabinet.hash)
    };

    // §6.5: X-Parent-Request-Id, when present, is the gateway's wrapper
    // request id; threaded onto every seat call so the ledger can fold seat
    // cost into the parent row.
    let parent_request_id = headers
        .get("X-Parent-Request-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let topic = req
        .messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|r| r.as_str())?;
            let content = m.get("content").and_then(|c| c.as_str())?;
            Some(format!("[{}]: {}", role, content))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let context = String::new();

    let cancel = CancellationToken::new();
    let _drop_guard = CancelOnDrop(cancel.clone());

    let req_ctx = RequestContext {
        parent_request_id,
        council_session_id: None, // engine fills this in after mint
        depth: 0,
        council_auto_escalate: req.council_auto_escalate.unwrap_or(false),
        // /api/deliberate keeps process-wide gateway routing (feature contract is a WS
        // surface) — None falls back to COUNCIL_VIA_GATEWAY.
        via_gateway: None,
        sensitivity: None,
    };

    // model_in already assigned sticky hash above
    let cancel_for_engine = cancel.clone();

    // Cost-exhaustion guard (audit #6): cap concurrent deliberations. Acquired
    // here — after cheap validation, so malformed requests never consume a slot,
    // and immediately before the expensive engine fan-out. Non-blocking: a full
    // pool fails fast with 429 instead of queueing (queued callers would pile up
    // holding connections + memory). The permit is held for the rest of the
    // handler and released on drop when it returns.
    let _deliberation_permit = match state.deliberate_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            // H1 saturation signal (matches the file's eprintln! warn convention):
            // a sustained stream of these is the cost-exhaustion guard doing its job
            // OR the cap being too low for legitimate load — operators tune via
            // COUNCIL_MAX_CONCURRENT_DELIBERATIONS. /api/health surfaces the live
            // permit count.
            eprintln!("⚠️  deliberate_at_capacity: 429 (audit #6 concurrency cap reached)");
            return openai_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "council_at_capacity",
                "Council is at capacity for concurrent deliberations; retry shortly",
            );
        }
    };

    // `tokio::select!` is not itself a Future — wrap it in an async block so
    // we can apply `tokio::time::timeout`.
    let outcome = tokio::time::timeout(handler_timeout(), async {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(HandlerError::Cancelled),
            r = engine_deliberate::run_with_cancel(
                &state.config,
                &cabinet_name,
                &topic,
                &context,
                // feature contract: knobs parsed from the request body; defaults match
                // the previous hardcoded literals (TearDown, blind=false,
                // budget $1.00, tier "best", validate off).
                knobs.mode,
                /* blind         */ knobs.blind,
                /* frame_check   */ true,
                /* verbose       */ false,
                /* budget_max    */ knobs.budget_max_usd.or(Some(1.0)),
                /* tier          */ &knobs.tier,
                /* validate      */ knobs.validate,
                /* validate_prov */ "grok_cli",
                /* validate_gate */ knobs.validate_gate,
                SessionOrigin::Api,
                req_ctx,
                req.worker_provenance,
                Some(cancel_for_engine.clone()),
            ) => r.map_err(classify_engine_error),
        }
    })
    .await;

    let session = match outcome {
        Ok(Ok(s)) => s,
        Ok(Err(HandlerError::Cancelled)) => {
            return openai_error(
                StatusCode::from_u16(STATUS_CLIENT_CLOSED).unwrap(),
                "client_error",
                "client_closed_request",
                "Client disconnected before deliberation finished",
            );
        }
        Ok(Err(HandlerError::QuorumFailed(e))) => {
            eprintln!("⚠️  quorum_failed: {:?}", e);
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "quorum_failed",
                "Deliberation failed to reach quorum",
            );
        }
        Ok(Err(HandlerError::Unavailable(e))) => {
            eprintln!("❌ council_unavailable: {:?}", e);
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "council_unavailable",
                "Council engine unreachable",
            );
        }
        Ok(Err(HandlerError::Internal(e))) => {
            eprintln!("❌ internal: {:?}", e);
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal_error",
                "Internal council error",
            );
        }
        Err(_elapsed) => {
            return openai_error(
                StatusCode::GATEWAY_TIMEOUT,
                "server_error",
                "council_timeout",
                "Deliberation exceeded budget",
            );
        }
    };

    // D2: structurally validate the directive_proposal_v1 fence before returning
    // 200. The triage Chair is contracted to emit a well-formed
    // `irin.directive.proposal.v1` fence; reject malformed machine-output with
    // 422 here so the gateway never dead-letters a 200-wrapped bad proposal. Gateway
    // stays the second validator (cross-field tenant / in_response_to exact-match
    // against the live escalation). Only runs for cabinets in directive-proposal mode.
    let is_directive_proposal = state
        .config
        .resolve_cabinet_owned(&cabinet_name)
        .map(|c| c.synthesis_mode == SynthesisMode::DirectiveProposalV1)
        .unwrap_or(false);
    if is_directive_proposal {
        let synthesis = session.synthesis.as_deref().unwrap_or("");
        if let Err(reason) =
            crate::engine::directive_fence::validate_directive_proposal_v1(synthesis)
        {
            eprintln!("⚠️  malformed_directive_proposal: {reason}");
            return openai_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "server_error",
                "malformed_directive_proposal",
                &format!("council-triage produced a malformed directive proposal: {reason}"),
            );
        }
    }

    // Token math — sum seat tokens across every round + chair tokens.
    let (seat_in, seat_out) = session
        .rounds
        .iter()
        .flat_map(|r| r.responses.iter())
        .fold((0u64, 0u64), |(p, c), s| {
            (p + s.tokens_in as u64, c + s.tokens_out as u64)
        });

    let chair_in = session.chair_tokens_in as u64;
    let chair_out = session.chair_tokens_out as u64;

    let prompt_tokens = seat_in + chair_in;
    let completion_tokens = seat_out + chair_out;
    let total_cost = session.total_cost_usd;

    let body = json!({
        "id":      format!("chatcmpl-{}", session.session_id),
        "object":  "chat.completion",
        "created": session.timestamp.timestamp(),
        "model":   model_in,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": session.synthesis.clone().unwrap_or_default()
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens":     prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens":      prompt_tokens + completion_tokens
        }
    });

    let mut body_obj = body.as_object().unwrap().clone();
    if session.specops_triggered {
        let usage_obj = body_obj.get_mut("usage").unwrap().as_object_mut().unwrap();
        usage_obj.insert(
            "extra_charges".to_string(),
            json!([{
                "reason": "specops_escalation",
                "cost_usd": session.specops_cost_usd,
            }]),
        );
    }

    let mut resp = axum::Json(body_obj).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&session.session_id) {
        h.insert("X-Council-Session-Id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("{:.4}", total_cost)) {
        h.insert("X-Total-Cost-Usd", v);
    }
    if let Ok(v) = HeaderValue::from_str(&chair_out.to_string()) {
        h.insert("X-Chair-Tokens", v);
    }
    if session.specops_triggered {
        h.insert(
            "X-Council-Specops-Triggered",
            HeaderValue::from_static("true"),
        );
    }
    resp
}

/// WebSocket paths whose auth is checked via `Sec-WebSocket-Protocol`
/// (`token.<secret>`) in the handler, not the bearer `auth_middleware`.
/// Browsers cannot set `Authorization` on a WS upgrade. Covers `/ws/deliberate`
/// and `/ws/librarian/{chat_id}` (R20).
fn is_ws_subprotocol_path(norm_path: &str) -> bool {
    let p = norm_path.trim_end_matches('/');
    p == "/ws/deliberate" || p == "/ws/librarian" || p.starts_with("/ws/librarian/")
}

/// Validate WebSocket upgrade when bearer auth is required (browser cannot send Authorization).
fn validate_ws_upgrade(headers: &HeaderMap) -> Result<(), StatusCode> {
    let auth_config = AUTH_CONFIG.get().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    if auth_config.dev_no_auth && auth_config.token.is_none() && auth_config.gateway_token.is_none()
    {
        return Ok(());
    }

    let Some(expected) = auth_config.token.as_ref() else {
        return Ok(());
    };

    let protocols = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut token_from_protocol: Option<&str> = None;
    for part in protocols
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(t) = part.strip_prefix("token.") {
            token_from_protocol = Some(t);
        }
    }

    match token_from_protocol {
        Some(t) if subtle_eq(t, expected) => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// WS /ws/deliberate — streaming deliberation.
async fn ws_deliberate(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if validate_ws_upgrade(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Browsers require the server to echo a negotiated subprotocol in the 101 response.
    // War Room sends `["council", "token.<secret>"]` (see warroom/web/lib/ws.ts); axum splits
    // comma-separated `Sec-WebSocket-Protocol` values and `protocols(["council"])` selects it.
    ws.protocols(["council"])
        .on_upgrade(move |socket| handle_ws(socket, state))
}

/// WS /ws/librarian/{chat_id} — streaming librarian ask (R20).
///
/// Same upgrade/auth posture as `/ws/deliberate`: subprotocol `token.<secret>`
/// validated by `validate_ws_upgrade`, bearer skipped in `auth_middleware` via
/// `is_ws_subprotocol_path`, and the negotiated `council` subprotocol echoed on
/// the 101 so browser WebSocket open succeeds.
async fn ws_librarian(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if validate_ws_upgrade(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.protocols(["council"])
        .on_upgrade(move |socket| handle_ws_librarian(socket, state, chat_id))
}

/// Handle a `/ws/librarian/{chat_id}` connection (R20).
///
/// Wire sequence: client sends `{type:"ask", text, client_msg_id}`; server emits
/// `ask_started` → zero or more `ask_chunk` → `sources` (if any) →
/// `ask_complete` → `done`, or `error` on failure.
///
/// The upstream librarian `/ask` is a single buffered POST (see
/// `LibrarianState::run_ask`) — there is no partial-streaming capability — so
/// this handler emits ZERO `ask_chunk` frames and does NOT fake-chunk the
/// finished string. The UI must handle the no-chunk case.
///
/// WS close mid-ask = cancel: the `run_ask` future is dropped when this task
/// returns, which is cancel-safe by feature contract semantics (owned permit drops,
/// upstream reqwest aborts, no wedged state). We drive `run_ask` concurrently
/// with a close watcher so a client `Stop` (socket close) aborts the in-flight
/// ask instead of waiting for the upstream timeout.
async fn handle_ws_librarian(socket: WebSocket, state: AppState, chat_id: String) {
    use serde_json::json;

    let (mut ws_tx, mut ws_rx) = socket.split();

    // First message must be {type:"ask", text, client_msg_id}.
    let first = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<serde_json::Value>(&text).ok(),
        _ => None,
    };
    let Some(first) = first else {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"expected first message {type:'ask'}"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };
    if first.get("type").and_then(|v| v.as_str()) != Some("ask") {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"expected first message {type:'ask'}"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }
    let text = first
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let client_msg_id = first
        .get("client_msg_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Validate lengths up front (mirror POST /ask guards).
    if text.is_empty() || text.len() > librarian::routes::USER_CONTENT_MAX {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"content length"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }
    if client_msg_id.is_empty() || client_msg_id.len() > librarian::routes::CLIENT_MSG_ID_MAX {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"client_msg_id length"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    let _ = ws_tx
        .send(Message::Text(
            json!({"type":"ask_started"}).to_string().into(),
        ))
        .await;

    // Drive run_ask concurrently with a close watcher: a client Stop (socket
    // close, or any inbound frame) cancels the ask by dropping the future.
    let ask_fut = state.librarian.run_ask(&chat_id, &text, &client_msg_id);
    let outcome = tokio::select! {
        biased;
        // Client closed / sent another frame → treat as cancel. Dropping
        // `ask_fut` here is the feature contract cancel path.
        _ = ws_rx.next() => {
            return;
        }
        out = ask_fut => out,
    };

    use librarian::routes::AskOutcome;
    match outcome {
        AskOutcome::Cached(result) => {
            // Idempotent replay: surface the cached assistant turn (zero chunks)
            // so a reconnect with the same client_msg_id is consistent.
            let assistant = result.get("assistant_turn").cloned().unwrap_or(json!({}));
            send_librarian_sources(&mut ws_tx, &assistant).await;
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"ask_complete","message":assistant})
                        .to_string()
                        .into(),
                ))
                .await;
            let _ = ws_tx
                .send(Message::Text(json!({"type":"done"}).to_string().into()))
                .await;
        }
        AskOutcome::Busy => {
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"error","message":"librarian busy"})
                        .to_string()
                        .into(),
                ))
                .await;
        }
        AskOutcome::Failed(_code, msg) => {
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"error","message":msg}).to_string().into(),
                ))
                .await;
        }
        AskOutcome::Ok { assistant_turn, .. } => {
            send_librarian_sources(&mut ws_tx, &assistant_turn).await;
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"ask_complete","message":assistant_turn})
                        .to_string()
                        .into(),
                ))
                .await;
            let _ = ws_tx
                .send(Message::Text(json!({"type":"done"}).to_string().into()))
                .await;
        }
    }
}

/// Emit a `{type:"sources", sources:[...]}` frame from an assistant turn, but
/// only when the turn actually has sources (the frame is optional per R20).
async fn send_librarian_sources(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    assistant_turn: &serde_json::Value,
) {
    if let Some(sources) = assistant_turn.get("sources").and_then(|v| v.as_array())
        && !sources.is_empty()
    {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"sources","sources":sources})
                    .to_string()
                    .into(),
            ))
            .await;
    }
}

/// Parsed fields from a War Room WS `{ type: "start", payload: ... }` message.
#[derive(Debug, Clone)]
pub(crate) struct WsStartFields {
    pub topic: String,
    pub cabinet_name: String,
    pub context: String,
    pub blind: bool,
    pub max_rounds: Option<u32>,
    pub pause_after_each_round: bool,
    pub frame_check: bool,
    /// Scope auditor (steering / boundary review) — if true, run the scope_auditor role.
    pub scope_auditor: bool,
    pub mode: Mode,
    pub custom_cabinet: Option<crate::types::Cabinet>,
    pub parent_session_id: Option<String>,
    pub swaps: Vec<serde_json::Value>,
    pub validate: bool,
    pub validate_provider: String,
    pub validate_gate: bool,
    pub worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
    pub budget_max_usd: Option<f64>,
    pub tier: String,
    pub then_tear_down: bool,
    pub auto_specops_threshold: f64,
    pub smoke_only: bool,
    /// Per-session gateway routing (feature contract). `None` falls back to the
    /// process-wide `COUNCIL_VIA_GATEWAY` state.
    pub via_gateway: Option<bool>,
    /// UPPERCASE GREEN/YELLOW/RED, normalized from the lowercase wire values.
    pub sensitivity: Option<String>,
    /// Direct-fire single-shot mode (feature contract): contrarian | munger | kiss |
    /// specops | premortem.
    pub direct_fire: Option<String>,
}

#[derive(Debug)]
pub(crate) struct WsStartParseOutcome {
    pub fields: WsStartFields,
    /// Non-fatal: `then_tear_down` requested with non-pathfind mode — coerced to Pathfind.
    pub coerce_then_tear_down: bool,
}

pub(crate) const WS_MAX_ROUNDS_CAP: u32 = 6;

pub(crate) fn normalize_ws_tier(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("sovereign") => "sovereign".to_string(),
        Some("strict_sovereign") => "strict_sovereign".to_string(),
        Some("best") => "best".to_string(),
        _ => "best".to_string(),
    }
}

pub(crate) fn clamp_ws_max_rounds(requested: Option<u32>, cabinet_rounds: u32) -> u32 {
    let cap = cabinet_rounds.min(WS_MAX_ROUNDS_CAP);
    requested.unwrap_or(cabinet_rounds).clamp(1, cap)
}

/// Strictness for the shared engine-knob field parsers (feature contract).
///
/// The WS start payload keeps its Phase 5 wire contract: unknown `mode`/`tier`
/// and non-positive `budget_max_usd` silently coerce to defaults (mode-union
/// clients depend on it). `POST /api/deliberate` uses the SAME value rules via
/// the same parsers but rejects invalid values with a 4xx instead of coercing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnobStrictness {
    Lenient,
    Strict,
}

pub(crate) fn parse_mode_field(
    payload: &serde_json::Value,
    strict: KnobStrictness,
) -> Result<Mode, String> {
    let Some(v) = payload.get("mode").filter(|v| !v.is_null()) else {
        return Ok(Mode::TearDown);
    };
    match v.as_str() {
        Some("teardown") => Ok(Mode::TearDown),
        Some("pathfind") => Ok(Mode::Pathfind),
        Some("harden") => Ok(Mode::Harden),
        _ if strict == KnobStrictness::Strict => Err(format!(
            "mode: must be one of teardown|pathfind|harden, got {v}"
        )),
        _ => Ok(Mode::TearDown),
    }
}

pub(crate) fn parse_tier_field(
    payload: &serde_json::Value,
    strict: KnobStrictness,
) -> Result<String, String> {
    let raw = payload.get("tier");
    if strict == KnobStrictness::Strict
        && let Some(v) = raw.filter(|v| !v.is_null())
    {
        match v.as_str().map(str::trim) {
            Some("best" | "sovereign" | "strict_sovereign") => {}
            _ => {
                return Err(format!(
                    "tier: must be one of best|sovereign|strict_sovereign, got {v}"
                ));
            }
        }
    }
    Ok(normalize_ws_tier(raw.and_then(|v| v.as_str())))
}

/// Default upper bound for `budget_max_usd` on the strict REST path, in USD.
/// Overridable via `COUNCIL_MAX_BUDGET_USD`.
const DEFAULT_MAX_BUDGET_USD: f64 = 10.0;

/// Resolve the strict-path budget ceiling. Reads `COUNCIL_MAX_BUDGET_USD` and
/// falls back to `DEFAULT_MAX_BUDGET_USD` when unset or unparseable/non-positive.
fn max_budget_usd() -> f64 {
    std::env::var("COUNCIL_MAX_BUDGET_USD")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|&n| n.is_finite() && n > 0.0)
        .unwrap_or(DEFAULT_MAX_BUDGET_USD)
}

pub(crate) fn parse_budget_field(
    payload: &serde_json::Value,
    strict: KnobStrictness,
) -> Result<Option<f64>, String> {
    let Some(v) = payload.get("budget_max_usd").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let parsed = v.as_f64().filter(|&n| n.is_finite() && n > 0.0);
    if parsed.is_none() && strict == KnobStrictness::Strict {
        return Err(format!(
            "budget_max_usd: must be a finite number > 0, got {v}"
        ));
    }
    // Strict (REST) rejects an over-ceiling budget rather than silently
    // clamping; WS stays lenient and keeps whatever the client sent.
    if strict == KnobStrictness::Strict
        && let Some(n) = parsed
    {
        let max = max_budget_usd();
        if n > max {
            return Err(format!("budget_max_usd: must be <= {max}, got {n}"));
        }
    }
    Ok(parsed)
}

pub(crate) fn parse_bool_field(
    payload: &serde_json::Value,
    key: &str,
    default: bool,
    strict: KnobStrictness,
) -> Result<bool, String> {
    let Some(v) = payload.get(key).filter(|v| !v.is_null()) else {
        return Ok(default);
    };
    match v.as_bool() {
        Some(b) => Ok(b),
        None if strict == KnobStrictness::Strict => {
            Err(format!("{key}: must be a boolean, got {v}"))
        }
        None => Ok(default),
    }
}

/// Engine knobs accepted by `POST /api/deliberate` (feature contract) — same value rules
/// as the WS start payload, parsed Strict (invalid → 4xx).
#[derive(Debug, PartialEq)]
pub(crate) struct DeliberateKnobs {
    pub mode: Mode,
    pub tier: String,
    pub budget_max_usd: Option<f64>,
    pub validate: bool,
    pub validate_gate: bool,
    pub blind: bool,
    pub cabinet_name: Option<String>,
}

pub(crate) fn parse_deliberate_knobs(
    payload: &serde_json::Value,
) -> Result<DeliberateKnobs, String> {
    let strict = KnobStrictness::Strict;
    let mode = parse_mode_field(payload, strict)?;
    let tier = parse_tier_field(payload, strict)?;
    let budget_max_usd = parse_budget_field(payload, strict)?;
    let validate = parse_bool_field(payload, "validate", false, strict)?;
    let validate_gate = parse_bool_field(payload, "validate_gate", false, strict)?;
    let blind = parse_bool_field(payload, "blind", false, strict)?;
    let cabinet_name = match payload.get("cabinet_name").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v.as_str().map(str::trim) {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => {
                return Err(format!("cabinet_name: must be a non-empty string, got {v}"));
            }
        },
    };
    Ok(DeliberateKnobs {
        mode,
        tier,
        budget_max_usd,
        validate,
        validate_gate,
        blind,
        cabinet_name,
    })
}

/// Maximum accepted `topic` length, in bytes, on the WS deliberate entry points.
/// Without an explicit cap the only bound is tungstenite's ~64 MiB default frame
/// size, so a single client could pin server memory with a giant topic. 64 KiB is
/// far larger than any real deliberation prompt while keeping the parse cheap.
pub(crate) const MAX_WS_TOPIC_BYTES: usize = 64 * 1024;

/// Maximum accepted `context` length, in bytes, on the WS deliberate entry
/// points. Same unbounded-input surface as the topic, but `context` carries
/// supplementary background (often pasted material) rather than a one-line
/// prompt, so it gets its own, more generous cap — still far below tungstenite's
/// ~64 MiB default frame size that would otherwise be the only bound.
pub(crate) const MAX_WS_CONTEXT_BYTES: usize = 256 * 1024;

/// Shared WS start payload parsing for `handle_ws` and unit tests.
pub(crate) fn parse_ws_start_fields(
    payload: &serde_json::Value,
    smoke_only_env: bool,
) -> Result<WsStartParseOutcome, String> {
    let topic = payload
        .get("topic")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if topic.len() > MAX_WS_TOPIC_BYTES {
        return Err(format!(
            "topic: exceeds max length of {MAX_WS_TOPIC_BYTES} bytes (got {})",
            topic.len()
        ));
    }
    let cabinet_name = payload
        .get("cabinet_name")
        .and_then(|v| v.as_str())
        .unwrap_or("standard")
        .to_string();
    let context = payload
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if context.len() > MAX_WS_CONTEXT_BYTES {
        return Err(format!(
            "context: exceeds max length of {MAX_WS_CONTEXT_BYTES} bytes (got {})",
            context.len()
        ));
    }
    let blind = parse_bool_field(payload, "blind", false, KnobStrictness::Lenient)?;
    let max_rounds = payload
        .get("max_rounds")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let pause_after_each_round = payload
        .get("pause_after_each_round")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let frame_check = payload
        .get("frame_check")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let scope_auditor = payload
        .get("scope_auditor")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut mode = parse_mode_field(payload, KnobStrictness::Lenient)?;
    let then_tear_down = payload
        .get("then_tear_down")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut coerce_then_tear_down = false;
    if then_tear_down && mode != Mode::Pathfind {
        mode = Mode::Pathfind;
        coerce_then_tear_down = true;
    }
    let custom_cabinet: Option<crate::types::Cabinet> = payload
        .get("custom_cabinet")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let parent_session_id = payload
        .get("parent_session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let swaps: Vec<serde_json::Value> = payload
        .get("swaps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let validate = parse_bool_field(payload, "validate", false, KnobStrictness::Lenient)?;
    let validate_provider = payload
        .get("validate_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("grok_hermes")
        .to_string();
    let validate_gate = parse_bool_field(payload, "validate_gate", false, KnobStrictness::Lenient)?;
    let worker_provenance = match payload.get("worker_provenance") {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("worker_provenance: invalid JSON: {e}")),
    }?;
    let budget_max_usd = parse_budget_field(payload, KnobStrictness::Lenient)?;
    let tier = parse_tier_field(payload, KnobStrictness::Lenient)?;
    let auto_specops_threshold = payload
        .get("auto_specops_threshold")
        .and_then(|v| v.as_f64())
        .filter(|&n| n.is_finite() && (0.0..=1.0).contains(&n))
        .unwrap_or(0.8);
    // COUNCIL_WS_SMOKE_ONLY=1 means the server runs ONLY the synthetic shim —
    // a real (paid) deliberation must never fire on such a server, so the env
    // var alone forces smoke mode regardless of the payload. The env is the
    // sole authority: a client cannot force a synthetic run in production (env
    // unset) by sending smoke_only, nor request a real run when env is set.
    let smoke_only = smoke_only_env;
    let via_gateway = payload.get("via_gateway").and_then(|v| v.as_bool());
    // Pinned wire contract (feature contract): lowercase green|yellow|red ONLY — other
    // strings hard-error like worker_provenance. Normalized to UPPERCASE for
    // the provider layer / X-Sensitivity-Level gateway header.
    let sensitivity = match payload.get("sensitivity") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_str() {
            Some(level @ ("green" | "yellow" | "red")) => Some(level.to_ascii_uppercase()),
            _ => {
                return Err(format!(
                    "sensitivity: must be one of green|yellow|red, got {v}"
                ));
            }
        },
    };
    // Pinned wire contract (feature contract): unknown direct_fire values hard-error.
    let direct_fire = match payload.get("direct_fire") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_str() {
            Some(mode) if crate::engine::direct_fire::spec(mode).is_some() => {
                Some(mode.to_string())
            }
            _ => {
                return Err(format!(
                    "direct_fire: must be one of contrarian|munger|kiss|specops|premortem, got {v}"
                ));
            }
        },
    };

    Ok(WsStartParseOutcome {
        fields: WsStartFields {
            topic,
            cabinet_name,
            context,
            blind,
            max_rounds,
            pause_after_each_round,
            frame_check,
            scope_auditor,
            mode,
            custom_cabinet,
            parent_session_id,
            swaps,
            validate,
            validate_provider,
            validate_gate,
            worker_provenance,
            budget_max_usd,
            tier,
            then_tear_down,
            auto_specops_threshold,
            smoke_only,
            via_gateway,
            sensitivity,
            direct_fire,
        },
        coerce_then_tear_down,
    })
}

/// Handle a WebSocket connection for deliberation.
async fn handle_ws(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for the start message (first message must be {type: "start", payload: {...}})
    let start_msg = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v,
            Err(_) => {
                let err = StreamEvent::error("", "Config not JSON", true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            }
        },
        _ => return,
    };

    if start_msg.get("type").and_then(|v| v.as_str()) != Some("start") {
        let err = StreamEvent::error("", "Expected first message {type:'start'}", true);
        let _ = ws_tx
            .send(Message::Text(
                serde_json::to_string(&err).unwrap_or_default().into(),
            ))
            .await;
        return;
    }

    let payload = start_msg.get("payload").cloned().unwrap_or(json!({}));
    let ws_session_id = uuid::Uuid::new_v4().to_string()[..12].to_string();

    let parsed = match parse_ws_start_fields(&payload, ws_smoke_only_enabled()) {
        Ok(p) => p,
        Err(e) => {
            let err = StreamEvent::error(&ws_session_id, &e, true);
            let _ = ws_tx
                .send(Message::Text(
                    serde_json::to_string(&err).unwrap_or_default().into(),
                ))
                .await;
            return;
        }
    };
    let mut fields = parsed.fields;
    if parsed.coerce_then_tear_down {
        let _ = ws_tx
            .send(Message::Text(
                serde_json::to_string(&StreamEvent::info(
                    &ws_session_id,
                    "then_tear_down requires pathfind — mode coerced to pathfind",
                ))
                .unwrap_or_default()
                .into(),
            ))
            .await;
    }

    if let Some(map_dir) = payload
        .get("map_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        match warroom::safe_map::gather_map_context_for_deliberation(map_dir) {
            Ok(map_context) => {
                if !fields.context.is_empty() {
                    fields.context.push_str("\n\n---\n\n");
                }
                fields.context.push_str(&map_context);
            }
            Err(e) => {
                let err = StreamEvent::error(&ws_session_id, &format!("map_dir: {e}"), true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            }
        }
    }

    if fields.smoke_only {
        let session_id = "smoke-session";
        let smoke_via_gateway = fields
            .via_gateway
            .unwrap_or_else(provider::default_via_gateway);
        let smoke_sensitivity = fields
            .sensitivity
            .clone()
            .unwrap_or_else(provider::default_sensitivity);

        // Direct-fire synthetic single-shot (feature contract): canned synthesis, zero
        // provider spend, no disk writes. Mirrors the pinned real sequence:
        // session_started → synthesis_started → synthesis_complete →
        // session_saved → done.
        if let Some(ref slug) = fields.direct_fire {
            let Some(spec) = crate::engine::direct_fire::spec(slug) else {
                // Unreachable — parse_ws_start_fields rejected unknown modes.
                let err = StreamEvent::error(
                    session_id,
                    &format!("Unknown direct_fire mode: {slug}"),
                    true,
                );
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            };
            // Smoke shim: synthetic availability (the required provider is "available"
            // for canned direct-fire; mirrors the "do not filter" logic in the
            // non-direct-fire smoke branch below to avoid starving smoke paths
            // when real CLIs/APIs are absent in the smoke env).
            let available: Vec<(&'static str, bool)> = vec![(spec.provider, true)];
            let canned = format!(
                "[smoke] {} direct-fire synthesis for: {}",
                spec.display, fields.topic
            );
            let events = [
                StreamEvent::session_started(
                    session_id,
                    deliberate::direct_fire_session_started_data(
                        &fields.topic,
                        spec,
                        &available,
                        &fields.tier,
                        smoke_via_gateway,
                        &smoke_sensitivity,
                    ),
                ),
                StreamEvent::synthesis_started(session_id, spec.model),
                StreamEvent::synthesis_complete(session_id, &canned, spec.model, 0, 0.0, None),
                // Canned path — smoke mode never writes a session file.
                StreamEvent::session_saved(session_id, "sessions/smoke-session.json"),
                StreamEvent::done(
                    session_id,
                    0,
                    0.0,
                    0,
                    &canned,
                    1.0,
                    0,
                    Some(json!({ "direct_fire": slug })),
                ),
            ];
            for event in events {
                if ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&event).unwrap_or_default().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            return;
        }

        // resolve_cabinet_owned (feature contract): smoke session_started must see
        // cabinets saved after startup (disk fallback on a registry miss) so the
        // save→launch path is exercised end-to-end without a paid round.
        match state.config.resolve_cabinet_owned(&fields.cabinet_name) {
            Ok(cabinet) => {
                let available = provider::check_providers_with_gateway(smoke_via_gateway);
                // Synthetic shim: every seat streams regardless of provider
                // reachability (no real calls are made), so all cabinet seats are
                // active and none are dropped. Filtering by availability here would
                // empty the seat loop on a provider-less host and starve the
                // seat_chunk contract (phase9 N01).
                let active_seats = cabinet
                    .seats
                    .iter()
                    .map(|seat| {
                        json!({
                            "name": seat.name,
                            "provider": seat.provider,
                            "model": seat.model,
                        })
                    })
                    .collect::<Vec<_>>();
                let dropped_seats: Vec<serde_json::Value> = Vec::new();
                let rounds_planned = clamp_ws_max_rounds(fields.max_rounds, cabinet.rounds);
                let event = StreamEvent::session_started(
                    session_id,
                    json!({
                        "topic": fields.topic,
                        "cabinet_name": cabinet.name,
                        "rounds_planned": rounds_planned,
                        "mode": if fields.blind { "blind" } else { "normal" },
                        "active_seats": active_seats,
                        "dropped_seats": dropped_seats,
                        "chair": {
                            "provider": &cabinet.chair.provider,
                            "model": &cabinet.chair.model,
                        },
                        "available_providers": available
                            .iter()
                            .filter(|(_, ok)| *ok)
                            .map(|(name, _)| name)
                            .collect::<Vec<_>>(),
                        "council_version": env!("CARGO_PKG_VERSION"),
                        "stream_version": "rs-1.0.0",
                        "tier": fields.tier,
                        "then_tear_down": fields.then_tear_down,
                        "budget_max_usd": fields.budget_max_usd,
                        "auto_specops_threshold": fields.auto_specops_threshold,
                        // feature contract: also emitted by the real path
                        // (src/stream/deliberate.rs session_started) — keep in sync.
                        "via_gateway": smoke_via_gateway,
                        "execution_route": if smoke_via_gateway { "governed" } else { "direct" },
                        "sensitivity": smoke_sensitivity.to_lowercase(),
                    }),
                );
                if ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&event).unwrap_or_default().into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }

                // N01 smoke: synthetic seat/round/seat_complete/done loop with
                // THREE seat_chunk frames per seat (zero provider spend, no disk
                // writes). The real path lives in stream/deliberate.rs; this
                // mirrors its event ordering so the UI exercises chunk handling
                // without a paid round. Streaming-capable detection is irrelevant
                // here — smoke always emits synthetic chunks.
                let smoke_seats: Vec<(String, String, String)> = cabinet
                    .seats
                    .iter()
                    .map(|seat| (seat.name.clone(), seat.provider.clone(), seat.model.clone()))
                    .collect();
                let smoke_events = build_smoke_seat_events(
                    session_id,
                    rounds_planned.max(1),
                    &smoke_seats,
                    &cabinet.chair.provider,
                    &cabinet.chair.model,
                    &fields.topic,
                );
                for event in smoke_events {
                    if ws_tx
                        .send(Message::Text(
                            serde_json::to_string(&event).unwrap_or_default().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(e) => {
                let err =
                    StreamEvent::error(session_id, &format!("Cabinet load failed: {}", e), true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
            }
        }
        return;
    }

    // resolve_cabinet_owned (feature contract): clamp against the real cabinet's round
    // count even for cabinets saved after startup (disk fallback on miss).
    let cabinet_rounds = state
        .config
        .resolve_cabinet_owned(&fields.cabinet_name)
        .map(|c| c.rounds)
        .unwrap_or(WS_MAX_ROUNDS_CAP);
    let max_rounds = fields
        .max_rounds
        .map(|r| clamp_ws_max_rounds(Some(r), cabinet_rounds));

    let stream_config = StreamConfig {
        topic: fields.topic,
        cabinet_name: fields.cabinet_name,
        custom_cabinet: fields.custom_cabinet,
        context: fields.context,
        mode: fields.mode,
        blind: fields.blind,
        frame_check: fields.frame_check,
        scope_auditor: fields.scope_auditor,
        max_rounds,
        pause_after_each_round: fields.pause_after_each_round,
        auto_specops_threshold: fields.auto_specops_threshold,
        parent_session_id: fields.parent_session_id,
        swaps: fields.swaps,
        validate: fields.validate,
        validate_provider: fields.validate_provider,
        validate_gate: fields.validate_gate,
        worker_provenance: fields.worker_provenance,
        budget_max_usd: fields.budget_max_usd,
        tier: fields.tier,
        then_tear_down: fields.then_tear_down,
        via_gateway: fields.via_gateway,
        sensitivity: fields.sensitivity,
        direct_fire: fields.direct_fire,
    };

    // Channels
    let interventions = InterventionQueue::new();
    let intervention_sender = interventions.sender();
    let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(64);
    let cancel = CancellationToken::new();

    // Spawn the deliberation loop, but retain ownership so a disconnected
    // browser cannot leave provider work detached in the background.
    let config = state.config.clone();
    let run_cancel = cancel.clone();
    let run_handle = tokio::spawn(async move {
        deliberate::run(config, stream_config, event_tx, interventions, run_cancel).await;
    });

    // Spawn the intake loop (client → server interventions)
    let intake_cancel = cancel.clone();
    let intake_handle = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                && v.get("type").and_then(|t| t.as_str()) == Some("intervention")
                && let Some(payload) = v.get("payload")
                && let Some(action) = Intervention::from_value(payload)
            {
                let _ = intervention_sender.send(action).await;
            }
        }
        // Close frame, receive error, or EOF: stop the run promptly.
        intake_cancel.cancel();
    });

    // Forward events from deliberation → WebSocket
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            event = event_rx.recv() => match event {
                Some(event) => event,
                None => break,
            },
        };
        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if ws_tx.send(Message::Text(json.into())).await.is_err() {
            cancel.cancel();
            break; // Client disconnected
        }
    }

    cancel.cancel();
    intake_handle.abort();
    let _ = intake_handle.await;
    cancel_and_join_ws_run(cancel, run_handle, std::time::Duration::from_millis(750)).await;
}

/// Cancel the owned streaming run and give cooperative cleanup a short grace.
/// Returns `true` when the run stopped cooperatively; `false` when it required
/// an abort. Aborting drops in-flight local futures, but cannot retract a
/// request that an upstream provider already accepted.
async fn cancel_and_join_ws_run(
    cancel: CancellationToken,
    mut run_handle: tokio::task::JoinHandle<()>,
    grace: std::time::Duration,
) -> bool {
    cancel.cancel();
    if tokio::time::timeout(grace, &mut run_handle).await.is_ok() {
        return true;
    }
    run_handle.abort();
    let _ = run_handle.await;
    false
}

/// Build the synthetic seat/round/seat_complete/done event sequence for the
/// non-direct-fire smoke shim (N01). Emits, per round:
///   round_started → for each seat: seat_started, 3×seat_chunk, seat_complete
///   → convergence_scored → round_complete
/// then synthesis_started → synthesis_complete → session_saved → done.
///
/// THREE `seat_chunk` frames precede every `seat_complete`; `seat_complete.text`
/// is the authoritative full text (the three chunk deltas concatenated). Zero
/// provider spend, no disk writes — `session_saved` points at a canned path.
/// Synthetic N02 divergence points for the smoke shim — seats placed on a unit
/// circle so the UI scatter has plausible, deterministic geometry without any
/// embeddings call or provider spend.
pub(crate) fn smoke_divergence_points(
    seats: &[(String, String, String)],
) -> Vec<crate::warroom::divergence::DivergencePoint> {
    let n = seats.len();
    if n == 0 {
        return vec![];
    }
    seats
        .iter()
        .enumerate()
        .map(|(i, (name, _, _))| {
            let theta = (i as f64) * std::f64::consts::TAU / (n as f64);
            crate::warroom::divergence::DivergencePoint {
                seat: name.clone(),
                x: (theta.cos() * 1e6).round() / 1e6,
                y: (theta.sin() * 1e6).round() / 1e6,
            }
        })
        .collect()
}

pub(crate) fn build_smoke_seat_events(
    session_id: &str,
    rounds_planned: u32,
    seats: &[(String, String, String)],
    chair_provider: &str,
    chair_model: &str,
    topic: &str,
) -> Vec<StreamEvent> {
    const CHUNK_PARTS: [&str; 3] = ["[smoke] ", "synthetic ", "stream"];
    let mut events: Vec<StreamEvent> = Vec::new();
    let total_rounds = rounds_planned.max(1);

    for round_num in 1..=total_rounds {
        events.push(StreamEvent::round_started(
            session_id,
            round_num,
            total_rounds,
        ));
        for (name, provider, model) in seats {
            events.push(StreamEvent::seat_started(
                session_id, round_num, name, provider, model,
            ));
            for (seq, part) in CHUNK_PARTS.iter().enumerate() {
                events.push(StreamEvent::seat_chunk(
                    session_id, round_num, name, part, seq as u32,
                ));
            }
            // Authoritative full text = concatenated chunk deltas (the UI
            // replaces the accumulated chunks with this).
            let full_text = CHUNK_PARTS.concat();
            let resp = crate::types::SeatResponse {
                seat_name: name.clone(),
                provider: provider.clone(),
                model: model.clone(),
                text: full_text,
                round_num,
                latency_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                cached_in: 0,
                cost_usd: 0.0,
                error: None,
                gateway: None,
                provider_provenance: None,
            };
            events.push(StreamEvent::seat_complete(
                session_id,
                serde_json::to_value(&resp).unwrap_or_default(),
            ));
        }
        events.push(StreamEvent::convergence_scored(
            session_id, round_num, 1.0, true,
        ));
        // N02 smoke: synthetic round_divergence with plausible per-seat points
        // arranged on a circle (deterministic, no embeddings call). Only when
        // there are >= 2 seats — mirrors the real path's omit-when-<2 rule.
        if smoke_divergence_points(seats).len() >= 2 {
            events.push(StreamEvent::round_divergence(
                session_id,
                round_num,
                smoke_divergence_points(seats),
            ));
        }
        events.push(StreamEvent::round_complete(
            session_id, round_num, 1.0, true, false,
        ));
    }

    let canned = format!("[smoke] synthesis for: {topic}");
    let _ = chair_provider; // chair provider not surfaced in synthesis events
    events.push(StreamEvent::synthesis_started(session_id, chair_model));
    events.push(StreamEvent::synthesis_complete(
        session_id,
        &canned,
        chair_model,
        0,
        0.0,
        None,
    ));
    events.push(StreamEvent::session_saved(
        session_id,
        "sessions/smoke-session.json",
    ));
    events.push(StreamEvent::done(
        session_id,
        0,
        0.0,
        0,
        &canned,
        1.0,
        total_rounds,
        None,
    ));
    events
}

/// Paths that authenticate WebSocket via subprotocol, not Bearer.
#[cfg(test)]
pub(crate) fn ws_path_skips_bearer_auth(norm_path: &str) -> bool {
    is_ws_subprotocol_path(norm_path)
}

#[cfg(test)]
mod precedent_reindex_tests {
    use super::precedent_reindex_success_json;

    #[test]
    fn success_json_includes_reindexed_count() {
        let v = precedent_reindex_success_json(12);
        assert_eq!(v.get("reindexed").and_then(|x| x.as_u64()), Some(12));
    }
}

#[cfg(test)]
mod ws_cancel_tests {
    use super::cancel_and_join_ws_run;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn disconnect_cancellation_reaches_owned_run() {
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let observed = Arc::new(AtomicBool::new(false));
        let child_observed = observed.clone();
        let run = tokio::spawn(async move {
            child_cancel.cancelled().await;
            child_observed.store(true, Ordering::SeqCst);
        });

        let cooperative = cancel_and_join_ws_run(cancel, run, Duration::from_millis(100)).await;

        assert!(
            cooperative,
            "token-aware run should stop within the grace period"
        );
        assert!(observed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn disconnect_cleanup_aborts_an_uncooperative_run_after_grace() {
        let cancel = CancellationToken::new();
        let run = tokio::spawn(async { std::future::pending::<()>().await });

        let cooperative = cancel_and_join_ws_run(cancel, run, Duration::from_millis(10)).await;

        assert!(
            !cooperative,
            "stuck run must be aborted rather than detached"
        );
    }
}

#[cfg(test)]
mod deliberate_cap_tests {
    use super::{DEFAULT_MAX_DELIBERATIONS, resolve_max_deliberations};

    #[test]
    fn unset_uses_default() {
        assert_eq!(resolve_max_deliberations(None), DEFAULT_MAX_DELIBERATIONS);
    }

    #[test]
    fn valid_value_is_used() {
        assert_eq!(resolve_max_deliberations(Some("8".to_string())), 8);
        assert_eq!(resolve_max_deliberations(Some("1".to_string())), 1);
        // surrounding whitespace is tolerated
        assert_eq!(resolve_max_deliberations(Some("  3 ".to_string())), 3);
    }

    #[test]
    fn zero_falls_back_to_default_not_deadlock() {
        // A 0-permit Semaphore would block every deliberation forever — must
        // fail closed to the safe default, never to "no service".
        assert_eq!(
            resolve_max_deliberations(Some("0".to_string())),
            DEFAULT_MAX_DELIBERATIONS
        );
    }

    #[test]
    fn garbage_and_negative_fall_back_to_default() {
        for raw in ["", "abc", "-2", "4.5", "  "] {
            assert_eq!(
                resolve_max_deliberations(Some(raw.to_string())),
                DEFAULT_MAX_DELIBERATIONS,
                "input {raw:?} must fall back to default"
            );
        }
    }
}

#[cfg(test)]
mod normalize_index_entry_tests {
    use super::normalize_index_entry;
    use serde_json::json;

    #[test]
    fn normalize_index_entry_aliases_rust_era_keys() {
        let line =
            r#"{"session_id":"abc-123","timestamp":"2026-01-01T00:00:00","digest":"ruling text"}"#;
        let v = normalize_index_entry(line).unwrap();
        assert_eq!(v["id"], "abc-123");
        assert_eq!(v["ts"], "2026-01-01T00:00:00");
        assert_eq!(v["ruling_digest"], "ruling text");
    }

    #[test]
    fn normalize_index_entry_keeps_python_era_keys() {
        let line = r#"{"id":"py-1","ts":"2025-06-01T00:00:00","ruling_digest":"old"}"#;
        let v = normalize_index_entry(line).unwrap();
        assert_eq!(v["id"], "py-1");
        assert_eq!(v["ts"], "2025-06-01T00:00:00");
        assert_eq!(v["ruling_digest"], "old");
    }

    #[test]
    fn normalize_index_entry_fills_ui_defaults() {
        let v = normalize_index_entry(r#"{"id":"x"}"#).unwrap();
        assert_eq!(v["topic"], "");
        assert_eq!(v["keywords"], json!([]));
        assert_eq!(v["ruling_digest"], "");
        assert_eq!(v["confidence"], "");
        assert_eq!(v["cabinet"], "");
        assert_eq!(v["convergence"], 0.0);
        assert_eq!(v["mode"], "normal");
        assert_eq!(v["seat_count"], 0);
        assert_eq!(v["rounds"], 0);
        assert_eq!(v["synthesis_model"], "");
        assert_eq!(v["version"], "");
    }

    #[test]
    fn normalize_index_entry_preserves_existing_values() {
        let v = normalize_index_entry(r#"{"id":"x","mode":"wargame","synthesis_model":"opus"}"#)
            .unwrap();
        assert_eq!(v["mode"], "wargame");
        assert_eq!(v["synthesis_model"], "opus");
    }

    #[test]
    fn normalize_index_entry_skips_malformed_lines() {
        assert!(normalize_index_entry("").is_none());
        assert!(normalize_index_entry("   ").is_none());
        assert!(normalize_index_entry("{not json").is_none());
        // Valid JSON but not an object — skipped, matching the old inline loop.
        assert!(normalize_index_entry("42").is_none());
        assert!(normalize_index_entry("[1,2]").is_none());
    }
}

#[cfg(test)]
mod ws_upgrade_auth_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};

    fn install_auth(token: &str) {
        let _ = AUTH_CONFIG.get_or_init(|| AuthConfig {
            token: Some(token.to_string()),
            gateway_token: None,
            dev_no_auth: false,
        });
    }

    #[test]
    fn ws_path_skips_bearer_middleware() {
        assert!(ws_path_skips_bearer_auth("/ws/deliberate"));
        assert!(!ws_path_skips_bearer_auth("/api/health"));
    }

    #[test]
    fn validate_ws_upgrade_accepts_matching_subprotocol_without_bearer() {
        install_auth("ws-test-secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("council, token.ws-test-secret"),
        );
        assert_eq!(validate_ws_upgrade(&headers), Ok(()));
    }

    #[test]
    fn validate_ws_upgrade_rejects_wrong_subprotocol_token() {
        install_auth("ws-test-secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("council, token.wrong"),
        );
        assert_eq!(validate_ws_upgrade(&headers), Err(StatusCode::UNAUTHORIZED));
    }

    /// Minimal `Config` for router-level auth tests — no cabinets/models needed
    /// because the request is rejected by `auth_middleware` before any handler
    /// touches state.
    fn empty_config() -> Arc<Config> {
        Arc::new(Config {
            cabinets: std::collections::HashMap::new(),
            models: crate::types::ModelRegistry {
                models: std::collections::HashMap::new(),
            },
            roles: crate::types::RolesConfig::default(),
            tera: tera::Tera::default(),
            base_dir: std::env::temp_dir(),
        })
    }

    /// A bearer-protected mutating route must 401 when no Authorization header
    /// is present. POST /api/cabinets/save is covered by the router-wide
    /// `auth_middleware` (same posture as the WS subprotocol auth above) — this
    /// proves the wiring, not just the helper.
    #[tokio::test]
    async fn cabinets_save_requires_auth() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        // Ensure a token is set (consistent with the other tests in this
        // module; `get_or_init` makes repeated installs harmless).
        install_auth("ws-test-secret");

        let app = router(empty_config());
        let req = Request::builder()
            .method("POST")
            .uri("/api/cabinets/save")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"my-cab","yaml":"name: X\nrounds: 1\nseats: []\nchair: {name: c, provider: grok, model: grok-4}"}"#))
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "save without bearer must be rejected by auth_middleware"
        );
    }

    #[test]
    fn parse_ws_start_fields_budget_tier_then_tear_down_and_specops_threshold() {
        let payload = serde_json::json!({
            "topic": "smoke",
            "mode": "pathfind",
            "budget_max_usd": 0.01,
            "tier": "sovereign",
            "then_tear_down": true,
            "auto_specops_threshold": 0.65,
        });
        let out = parse_ws_start_fields(&payload, false).unwrap();
        assert_eq!(out.fields.budget_max_usd, Some(0.01));
        assert_eq!(out.fields.tier, "sovereign");
        assert!(out.fields.then_tear_down);
        assert!((out.fields.auto_specops_threshold - 0.65).abs() < f64::EPSILON);
        assert!(!out.coerce_then_tear_down);
    }

    #[test]
    fn parse_ws_start_fields_rejects_invalid_worker_provenance() {
        let payload = serde_json::json!({
            "worker_provenance": "not-an-object"
        });
        assert!(parse_ws_start_fields(&payload, false).is_err());
    }

    #[test]
    fn parse_ws_start_fields_accepts_gateway_and_direct_fire() {
        let payload = serde_json::json!({
            "topic": "smoke",
            "via_gateway": true,
            "sensitivity": "yellow",
            "direct_fire": "munger",
        });
        let out = parse_ws_start_fields(&payload, false).unwrap();
        assert_eq!(out.fields.via_gateway, Some(true));
        // Lowercase wire value normalized to the provider layer's UPPERCASE.
        assert_eq!(out.fields.sensitivity.as_deref(), Some("YELLOW"));
        assert_eq!(out.fields.direct_fire.as_deref(), Some("munger"));
    }

    #[test]
    fn parse_ws_start_fields_defaults_gateway_fields_to_none() {
        let out = parse_ws_start_fields(&serde_json::json!({ "topic": "x" }), false).unwrap();
        assert_eq!(out.fields.via_gateway, None);
        assert_eq!(out.fields.sensitivity, None);
        assert_eq!(out.fields.direct_fire, None);
    }

    #[test]
    fn parse_ws_start_fields_rejects_invalid_sensitivity() {
        // Pinned contract: lowercase green|yellow|red only — uppercase rejects too.
        for bad in [
            serde_json::json!("GREEN"),
            serde_json::json!("amber"),
            serde_json::json!(" red"),
            serde_json::json!(1),
        ] {
            let payload = serde_json::json!({ "sensitivity": bad });
            assert!(
                parse_ws_start_fields(&payload, false).is_err(),
                "sensitivity {bad} should be rejected"
            );
        }
    }

    #[test]
    fn parse_ws_start_fields_rejects_unknown_direct_fire() {
        for bad in [
            serde_json::json!("kiss-review"),
            serde_json::json!("MUNGER"),
            serde_json::json!("wargame"),
            serde_json::json!(""),
            serde_json::json!(7),
        ] {
            let payload = serde_json::json!({ "direct_fire": bad });
            assert!(
                parse_ws_start_fields(&payload, false).is_err(),
                "direct_fire {bad} should be rejected"
            );
        }
    }

    #[test]
    fn parse_ws_start_fields_caps_topic_length() {
        // Over-cap topic is rejected with a clear client error.
        let over = "a".repeat(MAX_WS_TOPIC_BYTES + 1);
        let payload = serde_json::json!({ "topic": over });
        assert!(
            parse_ws_start_fields(&payload, false).is_err(),
            "topic longer than {MAX_WS_TOPIC_BYTES} bytes should be rejected"
        );

        // A topic exactly at the cap is accepted.
        let at_cap = "a".repeat(MAX_WS_TOPIC_BYTES);
        let out = parse_ws_start_fields(&serde_json::json!({ "topic": at_cap }), false)
            .expect("topic exactly at the cap should be accepted");
        assert_eq!(out.fields.topic.len(), MAX_WS_TOPIC_BYTES);
    }

    #[test]
    fn parse_ws_start_fields_caps_context_length() {
        // Over-cap context is rejected with a clear client error.
        let over = "a".repeat(MAX_WS_CONTEXT_BYTES + 1);
        let payload = serde_json::json!({ "topic": "x", "context": over });
        assert!(
            parse_ws_start_fields(&payload, false).is_err(),
            "context longer than {MAX_WS_CONTEXT_BYTES} bytes should be rejected"
        );

        // Context exactly at the cap is accepted.
        let at_cap = "a".repeat(MAX_WS_CONTEXT_BYTES);
        let out = parse_ws_start_fields(
            &serde_json::json!({ "topic": "x", "context": at_cap }),
            false,
        )
        .expect("context exactly at the cap should be accepted");
        assert_eq!(out.fields.context.len(), MAX_WS_CONTEXT_BYTES);
    }

    #[test]
    fn parse_ws_start_fields_coerces_then_tear_down_to_pathfind() {
        let payload = serde_json::json!({
            "mode": "teardown",
            "then_tear_down": true
        });
        let out = parse_ws_start_fields(&payload, false).unwrap();
        assert!(out.coerce_then_tear_down);
        assert_eq!(out.fields.mode, Mode::Pathfind);
    }

    #[test]
    fn normalize_ws_tier_unknown_defaults_to_best() {
        assert_eq!(normalize_ws_tier(Some("bogus")), "best");
        assert_eq!(
            normalize_ws_tier(Some("strict_sovereign")),
            "strict_sovereign"
        );
    }

    /// Phase 5 regression pin: the WS payload stays LENIENT — unknown mode,
    /// tier, and non-positive budget silently coerce to defaults, never error.
    /// (feature contract strictness applies only to POST /api/deliberate.)
    #[test]
    fn parse_ws_start_fields_stays_lenient_for_mode_tier_budget() {
        let payload = serde_json::json!({
            "topic": "x",
            "mode": "bogus-mode",
            "tier": "bogus-tier",
            "budget_max_usd": -3.0,
            "blind": "not-a-bool",
        });
        let out = parse_ws_start_fields(&payload, false).unwrap();
        assert_eq!(out.fields.mode, Mode::TearDown);
        assert_eq!(out.fields.tier, "best");
        assert_eq!(out.fields.budget_max_usd, None);
        assert!(!out.fields.blind);
    }

    #[test]
    fn parse_deliberate_knobs_accepts_full_valid_set() {
        let payload = serde_json::json!({
            "mode": "pathfind",
            "tier": "sovereign",
            "budget_max_usd": 0.5,
            "validate": true,
            "validate_gate": true,
            "blind": true,
            "cabinet_name": "wargame",
        });
        let k = parse_deliberate_knobs(&payload).unwrap();
        assert_eq!(k.mode, Mode::Pathfind);
        assert_eq!(k.tier, "sovereign");
        assert_eq!(k.budget_max_usd, Some(0.5));
        assert!(k.validate);
        assert!(k.validate_gate);
        assert!(k.blind);
        assert_eq!(k.cabinet_name.as_deref(), Some("wargame"));
    }

    /// All knobs optional — defaults match the WS payload defaults.
    #[test]
    fn parse_deliberate_knobs_defaults_match_ws_defaults() {
        let k = parse_deliberate_knobs(&serde_json::json!({})).unwrap();
        assert_eq!(k.mode, Mode::TearDown);
        assert_eq!(k.tier, "best");
        assert_eq!(k.budget_max_usd, None);
        assert!(!k.validate);
        assert!(!k.validate_gate);
        assert!(!k.blind);
        assert_eq!(k.cabinet_name, None);
    }

    /// feature contract pinned contract: unknown/invalid values 4xx (Strict), unlike the
    /// lenient WS coercion — exercised per-field.
    #[test]
    fn parse_deliberate_knobs_rejects_invalid_values() {
        for (field, bad) in [
            ("mode", serde_json::json!("bogus")),
            ("mode", serde_json::json!(7)),
            ("tier", serde_json::json!("bogus")),
            ("tier", serde_json::json!("")),
            ("budget_max_usd", serde_json::json!(0)),
            ("budget_max_usd", serde_json::json!(-1.0)),
            ("budget_max_usd", serde_json::json!("free")),
            ("validate", serde_json::json!("yes")),
            ("validate_gate", serde_json::json!(1)),
            ("blind", serde_json::json!("true")),
            ("cabinet_name", serde_json::json!("")),
            ("cabinet_name", serde_json::json!(7)),
        ] {
            let payload = serde_json::json!({ field: bad });
            let err = parse_deliberate_knobs(&payload)
                .expect_err(&format!("{field}={payload} should be rejected"));
            assert!(
                err.starts_with(&format!("{field}:")),
                "error should name the field: {err}"
            );
        }
    }

    /// PR fix: the strict REST path enforces an upper budget ceiling (default
    /// 10.0). A value under the ceiling is accepted; an absurd value is
    /// rejected with a field-named error. Negative / non-numeric are still
    /// rejected by the pre-existing finite-and-positive guard. (NaN is not a
    /// representable JSON number — serde renders it as `null`, which is treated
    /// as an absent field, so it never reaches this path.)
    #[test]
    fn parse_deliberate_knobs_clamps_budget_to_max() {
        // Under the default ceiling — accepted.
        let ok = parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": 9.0 })).unwrap();
        assert_eq!(ok.budget_max_usd, Some(9.0));

        // Over the ceiling — rejected.
        let err = parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": 1e9 }))
            .expect_err("over-ceiling budget must be rejected");
        assert!(
            err.starts_with("budget_max_usd:"),
            "error names field: {err}"
        );

        // Negative and non-numeric are rejected by the finite/positive guard.
        for bad in [serde_json::json!(-1.0), serde_json::json!("free")] {
            assert!(
                parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": bad })).is_err(),
                "budget {bad} must be rejected"
            );
        }
    }

    /// The ceiling is overridable via COUNCIL_MAX_BUDGET_USD. This is the only
    /// test that mutates that var (no other test reads it), so the set→read→
    /// unset window does not race the parallel suite.
    #[test]
    fn budget_ceiling_honors_env_override() {
        // SAFETY: test-only env mutation; restored before returning.
        unsafe {
            std::env::set_var("COUNCIL_MAX_BUDGET_USD", "100.0");
        }
        let ok = parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": 50.0 }));
        let restore = ok.is_ok() && ok.as_ref().unwrap().budget_max_usd == Some(50.0);
        // SAFETY: test-only env mutation.
        unsafe {
            std::env::remove_var("COUNCIL_MAX_BUDGET_USD");
        }
        assert!(restore, "50.0 should pass when ceiling raised to 100.0");
    }

    /// WS stays lenient — a budget over the strict ceiling is kept verbatim,
    /// never rejected (mode-union clients depend on the Phase 5 contract).
    #[test]
    fn ws_budget_not_clamped() {
        let payload = serde_json::json!({ "topic": "x", "budget_max_usd": 1e9 });
        let out = parse_ws_start_fields(&payload, false).unwrap();
        assert_eq!(out.fields.budget_max_usd, Some(1e9));
    }

    /// Explicit nulls behave like absent fields on both parse paths.
    #[test]
    fn parse_deliberate_knobs_treats_null_as_absent() {
        let payload = serde_json::json!({
            "mode": null,
            "tier": null,
            "budget_max_usd": null,
            "validate": null,
            "blind": null,
            "cabinet_name": null,
        });
        let k = parse_deliberate_knobs(&payload).unwrap();
        assert_eq!(k.mode, Mode::TearDown);
        assert_eq!(k.tier, "best");
        assert_eq!(k.budget_max_usd, None);
        assert_eq!(k.cabinet_name, None);
    }

    #[tokio::test]
    async fn health_exposes_embedded_build_identity_without_local_base_dir() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        install_auth("ws-test-secret");
        let response = router(empty_config())
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("authorization", "Bearer ws-test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            payload["build_sha"].as_str().is_some_and(|sha| {
                sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
            "health must identify the running Council build: {payload}"
        );
        assert!(
            payload["build_dirty"].is_boolean(),
            "health must identify whether the embedded build was dirty: {payload}"
        );
        assert!(
            payload.get("base_dir").is_none(),
            "remote health must not expose the local Council base directory: {payload}"
        );
    }

    #[test]
    fn clamp_ws_max_rounds_respects_cabinet_and_cap() {
        assert_eq!(clamp_ws_max_rounds(Some(99), 2), 2);
        assert_eq!(clamp_ws_max_rounds(Some(4), 8), 4);
        assert_eq!(clamp_ws_max_rounds(None, 3), 3);
    }
}

#[cfg(test)]
mod smoke_seat_events_tests {
    use super::build_smoke_seat_events;

    #[test]
    fn emits_three_chunks_per_seat_before_seat_complete() {
        let seats = vec![
            (
                "Hawk".to_string(),
                "openrouter".to_string(),
                "m-a".to_string(),
            ),
            ("Owl".to_string(), "nous".to_string(), "m-b".to_string()),
        ];
        let events = build_smoke_seat_events("smoke-session", 1, &seats, "grok", "grok-4.3", "T");

        // Exactly one round.
        let round_starts = events
            .iter()
            .filter(|e| e.event_type == "round_started")
            .count();
        assert_eq!(round_starts, 1);

        // Per seat: 3 seat_chunk frames; total 6 across 2 seats.
        let chunks: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "seat_chunk")
            .collect();
        assert_eq!(chunks.len(), 6);

        // For each seat, the three chunks precede the seat_complete and carry
        // monotonic seq 0,1,2.
        for seat in ["Hawk", "Owl"] {
            let seat_chunk_idxs: Vec<usize> = events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.event_type == "seat_chunk" && e.data["seat_name"] == seat)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(seat_chunk_idxs.len(), 3, "seat {seat} chunk count");
            let complete_idx = events
                .iter()
                .position(|e| e.event_type == "seat_complete" && e.data["seat_name"] == seat)
                .expect("seat_complete present");
            for (expected_seq, idx) in seat_chunk_idxs.iter().enumerate() {
                assert!(*idx < complete_idx, "chunk must precede seat_complete");
                assert_eq!(events[*idx].data["seq"], expected_seq as u64);
            }
            // seat_complete.text is the authoritative full text (chunk concat).
            assert_eq!(
                events[complete_idx].data["text"],
                "[smoke] synthetic stream"
            );
        }

        // Terminal ordering: synthesis_complete → session_saved → done.
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"synthesis_complete"));
        assert_eq!(types.last(), Some(&"done"));
    }

    #[test]
    fn empty_seats_still_emits_round_and_done() {
        let events = build_smoke_seat_events("smoke-session", 2, &[], "grok", "grok-4.3", "T");
        let round_starts = events
            .iter()
            .filter(|e| e.event_type == "round_started")
            .count();
        assert_eq!(round_starts, 2);
        assert!(events.iter().all(|e| e.event_type != "seat_chunk"));
        assert_eq!(events.last().map(|e| e.event_type.as_str()), Some("done"));
    }
}

#[cfg(test)]
mod bind_hardening_tests {
    use super::*;

    /// Save/restore both COUNCIL_AUTH_TOKEN and COUNCIL_DEV_NO_AUTH so a
    /// developer's real env is not mutated by env-path tests. Serialized by a
    /// mutex: process env is global, so two guarded tests on parallel threads
    /// race save/restore (one removes the token while the other asserts on it).
    fn with_env_guard<F>(set_auth: Option<&str>, set_dev: Option<&str>, f: F)
    where
        F: FnOnce(),
    {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let orig_auth = std::env::var("COUNCIL_AUTH_TOKEN").ok();
        let orig_dev = std::env::var("COUNCIL_DEV_NO_AUTH").ok();
        unsafe {
            match set_auth {
                Some(v) => std::env::set_var("COUNCIL_AUTH_TOKEN", v),
                None => std::env::remove_var("COUNCIL_AUTH_TOKEN"),
            }
            match set_dev {
                Some(v) => std::env::set_var("COUNCIL_DEV_NO_AUTH", v),
                None => std::env::remove_var("COUNCIL_DEV_NO_AUTH"),
            }
        }
        f();
        unsafe {
            match orig_auth {
                Some(v) => std::env::set_var("COUNCIL_AUTH_TOKEN", v),
                None => std::env::remove_var("COUNCIL_AUTH_TOKEN"),
            }
            match orig_dev {
                Some(v) => std::env::set_var("COUNCIL_DEV_NO_AUTH", v),
                None => std::env::remove_var("COUNCIL_DEV_NO_AUTH"),
            }
        }
    }

    #[test]
    fn loopback_hosts_are_recognized() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
    }

    #[test]
    fn non_loopback_hosts_are_detected() {
        let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
        assert!(!is_loopback_host(&nl));
        assert!(!is_loopback_host("192.168.0.1"));
        assert!(!is_loopback_host("10.1.2.3"));
        assert!(!is_loopback_host("::"));
        assert!(!is_loopback_host("8.8.8.8"));
    }

    #[test]
    fn cors_loopback_origins_allowed_any_port() {
        for o in [
            "http://127.0.0.1:3011",
            "http://localhost:9999",
            "http://[::1]:3010",
            "http://127.0.0.1",
            "https://localhost:3010",
        ] {
            assert!(
                origin_is_loopback(&HeaderValue::from_str(o).unwrap()),
                "expected loopback: {o}"
            );
        }
    }

    #[test]
    fn cors_non_loopback_origins_rejected() {
        for o in [
            "https://evil.com",
            "http://192.168.1.20:3010",
            "http://device.example.ts.net:3010",
            "http://evil.com/127.0.0.1:3010",
            "http://127.0.0.1@evil.com",
            "http://127.0.0.1:80@evil.com",
            "http://[::1]@evil.com",
            "http://[::1]:80@evil.com",
            "http://[::1]x.evil.com",
            "tauri://localhost",
            "null",
        ] {
            assert!(
                !origin_is_loopback(&HeaderValue::from_str(o).unwrap()),
                "expected non-loopback: {o}"
            );
        }
    }

    #[test]
    fn default_loopback_bind_ok_without_token() {
        let r = resolve_serve_addr_with_token("127.0.0.1", 8765, false);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), "127.0.0.1:8765");
    }

    #[test]
    fn ipv6_loopback_bind_ok_without_token_bracketed() {
        // IPv6 literals are bracketed in resolved addr for bindability and URLs.
        let r = resolve_serve_addr_with_token("::1", 8765, false);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), "[::1]:8765");

        let r2 = resolve_serve_addr_with_token("[::1]", 8765, false);
        assert!(r2.is_ok());
        assert_eq!(r2.unwrap(), "[::1]:8765");
    }

    #[test]
    fn localhost_bind_ok_without_token() {
        let r = resolve_serve_addr_with_token("localhost", 3000, false);
        assert!(r.is_ok());
        // value not asserted to keep test focused; ipv4/6 covered elsewhere
    }

    #[test]
    fn non_loopback_with_auth_token_ok() {
        let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
        let r = resolve_serve_addr_with_token(&nl, 8765, true);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), format!("{}:{}", nl, 8765));
    }

    #[test]
    fn non_loopback_without_token_refuses_with_loud_error() {
        // PIN the fatal refusal: this exact error string from resolver is what
        // main prints then exit(1) with — startup cannot proceed to router/bind.
        let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
        let r = resolve_serve_addr_with_token(&nl, 8080, false);
        assert!(r.is_err());
        let err = r.unwrap_err();
        let expected = format!(
            "ERROR: Non-loopback bind to '{}' requested without COUNCIL_AUTH_TOKEN.\n\
             Council refuses to bind non-loopback addresses unless BOTH an explicit\n\
             non-loopback --host is given AND COUNCIL_AUTH_TOKEN is set.\n\
             COUNCIL_DEV_NO_AUTH=1 does NOT unlock non-loopback binding.\n\
             Set COUNCIL_AUTH_TOKEN=... or use --host 127.0.0.1 (default).",
            nl
        );
        assert_eq!(err, expected);
    }

    #[test]
    fn dev_no_auth_cannot_unlock_non_loopback() {
        // the has_auth_token=false case models "no AUTH_TOKEN even if dev set"
        // PIN refusal (not mere !ok) — resolver error drives fatal startup exit.
        let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
        let r = resolve_serve_addr_with_token(&nl, 8765, false);
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert!(err.contains("ERROR: Non-loopback bind to '0.0.0.0'"));
        assert!(err.contains("COUNCIL_AUTH_TOKEN"));
        // also for other non-loop like LAN IP
        let r2 = resolve_serve_addr_with_token("192.168.1.5", 8765, false);
        assert!(r2.is_err());
        let err2 = r2.unwrap_err();
        assert!(err2.contains("Non-loopback bind to '192.168.1.5'"));
    }

    #[test]
    fn resolve_env_without_token_refuses_nonloop() {
        // Direct env read path; save/restore both envs so dev's real vars survive.
        with_env_guard(None, None, || {
            let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
            let r = resolve_serve_addr(&nl, 8765);
            assert!(r.is_err());
            let err = r.unwrap_err();
            assert!(err.contains("Non-loopback bind to '0.0.0.0'"));
            assert!(err.contains("COUNCIL_AUTH_TOKEN"));
            assert!(err.contains("refuses to bind"));
        });
    }

    #[test]
    fn resolve_env_with_token_allows_nonloop() {
        // Save/restore prior values of BOTH vars (not remove) so dev env survives.
        with_env_guard(Some("test-token-for-bind"), None, || {
            let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
            let r = resolve_serve_addr(&nl, 8765);
            assert!(r.is_ok());
            // also pin that it succeeds with the token (resolver allows nonloop)
            assert_eq!(r.unwrap(), "0.0.0.0:8765");
        });
    }
}
