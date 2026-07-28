// Health + provider discovery handlers (moved from server.rs).

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use super::{AppState, problem};
use crate::provider;

pub(super) fn ws_smoke_only_enabled() -> bool {
    std::env::var("COUNCIL_WS_SMOKE_ONLY").as_deref() == Ok("1")
}

/// GET /api/health
pub(super) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // Liveness must stay cheap and deterministic: no provider CLI discovery,
    // no gcloud, no version probes. GW_API_KEY alone establishes the configured
    // Gateway route for the env-only summary. Full CLI discovery is
    // `GET /api/discover`. Exact model readiness remains Gateway /v1/models
    // immediately before each governed dispatch.
    let providers = provider::check_providers_liveness(provider::env_nonempty("GW_API_KEY"));
    let available: Vec<&str> = providers
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(name, _)| *name)
        .collect();
    let missing: Vec<&str> = providers
        .iter()
        .filter(|(_, ok)| !*ok)
        .map(|(name, _)| *name)
        .collect();

    let sessions_dir =
        std::env::var("COUNCIL_SESSIONS_DIR").unwrap_or_else(|_| "sessions".to_string());
    let index_path = format!("{}/index.jsonl", sessions_dir);
    let index_exists = std::path::Path::new(&index_path).exists();

    axum::Json(json!({
        "council_version": env!("CARGO_PKG_VERSION"),
        "build_sha": option_env!("COUNCIL_BUILD_GIT_SHA").unwrap_or("unknown"),
        "build_dirty": option_env!("COUNCIL_BUILD_DIRTY") != Some("false"),
        "stream_version": "rs-1.0.0",
        "providers_available": available,
        "providers_missing": missing,
        "sessions_dir": sessions_dir,
        "index_path": index_path,
        "index_exists": index_exists,
        "ws_smoke_only": ws_smoke_only_enabled(),
        // H1 (audit #6): live free slots in the /api/deliberate concurrency cap.
        // 0 = saturated (further deliberations get 429 until one completes).
        "deliberate_permits_available": state.deliberate_semaphore.available_permits(),
    }))
}

/// GET /api/discover — JSON mirror of `council --discover` (feature contract).
///
/// Same bearer posture as sibling GET routes (router-wide auth middleware).
/// Wire shape + env-hint privacy live in `ProviderRegistry::to_discover_json`.
pub(super) async fn discover_providers() -> Response {
    // ProviderRegistry::discover() shells out (gcloud) and TCP-probes
    // localhost — offload per the spawn_blocking convention used by
    // precedent_search and embeddings_rebuild.
    match tokio::task::spawn_blocking(crate::registry::ProviderRegistry::discover).await {
        Ok(registry) => axum::Json(registry.to_discover_json()).into_response(),
        Err(e) => {
            eprintln!("ERROR: /api/discover spawn_blocking join failed: {e}");
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Discovery failed",
                &format!("spawn_blocking join failed: {e}"),
            )
        }
    }
}

#[cfg(test)]
mod health_router_tests {
    use super::super::{AUTH_CONFIG, AuthConfig, router};
    use crate::config::Config;
    use axum::http::StatusCode;
    use std::sync::Arc;

    fn install_auth(token: &str) {
        let _ = AUTH_CONFIG.get_or_init(|| AuthConfig {
            token: Some(token.to_string()),
            gateway_token: None,
            dev_no_auth: false,
        });
    }

    /// Minimal `Config` for router-level auth tests — no cabinets/models needed
    /// because the request is rejected by `auth_middleware` before any handler
    /// touches state.
    fn empty_config() -> Arc<Config> {
        Arc::new(Config {
            cabinets: std::collections::HashMap::new(),
            models: crate::types::ModelRegistry {
                models: std::collections::HashMap::new(),
            },
            roles: crate::types::RolesConfig::default(),
            tera: tera::Tera::default(),
            base_dir: std::env::temp_dir(),
        })
    }

    #[tokio::test]
    async fn health_exposes_embedded_build_identity_without_local_base_dir() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        install_auth("ws-test-secret");
        let response = router(empty_config())
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("authorization", "Bearer ws-test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            payload["build_sha"].as_str().is_some_and(|sha| {
                sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
            "health must identify the running Council build: {payload}"
        );
        assert!(
            payload["build_dirty"].is_boolean(),
            "health must identify whether the embedded build was dirty: {payload}"
        );
        assert!(
            payload.get("base_dir").is_none(),
            "remote health must not expose the local Council base directory: {payload}"
        );
        // Documented client fields retained (War Room seat matrix, smoke probes).
        assert!(
            payload["providers_available"].is_array(),
            "health must retain providers_available: {payload}"
        );
        assert!(
            payload["providers_missing"].is_array(),
            "health must retain providers_missing: {payload}"
        );
        assert!(
            payload["council_version"].is_string(),
            "health must retain council_version: {payload}"
        );
        assert!(
            payload["deliberate_permits_available"].is_number(),
            "health must retain deliberate_permits_available: {payload}"
        );
    }

    #[tokio::test]
    async fn health_uses_liveness_provider_summary_not_cli_discovery() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        // Prove /api/health does not claim host-only CLI seats without env:
        // liveness never shells out, so grok_build/claude_code stay unavailable
        // even if those CLIs exist on the build host.
        install_auth("ws-test-secret");
        let response = router(empty_config())
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("authorization", "Bearer ws-test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let available = payload["providers_available"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let available_s: Vec<String> = available
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        for host_only in ["grok_build", "claude_code", "gemini_agy"] {
            assert!(
                !available_s.iter().any(|p| p == host_only),
                "liveness must not report host-only CLI {host_only} as available: {available_s:?}"
            );
        }
    }
}
