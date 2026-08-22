// Meta-review handlers (moved from server.rs).

use axum::{extract::State, response::IntoResponse};
use serde_json::json;

use super::AppState;
use crate::warroom;

// `Json<Value>` is a content-type guard: a cross-site simple-form POST (no
// JSON content type) is refused with 415 before the lock is taken (B-10).
pub(super) async fn meta_review_run(
    State(state): State<AppState>,
    axum::Json(_): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
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

#[cfg(test)]
mod json_body_required_tests {
    use super::meta_review_run;
    use crate::config::Config;
    use crate::librarian;
    use crate::server::AppState;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(Config {
                cabinets: std::collections::HashMap::new(),
                models: crate::types::ModelRegistry {
                    models: std::collections::HashMap::new(),
                },
                roles: crate::types::RolesConfig::default(),
                tera: tera::Tera::default(),
                base_dir: std::env::temp_dir(),
            }),
            librarian: librarian::routes::LibrarianState::from_env(),
            deliberate_semaphore: Arc::new(Semaphore::new(1)),
        }
    }

    #[tokio::test]
    async fn meta_review_run_rejects_text_plain() {
        let app = Router::new()
            .route("/api/meta-review/run", post(meta_review_run))
            .with_state(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/meta-review/run")
                    .header("content-type", "text/plain")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "meta_review_run must reject a simple POST"
        );
    }
}
