// Sessions, precedent search, interventions/patterns/clusters (moved from server.rs).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, problem};
use crate::precedent;
use crate::warroom;

#[derive(Deserialize)]
pub(super) struct SessionsQuery {
    limit: Option<usize>,
}

/// Normalize one sessions/index.jsonl line into the wire shape the React
/// SessionIndexEntry interface reads. Returns None for blank, malformed, or
/// non-object lines so the caller can skip them.
pub(crate) fn normalize_index_entry(line: &str) -> Option<serde_json::Value> {
    if line.trim().is_empty() {
        return None;
    }
    let mut v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    {
        // Ensure every field the React SessionIndexEntry interface reads is present.
        let obj = v.as_object_mut()?;
        // Aliases: Rust-era writers use {session_id, timestamp, digest};
        // Python-era writers use {id, ts, ruling_digest}.
        if !obj.contains_key("id")
            && let Some(sid) = obj.get("session_id").cloned()
        {
            obj.insert("id".to_string(), sid);
        }
        if !obj.contains_key("ts")
            && let Some(t) = obj.get("timestamp").cloned()
        {
            obj.insert("ts".to_string(), t);
        }
        if !obj.contains_key("ruling_digest")
            && let Some(d) = obj.get("digest").cloned()
        {
            obj.insert("ruling_digest".to_string(), d);
        }
        // Fill in defaults the UI reads. mode defaults to "normal" — legacy
        // entries lacking the key are Python-era normal sessions, matching
        // the lenient CouncilSession::mode deserialization default.
        obj.entry("topic".to_string()).or_insert(json!(""));
        obj.entry("keywords".to_string()).or_insert(json!([]));
        obj.entry("ruling_digest".to_string()).or_insert(json!(""));
        obj.entry("confidence".to_string()).or_insert(json!(""));
        obj.entry("cabinet".to_string()).or_insert(json!(""));
        obj.entry("convergence".to_string()).or_insert(json!(0.0));
        obj.entry("mode".to_string()).or_insert(json!("normal"));
        obj.entry("seat_count".to_string()).or_insert(json!(0));
        obj.entry("rounds".to_string()).or_insert(json!(0));
        obj.entry("synthesis_model".to_string())
            .or_insert(json!(""));
        obj.entry("version".to_string()).or_insert(json!(""));
    }
    Some(v)
}

pub(super) async fn sessions_list(Query(q): Query<SessionsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(500);

    let path = std::path::PathBuf::from(
        std::env::var("COUNCIL_SESSIONS_DIR").unwrap_or_else(|_| "sessions".to_string()),
    )
    .join("index.jsonl");

    let mut entries: Vec<serde_json::Value> = Vec::new();
    if let Ok(file) = std::fs::File::open(&path) {
        use std::io::BufRead;
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(v) = normalize_index_entry(&line) {
                entries.push(v);
            }
        }
    }

    // Newest first by ts (ISO-8601 sorts lexicographically).
    entries.sort_by(|a, b| {
        b.get("ts")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .cmp(a.get("ts").and_then(|x| x.as_str()).unwrap_or(""))
    });
    entries.truncate(limit);

    axum::Json(json!({ "sessions": entries }))
}

/// GET /api/sessions/:id
pub(super) async fn session_detail(Path(id): Path<String>) -> impl IntoResponse {
    match precedent::load_session(&id) {
        Some(session) => axum::Json(json!(session)).into_response(),
        None => problem(
            axum::http::StatusCode::NOT_FOUND,
            "error",
            &format!("Session not found: {}", id),
        ),
    }
}

/// GET /api/precedent?q=...&limit=20
#[derive(Deserialize)]
pub(super) struct PrecedentQuery {
    q: String,
    limit: Option<usize>,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    mode: Option<String>,
}

pub(super) async fn precedent_search(Query(q): Query<PrecedentQuery>) -> impl IntoResponse {
    // Defaults mirror the engine's injection parameters so a bare query
    // previews exactly what a convene would inject.
    let limit = q.limit.unwrap_or(precedent::RETRIEVE_LIMIT).min(100);
    let threshold = q.threshold.unwrap_or(precedent::RETRIEVE_THRESHOLD);
    let mode = q.mode.as_deref().unwrap_or("auto");

    // Same retrieve() the deliberation engine uses — the preview IS the
    // injection set when queried with the engine's limit + threshold.
    // Keep synchronous precedent lookup off the async request worker:
    // retrieve() does FS load_index + (lazy fastembed model + embed) and can
    // block the axum worker (WarRoom / --serve responsiveness). Follows the
    // spawn_blocking pattern at embeddings_rebuild. On join error: log + 500.
    let q_clone = q.q.clone();
    let force_keyword = mode == "keyword";
    let join_res = tokio::task::spawn_blocking(move || {
        precedent::retrieve_with_mode(&q_clone, limit, threshold, false, force_keyword)
    })
    .await;

    let (matches, actual_mode, engine) = match join_res {
        Ok(receipt) => (
            precedent::receipt_to_match_values(&receipt),
            // UI contract: "semantic" | "keyword". hybrid-v1 carries the dense
            // layer, so it reports as semantic; `engine` holds the exact ranker.
            if receipt.engine == "hybrid-v1" {
                "semantic"
            } else {
                "keyword"
            },
            receipt.engine,
        ),
        Err(e) => {
            eprintln!(
                "ERROR: precedent_search spawn_blocking join failed for q (len={}): {}",
                q.q.len(),
                e
            );
            (vec![], "error", "error")
        }
    };

    let body = json!({
        "matches": matches,
        "query": q.q,
        "mode": actual_mode,
        "engine": engine,
        "threshold": threshold,
    });
    if actual_mode == "error" {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(body),
        )
            .into_response();
    }
    axum::Json(body).into_response()
}

// ───── Session lineage / fork ─────────────────────────────────

#[derive(Deserialize)]
pub(super) struct ForkBody {
    #[serde(default)]
    swaps: Vec<serde_json::Value>,
}

pub(super) async fn session_fork(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<ForkBody>,
) -> impl IntoResponse {
    let swaps = body.swaps;
    let result = warroom::fork::fork_session(&state.config, &id, &swaps);
    if result.get("error").is_some() {
        return (axum::http::StatusCode::NOT_FOUND, axum::Json(result)).into_response();
    }
    axum::Json(result).into_response()
}

pub(super) async fn session_lineage(Path(id): Path<String>) -> impl IntoResponse {
    let parent = warroom::lineage::parent_of(&id);
    let children = warroom::lineage::children_of(&id);
    axum::Json(json!({
        "session_id": id,
        "parent": parent,
        "children": children,
    }))
}

/// POST /api/sessions/{id}/export/pdf (N06) — render the session's ruling to a
/// downloadable PDF. 404 when the session is unknown. The PDF is a hand-rolled
/// paginated text document (no new crate); the browser receives it as an
/// attachment `council_<id>.pdf` (works in the Tauri webview too).
pub(super) async fn session_export_pdf(Path(id): Path<String>) -> Response {
    // load_session does sync FS reads — offload per the spawn_blocking
    // convention used by precedent_search / embeddings_rebuild.
    let id_for_load = id.clone();
    let join = tokio::task::spawn_blocking(move || {
        precedent::load_session(&id_for_load).map(|session| warroom::pdf::render_session(&session))
    })
    .await;

    match join {
        Ok(Some(bytes)) => {
            let disposition = format!("attachment; filename=\"council_{}.pdf\"", id);
            let headers = [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/pdf"),
                ),
                (
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_str(&disposition).unwrap_or_else(|_| {
                        HeaderValue::from_static("attachment; filename=\"council.pdf\"")
                    }),
                ),
            ];
            (StatusCode::OK, headers, bytes).into_response()
        }
        Ok(None) => problem(
            StatusCode::NOT_FOUND,
            "error",
            &format!("Session not found: {}", id),
        ),
        Err(e) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "error",
            &format!("PDF render join failed: {e}"),
        ),
    }
}

pub(super) async fn session_diff(Path((a, b)): Path<(String, String)>) -> impl IntoResponse {
    let parent = precedent::load_session(&a);
    let child = precedent::load_session(&b);
    let (Some(parent), Some(child)) = (parent, child) else {
        return problem(
            axum::http::StatusCode::NOT_FOUND,
            "error",
            "one or both sessions not found",
        );
    };
    let parent_v = serde_json::to_value(&parent).unwrap_or(json!({}));
    let child_v = serde_json::to_value(&child).unwrap_or(json!({}));
    axum::Json(warroom::lineage::diff_synthesis(&parent_v, &child_v)).into_response()
}

// ───── Interventions / patterns ───────────────────────────────

#[derive(Deserialize)]
pub(super) struct InterventionsQuery {
    days: Option<i64>,
    limit: Option<usize>,
}

pub(super) async fn interventions_list(Query(q): Query<InterventionsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(200).min(1000);
    let entries = warroom::intervention_log::load_all(q.days);
    let total = entries.len();
    // Tail of `limit`, reversed (newest first)
    let mut tail: Vec<_> = entries.into_iter().rev().take(limit).collect();
    tail.shrink_to_fit();
    axum::Json(json!({
        "entries": tail,
        "total": total,
    }))
}

#[derive(Deserialize)]
pub(super) struct PatternsQuery {
    days: Option<i64>,
}

pub(super) async fn patterns_aggregate(Query(q): Query<PatternsQuery>) -> impl IntoResponse {
    axum::Json(warroom::intervention_log::patterns(q.days))
}

/// GET /api/interventions/predict?convergence=<f64>&round=<u32> (N04).
#[derive(Deserialize)]
pub(super) struct PredictQuery {
    convergence: f64,
    round: u32,
}

/// N04: probability that the operator escalates at the given pause point.
/// Trains a tiny logistic regression at request time from the intervention log;
/// < 30 usable samples falls back to overall escalation frequency. Cheap — runs
/// inline (a few thousand gradient steps over a handful of rows).
pub(super) async fn interventions_predict(Query(q): Query<PredictQuery>) -> impl IntoResponse {
    axum::Json(warroom::predict::predict(q.convergence, q.round))
}

/// GET /api/clusters (N03) — topic clusters over the session embedding index.
pub(super) async fn clusters_get() -> impl IntoResponse {
    // Reads + parses the embeddings/index JSONL and runs k-means; offload off
    // the request thread per the spawn_blocking convention.
    let result = tokio::task::spawn_blocking(warroom::clusters::build)
        .await
        .unwrap_or_else(|e| {
            json!({
                "clusters": [],
                "method": "kmeans",
                "k": 0,
                "n_sessions": 0,
                "error": format!("join: {e}"),
            })
        });
    axum::Json(result)
}

// ───── Drift reports ──────────────────────────────────────────

#[cfg(test)]
mod normalize_index_entry_tests {
    use super::normalize_index_entry;
    use serde_json::json;

    #[test]
    fn normalize_index_entry_aliases_rust_era_keys() {
        let line =
            r#"{"session_id":"abc-123","timestamp":"2026-01-01T00:00:00","digest":"ruling text"}"#;
        let v = normalize_index_entry(line).unwrap();
        assert_eq!(v["id"], "abc-123");
        assert_eq!(v["ts"], "2026-01-01T00:00:00");
        assert_eq!(v["ruling_digest"], "ruling text");
    }

    #[test]
    fn normalize_index_entry_keeps_python_era_keys() {
        let line = r#"{"id":"py-1","ts":"2025-06-01T00:00:00","ruling_digest":"old"}"#;
        let v = normalize_index_entry(line).unwrap();
        assert_eq!(v["id"], "py-1");
        assert_eq!(v["ts"], "2025-06-01T00:00:00");
        assert_eq!(v["ruling_digest"], "old");
    }

    #[test]
    fn normalize_index_entry_fills_ui_defaults() {
        let v = normalize_index_entry(r#"{"id":"x"}"#).unwrap();
        assert_eq!(v["topic"], "");
        assert_eq!(v["keywords"], json!([]));
        assert_eq!(v["ruling_digest"], "");
        assert_eq!(v["confidence"], "");
        assert_eq!(v["cabinet"], "");
        assert_eq!(v["convergence"], 0.0);
        assert_eq!(v["mode"], "normal");
        assert_eq!(v["seat_count"], 0);
        assert_eq!(v["rounds"], 0);
        assert_eq!(v["synthesis_model"], "");
        assert_eq!(v["version"], "");
    }

    #[test]
    fn normalize_index_entry_preserves_existing_values() {
        let v = normalize_index_entry(r#"{"id":"x","mode":"wargame","synthesis_model":"opus"}"#)
            .unwrap();
        assert_eq!(v["mode"], "wargame");
        assert_eq!(v["synthesis_model"], "opus");
    }

    #[test]
    fn normalize_index_entry_skips_malformed_lines() {
        assert!(normalize_index_entry("").is_none());
        assert!(normalize_index_entry("   ").is_none());
        assert!(normalize_index_entry("{not json").is_none());
        // Valid JSON but not an object — skipped, matching the old inline loop.
        assert!(normalize_index_entry("42").is_none());
        assert!(normalize_index_entry("[1,2]").is_none());
    }
}

#[cfg(test)]
mod json_body_required_tests {
    use super::session_fork;
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
    async fn production_session_fork_rejects_text_plain() {
        let app = Router::new()
            .route("/api/sessions/{id}/fork", post(session_fork))
            .with_state(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions/nonexistent/fork")
                    .header("content-type", "text/plain")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "session_fork must reject a simple POST"
        );
    }
}
