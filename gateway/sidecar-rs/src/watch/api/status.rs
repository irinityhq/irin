//! Read-only Watch status surfaces: list, temperature, and chain verification.

use crate::watch::db::WatchDb;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use super::helpers::{json_response, problem, problem_with_tenant};

pub const VERIFY_CHAIN_BUDGET: Duration = Duration::from_secs(5);

/// `GET /watch/list/{tenant}` — registered sentinels + per-sentinel
/// stats (last fire / fires in last hour / hard-kill marker).
pub async fn list_json(db: Arc<WatchDb>, tenant: String) -> Response {
    match db.list_registered(&tenant).await {
        Ok(rows) => json_response(
            StatusCode::OK,
            json!({
                "tenant": tenant,
                "sentinels": rows,
            }),
        ),
        Err(e) => problem_with_tenant(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
        ),
    }
}

/// T28: `GET /watch/temperature/{tenant}` — single-scalar liveness gauge.
///
/// Formula: `clamp01(0.7 * fires_1h/5 + 0.3 * fires_24h/24)`.
/// Levels (strict `<` thresholds, picked once and frozen):
///   * `temperature < 0.15` → `"cold"`
///   * `temperature < 0.6`  → `"warm"`
///   * otherwise            → `"hot"`
pub async fn temperature_json(db: Arc<WatchDb>, tenant: String) -> Response {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let one_hour_ago = now_ms - 3_600_000;
    let one_day_ago = now_ms - 86_400_000;
    let fires_1h = match db.count_fires_since(&tenant, one_hour_ago).await {
        Ok(n) => n,
        Err(e) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                &e.to_string(),
            );
        }
    };
    let fires_24h = match db.count_fires_since(&tenant, one_day_ago).await {
        Ok(n) => n,
        Err(e) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                &e.to_string(),
            );
        }
    };
    let raw = 0.7 * (fires_1h as f64 / 5.0) + 0.3 * (fires_24h as f64 / 24.0);
    let temperature = raw.clamp(0.0, 1.0);
    let level = if temperature < 0.15 {
        "cold"
    } else if temperature < 0.6 {
        "warm"
    } else {
        "hot"
    };
    json_response(
        StatusCode::OK,
        json!({
            "tenant": tenant,
            "temperature": temperature,
            "level": level,
            "fires_last_hour": fires_1h,
            "fires_last_24h": fires_24h,
        }),
    )
}

/// T31: walk the per-tenant hash chain, return the verification result.
///
/// Returns:
/// * `200 OK` + serialized `VerifyResult` on every chain (intact or
///   broken — `ok: false` is still a valid response).
/// * `504 Gateway Timeout` if the walk exceeds the 5s budget.
/// * `500 Internal Server Error` on any other db-level failure.
pub async fn verify_chain_json(db: Arc<WatchDb>, tenant: String) -> Response {
    match tokio::time::timeout(VERIFY_CHAIN_BUDGET, db.verify_chain(&tenant)).await {
        Err(_) => problem_with_tenant(
            StatusCode::GATEWAY_TIMEOUT,
            "error",
            "verify_chain exceeded 5s budget",
            &tenant,
        ),
        Ok(Err(e)) => problem_with_tenant(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
        ),
        Ok(Ok(result)) => match serde_json::to_value(&result) {
            Ok(body) => json_response(StatusCode::OK, body),
            // T33.9 P1-4 — VerifyResult Serialize impl is derived + total
            // over its fields, so this branch is practically unreachable.
            // But returning 200 with an `error` body was the prior failure
            // mode that broke client tooling, retries, and monitoring on
            // the contract surface; on serialization failure return 500 so
            // the operator semantics match every other handler in this
            // file (audit_json:155, list_json:54, force_wake_json:235,
            // clear_quarantine_json:380).
            Err(e) => problem_with_tenant(
                StatusCode::INTERNAL_SERVER_ERROR,
                "error",
                &format!("serialize VerifyResult: {e}"),
                &tenant,
            ),
        },
    }
}
