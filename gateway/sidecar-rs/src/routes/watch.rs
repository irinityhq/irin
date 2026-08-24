// Watch route handlers (moved from main.rs).

use axum::response::IntoResponse;
use std::sync::Arc;

use crate::watch;
use crate::AppState;

/// T31 — `GET /watch/verify-chain/:tenant`. Thin wrapper over
/// `watch::api::verify_chain_json`; the impl lives in the library crate so
/// integration tests can exercise the handler without spinning up AppState.
pub(super) async fn watch_verify_chain(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::verify_chain_json(state.watch_db.clone(), tenant).await
}

/// T27 — `GET /watch/list/:tenant`. Thin wrapper over `watch::api::list_json`.
pub(super) async fn watch_list(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::list_json(state.watch_db.clone(), tenant).await
}

/// T28 — `GET /watch/temperature/:tenant`. Thin wrapper over
/// `watch::api::temperature_json`.
pub(super) async fn watch_get_tenant_policy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::watch_get_tenant_policy(state.watch_db.clone(), tenant, &state.watch_canary_tenant)
        .await
}

pub(super) async fn watch_set_tenant_policy(
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

pub(super) async fn watch_temperature(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> impl IntoResponse {
    watch::api::temperature_json(state.watch_db.clone(), tenant).await
}

/// Gate 4 operator snapshot: admin-authenticated, canary-guarded, strict
/// whitelist projection. This is the only general Watch read exposed through
/// nginx; all mutation and arming routes remain UDS-only.
pub(super) async fn watch_ui_snapshot(
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
pub(super) async fn watch_force_wake(
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
pub(super) async fn watch_clear_quarantine(
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
pub(super) async fn watch_list_outbox(
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
    watch::api::list_outbox_json(
        state.watch_db.clone(),
        tenant,
        (status, cursor, limit),
        state.watch_admin_token.clone(),
        bearer,
        &state.watch_canary_tenant,
    )
    .await
}

/// P1 — `GET /watch/outbox/{tenant}/{id}`. A non-canary path tenant is rejected
/// with 403 `single_tenant_violation` (Wave-1 tripwire, fires before the DB
/// lookup); a canary-tenant miss returns 404. Admin-only: unauthed -> 401 before any
/// store lookup (Invariant, Option 3; D1/T1 projection removed).
pub(super) async fn watch_get_outbox(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path((tenant, id)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::get_outbox_json(
        state.watch_db.clone(),
        tenant,
        id,
        state.watch_admin_token.clone(),
        bearer,
        &state.watch_canary_tenant,
    )
    .await
}

/// P1 — `GET /watch/outbox/pubkey`. Public verification key for directives.
pub(super) async fn watch_outbox_pubkey() -> impl IntoResponse {
    watch::api::outbox_pubkey_json().await
}

/// P1 — `POST /watch/outbox/{id}/ack`. Requires `X-Tenant-Scope`.
pub(super) async fn watch_ack_outbox(
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

pub(super) async fn watch_claim_outbox(
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

pub(super) async fn watch_heartbeat_outbox(
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

pub(super) async fn watch_worker_ack_outbox(
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

pub(super) async fn watch_nack_outbox(
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

/// PR1 — `POST /watch/capability-token/mint`. Admin-bearer mint of a v1
/// structured execute capability token (token_id + directive bind).
pub(super) async fn watch_mint_capability_token(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<watch::api::MintCapabilityTokenRequest>,
) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    watch::api::mint_capability_token_json(
        state.watch_admin_token.clone(),
        bearer,
        body,
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
pub(super) async fn watch_stats(
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
pub(super) async fn watch_audit(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = q.get("limit").and_then(|s| s.parse::<i64>().ok());
    let before_id = q.get("before_id").and_then(|s| s.parse::<i64>().ok());
    watch::api::audit_json(state.watch_db.clone(), tenant, limit, before_id).await
}
