// Ledger route handlers + admin gate (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

use crate::auth;
use crate::ledger;
use crate::AppState;

#[derive(Deserialize)]
pub(super) struct RecordLedgerRequest {
    source: String,
    target: String,
    payload: serde_json::Value,
    metadata: serde_json::Value,
    #[serde(default)]
    caller_key: Option<String>,
}

#[derive(Serialize)]
pub(super) struct RecordLedgerResponse {
    recorded: bool,
    event_id: Option<i64>,
    hash: String,
    latency_ms: u64,
}

pub(super) async fn record_ledger(
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
pub(super) async fn require_admin_header(
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

pub(super) async fn ledger_verify(
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
pub(super) struct LedgerExportQuery {
    #[serde(default = "default_export_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}
fn default_export_limit() -> u32 {
    1000
}

pub(super) async fn ledger_export(
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
