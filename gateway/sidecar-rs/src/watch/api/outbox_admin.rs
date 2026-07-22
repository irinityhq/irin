//! Directive outbox REST surface and tenant-policy admin.

use crate::keymgmt::directive_signing_key;
use crate::watch::db::WatchDb;
use crate::watch::outbox::{AckOutcome, DirectiveOutboxRecord};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde_json::{json, Value};
use sovereign_protocol::types::ProblemDetails;
use std::sync::Arc;

use super::helpers::{
    admin_token_matches, assert_canary_tenant, json_response, problem, problem_with_id,
    problem_with_tenant, problem_with_tenant_id,
};

// ==========================================================================
// P1 — Directive outbox REST surface (read/list + admin ack + pubkey)
// All handlers are deliberately narrow (take only the Arcs they need) so the
// library crate stays buildable in isolation. Tenant scope is enforced at
// every DB call and 404/401 is returned for cross-tenant or unauthed access.
// Signature verification bytes are *always* the stored envelope_json_canonical
// (never re-serialized). Ack is idempotent and returns 409 for dismissed/expired.
// ==========================================================================

/// `GET /watch/outbox/{tenant}?status=&limit=` — tenant-scoped list (newest first).
/// Returns the rows plus the canonical + signature fields needed for UI-side
/// Ed25519 verification against the pubkey from /watch/outbox/pubkey.
pub async fn list_outbox_json(
    db: Arc<WatchDb>,
    tenant: String,
    status: Option<String>,
    cursor: Option<String>,
    limit: i64,
    authed: bool,
    canary_tenant: &str,
) -> Response {
    // Admin-only (Invariant, Option 3): reject unauthed BEFORE
    // any tenant/cursor/store lookup. No store read on the unauthed path → a
    // 401-vs-403/404 status-differential oracle is structurally impossible, and
    // no projection / cadence / count is exposed. Constant 401, no tenant echo.
    if !authed {
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "request is missing valid credentials",
        );
    }
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }
    let requested_limit = limit.clamp(1, 200);
    let cursor_tuple = match cursor.as_deref().map(decode_outbox_cursor).transpose() {
        Ok(c) => c,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_cursor", "detail": e}),
            );
        }
    };

    match db
        .list_outbox(
            &tenant,
            status.as_deref(),
            requested_limit + 1,
            cursor_tuple,
        )
        .await
    {
        Ok(rows) => {
            let mut rows = rows;
            let has_more = rows.len() > requested_limit as usize;
            if has_more {
                rows.truncate(requested_limit as usize);
            }
            let next_cursor = if has_more {
                rows.last().map(encode_outbox_cursor)
            } else {
                None
            };
            let directives: Vec<Value> = rows.iter().map(outbox_record_to_json).collect();
            json_response(
                StatusCode::OK,
                json!({
                    "directives": directives,
                    "next_cursor": next_cursor,
                }),
            )
        }
        Err(e) => problem_with_tenant(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
        ),
    }
}

/// `GET /watch/outbox/{tenant}/{id}` — single row. A non-canary path tenant is
/// rejected with 403 `single_tenant_violation` (Wave-1 single-tenant tripwire)
/// BEFORE any DB lookup; a canary-tenant miss returns 404.
/// The envelope_json_canonical + signature_b64 are the exact bytes that were
/// signed; clients must verify against them + the pubkey, not a re-encode.
pub async fn get_outbox_json(
    db: Arc<WatchDb>,
    tenant: String,
    id: String,
    authed: bool,
    canary_tenant: &str,
) -> Response {
    // Admin-only (Invariant, Option 3): reject unauthed BEFORE
    // any tenant/id/store lookup, so no 401-vs-403/404 oracle and no projection
    // leak. Constant 401, no tenant/id echo.
    if !authed {
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "request is missing valid credentials",
        );
    }
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }
    match db.get_outbox(&tenant, &id).await {
        Ok(Some(rec)) if rec.tenant == tenant => {
            json_response(StatusCode::OK, outbox_record_to_json(&rec))
        }
        Ok(Some(_)) | Ok(None) => problem_with_tenant_id(
            StatusCode::NOT_FOUND,
            "not-found",
            "resource not found",
            &tenant,
            &id,
        ),
        Err(e) => problem_with_tenant_id(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
            &id,
        ),
    }
}

/// `GET /watch/outbox/pubkey` — current Ed25519 verifying key for all directives.
/// Shape is stable for rotation detection (kid changes on rotation).
/// Clients fetch once per session (or on kid change) and verify a directive's
/// signature against its envelope_json_canonical bytes — which are returned ONLY
/// on the AUTHED outbox read (outbox reads are admin-only per Invariant
/// `0dd59bc8-afc`, Option 3; there is no unauthed projection). The pubkey itself
/// stays public so an authed client need not re-fetch it over the privileged path.
pub async fn outbox_pubkey_json() -> Response {
    // The singleton is guaranteed initialized by the time any HTTP request
    // can arrive (load happens before the server starts accepting on the UDS).
    let key = directive_signing_key();
    let vk = key.verifying_key();
    let pubkey_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    json_response(
        StatusCode::OK,
        json!({
            "alg": "Ed25519",
            "kid": key.kid(),
            "kid_format": "sidecar-v1-{first8_hex_of_sha256(pubkey)}",
            "pubkey_b64": pubkey_b64,
            "note": "signature verification requires the exact envelope_json_canonical UTF-8 bytes, returned ONLY on the authed (admin-only) outbox read; there is no unauthed outbox projection"
        }),
    )
}

/// `POST /watch/outbox/{id}/ack` — admin-only idempotent ack.
/// Requires `X-Tenant-Scope` for tenant scoping. Returns 204 on success or
/// already-acked; 403 `single_tenant_violation` for a non-canary scope
/// (Wave-1 tripwire); 403 `tenant_scope_mismatch` on row/scope tenant
/// mismatch; 409 for dismissed/expired.
pub async fn ack_outbox_json(
    db: Arc<WatchDb>,
    admin_token: String,
    bearer: Option<String>,
    id: String,
    tenant_scope: Option<String>,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tenant = match tenant_scope.filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "tenant_scope_required"})),
            )
                .into_response();
        }
    };
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }

    match db.ack_outbox(&tenant, &id).await {
        Ok(AckOutcome::Acked { .. }) => StatusCode::NO_CONTENT.into_response(),
        Ok(AckOutcome::NotActionable { id, status }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "not_actionable",
                "id": id,
                "status": status,
                "message": "only staged directives can be acked; dismissed/expired are terminal"
            })),
        )
            .into_response(),
        Ok(AckOutcome::TenantMismatch { id }) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "tenant_scope_mismatch", "id": id})),
        )
            .into_response(),
        Ok(AckOutcome::NotFound { id }) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "not_found", "id": id, "tenant": tenant})),
        )
            .into_response(),
        Ok(AckOutcome::InvalidHandle { id }) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_handle", "id": id, "message": "claim handle does not match"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string(), "id": id, "tenant": tenant})),
        )
            .into_response(),
    }
}

/// Full authed serialization of an outbox record (plaintext, incl.
/// `envelope_json_canonical`). Outbox reads are **admin-only**: the unauthed
/// public hash projection from an earlier design was REMOVED (Invariant
/// `0dd59bc8-afc`, RATIFY Option 3 — the §6 cadence/tenant leak; a projection
/// without the authed-only preimage cannot self-verify, so it advertised a
/// capability that did not exist). `get_outbox_json`/`list_outbox_json` now
/// return 401 BEFORE any store lookup, so this serializer is only ever reached
/// on an authed path. Re-introducing a public verifiability surface is a Wave-2
/// ADR (transparency log; must publish the preimage or it cannot verify).
fn outbox_record_to_json(rec: &DirectiveOutboxRecord) -> Value {
    let envelope = serde_json::from_str::<Value>(&rec.envelope_json)
        .unwrap_or_else(|_| json!({"raw": rec.envelope_json}));
    json!({
        "id": rec.id.clone(),
        "in_response_to": rec.in_response_to.clone(),
        "tenant": rec.tenant.clone(),
        "status": rec.status.clone(),
        "verdict": rec.verdict.clone(),
        "authority": rec.authority.clone(),
        "created_at_ms": rec.created_at_ms,
        "envelope": envelope,
        "envelope_json_canonical": rec.envelope_json_canonical.clone(),
        "signature": {
            "alg": "Ed25519",
            "kid": rec.signing_kid.clone(),
            "value": rec.signature_b64.clone(),
        },
        "council_session_id": rec.council_session_id.clone(),
        "council_cost_usd": rec.council_cost_usd,
        "expires_at_ms": rec.expires_at_ms,
        "acked_at_ms": rec.acked_at_ms,
        "worker_provenance": rec.worker_provenance.as_ref().map(|g| serde_json::to_value(g).unwrap()).unwrap_or(Value::Null),
    })
}

fn encode_outbox_cursor(rec: &DirectiveOutboxRecord) -> String {
    let raw = format!("{}:{}", rec.created_at_ms, rec.id);
    base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
}

fn decode_outbox_cursor(cursor: &str) -> Result<(i64, String), String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(cursor)
        .map_err(|_| "cursor is not base64".to_string())?;
    let raw = String::from_utf8(decoded).map_err(|_| "cursor is not utf8".to_string())?;
    let (created_at, id) = raw
        .split_once(':')
        .ok_or_else(|| "cursor must be '<created_at_ms>:<id>'".to_string())?;
    let created_at = created_at
        .parse::<i64>()
        .map_err(|_| "cursor created_at_ms is not an integer".to_string())?;
    if id.is_empty() {
        return Err("cursor id is empty".to_string());
    }
    Ok((created_at, id.to_string()))
}

#[derive(serde::Deserialize)]
pub struct ClaimRequest {
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default = "default_lease_ms")]
    pub lease_duration_ms: i64,
}

fn default_limit() -> u32 {
    10
}
fn default_lease_ms() -> i64 {
    30_000
}

pub async fn claim_outbox_json(
    db: Arc<WatchDb>,
    admin_token: String,
    bearer: Option<String>,
    tenant_scope: Option<String>,
    req: ClaimRequest,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tenant = match tenant_scope.filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "tenant_scope_required"})),
            )
                .into_response();
        }
    };
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // Clamp DoS vectors before the DB call: bound fan-out (floor 1 so limit=0
    // can't floor-spin) and bound the lease window.
    let claim_limit = req.limit.clamp(1, 200);
    let lease_ms = req.lease_duration_ms.clamp(1_000, 300_000);

    match db
        .claim_outbox(&tenant, claim_limit, now_ms, lease_ms)
        .await
    {
        Ok(records) => {
            let json_records: Vec<Value> = records.iter().map(outbox_record_to_json).collect();
            json_response(
                StatusCode::OK,
                json!({"tenant": tenant, "claimed": json_records}),
            )
            .into_response()
        }
        Err(e) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct HeartbeatRequest {
    pub worker_provenance: sovereign_protocol::types::WorkerProvenanceGuard,
    #[serde(default = "default_lease_ms")]
    pub extension_ms: i64,
}

pub async fn heartbeat_outbox_json(
    db: Arc<WatchDb>,
    admin_token: String,
    bearer: Option<String>,
    tenant_scope: Option<String>,
    id: String,
    req: HeartbeatRequest,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tenant = match tenant_scope.filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => {
            return problem(
                StatusCode::BAD_REQUEST,
                "tenant-scope-required",
                "request must include tenant scope",
            )
        }
    };
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // Clamp the lease-extension window before the DB call (DoS bound).
    let ext_ms = req.extension_ms.clamp(1_000, 300_000);
    let claim_handle = req.worker_provenance.opaque_handle.unwrap_or_default();
    match db
        .heartbeat_outbox(&tenant, &id, &claim_handle, now_ms, ext_ms)
        .await
    {
        Ok(crate::watch::outbox::AckOutcome::Acked { .. }) => {
            tracing::debug!(
                tenant = %tenant,
                id = %id,
                extension_ms = %ext_ms,
                "worker lease extended (heartbeat via api)"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(crate::watch::outbox::AckOutcome::InvalidHandle { id }) => problem_with_id(
            StatusCode::BAD_REQUEST,
            "invalid-handle",
            "provided outbox handle is invalid or expired",
            &id,
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::NotActionable { id, status }) => json_response(
            StatusCode::CONFLICT,
            ProblemDetails::new("error", "not_actionable")
                .with_extension("id", id)
                .with_extension("status", status),
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::TenantMismatch { id }) => problem_with_id(
            StatusCode::FORBIDDEN,
            "tenant-scope-mismatch",
            "claim tenant does not match operation scope",
            &id,
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::NotFound { id }) => problem_with_id(
            StatusCode::NOT_FOUND,
            "not-found",
            "resource not found",
            &id,
        )
        .into_response(),
        Err(e) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct WorkerAckRequest {
    pub worker_provenance: sovereign_protocol::types::WorkerProvenanceGuard,
    #[serde(default)]
    pub worker_result: Option<serde_json::Value>,
    #[serde(default)]
    pub worker_metrics: Option<serde_json::Value>,
}

pub async fn worker_ack_outbox_json(
    db: Arc<WatchDb>,
    admin_token: String,
    bearer: Option<String>,
    tenant_scope: Option<String>,
    id: String,
    req: WorkerAckRequest,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tenant = match tenant_scope.filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => {
            return problem(
                StatusCode::BAD_REQUEST,
                "tenant-scope-required",
                "request must include tenant scope",
            )
        }
    };
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }

    // Pass the *full* worker_provenance (VerifiedExact etc) to be persisted on successful worker ack (so list/get return VerifiedExact).
    // Extract opaque_handle (only) to satisfy the lease check inside worker_ack_outbox.
    // claim_handle column remains the source of truth for in-flight lease validation.
    let claim_handle = req
        .worker_provenance
        .opaque_handle
        .clone()
        .unwrap_or_default();
    match db
        .worker_ack_outbox(&tenant, &id, &claim_handle, req.worker_provenance)
        .await
    {
        Ok(crate::watch::outbox::AckOutcome::Acked { .. }) => {
            tracing::info!(
                tenant = %tenant,
                id = %id,
                worker_result = ?req.worker_result,
                worker_metrics = ?req.worker_metrics,
                "worker execution completed (ack via api)"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(crate::watch::outbox::AckOutcome::InvalidHandle { id }) => problem_with_id(
            StatusCode::BAD_REQUEST,
            "invalid-handle",
            "provided outbox handle is invalid or expired",
            &id,
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::NotActionable { id, status }) => json_response(
            StatusCode::CONFLICT,
            ProblemDetails::new("error", "not_actionable")
                .with_extension("id", id)
                .with_extension("status", status),
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::TenantMismatch { id }) => problem_with_id(
            StatusCode::FORBIDDEN,
            "tenant-scope-mismatch",
            "claim tenant does not match operation scope",
            &id,
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::NotFound { id }) => problem_with_id(
            StatusCode::NOT_FOUND,
            "not-found",
            "resource not found",
            &id,
        )
        .into_response(),
        Err(e) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct NackRequest {
    pub worker_provenance: sovereign_protocol::types::WorkerProvenanceGuard,
    pub error_reason: String,
}

pub async fn nack_outbox_json(
    db: Arc<WatchDb>,
    admin_token: String,
    bearer: Option<String>,
    tenant_scope: Option<String>,
    id: String,
    req: NackRequest,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tenant = match tenant_scope.filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => {
            return problem(
                StatusCode::BAD_REQUEST,
                "tenant-scope-required",
                "request must include tenant scope",
            )
        }
    };
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }

    let claim_handle = req.worker_provenance.opaque_handle.unwrap_or_default();
    match db
        .nack_outbox(&tenant, &id, &claim_handle, &req.error_reason)
        .await
    {
        Ok(crate::watch::outbox::AckOutcome::Acked { .. }) => {
            tracing::warn!(
                tenant = %tenant,
                id = %id,
                error_reason = %req.error_reason,
                "worker execution blocked or failed (nack via api)"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(crate::watch::outbox::AckOutcome::InvalidHandle { id }) => problem_with_id(
            StatusCode::BAD_REQUEST,
            "invalid-handle",
            "provided outbox handle is invalid or expired",
            &id,
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::NotActionable { id, status }) => json_response(
            StatusCode::CONFLICT,
            ProblemDetails::new("error", "not_actionable")
                .with_extension("id", id)
                .with_extension("status", status),
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::TenantMismatch { id }) => problem_with_id(
            StatusCode::FORBIDDEN,
            "tenant-scope-mismatch",
            "claim tenant does not match operation scope",
            &id,
        )
        .into_response(),
        Ok(crate::watch::outbox::AckOutcome::NotFound { id }) => problem_with_id(
            StatusCode::NOT_FOUND,
            "not-found",
            "resource not found",
            &id,
        )
        .into_response(),
        Err(e) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
        ),
    }
}

pub async fn watch_get_tenant_policy(
    db: Arc<WatchDb>,
    tenant: String,
    canary_tenant: &str,
) -> Response {
    // Wave-1 single-tenant tripwire: a non-canary tenant policy can't even be
    // read (fail-loud) until per-tenant capability tokens land in Wave 2.
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }
    match db.get_tenant_policy(&tenant).await {
        Ok(Some(policy)) => json_response(StatusCode::OK, json!(policy)).into_response(),
        Ok(None) => problem_with_tenant(
            StatusCode::NOT_FOUND,
            "tenant-policy-not-found",
            "tenant policy does not exist",
            &tenant,
        ),
        Err(e) => problem_with_tenant(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
        ),
    }
}

pub async fn watch_set_tenant_policy(
    db: Arc<WatchDb>,
    admin_token: String,
    bearer: Option<String>,
    tenant: String,
    mut policy: crate::watch::db::TenantPolicy,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }

    policy.tenant = tenant.clone();
    match db.set_tenant_policy(policy).await {
        Ok(()) => json_response(StatusCode::OK, json!({"status": "ok"})).into_response(),
        Err(e) => problem_with_tenant(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
        ),
    }
}
