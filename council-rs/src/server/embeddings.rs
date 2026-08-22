// Embeddings + precedent reindex handlers (moved from server.rs).

use axum::{extract::Query, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;

use crate::precedent;
use crate::warroom;

pub(super) async fn embeddings_stats() -> impl IntoResponse {
    axum::Json(warroom::embeddings::stats())
}

#[derive(Deserialize)]
pub(super) struct RebuildQuery {
    #[serde(default)]
    force: bool,
}

/// JSON body for successful `POST /api/precedent/reindex`.
pub(crate) fn precedent_reindex_success_json(count: usize) -> serde_json::Value {
    json!({ "reindexed": count })
}

// `Json<Value>` is a content-type guard: a cross-site simple-form POST (no
// JSON content type) is refused with 415 before any work runs (B-10).
pub(super) async fn precedent_reindex(
    axum::Json(_): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let join = tokio::task::spawn_blocking(precedent::reindex).await;
    match join {
        Ok(Ok(count)) => axum::Json(precedent_reindex_success_json(count)).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("join: {}", e) })),
        )
            .into_response(),
    }
}

pub(super) async fn embeddings_rebuild(
    Query(q): Query<RebuildQuery>,
    axum::Json(_): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    // First pass through the model can take ~30s for download. Offload off
    // the request thread so axum keeps responding.
    let force = q.force;
    let result = tokio::task::spawn_blocking(move || warroom::embeddings::build_index(force))
        .await
        .unwrap_or_else(|e| json!({"built": false, "error": format!("join: {}", e)}));
    if result.get("error").is_some() && result.get("built").and_then(|x| x.as_bool()) != Some(true)
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(result),
        )
            .into_response();
    }
    axum::Json(result).into_response()
}

// ───── Meta-review ───────────────────────────────────────────

#[cfg(test)]
mod precedent_reindex_tests {
    use super::precedent_reindex_success_json;

    #[test]
    fn success_json_includes_reindexed_count() {
        let v = precedent_reindex_success_json(12);
        assert_eq!(v.get("reindexed").and_then(|x| x.as_u64()), Some(12));
    }
}

#[cfg(test)]
mod json_body_required_tests {
    use super::{embeddings_rebuild, precedent_reindex};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/api/precedent/reindex", post(precedent_reindex))
            .route("/api/embeddings/rebuild", post(embeddings_rebuild))
    }

    #[tokio::test]
    async fn bodyless_mutations_reject_text_plain() {
        for path in ["/api/precedent/reindex", "/api/embeddings/rebuild"] {
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
}
