//! Read-only Council BFF for Gateway governance surfaces.
//!
//! Browser clients authenticate only to Council. Council obtains the Gateway
//! admin credential from its process environment and never serializes it into
//! a response. The router deliberately contains GET routes only.

use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:18080";
const DEFAULT_CANARY_TENANT: &str = "sovereign";

struct GovernanceClient {
    http: reqwest::Client,
    gateway_base: reqwest::Url,
    admin_token: String,
    canary_tenant: String,
}

impl GovernanceClient {
    fn from_env() -> Result<Self, &'static str> {
        let admin_token = std::env::var("WATCH_ADMIN_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("BOOTSTRAP_TOKEN")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .ok_or("watch_admin_token_unavailable")?;
        let gateway_raw = std::env::var("GATEWAY_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string());
        let gateway_base = reqwest::Url::parse(&gateway_raw).map_err(|_| "gateway_url_invalid")?;
        if !matches!(gateway_base.scheme(), "http" | "https") {
            return Err("gateway_url_invalid");
        }
        let canary_tenant = std::env::var("WATCH_CANARY_TENANT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_CANARY_TENANT.to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(7))
            .build()
            .map_err(|_| "gateway_client_unavailable")?;
        Ok(Self {
            http,
            gateway_base,
            admin_token,
            canary_tenant,
        })
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url, &'static str> {
        let mut url = self.gateway_base.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| "gateway_url_invalid")?;
            path.clear();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    async fn get_json(&self, url: reqwest::Url) -> Result<Value, GatewayReadError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.admin_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| GatewayReadError::Unavailable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(GatewayReadError::Status(status.as_u16()));
        }
        response
            .json::<Value>()
            .await
            .map_err(|_| GatewayReadError::InvalidResponse)
    }
}

#[derive(Debug)]
enum GatewayReadError {
    Unavailable,
    Status(u16),
    InvalidResponse,
}

impl IntoResponse for GatewayReadError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Unavailable => (StatusCode::BAD_GATEWAY, "gateway_unavailable"),
            Self::Status(401 | 403) => (StatusCode::BAD_GATEWAY, "gateway_auth_failed"),
            Self::Status(404) => (StatusCode::BAD_GATEWAY, "gateway_route_unavailable"),
            Self::Status(_) | Self::InvalidResponse => {
                (StatusCode::BAD_GATEWAY, "gateway_response_invalid")
            }
        };
        (status, Json(json!({ "error": code }))).into_response()
    }
}

fn configuration_error(code: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": code })),
    )
        .into_response()
}

/// The BFF surface is GET-only. The parent Council router applies its normal
/// bearer middleware to every nested route.
pub fn router() -> Router {
    Router::new()
        .route("/watch", get(watch_snapshot))
        .route("/outbox", get(outbox_list))
        .route("/outbox/pubkey", get(outbox_pubkey))
        .route("/outbox/{id}", get(outbox_detail))
}

async fn watch_snapshot() -> Response {
    let client = match GovernanceClient::from_env() {
        Ok(client) => client,
        Err(code) => return configuration_error(code),
    };
    let url = match client.url(&["watch", "ui-snapshot", &client.canary_tenant]) {
        Ok(url) => url,
        Err(code) => return configuration_error(code),
    };
    match client.get_json(url).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn outbox_list() -> Response {
    let client = match GovernanceClient::from_env() {
        Ok(client) => client,
        Err(code) => return configuration_error(code),
    };
    let mut url = match client.url(&["watch", "outbox", &client.canary_tenant]) {
        Ok(url) => url,
        Err(code) => return configuration_error(code),
    };
    url.query_pairs_mut().append_pair("limit", "50");
    match client.get_json(url).await {
        Ok(value) => match project_outbox_list(value) {
            Ok(value) => Json(json!({
                "canary_tenant": client.canary_tenant,
                "directives": value.directives,
                "next_cursor": value.next_cursor,
            }))
            .into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => error.into_response(),
    }
}

async fn outbox_pubkey() -> Response {
    let client = match GovernanceClient::from_env() {
        Ok(client) => client,
        Err(code) => return configuration_error(code),
    };
    let url = match client.url(&["watch", "outbox", "pubkey"]) {
        Ok(url) => url,
        Err(code) => return configuration_error(code),
    };
    match client.get_json(url).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn outbox_detail(Path(id): Path<String>) -> Response {
    let client = match GovernanceClient::from_env() {
        Ok(client) => client,
        Err(code) => return configuration_error(code),
    };
    let detail_url = match client.url(&["watch", "outbox", &client.canary_tenant, &id]) {
        Ok(url) => url,
        Err(code) => return configuration_error(code),
    };
    let pubkey_url = match client.url(&["watch", "outbox", "pubkey"]) {
        Ok(url) => url,
        Err(code) => return configuration_error(code),
    };
    let (directive, pubkey) =
        tokio::join!(client.get_json(detail_url), client.get_json(pubkey_url),);
    let directive = match directive {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let pubkey = match pubkey {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let verification = verify_outbox_signature(&directive, &pubkey);
    Json(json!({
        "directive": directive,
        "verification": verification,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct UpstreamOutboxList {
    directives: Vec<OutboxSummary>,
    next_cursor: Option<String>,
}

/// Browser list rows deliberately omit the signed envelope and canonical
/// bytes. Those are fetched only for an explicitly selected detail row, where
/// Council also verifies the signature before returning them.
#[derive(Debug, Deserialize, Serialize)]
struct OutboxSummary {
    id: String,
    status: String,
    verdict: String,
    authority: String,
    created_at_ms: i64,
    signature: OutboxSignatureSummary,
    council_session_id: Option<String>,
    council_cost_usd: Option<f64>,
    expires_at_ms: Option<i64>,
    acked_at_ms: Option<i64>,
    worker_provenance: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OutboxSignatureSummary {
    alg: String,
    kid: String,
    value: String,
}

fn project_outbox_list(value: Value) -> Result<UpstreamOutboxList, GatewayReadError> {
    serde_json::from_value(value).map_err(|_| GatewayReadError::InvalidResponse)
}

#[derive(Debug, Clone, Serialize)]
pub struct SignatureVerification {
    pub verified: bool,
    pub algorithm: &'static str,
    pub kid: Option<String>,
    pub detail: &'static str,
}

/// Verify the signature over the exact UTF-8 bytes supplied in
/// `envelope_json_canonical`. The JSON is never parsed or re-serialized.
pub fn verify_outbox_signature(directive: &Value, pubkey: &Value) -> SignatureVerification {
    let kid = directive
        .pointer("/signature/kid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let fail = |detail| SignatureVerification {
        verified: false,
        algorithm: "Ed25519",
        kid: kid.clone(),
        detail,
    };

    if directive.pointer("/signature/alg").and_then(Value::as_str) != Some("Ed25519")
        || pubkey.get("alg").and_then(Value::as_str) != Some("Ed25519")
    {
        return fail("algorithm_mismatch");
    }
    if directive.pointer("/signature/kid").and_then(Value::as_str)
        != pubkey.get("kid").and_then(Value::as_str)
    {
        return fail("key_id_mismatch");
    }
    let Some(canonical) = directive
        .get("envelope_json_canonical")
        .and_then(Value::as_str)
    else {
        return fail("canonical_envelope_missing");
    };
    let Some(pubkey_b64) = pubkey.get("pubkey_b64").and_then(Value::as_str) else {
        return fail("public_key_missing");
    };
    let Some(signature_b64) = directive
        .pointer("/signature/value")
        .and_then(Value::as_str)
    else {
        return fail("signature_missing");
    };

    let Ok(pubkey_bytes) = base64::engine::general_purpose::STANDARD.decode(pubkey_b64) else {
        return fail("public_key_invalid");
    };
    let Ok(pubkey_bytes) = <[u8; 32]>::try_from(pubkey_bytes.as_slice()) else {
        return fail("public_key_invalid");
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&pubkey_bytes) else {
        return fail("public_key_invalid");
    };
    let Ok(signature_bytes) = base64::engine::general_purpose::STANDARD.decode(signature_b64)
    else {
        return fail("signature_invalid");
    };
    let Ok(signature) = Signature::from_slice(&signature_bytes) else {
        return fail("signature_invalid");
    };
    if verifying_key
        .verify(canonical.as_bytes(), &signature)
        .is_err()
    {
        return fail("signature_mismatch");
    }
    SignatureVerification {
        verified: true,
        algorithm: "Ed25519",
        kid,
        detail: "verified_exact_canonical_utf8",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed_fixture(canonical: &str) -> (Value, Value) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let signature = key.sign(canonical.as_bytes());
        let kid = "sidecar-v1-test";
        let directive = json!({
            "id": "directive-1",
            "envelope_json_canonical": canonical,
            "signature": {
                "alg": "Ed25519",
                "kid": kid,
                "value": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            }
        });
        let pubkey = json!({
            "alg": "Ed25519",
            "kid": kid,
            "pubkey_b64": base64::engine::general_purpose::STANDARD
                .encode(key.verifying_key().as_bytes()),
        });
        (directive, pubkey)
    }

    #[test]
    fn directives_contract_accepts_directives_and_rejects_stale_records() {
        assert!(project_outbox_list(json!({ "directives": [], "next_cursor": null })).is_ok());
        assert!(project_outbox_list(json!({ "records": [] })).is_err());
    }

    #[test]
    fn outbox_list_projection_omits_envelope_material() {
        let projected = project_outbox_list(json!({
            "directives": [{
                "id": "directive-1",
                "status": "pending",
                "verdict": "allow",
                "authority": "recommend",
                "created_at_ms": 1,
                "signature": { "alg": "Ed25519", "kid": "k1", "value": "sig" },
                "council_session_id": null,
                "council_cost_usd": null,
                "expires_at_ms": null,
                "acked_at_ms": null,
                "worker_provenance": null,
                "envelope": { "private": "body" },
                "envelope_json_canonical": "secret-canonical-bytes"
            }],
            "next_cursor": null
        }))
        .expect("valid upstream response");
        let serialized = serde_json::to_value(projected.directives).expect("serialize summary");
        assert!(serialized[0].get("envelope").is_none());
        assert!(serialized[0].get("envelope_json_canonical").is_none());
    }

    #[test]
    fn signature_verification_accepts_exact_canonical_utf8() {
        let (directive, pubkey) = signed_fixture(r#"{"verdict":"allow","n":1}"#);
        let result = verify_outbox_signature(&directive, &pubkey);
        assert!(result.verified);
        assert_eq!(result.detail, "verified_exact_canonical_utf8");
    }

    #[test]
    fn signature_verification_rejects_tampered_canonical_utf8() {
        let (mut directive, pubkey) = signed_fixture(r#"{"verdict":"allow","n":1}"#);
        directive["envelope_json_canonical"] =
            Value::String(r#"{"verdict":"deny","n":1}"#.to_string());
        let result = verify_outbox_signature(&directive, &pubkey);
        assert!(!result.verified);
        assert_eq!(result.detail, "signature_mismatch");
    }
}
