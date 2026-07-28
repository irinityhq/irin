// Meta-review handlers (moved from server.rs).

use axum::{extract::State, response::IntoResponse};
use serde_json::json;

use super::AppState;
use crate::warroom;

pub(super) async fn meta_review_run(State(state): State<AppState>) -> impl IntoResponse {
    if warroom::meta_review::is_running() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "meta-review already in progress"})),
        )
            .into_response();
    }
    if !warroom::meta_review::acquire_lock() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "could not acquire meta-review lock"})),
        )
            .into_response();
    }
    let tera = state.config.tera.clone();
    let result = tokio::task::spawn_blocking(move || {
        let r = warroom::meta_review::run(Some(&tera));
        warroom::meta_review::release_lock();
        r
    })
    .await
    .unwrap_or_else(|e| json!({"status": "error", "error": format!("join: {}", e)}));
    axum::Json(result).into_response()
}

pub(super) async fn meta_review_latest() -> impl IntoResponse {
    match warroom::meta_review::latest() {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": "no meta-review report found"})),
        )
            .into_response(),
    }
}
