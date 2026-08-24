// Drift report handlers (moved from server.rs).

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, problem};
use crate::warroom;

pub(super) async fn drift_reports_list() -> impl IntoResponse {
    axum::Json(json!({
        "reports": warroom::drift::list_reports(),
        "running": warroom::drift::is_running(),
    }))
}

pub(super) async fn drift_report_get(Path(name): Path<String>) -> impl IntoResponse {
    if !name.starts_with("drift_") || !name.ends_with(".md") {
        return problem(
            axum::http::StatusCode::NOT_FOUND,
            "error",
            &format!("report {} not found", name),
        );
    }
    match warroom::drift::get_report(&name) {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": format!("report {} not found", name)})),
        )
            .into_response(),
    }
}

pub(super) async fn drift_weekly_latest() -> impl IntoResponse {
    match warroom::drift::latest_weekly_summary() {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({
                "detail": "no weekly summary yet — POST /api/drift/weekly/run to generate one"
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct DriftRunBody {
    #[serde(default = "default_window")]
    window: u32,
    #[serde(default = "default_drift_limit")]
    limit: Option<usize>,
}
pub(super) fn default_window() -> u32 {
    7
}
pub(super) fn default_drift_limit() -> Option<usize> {
    Some(8)
}

pub(super) async fn drift_run(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<DriftRunBody>,
) -> impl IntoResponse {
    if warroom::drift::is_running() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "drift run already in progress"})),
        )
            .into_response();
    }
    if !warroom::drift::acquire_lock() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "could not acquire drift lock"})),
        )
            .into_response();
    }
    let window = body.window.clamp(1, 90);
    let limit = body.limit.map(|l| l.clamp(1, 50));

    let cfg = state.config.clone();
    tokio::spawn(async move {
        let _ = warroom::drift::run_drift_report(&cfg, window, limit).await;
        warroom::drift::release_lock();
    });

    axum::Json(json!({"status": "started", "window": window, "limit": limit})).into_response()
}

#[derive(Deserialize)]
pub(super) struct WeeklyRunBody {
    #[serde(default = "default_window")]
    window: u32,
    #[serde(default = "default_drift_limit")]
    limit: Option<usize>,
    #[serde(default)]
    post_webhooks: bool,
}

pub(super) async fn drift_weekly_run(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<WeeklyRunBody>,
) -> impl IntoResponse {
    if warroom::drift::is_running() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "drift run already in progress"})),
        )
            .into_response();
    }
    if !warroom::drift::acquire_lock() {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"detail": "could not acquire drift lock"})),
        )
            .into_response();
    }
    let window = body.window.clamp(1, 90);
    let limit = body.limit.map(|l| l.clamp(1, 50));
    let post = body.post_webhooks;

    let cfg = state.config.clone();
    tokio::spawn(async move {
        let _ = warroom::drift::run_weekly_summary(&cfg, window, limit, post).await;
        warroom::drift::release_lock();
    });

    axum::Json(json!({"status": "started", "window": window, "limit": limit})).into_response()
}

#[derive(Deserialize)]
pub(super) struct WeeklyHistoryQuery {
    limit: Option<usize>,
}

pub(super) async fn drift_weekly_history(Query(q): Query<WeeklyHistoryQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(12).min(52);
    axum::Json(json!({
        "summaries": warroom::drift::weekly_history(limit),
    }))
}

#[cfg(test)]
mod json_body_required_tests {
    use super::{DriftRunBody, WeeklyRunBody, drift_run, drift_weekly_run};
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

    fn app() -> Router {
        Router::new()
            .route("/api/drift/run", post(drift_run))
            .route("/api/drift/weekly/run", post(drift_weekly_run))
            .with_state(test_state())
    }

    #[tokio::test]
    async fn production_handlers_reject_text_plain() {
        for path in ["/api/drift/run", "/api/drift/weekly/run"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "text/plain")
                        .body(Body::from("x"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{path} must reject a simple POST"
            );
        }
    }

    #[test]
    fn empty_json_uses_defaults() {
        let body: DriftRunBody = serde_json::from_str("{}").unwrap();
        assert_eq!(body.window, 7);
        assert_eq!(body.limit, Some(8));
    }

    #[test]
    fn weekly_json_uses_defaults() {
        let body: WeeklyRunBody = serde_json::from_str("{}").unwrap();
        assert_eq!(body.window, 7);
        assert_eq!(body.limit, Some(8));
        assert!(!body.post_webhooks);
    }
}
