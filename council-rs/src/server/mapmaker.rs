// Mapmaker brief + preview handlers (moved from server.rs).

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::warroom;

#[derive(Deserialize)]
pub(super) struct BriefsQuery {
    limit: Option<usize>,
}

pub(super) async fn mapmaker_briefs_list(Query(q): Query<BriefsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).min(200);
    axum::Json(json!({
        "briefs": warroom::mapmaker::list_briefs(limit),
    }))
}

pub(super) async fn mapmaker_brief_get(Path(name): Path<String>) -> impl IntoResponse {
    match warroom::mapmaker::get_brief(&name) {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": format!("brief {} not found", name)})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct MapPreviewBody {
    dir_path: String,
}

pub(super) async fn map_preview(axum::Json(body): axum::Json<MapPreviewBody>) -> impl IntoResponse {
    let result = warroom::safe_map::gather_map_preview(&body.dir_path);
    if result.get("error").is_some() {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(result)).into_response();
    }
    axum::Json(result).into_response()
}

#[derive(Deserialize)]
pub(super) struct MapmakerRunBody {
    dir_path: String,
    task: String,
    #[serde(default = "default_auto")]
    model: String,
}
pub(super) fn default_auto() -> String {
    "auto".into()
}

pub(super) async fn mapmaker_run(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<MapmakerRunBody>,
) -> impl IntoResponse {
    let model = match warroom::mapmaker::MapmakerModel::parse(&body.model) {
        Some(m) => m,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"detail": format!("unknown model: {}", body.model)})),
            )
                .into_response();
        }
    };
    let result =
        warroom::mapmaker::run_mapmaker(&state.config, &body.dir_path, &body.task, model).await;
    if result.get("error").is_some() {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(result)).into_response();
    }
    axum::Json(result).into_response()
}

#[cfg(test)]
mod json_body_required_tests {
    use super::{map_preview, mapmaker_run};
    use crate::server::test_support::test_app_state;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tower::ServiceExt;

    #[tokio::test]
    async fn mapmaker_run_rejects_text_plain() {
        let app = Router::new()
            .route("/api/mapmaker/run", post(mapmaker_run))
            .with_state(test_app_state(1));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mapmaker/run")
                    .header("content-type", "text/plain")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "mapmaker_run must reject a simple POST (B-10)"
        );
    }

    #[tokio::test]
    async fn map_preview_rejects_text_plain() {
        let app = Router::new()
            .route("/api/map/preview", post(map_preview))
            .with_state(test_app_state(1));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/map/preview")
                    .header("content-type", "text/plain")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "map_preview must reject a simple POST (B-10)"
        );
    }
}
