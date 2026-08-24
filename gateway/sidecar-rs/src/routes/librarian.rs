// Librarian proxy handlers (moved from main.rs).

use axum::{http::StatusCode, response::IntoResponse};
use std::sync::Arc;

use crate::AppState;

pub(super) async fn librarian_commit(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(payload): axum::extract::Json<serde_json::Value>,
) -> axum::response::Response {
    let client = reqwest::Client::new();
    let url = format!("{}/api/librarian/commits", state.librarian_base_url);
    match client.post(&url).json(&payload).send().await {
        Ok(resp) => {
            let status = resp.status();
            match resp.bytes().await {
                Ok(b) => (status, b).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("librarian upstream error: {}", e),
        )
            .into_response(),
    }
}

pub(super) async fn librarian_context(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(tenant): axum::extract::Path<String>,
) -> axum::response::Response {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/librarian/context/{}",
        state.librarian_base_url, tenant
    );
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            match resp.bytes().await {
                Ok(b) => (status, b).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("librarian upstream error: {}", e),
        )
            .into_response(),
    }
}
