//! Admin-bearer mint for v1 structured execute capability tokens.
//!
//! Wraps [`DirectiveSigningKey::sign_capability_token`] with the fixed PR1 field
//! shape. Tenant-scoped + canary-guarded. Returns the signed token JSON once
//! to the admin caller — never logs raw token or signature material.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::json;

use super::helpers::{admin_token_matches, assert_canary_tenant, json_response, problem};
use crate::watch::dispatcher::MAX_CAPABILITY_TOKEN_LIFETIME_MS;

const DEFAULT_TOKEN_LIFETIME_MS: u64 = 60 * 60 * 1000; // 1h

#[derive(Debug, Deserialize)]
pub struct MintCapabilityTokenRequest {
    pub tenant: String,
    pub directive_id: String,
    /// Optional actor label recorded on the token (default: `operator`).
    #[serde(default)]
    pub actor: Option<String>,
    /// Optional absolute expiry (unix ms). Must be in (now, now+24h].
    #[serde(default)]
    pub expires_at: Option<u64>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn fresh_token_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("tok-{}", hex::encode(bytes))
}

/// `POST /watch/capability-token/mint` — admin-bearer, canary tenant.
///
/// Produces only v1 execute shapes: `token_id`, `directive_id` bind,
/// `subject=watch-producer`, `allowed_actions=["execute"]`,
/// `max_cost_usd=0.0`, `approval_required=true`, T21a expiry.
pub async fn mint_capability_token_json(
    admin_token: String,
    bearer: Option<String>,
    body: MintCapabilityTokenRequest,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    if let Some(resp) = assert_canary_tenant(&body.tenant, canary_tenant) {
        return resp;
    }
    if body.directive_id.trim().is_empty() {
        return problem(
            StatusCode::BAD_REQUEST,
            "invalid-mint-request",
            "directive_id must be non-empty",
        );
    }
    if body.tenant.trim().is_empty() {
        return problem(
            StatusCode::BAD_REQUEST,
            "invalid-mint-request",
            "tenant must be non-empty",
        );
    }

    let now = now_ms();
    let expires_at = match body.expires_at {
        Some(exp) => {
            if exp <= now || exp > now + MAX_CAPABILITY_TOKEN_LIFETIME_MS {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "invalid-mint-request",
                    "expires_at must be > now and remaining lifetime ≤ 24h",
                );
            }
            exp
        }
        None => now + DEFAULT_TOKEN_LIFETIME_MS,
    };

    let signing_key = match crate::keymgmt::try_directive_signing_key() {
        Some(k) => k,
        None => {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "signing-key-unavailable",
                "directive signing key is not initialized",
            );
        }
    };

    let token_id = fresh_token_id();
    let actor = body
        .actor
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "operator".to_string());

    let unsigned = sovereign_protocol::types::CapabilityToken {
        actor,
        subject: "watch-producer".to_string(),
        tenant: body.tenant.clone(),
        allowed_actions: vec!["execute".to_string()],
        approval_required: true,
        expires_at,
        max_cost_usd: Some(0.0),
        token_id: token_id.clone(),
        directive_id: body.directive_id.clone(),
        signature: None,
    };
    let signed = signing_key.sign_capability_token(unsigned);

    // Return once to the admin caller. Do not log raw token/signature.
    tracing::info!(
        tenant = %body.tenant,
        token_id = %token_id,
        directive_id = %body.directive_id,
        expires_at,
        "minted structured execute capability token"
    );

    json_response(
        StatusCode::OK,
        json!({
            "token_id": token_id,
            "directive_id": body.directive_id,
            "tenant": body.tenant,
            "expires_at": expires_at,
            "capability_token": signed,
        }),
    )
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymgmt::{directive_signing_key, DirectiveSigningKey};
    use crate::watch::api::helpers::CANARY_TENANT_DEFAULT;
    use crate::watch::db::WatchDb;
    use crate::watch::dispatcher::is_capability_token_valid;
    use rusqlite::Connection;

    #[tokio::test]
    async fn mint_rejects_missing_admin_token() {
        let resp = mint_capability_token_json(
            "admin-secret".to_string(),
            None,
            MintCapabilityTokenRequest {
                tenant: CANARY_TENANT_DEFAULT.to_string(),
                directive_id: "dir-1".to_string(),
                actor: None,
                expires_at: None,
            },
            CANARY_TENANT_DEFAULT,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mint_rejects_non_canary_tenant() {
        let resp = mint_capability_token_json(
            "admin-secret".to_string(),
            Some("admin-secret".to_string()),
            MintCapabilityTokenRequest {
                tenant: "other-tenant".to_string(),
                directive_id: "dir-1".to_string(),
                actor: None,
                expires_at: None,
            },
            CANARY_TENANT_DEFAULT,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mint_rejects_empty_directive_id() {
        let resp = mint_capability_token_json(
            "admin-secret".to_string(),
            Some("admin-secret".to_string()),
            MintCapabilityTokenRequest {
                tenant: CANARY_TENANT_DEFAULT.to_string(),
                directive_id: "  ".to_string(),
                actor: None,
                expires_at: None,
            },
            CANARY_TENANT_DEFAULT,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Success path: mint returns v1 field shape, signature verifies under the
    /// process-pinned key, and the token authorizes execute for its bind.
    #[tokio::test]
    async fn mint_success_exact_fields_sign_and_authorize() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("watch.db");
        let db = WatchDb::open(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
        let identity = tmp.path().join("directive_identity.json");
        // Publish the process-global key (mint signs through this seam).
        let _ = DirectiveSigningKey::load_or_initialize(&identity, &db)
            .await
            .unwrap();

        let directive_id = "dir-mint-success";
        let resp = mint_capability_token_json(
            "admin-secret".to_string(),
            Some("admin-secret".to_string()),
            MintCapabilityTokenRequest {
                tenant: CANARY_TENANT_DEFAULT.to_string(),
                directive_id: directive_id.to_string(),
                actor: Some("operator-a".to_string()),
                expires_at: None,
            },
            CANARY_TENANT_DEFAULT,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload["tenant"], CANARY_TENANT_DEFAULT);
        assert_eq!(payload["directive_id"], directive_id);
        let token_id = payload["token_id"].as_str().expect("token_id string");
        assert!(token_id.starts_with("tok-") && token_id.len() > 8);
        let response_expires_at = payload["expires_at"]
            .as_u64()
            .expect("response expires_at");

        let cap = payload
            .get("capability_token")
            .expect("capability_token object");
        assert_eq!(cap["actor"], "operator-a");
        assert_eq!(cap["subject"], "watch-producer");
        assert_eq!(cap["tenant"], CANARY_TENANT_DEFAULT);
        assert_eq!(cap["directive_id"], directive_id);
        assert_eq!(cap["token_id"], token_id);
        assert_eq!(cap["approval_required"], true);
        assert_eq!(cap["max_cost_usd"], 0.0);
        assert_eq!(
            cap["allowed_actions"],
            serde_json::json!(["execute"]),
            "v1 mint must pin allowed_actions exactly to [\"execute\"]"
        );
        assert_eq!(
            cap["expires_at"], response_expires_at,
            "capability_token.expires_at must match response expires_at"
        );
        assert!(
            cap["signature"].as_str().is_some_and(|s| !s.is_empty()),
            "mint must return a signature"
        );

        let typed: sovereign_protocol::types::CapabilityToken =
            serde_json::from_value(cap.clone()).unwrap();
        assert!(
            directive_signing_key().verify_capability_token(&typed),
            "minted token must verify under the process-global DirectiveSigningKey mint uses"
        );

        let token_json = serde_json::to_string(&typed).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        assert!(
            is_capability_token_valid(
                &conn,
                CANARY_TENANT_DEFAULT,
                &token_json,
                "execute",
                Some(directive_id),
            ),
            "minted token must authorize execute for its bound directive"
        );
        assert!(
            !is_capability_token_valid(
                &conn,
                CANARY_TENANT_DEFAULT,
                &token_json,
                "execute",
                Some("dir-other"),
            ),
            "minted token must refuse a foreign directive id"
        );
    }
}
