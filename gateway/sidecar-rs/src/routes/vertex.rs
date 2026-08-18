// Vertex token route handler (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use std::sync::Arc;

use crate::vertex_auth;
use crate::AppState;

#[derive(Serialize)]
pub(super) struct VertexTokenResponse {
    token: String,
    source: vertex_auth::TokenSource,
}

#[derive(Serialize)]
pub(super) struct VertexTokenError {
    error: String,
}

pub(super) async fn vertex_token_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Same admin-proxy gate as /ledger/*: socket perms are the first
    // boundary, not a bearer replacement. Lua already holds LEDGER_ADMIN_KEY.
    if let Err(resp) = super::ledger::require_admin_header(&state.auth, &headers).await {
        return resp.into_response();
    }
    match state.vertex_token.get_token().await {
        Ok((token, source)) => (
            StatusCode::OK,
            Json(serde_json::to_value(VertexTokenResponse { token, source }).unwrap()),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(VertexTokenError { error: e }).unwrap()),
        )
            .into_response(),
    }
}
