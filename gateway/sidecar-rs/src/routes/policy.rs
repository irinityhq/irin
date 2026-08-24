// Policy route handlers (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::policy;
use crate::AppState;

#[derive(Deserialize)]
pub(super) struct PolicyEvalRequest {
    provider: String,
    #[serde(default)]
    sensitivity_level: Option<policy::SensitivityLevel>,
}

pub(super) async fn policy_evaluate(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<PolicyEvalRequest>,
) -> impl IntoResponse {
    let decision = state.policy.evaluate(&req.provider, req.sensitivity_level);
    if !decision.allowed {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::to_value(&decision).unwrap()),
        )
    } else {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&decision).unwrap()),
        )
    }
}
