// Auth / admin route handlers (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::keymgmt;
use crate::ledger;
use crate::AppState;

#[derive(Deserialize)]
pub(super) struct AuthCheckRequest {
    raw_key: String,
    ip: String,
}

#[derive(Deserialize)]
pub(super) struct IpCheckRequest {
    ip: String,
}

#[derive(Deserialize)]
pub(super) struct ProvisionKeyRequest {
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
pub(super) struct RevokeKeyRequest {
    key_id: String,
    admin_key: String,
}

#[derive(Deserialize)]
pub(super) struct RotateKeyRequest {
    admin_key: String,
}

pub(super) async fn auth_check(
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

pub(super) async fn auth_ip_check(
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

pub(super) async fn admin_provision_key(
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
    // Same CT comparator as the watch-plane admin bearer path — never plain `==`
    // on secret material (length/timing oracle).
    if crate::watch::api::admin_token_matches(&bootstrap_token, Some(admin_key.as_str())) {
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

pub(super) async fn admin_revoke_key(
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

pub(super) async fn auth_rotate_key(
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
