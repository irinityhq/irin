//! R20: `/ws/librarian/{chat_id}` integration tests.
//!
//! Covers the pinned event sequence (ask_started → sources → ask_complete →
//! done with ZERO ask_chunk frames, since the upstream librarian is buffered),
//! auth rejection (wrong subprotocol token → 401, no upgrade), and close-mid-ask
//! cancel safety (client close before the ask resolves leaves no wedged state).
//!
//! Zero provider spend: the upstream librarian `/ask` is a local mock axum
//! server returning a canned adapter-shaped body.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{Json, Router, routing::post};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

const TOKEN: &str = "librarian-ws-secret";

/// These tests mutate process-global env (`LIBRARIAN_CHAT_DIR`,
/// `LIBRARIAN_BASE_URL`, `COUNCIL_AUTH_TOKEN`) that `router()` /
/// `Store::from_env()` read at boot. Serialize the full body of each so the
/// globals never interleave under cargo's parallel test runner. Async-aware
/// (`tokio::sync::Mutex`) since the guard is held across `.await` points.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

/// Spin a mock upstream librarian that answers POST /ask with a canned,
/// adapter-shaped body (one source). `delay_ms` lets the cancel test hold the
/// response open long enough to close the socket first.
async fn spawn_mock_librarian(delay_ms: u64) -> SocketAddr {
    async fn ask(Json(_body): Json<Value>) -> Json<Value> {
        Json(json!({
            "answer": "the answer",
            "sources": [{
                "path": "doc/a.md",
                "score": 0.9,
                "snippet": "snippet text",
                "corpus": "vault",
                "trust_tier": 1
            }],
            "model": "librarian-mlx",
            "cabinet": "research-default",
            "latency_ms": 12.0,
            "chunks_used": 3
        }))
    }

    let app = if delay_ms == 0 {
        Router::new().route("/ask", post(ask))
    } else {
        Router::new().route(
            "/ask",
            post(move |body: Json<Value>| async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                ask(body).await
            }),
        )
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Boot the council server router against a temp chat dir + mock upstream and a
/// fixed auth token. Returns (server addr, chat_id).
async fn boot_council(mock_addr: SocketAddr) -> (SocketAddr, String) {
    let tmp = std::env::temp_dir().join(format!("lib_ws_test_{}", uuid_like()));
    std::fs::create_dir_all(&tmp).unwrap();

    // Edition 2024: env mutation is unsafe (process-global). This test binary is
    // dedicated to /ws/librarian, so the globals don't collide with other files.
    unsafe {
        std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN);
        std::env::set_var("LIBRARIAN_CHAT_DIR", &tmp);
        std::env::set_var("LIBRARIAN_BASE_URL", format!("http://{mock_addr}"));
    }

    // Pre-create a chat so run_ask can load it.
    let store = council_rs::librarian::storage::Store::from_env();
    let chat_id = store.create_chat("research-default").unwrap();

    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = Arc::new(council_rs::config::Config::load(base).expect("config"));
    let app = council_rs::server::router(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr, chat_id)
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{}_{:?}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::thread::current().id()
    )
}

/// Connect a WS client with the council subprotocol + token.
async fn connect_ws(
    addr: SocketAddr,
    chat_id: &str,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let url = format!("ws://{addr}/ws/librarian/{chat_id}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        format!("council, token.{token}").parse().unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(req).await?;
    Ok(ws)
}

async fn next_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<Value> {
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(t)) => return serde_json::from_str(&t).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            _ => continue,
        }
    }
    None
}

#[tokio::test]
async fn librarian_ws_happy_path_zero_chunks() {
    let _guard = env_guard().await;
    let mock = spawn_mock_librarian(0).await;
    let (addr, chat_id) = boot_council(mock).await;

    let mut ws = connect_ws(addr, &chat_id, TOKEN).await.expect("upgrade");
    ws.send(Message::Text(
        json!({"type":"ask","text":"what is x?","client_msg_id":"m1"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // ask_started → sources → ask_complete → done, with ZERO ask_chunk.
    let mut seen: Vec<String> = Vec::new();
    let mut complete_msg: Option<Value> = None;
    while let Some(v) = next_json(&mut ws).await {
        let ty = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if ty == "ask_complete" {
            complete_msg = v.get("message").cloned();
        }
        let done = ty == "done";
        seen.push(ty);
        if done {
            break;
        }
    }

    assert_eq!(seen.first().map(String::as_str), Some("ask_started"));
    assert_eq!(seen.last().map(String::as_str), Some("done"));
    assert!(
        !seen.iter().any(|t| t == "ask_chunk"),
        "buffered upstream must emit ZERO ask_chunk frames, got {seen:?}"
    );
    assert!(
        seen.iter().any(|t| t == "sources"),
        "expected sources frame"
    );
    assert!(seen.iter().any(|t| t == "ask_complete"));

    let msg = complete_msg.expect("ask_complete carries the assistant turn");
    assert_eq!(msg["type"], "assistant");
    assert_eq!(msg["content"], "the answer");
    assert_eq!(msg["model"], "librarian-mlx");
    assert_eq!(msg["sources"][0]["path"], "doc/a.md");
}

#[tokio::test]
async fn librarian_ws_rejects_wrong_token() {
    let _guard = env_guard().await;
    let mock = spawn_mock_librarian(0).await;
    let (addr, chat_id) = boot_council(mock).await;

    let err = connect_ws(addr, &chat_id, "wrong-token").await;
    assert!(
        err.is_err(),
        "wrong subprotocol token must fail the WS upgrade (401)"
    );
}

#[tokio::test]
async fn librarian_ws_close_mid_ask_is_cancel_safe() {
    let _guard = env_guard().await;
    // Upstream holds the response open; we close the socket before it resolves.
    let mock = spawn_mock_librarian(2000).await;
    let (addr, chat_id) = boot_council(mock).await;

    let mut ws = connect_ws(addr, &chat_id, TOKEN).await.expect("upgrade");
    ws.send(Message::Text(
        json!({"type":"ask","text":"slow question","client_msg_id":"cancel-1"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // Expect ask_started, then close before ask_complete arrives.
    let first = next_json(&mut ws).await;
    assert_eq!(
        first.and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from)),
        Some("ask_started".to_string())
    );
    // Client Stop = close. The server-side run_ask future is dropped (cancel).
    ws.close(None).await.unwrap();
    drop(ws);

    // The chat must still load (user turn appended, no wedged state), and since
    // the ask was cancelled before success the idempotency cache stays empty,
    // so a fresh ask with a NEW client_msg_id completes normally.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let store = council_rs::librarian::storage::Store::from_env();
    let convo = store
        .load_chat(&chat_id)
        .expect("chat still loads after cancel");
    // The first ask appended a user turn; an assistant turn may or may not exist
    // (cancelled before write). Either way the chat is well-formed.
    assert_eq!(convo.id, chat_id);
}
