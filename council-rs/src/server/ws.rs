// WebSocket upgrade validation + librarian socket (moved from server.rs).

use axum::{
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;

use super::AppState;
use crate::librarian;

/// WebSocket paths whose auth is checked via `Sec-WebSocket-Protocol`
/// (`token.<secret>`) in the handler, not the bearer `auth_middleware`.
/// Browsers cannot set `Authorization` on a WS upgrade. Covers `/ws/deliberate`
/// and `/ws/librarian/{chat_id}` (R20).
pub(super) fn is_ws_subprotocol_path(norm_path: &str) -> bool {
    let p = norm_path.trim_end_matches('/');
    p == "/ws/deliberate" || p == "/ws/librarian" || p.starts_with("/ws/librarian/")
}

/// Validate WebSocket upgrade when bearer auth is required (browser cannot send Authorization).
pub(super) fn validate_ws_upgrade(headers: &HeaderMap) -> Result<(), StatusCode> {
    let auth_config = super::AUTH_CONFIG
        .get()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    if auth_config.dev_no_auth && auth_config.token.is_none() && auth_config.gateway_token.is_none()
    {
        return Ok(());
    }

    let Some(expected) = auth_config.token.as_ref() else {
        return Ok(());
    };

    let protocols = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut token_from_protocol: Option<&str> = None;
    for part in protocols
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(t) = part.strip_prefix("token.") {
            token_from_protocol = Some(t);
        }
    }

    match token_from_protocol {
        Some(t) if super::subtle_eq(t, expected) => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// WS /ws/deliberate — streaming deliberation.
pub(super) async fn ws_deliberate(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if validate_ws_upgrade(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Browsers require the server to echo a negotiated subprotocol in the 101 response.
    // War Room sends `["council", "token.<secret>"]` (see warroom/web/lib/ws.ts); axum splits
    // comma-separated `Sec-WebSocket-Protocol` values and `protocols(["council"])` selects it.
    ws.protocols(["council"])
        .on_upgrade(move |socket| super::ws_deliberate::handle_ws(socket, state))
}

/// WS /ws/librarian/{chat_id} — streaming librarian ask (R20).
///
/// Same upgrade/auth posture as `/ws/deliberate`: subprotocol `token.<secret>`
/// validated by `validate_ws_upgrade`, bearer skipped in `auth_middleware` via
/// `is_ws_subprotocol_path`, and the negotiated `council` subprotocol echoed on
/// the 101 so browser WebSocket open succeeds.
pub(super) async fn ws_librarian(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if validate_ws_upgrade(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.protocols(["council"])
        .on_upgrade(move |socket| handle_ws_librarian(socket, state, chat_id))
}

/// Handle a `/ws/librarian/{chat_id}` connection (R20).
///
/// Wire sequence: client sends `{type:"ask", text, client_msg_id}`; server emits
/// `ask_started` → zero or more `ask_chunk` → `sources` (if any) →
/// `ask_complete` → `done`, or `error` on failure.
///
/// The upstream librarian `/ask` is a single buffered POST (see
/// `LibrarianState::run_ask`) — there is no partial-streaming capability — so
/// this handler emits ZERO `ask_chunk` frames and does NOT fake-chunk the
/// finished string. The UI must handle the no-chunk case.
///
/// WS close mid-ask = cancel: the `run_ask` future is dropped when this task
/// returns, which is cancel-safe by feature contract semantics (owned permit drops,
/// upstream reqwest aborts, no wedged state). We drive `run_ask` concurrently
/// with a close watcher so a client `Stop` (socket close) aborts the in-flight
/// ask instead of waiting for the upstream timeout.
pub(super) async fn handle_ws_librarian(socket: WebSocket, state: AppState, chat_id: String) {
    use serde_json::json;

    let (mut ws_tx, mut ws_rx) = socket.split();

    // First message must be {type:"ask", text, client_msg_id}.
    let first = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<serde_json::Value>(&text).ok(),
        _ => None,
    };
    let Some(first) = first else {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"expected first message {type:'ask'}"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };
    if first.get("type").and_then(|v| v.as_str()) != Some("ask") {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"expected first message {type:'ask'}"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }
    let text = first
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let client_msg_id = first
        .get("client_msg_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Validate lengths up front (mirror POST /ask guards).
    if text.is_empty() || text.len() > librarian::routes::USER_CONTENT_MAX {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"content length"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }
    if client_msg_id.is_empty() || client_msg_id.len() > librarian::routes::CLIENT_MSG_ID_MAX {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"error","message":"client_msg_id length"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    let _ = ws_tx
        .send(Message::Text(
            json!({"type":"ask_started"}).to_string().into(),
        ))
        .await;

    // Drive run_ask concurrently with a close watcher: a client Stop (socket
    // close, or any inbound frame) cancels the ask by dropping the future.
    let ask_fut = state.librarian.run_ask(&chat_id, &text, &client_msg_id);
    let outcome = tokio::select! {
        biased;
        // Client closed / sent another frame → treat as cancel. Dropping
        // `ask_fut` here is the feature contract cancel path.
        _ = ws_rx.next() => {
            return;
        }
        out = ask_fut => out,
    };

    use librarian::routes::AskOutcome;
    match outcome {
        AskOutcome::Cached(result) => {
            // Idempotent replay: surface the cached assistant turn (zero chunks)
            // so a reconnect with the same client_msg_id is consistent.
            let assistant = result.get("assistant_turn").cloned().unwrap_or(json!({}));
            send_librarian_sources(&mut ws_tx, &assistant).await;
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"ask_complete","message":assistant})
                        .to_string()
                        .into(),
                ))
                .await;
            let _ = ws_tx
                .send(Message::Text(json!({"type":"done"}).to_string().into()))
                .await;
        }
        AskOutcome::Busy => {
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"error","message":"librarian busy"})
                        .to_string()
                        .into(),
                ))
                .await;
        }
        AskOutcome::Failed(_code, msg) => {
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"error","message":msg}).to_string().into(),
                ))
                .await;
        }
        AskOutcome::Ok { assistant_turn, .. } => {
            send_librarian_sources(&mut ws_tx, &assistant_turn).await;
            let _ = ws_tx
                .send(Message::Text(
                    json!({"type":"ask_complete","message":assistant_turn})
                        .to_string()
                        .into(),
                ))
                .await;
            let _ = ws_tx
                .send(Message::Text(json!({"type":"done"}).to_string().into()))
                .await;
        }
    }
}

/// Emit a `{type:"sources", sources:[...]}` frame from an assistant turn, but
/// only when the turn actually has sources (the frame is optional per R20).
pub(super) async fn send_librarian_sources(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    assistant_turn: &serde_json::Value,
) {
    if let Some(sources) = assistant_turn.get("sources").and_then(|v| v.as_array())
        && !sources.is_empty()
    {
        let _ = ws_tx
            .send(Message::Text(
                json!({"type":"sources","sources":sources})
                    .to_string()
                    .into(),
            ))
            .await;
    }
}

#[cfg(test)]
pub(crate) fn ws_path_skips_bearer_auth(norm_path: &str) -> bool {
    is_ws_subprotocol_path(norm_path)
}

#[cfg(test)]
mod ws_upgrade_auth_tests {
    use super::super::{AUTH_CONFIG, AuthConfig, router};
    use super::*;
    use crate::config::Config;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use std::sync::Arc;

    fn install_auth(token: &str) {
        let _ = AUTH_CONFIG.get_or_init(|| AuthConfig {
            token: Some(token.to_string()),
            gateway_token: None,
            dev_no_auth: false,
        });
    }

    /// Minimal `Config` for router-level auth tests — no cabinets/models needed
    /// because the request is rejected by `auth_middleware` before any handler
    /// touches state.
    fn empty_config() -> Arc<Config> {
        Arc::new(Config {
            cabinets: std::collections::HashMap::new(),
            models: crate::types::ModelRegistry {
                models: std::collections::HashMap::new(),
            },
            roles: crate::types::RolesConfig::default(),
            tera: tera::Tera::default(),
            base_dir: std::env::temp_dir(),
        })
    }

    #[test]
    fn ws_path_skips_bearer_middleware() {
        assert!(ws_path_skips_bearer_auth("/ws/deliberate"));
        assert!(!ws_path_skips_bearer_auth("/api/health"));
    }

    #[test]
    fn validate_ws_upgrade_accepts_matching_subprotocol_without_bearer() {
        install_auth("ws-test-secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("council, token.ws-test-secret"),
        );
        assert_eq!(validate_ws_upgrade(&headers), Ok(()));
    }

    #[test]
    fn validate_ws_upgrade_rejects_wrong_subprotocol_token() {
        install_auth("ws-test-secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("council, token.wrong"),
        );
        assert_eq!(validate_ws_upgrade(&headers), Err(StatusCode::UNAUTHORIZED));
    }

    /// A bearer-protected mutating route must 401 when no Authorization header
    /// is present. POST /api/cabinets/save is covered by the router-wide
    /// `auth_middleware` (same posture as the WS subprotocol auth above) — this
    /// proves the wiring, not just the helper.
    #[tokio::test]
    async fn cabinets_save_requires_auth() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        // Ensure a token is set (consistent with the other tests in this
        // module; `get_or_init` makes repeated installs harmless).
        install_auth("ws-test-secret");

        let app = router(empty_config());
        let req = Request::builder()
            .method("POST")
            .uri("/api/cabinets/save")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"my-cab","yaml":"name: X\nrounds: 1\nseats: []\nchair: {name: c, provider: grok, model: grok-4}"}"#))
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "save without bearer must be rejected by auth_middleware"
        );
    }
}
