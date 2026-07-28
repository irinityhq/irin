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

mod cabinets;
mod deliberate;
mod drift;
mod embeddings;
mod health;
mod knobs;
mod mapmaker;
mod meta_review;
mod sessions;
mod ws;
mod ws_deliberate;

use sovereign_protocol::types::ProblemDetails;

pub(super) fn problem(
    status: axum::http::StatusCode,
    title: &str,
    detail: &str,
) -> axum::response::Response {
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
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{any, get, post},
};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::Config;
use crate::governance;
use crate::librarian;
use crate::static_web::WebDist;

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
pub(super) struct AuthConfig {
    pub(super) token: Option<String>,
    pub(super) gateway_token: Option<String>,
    pub(super) dev_no_auth: bool,
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
pub(super) fn is_loopback_host(host: &str) -> bool {
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
pub(super) fn subtle_eq(a: &str, b: &str) -> bool {
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
    let is_ws_subprotocol_auth = ws::is_ws_subprotocol_path(norm_path);

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

pub(super) static AUTH_CONFIG: std::sync::OnceLock<AuthConfig> = std::sync::OnceLock::new();

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
pub(super) const DEFAULT_MAX_DELIBERATIONS: usize = 4;

/// Resolve the concurrent-deliberation cap (audit #6) from a raw env value. Pure
/// (takes the already-read `Option<String>`, never reads env itself) so it is
/// unit-testable without the process-global env race. Any missing / unparseable /
/// zero value falls back to [`DEFAULT_MAX_DELIBERATIONS`] — it never returns 0,
/// because a 0-permit `Semaphore` would deadlock every deliberation (fail-closed
/// to a safe cap, not to "no service").
pub(super) fn resolve_max_deliberations(raw: Option<String>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_DELIBERATIONS)
}

/// Build the axum router with the API and WebSocket surface only.
///
/// Behaviour is unchanged from before `--web-dist` existed: an unmatched path
/// still falls through to axum's default 404 behind the auth layer.
pub fn router(config: Arc<Config>) -> Router {
    router_with_web_dist(config, None)
}

/// Build the axum router, optionally serving the War Room static export from
/// the same origin.
///
/// Real and unmatched `/api/**` and `/ws/**` paths remain inside the existing
/// auth layer. Only non-reserved unmatched paths can reach the public static
/// fallback.
pub fn router_with_web_dist(config: Arc<Config>, web_dist: Option<WebDist>) -> Router {
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

    let api = Router::new()
        .route("/api/health", get(health::health))
        .route("/api/discover", get(health::discover_providers))
        .route("/api/cabinets", get(cabinets::cabinets))
        .route("/api/cabinets/save", post(cabinets::cabinets_save_handler))
        .route("/api/sessions", get(sessions::sessions_list))
        .route("/api/sessions/{id}", get(sessions::session_detail))
        .route("/api/sessions/{id}/fork", post(sessions::session_fork))
        .route("/api/sessions/{id}/lineage", get(sessions::session_lineage))
        .route(
            "/api/sessions/{id}/export/pdf",
            post(sessions::session_export_pdf),
        )
        .route("/api/sessions/{a}/diff/{b}", get(sessions::session_diff))
        .route("/api/precedent", get(sessions::precedent_search))
        .route(
            "/api/precedent/reindex",
            post(embeddings::precedent_reindex),
        )
        .route("/api/interventions", get(sessions::interventions_list))
        .route(
            "/api/interventions/predict",
            get(sessions::interventions_predict),
        )
        .route("/api/patterns", get(sessions::patterns_aggregate))
        .route("/api/clusters", get(sessions::clusters_get))
        .route("/api/drift/reports", get(drift::drift_reports_list))
        .route("/api/drift/reports/{name}", get(drift::drift_report_get))
        .route("/api/drift/run", post(drift::drift_run))
        .route("/api/drift/weekly", get(drift::drift_weekly_latest))
        .route(
            "/api/drift/weekly/history",
            get(drift::drift_weekly_history),
        )
        .route("/api/drift/weekly/run", post(drift::drift_weekly_run))
        .route("/api/mapmaker/run", post(mapmaker::mapmaker_run))
        .route("/api/mapmaker/briefs", get(mapmaker::mapmaker_briefs_list))
        .route(
            "/api/mapmaker/briefs/{name}",
            get(mapmaker::mapmaker_brief_get),
        )
        .route("/api/map/preview", post(mapmaker::map_preview))
        .route("/api/embeddings/stats", get(embeddings::embeddings_stats))
        .route(
            "/api/embeddings/rebuild",
            post(embeddings::embeddings_rebuild),
        )
        .route("/api/meta-review/run", post(meta_review::meta_review_run))
        .route(
            "/api/meta-review/latest",
            get(meta_review::meta_review_latest),
        )
        .route("/api/deliberate", post(deliberate::deliberate_handler))
        // TODO(multi-tenant): /ws/deliberate is intentionally NOT behind
        // deliberate_semaphore (audit #6) — single-sovereign warroom UI, not
        // gateway-reachable. The moment it becomes gateway-reachable or serves a
        // second auth subject, it must inherit the same concurrency cap.
        .route("/ws/deliberate", get(ws::ws_deliberate))
        .route("/ws/librarian/{chat_id}", get(ws::ws_librarian))
        // Keep unknown reserved-prefix paths behind the same auth middleware
        // as real API and WebSocket routes. These also prevent the static SPA
        // fallback from turning an API typo into a 200 HTML response.
        .route("/api", any(reserved_not_found))
        .route("/api/{*path}", any(reserved_not_found))
        .route("/ws", any(reserved_not_found))
        .route("/ws/{*path}", any(reserved_not_found))
        .with_state(state)
        .nest("/api/librarian", librarian::routes::router(librarian_state))
        .nest("/api/governance", governance::router())
        .layer(middleware::from_fn(auth_middleware));

    let app = match web_dist {
        Some(dist) => {
            let dist = Arc::new(dist);
            api.fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
                let dist = Arc::clone(&dist);
                async move { dist.serve(&method, uri.path()).await }
            })
        }
        None => api,
    };

    app.layer(cors)
}

async fn reserved_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
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

// Preserve former `server::` paths for pub(crate) helpers used by tests/crate code.
#[allow(unused_imports)]
pub(crate) use embeddings::precedent_reindex_success_json;
#[allow(unused_imports)]
pub(crate) use knobs::{
    DeliberateKnobs, KnobStrictness, WsStartFields, WsStartParseOutcome, clamp_ws_max_rounds,
    normalize_ws_tier, parse_bool_field, parse_budget_field, parse_deliberate_knobs,
    parse_mode_field, parse_tier_field, parse_ws_start_fields,
};
#[allow(unused_imports)]
pub(crate) use sessions::normalize_index_entry;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use ws::ws_path_skips_bearer_auth;
#[allow(unused_imports)]
pub(crate) use ws_deliberate::{build_smoke_seat_events, smoke_divergence_points};

#[cfg(test)]
#[path = "bind_hardening_tests.rs"]
mod bind_hardening_tests;
