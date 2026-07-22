//! W1b — admin-key auth gate on the `/ledger/*` HTTP surface.
//!
//! `GET /ledger/verify` + `GET /ledger/export` are network-exposed via nginx
//! (nginx.conf:382/387 → `lua/sidecar.lua::admin_proxy`) and were completely
//! unauthenticated: chain-validity readout + full audit-row exfil (payload,
//! metadata, caller_key, signatures, ≤10k/page). `POST /ledger/record` is NOT
//! network-exposed but is gated as defense-in-depth (audit-forgery on the UDS).
//!
//! The ledger lives on the `admin_proxy` surface, so W1b authorizes the way its
//! sibling admin routes do (`admin_provision_key`/`revoke`/`auth_rotate`,
//! main.rs:1238/1276/1311): `X-Admin-Key` header → `auth.check(key,"127.0.0.1")`
//! → require `allowed && tier=="admin"`. NOT the watch/outbox bearer model.
//!
//! The real `record_ledger`/`ledger_verify`/`ledger_export` handlers are private
//! to the binary crate, so — exactly as `force_wake_router` in `watch_api.rs`
//! does — these tests build a standalone router whose closures replicate the
//! EXACT `require_admin_header` gate (X-Admin-Key → real `auth.check` → tier),
//! wired to a REAL `AuditLedger` AND a REAL `AuthService` with provisioned keys.
//! This pins the full auth matrix per route:
//!   * no X-Admin-Key            → 401
//!   * junk (unknown) key        → 401
//!   * valid non-admin tier key  → 403
//!   * valid admin-tier key      → success (200 / normal response)

use axum::{
    body::{to_bytes, Body},
    extract::{Query, State},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use gateway_sidecar::auth::AuthService;
use gateway_sidecar::ledger::{AuditLedger, EventInput};
use serde::Deserialize;
use std::sync::Arc;
use tower::ServiceExt;

#[derive(Clone)]
struct LedgerState {
    ledger: Arc<AuditLedger>,
    auth: Arc<AuthService>,
}

#[derive(Deserialize)]
struct ExportQuery {
    #[serde(default = "default_export_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}
fn default_export_limit() -> u32 {
    1000
}

/// Test-side replica of `require_admin_header` in main.rs. Same algorithm:
/// X-Admin-Key missing/empty → 401; `auth.check` not-allowed → 401; allowed but
/// tier != "admin" → 403. Keep in lockstep with main.rs::require_admin_header.
async fn require_admin_header(
    auth: &AuthService,
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

fn ledger_router(state: LedgerState) -> Router {
    Router::new()
        .route(
            "/ledger/record",
            post(
                |State(s): State<LedgerState>,
                 headers: axum::http::HeaderMap,
                 Json(input): Json<EventInput>| async move {
                    if let Err((code, body)) = require_admin_header(&s.auth, &headers).await {
                        return (code, body).into_response();
                    }
                    match s.ledger.record_event(input).await {
                        Ok(event) => (
                            StatusCode::OK,
                            Json(serde_json::json!({"recorded": true, "hash": event.hash})),
                        )
                            .into_response(),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": e})),
                        )
                            .into_response(),
                    }
                },
            ),
        )
        .route(
            "/ledger/verify",
            get(
                |State(s): State<LedgerState>, headers: axum::http::HeaderMap| async move {
                    if let Err((code, body)) = require_admin_header(&s.auth, &headers).await {
                        return (code, body).into_response();
                    }
                    match s.ledger.verify_chain().await {
                        Ok(valid) => (StatusCode::OK, Json(serde_json::json!({"valid": valid})))
                            .into_response(),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": e})),
                        )
                            .into_response(),
                    }
                },
            ),
        )
        .route(
            "/ledger/export",
            get(
                |State(s): State<LedgerState>,
                 headers: axum::http::HeaderMap,
                 Query(q): Query<ExportQuery>| async move {
                    if let Err((code, body)) = require_admin_header(&s.auth, &headers).await {
                        return (code, body).into_response();
                    }
                    let limit = q.limit.min(10_000);
                    match s.ledger.export_events(limit, q.offset).await {
                        Ok(events) => (StatusCode::OK, Json(events)).into_response(),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": e})),
                        )
                            .into_response(),
                    }
                },
            ),
        )
        .with_state(state)
}

/// Real ledger + real auth service, both over temp files. Provisions one
/// admin-tier key (success path) and one default-tier key (403 path) on the
/// SAME AuthService instance so the in-memory pepper + key map are consistent
/// with `check`. Returns the two raw keys.
///
/// Dev env so `AuthService::new` does not panic: it requires either
/// `AUTH_PEPPER` set or `GATEWAY_AUTH_FAIL_CLOSED=false`. We set both
/// (idempotent — process-global, safe under cargo's parallel test threads).
async fn ledger_fixture() -> (tempfile::TempDir, LedgerState, String, String) {
    std::env::set_var("GATEWAY_AUTH_FAIL_CLOSED", "false");
    std::env::set_var("AUTH_PEPPER", "w1b-test-pepper");

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("ledger.db");
    let ledger = AuditLedger::new(db_path.to_str().unwrap(), None, None)
        .await
        .unwrap();

    let auth_cfg = tmp.path().join("auth_keys.json");
    let auth = AuthService::new(Some(auth_cfg));
    let admin = auth
        .provision_key("ledger_admin", "admin", 600, None)
        .await
        .unwrap();
    let user = auth
        .provision_key("ledger_user", "default", 600, None)
        .await
        .unwrap();

    let state = LedgerState {
        ledger: Arc::new(ledger),
        auth: Arc::new(auth),
    };
    (tmp, state, admin.raw_key, user.raw_key)
}

fn record_body() -> Body {
    Body::from(
        serde_json::to_vec(&serde_json::json!({
            "source": "test",
            "target": "test",
            "payload": {"k": "v"},
            "metadata": {},
        }))
        .unwrap(),
    )
}

// ---------------------------------------------------------------------------
// /ledger/record  (defense-in-depth — not network-exposed, same gate)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w1b_record_no_key_returns_401() {
    let (_tmp, state, _admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ledger/record")
                .header("content-type", "application/json")
                .body(record_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"], serde_json::Value::from("X-Admin-Key required"));
}

#[tokio::test]
async fn w1b_record_junk_key_returns_401() {
    let (_tmp, state, _admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ledger/record")
                .header("x-admin-key", "gw_not_a_real_key")
                .header("content-type", "application/json")
                .body(record_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn w1b_record_non_admin_tier_returns_403() {
    let (_tmp, state, _admin, user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ledger/record")
                .header("x-admin-key", user)
                .header("content-type", "application/json")
                .body(record_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn w1b_record_admin_key_succeeds() {
    let (_tmp, state, admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ledger/record")
                .header("x-admin-key", admin)
                .header("content-type", "application/json")
                .body(record_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["recorded"], serde_json::Value::from(true));
}

// ---------------------------------------------------------------------------
// /ledger/verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w1b_verify_no_key_returns_401() {
    let (_tmp, state, _admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/verify")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn w1b_verify_junk_key_returns_401() {
    let (_tmp, state, _admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/verify")
                .header("x-admin-key", "gw_not_a_real_key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn w1b_verify_non_admin_tier_returns_403() {
    let (_tmp, state, _admin, user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/verify")
                .header("x-admin-key", user)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn w1b_verify_admin_key_succeeds() {
    let (_tmp, state, admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/verify")
                .header("x-admin-key", admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["valid"], serde_json::Value::from(true));
}

// ---------------------------------------------------------------------------
// /ledger/export  (the network-exposed exfil surface + query-string carrier)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w1b_export_no_key_returns_401() {
    let (_tmp, state, _admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/export")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn w1b_export_junk_key_returns_401() {
    let (_tmp, state, _admin, _user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/export?limit=10&offset=0")
                .header("x-admin-key", "gw_not_a_real_key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn w1b_export_non_admin_tier_returns_403() {
    let (_tmp, state, _admin, user) = ledger_fixture().await;
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/export?limit=10")
                .header("x-admin-key", user)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn w1b_export_admin_key_succeeds_and_parses_query() {
    let (_tmp, state, admin, _user) = ledger_fixture().await;
    // The ?limit/?offset carried here is the exact param set admin_proxy used
    // to drop (path = ngx.var.uri) — the lua fix now appends ngx.var.args.
    let resp = ledger_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ledger/export?limit=5&offset=0")
                .header("x-admin-key", admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.is_array(), "export must return a JSON array");
}
