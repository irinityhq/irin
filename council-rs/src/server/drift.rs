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

// ───── Mapmaker briefs / map preview ──────────────────────────

#[cfg(test)]
mod json_body_required_tests {
    use super::{DriftRunBody, WeeklyRunBody};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde::Deserialize;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn echo_drift(Json(body): Json<DriftRunBody>) -> Json<Value> {
        Json(serde_json::json!({"window": body.window, "limit": body.limit}))
    }

    async fn echo_weekly(Json(body): Json<WeeklyRunBody>) -> Json<Value> {
        Json(serde_json::json!({
            "window": body.window,
            "limit": body.limit,
            "post_webhooks": body.post_webhooks
        }))
    }

    fn app() -> Router {
        Router::new()
            .route("/run", post(echo_drift))
            .route("/weekly", post(echo_weekly))
    }

    #[derive(Deserialize)]
    struct Echo {
        window: u32,
        limit: Option<usize>,
        #[serde(default)]
        post_webhooks: bool,
    }

    #[tokio::test]
    async fn text_plain_is_rejected() {
        for path in ["/run", "/weekly"] {
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

    #[tokio::test]
    async fn empty_json_uses_defaults() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let echo: Echo = serde_json::from_slice(&body).unwrap();
        assert_eq!(echo.window, 7);
        assert_eq!(echo.limit, Some(8));
    }

    #[tokio::test]
    async fn weekly_json_uses_defaults() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/weekly")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let echo: Echo = serde_json::from_slice(&body).unwrap();
        assert_eq!(echo.window, 7);
        assert_eq!(echo.limit, Some(8));
        assert!(!echo.post_webhooks);
    }
}
