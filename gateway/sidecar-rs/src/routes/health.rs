// Health endpoint + contract tests (moved from main.rs).

use axum::{extract::Json, response::IntoResponse};
use serde::Serialize;

use crate::watch;

#[derive(Serialize)]
pub(super) struct HealthResponse {
    status: &'static str,
    service: &'static str,
    build_sha: &'static str,
    build_dirty: bool,
}

pub(super) async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        service: "gateway-sidecar",
        build_sha: watch::attest::build_sha(),
        build_dirty: watch::attest::build_is_dirty(),
    })
}

#[cfg(test)]
mod health_contract_tests {
    use super::*;

    #[tokio::test]
    async fn health_exposes_embedded_build_identity() {
        let response = health().await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(
            payload["build_sha"]
                .as_str()
                .is_some_and(|sha| sha.len() == 40),
            "sidecar health must expose the full embedded commit: {payload}"
        );
        assert_eq!(payload["build_sha"], watch::attest::build_sha());
        assert_eq!(payload["build_dirty"], watch::attest::build_is_dirty());
        assert!(
            payload["build_dirty"].is_boolean(),
            "sidecar health must expose embedded tree cleanliness: {payload}"
        );
    }
}
