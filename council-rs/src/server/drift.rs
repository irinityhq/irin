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
    body: Option<axum::Json<DriftRunBody>>,
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
    let body = body.map(|b| b.0).unwrap_or(DriftRunBody {
        window: 7,
        limit: Some(8),
    });
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
    body: Option<axum::Json<WeeklyRunBody>>,
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
    let body = body.map(|b| b.0).unwrap_or(WeeklyRunBody {
        window: 7,
        limit: Some(8),
        post_webhooks: false,
    });
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
