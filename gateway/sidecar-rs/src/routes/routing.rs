// Routing route handlers (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

use crate::router;
use crate::AppState;

#[derive(Deserialize)]
pub(super) struct RouteDecideRequest {
    #[serde(default)]
    model: Option<String>,
    body: serde_json::Value,
    #[serde(default)]
    strategy: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RouteOutcomeRequest {
    /// The actually-routed model ID (not alias). Required for per-family
    /// health tracking — the router derives (provider, family) from this.
    model_id: String,
    success: bool,
    latency_ms: f64,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Serialize)]
pub(super) struct RouteOutcomeResponse {
    recorded: bool,
}

pub(super) async fn route_decide(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RouteDecideRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();

    // Strategy resolution: body field > X-Routing-Strategy header > default (Balanced)
    let strategy = req
        .strategy
        .as_deref()
        .and_then(router::RoutingStrategy::from_str_opt)
        .or_else(|| {
            headers
                .get("x-routing-strategy")
                .and_then(|v| v.to_str().ok())
                .and_then(router::RoutingStrategy::from_str_opt)
        })
        .unwrap_or_default();

    // Sensitivity level: header-trusted per COUNCIL_GATEWAY_CONTRACT.md.
    // The gateway has no opinion on payload sensitivity — IRIN or other
    // upstream callers classify and pass the verdict via X-Sensitivity-Level.
    // RED forces routing to a local provider regardless of requested model.
    let sensitivity = headers
        .get("x-sensitivity-level")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| "GREEN".to_string());

    // Sovereign mode: X-Sovereign-Mode header forces all routing to local
    // providers, regardless of sensitivity level. This is the "sovereign switch".

    let base_model = req
        .model
        .as_deref()
        .map(|m| m.split_once('@').map(|(base, _)| base).unwrap_or(m));

    let sovereign_mode = headers
        .get("x-sovereign-mode")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
        .unwrap_or(false);

    match state
        .router
        .route(
            &sensitivity,
            base_model,
            &req.body,
            strategy,
            sovereign_mode,
        )
        .await
    {
        Ok(decision) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            debug!(
                model = %decision.model_id,
                provider = %decision.provider,
                score = decision.score,
                task = ?decision.task_type,
                strategy = ?decision.strategy,
                sensitivity = %sensitivity,
                sovereign_mode,
                latency_ms,
                "route/decide"
            );
            (
                StatusCode::OK,
                Json(serde_json::to_value(&decision).unwrap()),
            )
        }
        Err(e) => {
            warn!(error = %e, "route/decide: failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e})),
            )
        }
    }
}

pub(super) async fn route_outcome(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<RouteOutcomeRequest>,
) -> impl IntoResponse {
    state
        .router
        .record_outcome(&req.model_id, req.success, req.latency_ms, req.error)
        .await;
    Json(RouteOutcomeResponse { recorded: true })
}
