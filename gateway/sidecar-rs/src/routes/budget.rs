// Budget route handlers (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;
use tracing::warn;

use crate::AppState;

#[derive(Deserialize)]
pub(super) struct BudgetCheckRequest {
    budget_key: String,
    estimated_cost: f64,
}

#[derive(Deserialize)]
pub(super) struct BudgetRecordRequest {
    budget_key: String,
    actual_cost: f64,
}

pub(super) async fn budget_check(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<BudgetCheckRequest>,
) -> impl IntoResponse {
    let result = state
        .budget
        .check(&req.budget_key, req.estimated_cost)
        .await;
    if !result.allowed {
        warn!(
            key = %req.budget_key,
            reason = %result.reason,
            "budget/check: rejected"
        );
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::to_value(&result).unwrap()),
        )
    } else {
        (StatusCode::OK, Json(serde_json::to_value(&result).unwrap()))
    }
}

pub(super) async fn budget_record(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<BudgetRecordRequest>,
) -> impl IntoResponse {
    let status = state.budget.record(&req.budget_key, req.actual_cost).await;
    Json(serde_json::to_value(&status).unwrap())
}
