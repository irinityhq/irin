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

/// Core rotate path (handler + tests). `root_pubkey` Some → refuse with 409
/// before any staging write or ledger record.
pub(crate) async fn rotate_key_impl(
    root_pubkey: Option<&ed25519_dalek::VerifyingKey>,
    auth: &crate::auth::AuthService,
    admin_key: &str,
    ledger: &ledger::AuditLedger,
    ledger_signing_key: &ed25519_dalek::SigningKey,
    staging_path: &std::path::Path,
) -> (StatusCode, Json<serde_json::Value>) {
    if admin_key.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin_key required"})),
        );
    }
    let auth_decision = auth.check(admin_key, "127.0.0.1").await;
    if !auth_decision.allowed || auth_decision.tier != "admin" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin tier required for key rotation"})),
        );
    }

    // Root mode: online rotate can only emit active-key-signed introduce
    // envelopes, which are unauthorized under a configured ROOT. Operators
    // use the offline ceremony tooling with a root-signed envelope instead.
    if root_pubkey.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "online /auth/rotate is disabled when ROOT_PUBKEY_HEX is configured; \
                          use offline ceremony tooling (gateway-ceremony introduce) with a \
                          root-signed envelope"
            })),
        );
    }

    let (new_signing_key, new_key_bytes) = keymgmt::generate_keypair();
    let new_pubkey_hex = hex::encode(new_signing_key.verifying_key().as_bytes());

    let introduce_payload = keymgmt::sign_introduce(
        ledger_signing_key,
        &new_signing_key.verifying_key(),
        keymgmt::CeremonyPurpose::LedgerSigning,
    );

    let input = ledger::EventInput {
        source: "keymgmt".into(),
        target: ledger::EVENT_KEY_INTRODUCE.into(),
        payload: serde_json::to_value(&introduce_payload).unwrap(),
        metadata: serde_json::json!({
            "admin_key_id": auth_decision.key_id,
            "action": "rotation",
        }),
        caller_key: Some(auth_decision.key_id),
    };

    if let Some(parent) = staging_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(staging_path, new_key_bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to stage new key: {}", e)})),
        );
    }
    let _ = std::fs::set_permissions(
        staging_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    );

    match ledger.record_event(input).await {
        Ok(event) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "new_pubkey_hex": new_pubkey_hex,
                "new_key_staging_path": staging_path.display().to_string(),
                "introduce_event_id": event.id,
                "introduce_event_hash": event.hash,
                "deploy_instructions": [
                    format!("1. Inspect staged key at {}", staging_path.display()),
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

pub(super) async fn auth_rotate_key(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<RotateKeyRequest>,
) -> impl IntoResponse {
    let staging_path = std::env::var("LEDGER_NEW_KEY_STAGING_PATH")
        .unwrap_or_else(|_| "/run/sidecar/new_ledger_key.bin".to_string());
    rotate_key_impl(
        state.root_pubkey.as_ref(),
        &state.auth,
        &req.admin_key,
        &state.ledger,
        &state.ledger_signing_key,
        std::path::Path::new(&staging_path),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::post,
        Json, Router,
    };
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use tower::ServiceExt;

    /// Real rotate handler body with root configured: HTTP 409, no staging, no ledger event.
    #[tokio::test]
    async fn rotate_with_root_returns_409_no_side_effects() {
        std::env::set_var("GATEWAY_AUTH_FAIL_CLOSED", "false");
        std::env::set_var("AUTH_PEPPER", "rotate-root-test-pepper");

        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("ledger.db");
        let staging_path = tmp.path().join("new_ledger_key.bin");
        let auth_cfg = tmp.path().join("auth_keys.json");

        let seed = SigningKey::generate(&mut OsRng).to_bytes();
        let ledger = ledger::AuditLedger::new(db_path.to_str().unwrap(), Some(&seed), None, None)
            .await
            .unwrap();
        let sk = SigningKey::from_bytes(&seed);
        let root = SigningKey::generate(&mut OsRng).verifying_key();

        let auth = crate::auth::AuthService::new(Some(auth_cfg));
        let admin = auth
            .provision_key("rotate_admin", "admin", 600, None)
            .await
            .unwrap();
        let admin_key = admin.raw_key.clone();

        let before = ledger.export_events(100, 0).await.unwrap().len();
        assert!(!staging_path.exists());

        // HTTP oneshot → rotate_key_impl (same body as auth_rotate_key).
        let auth = Arc::new(auth);
        let ledger = Arc::new(ledger);
        let ledger_check = Arc::clone(&ledger);
        let sk = Arc::new(sk);
        let root = Arc::new(root);
        let staging = Arc::new(staging_path.clone());
        let app = Router::new().route(
            "/auth/rotate",
            post(move |Json(req): Json<RotateKeyRequest>| {
                let auth = Arc::clone(&auth);
                let ledger = Arc::clone(&ledger);
                let sk = Arc::clone(&sk);
                let root = Arc::clone(&root);
                let staging = Arc::clone(&staging);
                async move {
                    rotate_key_impl(
                        Some(root.as_ref()),
                        auth.as_ref(),
                        &req.admin_key,
                        ledger.as_ref(),
                        sk.as_ref(),
                        staging.as_ref(),
                    )
                    .await
                }
            }),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/rotate")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({ "admin_key": admin_key })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap_or("")
                .contains("ROOT_PUBKEY_HEX"),
            "body={v}"
        );
        assert!(
            !staging_path.exists(),
            "staging file must not be created on root refusal"
        );
        let after = ledger_check.export_events(100, 0).await.unwrap().len();
        assert_eq!(
            before, after,
            "no ledger event may be recorded on root refusal"
        );
    }
}
