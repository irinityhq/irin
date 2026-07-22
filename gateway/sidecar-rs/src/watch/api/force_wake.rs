//! Force-wake and quarantine-clear admin surfaces.

use crate::watch::db::WatchDb;
use crate::watch::quarantine::{ClearOutcome, QuarantineState};
use crate::watch::runtime::{force_wake_pipeline, FireOutcome, QuarantineGate};
use crate::watch::{Sentinel, SentinelState};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use super::helpers::{
    admin_token_matches, json_response, problem, problem_with_id, problem_with_tenant_id,
    FORCE_WAKE_DEFAULT_TENANT,
};

/// Registry shape for `(tenant, sentinel_name) → Arc<dyn Sentinel>` lookup
/// used by force-wake. Built once at boot in `main.rs`, alongside the
/// sentinel Vec consumed by `WatchRunner::start`.
pub type ForceWakeRegistry = Arc<HashMap<(String, String), Arc<dyn Sentinel>>>;

/// T30 — `POST /watch/force-wake/{sentinel}` admin endpoint.
///
/// Auth: `Authorization: Bearer <admin-token>` via constant-time compare.
/// Skips `observe()` and `interesting()`; jumps straight to `escalate()` with
/// a synthetic `SentinelState`. Goes through the same audit-write path
/// (`QuarantineState::write_fire_row` → `WatchDb::insert_fire`) so the
/// resulting fire is hash-chained identically to a natural fire.
///
/// Response ordering: 401 (bad auth) → 404 (unknown sentinel) →
/// 409 (quarantined / hard_killed) → 200 (fired). Auth check comes first so
/// 404 does not leak sentinel existence to unauthenticated callers.
pub const FORCE_WAKE_DEFAULT_REASON: &str = "force-wake (admin)";

pub async fn force_wake_json(
    db: Arc<WatchDb>,
    registry: ForceWakeRegistry,
    quarantine: Arc<QuarantineState>,
    admin_token: String,
    bearer: Option<String>,
    sentinel_name: String,
    body: Option<Value>,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "request is missing valid credentials",
        );
    }
    let (tenant, reason) = parse_force_wake_body(body.as_ref());
    let key = (tenant.clone(), sentinel_name.clone());
    let Some(sentinel) = registry.get(&key).cloned() else {
        return problem_with_tenant_id(
            StatusCode::NOT_FOUND,
            "error",
            "unknown_sentinel",
            &tenant,
            &sentinel_name,
        );
    };

    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let synthetic = SentinelState {
        tenant: tenant.clone(),
        sentinel: sentinel_name.clone(),
        observed_at,
        payload: json!({"force_wake": true, "reason": reason.clone()}),
    };
    let outcome = force_wake_pipeline(&*sentinel, &quarantine, synthetic, reason).await;

    match outcome {
        FireOutcome::Fired(id) => match db.fetch_fire_by_id(id).await {
            Ok(Some(row)) => json_response(
                StatusCode::OK,
                json!({
                    "fire_id": row.id,
                    "hash": row.hash,
                    "fired_at": row.fired_at,
                }),
            ),
            Ok(None) => problem_with_id(
                StatusCode::INTERNAL_SERVER_ERROR,
                "error",
                "audit_row_missing",
                &id.to_string(),
            ),
            Err(e) => problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                &e.to_string(),
            ),
        },
        FireOutcome::Gated(QuarantineGate::Quarantined) => problem(
            StatusCode::CONFLICT,
            "quarantined",
            "sentinel is in quarantine",
        ),
        FireOutcome::Gated(QuarantineGate::HardKilled) => problem(
            StatusCode::CONFLICT,
            "hard-killed",
            "sentinel has been administratively killed",
        ),
        // Defensive wildcard. `force_wake_pipeline` filters `ProbationLogOnly`
        // into a reason-rewrite (spec §9.2 — probation is log-only, not
        // blocking), and `CooldownActive` is never produced by `is_blocked`.
        // The remaining variants (ObserveErr, Uninteresting) are unreachable
        // for the synthetic-state path. Any 500 here signals a contract bug
        // upstream of the handler, not a normal flow.
        other => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "error",
            &format!("unexpected_outcome: {other:?}"),
        ),
    }
}

fn parse_force_wake_body(body: Option<&Value>) -> (String, String) {
    let tenant = body
        .and_then(|v| v.get("tenant"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(FORCE_WAKE_DEFAULT_TENANT)
        .to_string();
    let reason = body
        .and_then(|v| v.get("reason"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(FORCE_WAKE_DEFAULT_REASON)
        .to_string();
    (tenant, reason)
}

/// T32 — `DELETE /watch/quarantine/{sentinel}` admin endpoint. Manual
/// quarantine + hard-kill release. Mirrors T30's auth ladder
/// (401 → 404 → 200) and constant-time bearer compare. Body is optional:
/// `{"tenant": "sovereign", "reset_probation": false}`. Maps
/// `reset_probation` → `skip_probation` straight (NOT negated).
///
/// Response 200 carries `cleared` (the labels of states actually cleared —
/// empty array for an already-healthy sentinel; idempotent) and
/// `probation_until` (Unix-ms; set only when a hard-kill cleared into the
/// 10-min log-only window).
///
/// 404 on unknown sentinel matches T30: typo'd names get a loud failure
/// rather than silent success. Hybrid 404/200 was the design call —
/// unknown_sentinel (registry miss) is distinct from "nothing to clear"
/// (healthy in-memory record).
// TODO(T35): hardcoded "sovereign" until WATCH_DEFAULT_TENANT env wiring
// sweeps this and FORCE_WAKE_DEFAULT_TENANT together in one commit.
pub const DELETE_QUARANTINE_DEFAULT_TENANT: &str = "sovereign";

pub async fn clear_quarantine_json(
    registry: ForceWakeRegistry,
    quarantine: Arc<QuarantineState>,
    admin_token: String,
    bearer: Option<String>,
    sentinel_name: String,
    body: Option<Value>,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "request is missing valid credentials",
        );
    }
    let (tenant, reset_probation) = parse_clear_quarantine_body(body.as_ref());
    let key = (tenant.clone(), sentinel_name.clone());
    if !registry.contains_key(&key) {
        return problem_with_tenant_id(
            StatusCode::NOT_FOUND,
            "error",
            "unknown_sentinel",
            &tenant,
            &sentinel_name,
        );
    }
    match quarantine
        .admin_clear_quarantine(&tenant, &sentinel_name, reset_probation)
        .await
    {
        Ok(out) => json_response(
            StatusCode::OK,
            clear_outcome_to_json(tenant, sentinel_name, out),
        ),
        Err(e) => problem_with_id(
            StatusCode::INTERNAL_SERVER_ERROR,
            "error",
            "persist_failed",
            &e.to_string(),
        ),
    }
}

fn parse_clear_quarantine_body(body: Option<&Value>) -> (String, bool) {
    let tenant = body
        .and_then(|v| v.get("tenant"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DELETE_QUARANTINE_DEFAULT_TENANT)
        .to_string();
    let reset_probation = body
        .and_then(|v| v.get("reset_probation"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (tenant, reset_probation)
}

fn clear_outcome_to_json(tenant: String, sentinel: String, out: ClearOutcome) -> Value {
    json!({
        "tenant": tenant,
        "sentinel": sentinel,
        "cleared": out.cleared,
        "probation_until": out.probation_until_ms,
    })
}
