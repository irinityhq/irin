//! Route-level proof that `/ws/deliberate` shares the deliberate concurrency
//! cap and releases permits when idle sockets close or start-frame timeout fires.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::Message;

const TOKEN: &str = "ws-capacity-test-secret";

/// Process-global env (`COUNCIL_AUTH_TOKEN`, cap, start timeout) is shared by
/// this binary's tests — serialize the full body of each.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

async fn boot_council(max_deliberations: usize) -> std::net::SocketAddr {
    // Edition 2024: env mutation is unsafe (process-global).
    unsafe {
        std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN);
        std::env::set_var(
            "COUNCIL_MAX_CONCURRENT_DELIBERATIONS",
            max_deliberations.to_string(),
        );
    }

    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config =
        Arc::new(council_rs::config::Config::load(base).expect("load config for capacity test"));
    let app = council_rs::server::router(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    addr
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_ws(addr: std::net::SocketAddr) -> Result<(WsStream, StatusCode), WsError> {
    let url = format!("ws://{addr}/ws/deliberate");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        format!("council, token.{TOKEN}").parse().unwrap(),
    );
    let (ws, resp) = tokio_tungstenite::connect_async(req).await?;
    Ok((ws, resp.status()))
}

/// Attempt upgrade; on HTTP refusal return the status code without a socket.
async fn upgrade_status(addr: std::net::SocketAddr) -> Result<StatusCode, StatusCode> {
    match connect_ws(addr).await {
        Ok((_ws, status)) => Ok(status),
        Err(WsError::Http(resp)) => Err(resp.status()),
        Err(e) => panic!("unexpected ws error: {e}"),
    }
}

/// POST /api/deliberate with a triage body + bearer.
async fn post_deliberate_status(
    addr: std::net::SocketAddr,
    messages: serde_json::Value,
) -> StatusCode {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resp = client
        .post(format!("http://{addr}/api/deliberate"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "model": "council-triage",
            "messages": messages
        }))
        .send()
        .await
        .expect("POST /api/deliberate");
    StatusCode::from_u16(resp.status().as_u16()).expect("status code")
}

#[tokio::test]
async fn post_deliberate_array_content_returns_400_without_taking_permit() {
    let _guard = env_guard().await;
    let addr = boot_council(1).await;

    let (held, status) = connect_ws(addr)
        .await
        .expect("hold the sole deliberate permit");
    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);

    let status = post_deliberate_status(
        addr,
        serde_json::json!([{
            "role": "user",
            "content": [{"type": "text", "text": "array topic"}]
        }]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty topic validation must run before semaphore acquisition"
    );

    let status = post_deliberate_status(
        addr,
        serde_json::json!([{"role": "user", "content": "   "}]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "whitespace-only topic validation must run before semaphore acquisition"
    );
    drop(held);
}

/// N idle upgrades fill the cap; N+1 is shed with HTTP 429. Closing one
/// held socket releases a permit so a later upgrade succeeds.
#[tokio::test]
async fn ws_deliberate_route_sheds_when_cap_full_and_releases_on_close() {
    let _guard = env_guard().await;
    let n = 2usize;
    let addr = boot_council(n).await;

    let mut held = Vec::new();
    for i in 0..n {
        let (ws, status) = connect_ws(addr)
            .await
            .unwrap_or_else(|e| panic!("upgrade {i} should succeed: {e}"));
        assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS, "held slot {i}");
        // Idle: no start frame — permit stays held for the connection lifetime.
        held.push(ws);
    }

    match upgrade_status(addr).await {
        Ok(status) => panic!("N+1 upgrade must not succeed, got {status}"),
        Err(status) => assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "N+1 concurrent idle WS must be shed by the shared cap"
        ),
    }

    // Cross-surface: the same semaphore must also shed HTTP POST /api/deliberate.
    let post_status = post_deliberate_status(
        addr,
        serde_json::json!([{"role": "user", "content": "capacity probe"}]),
    )
    .await;
    assert_eq!(
        post_status,
        StatusCode::TOO_MANY_REQUESTS,
        "POST /api/deliberate must 429 while WS holds all deliberate permits"
    );

    // Release one permit by closing a held socket.
    let dropped = held.pop().expect("held connection");
    drop(dropped);
    // Give the server task a moment to observe close and drop the permit.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (ws, status) = connect_ws(addr)
        .await
        .expect("upgrade after release should succeed");
    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);
    drop(ws);
    // Keep remaining held sockets alive until end of test.
    drop(held);
}

/// Short start-frame timeout closes idle upgrades and releases the permit.
#[tokio::test]
async fn ws_deliberate_start_timeout_releases_permit() {
    let _guard = env_guard().await;
    // Cap 1: one idle socket must block the next upgrade until timeout frees it.
    let addr = boot_council(1).await;
    unsafe {
        std::env::set_var("COUNCIL_WS_START_TIMEOUT_MS", "200");
    }

    let (mut idle, status) = connect_ws(addr).await.expect("idle upgrade");
    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);

    // While idle within the timeout window, capacity is exhausted.
    match upgrade_status(addr).await {
        Ok(s) => panic!("second upgrade must 429 while first is idle, got {s}"),
        Err(s) => assert_eq!(s, StatusCode::TOO_MANY_REQUESTS),
    }

    // Server should send a fatal error frame then close.
    let mut saw_timeout_error = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), idle.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).expect("json error frame");
                if v.get("type").and_then(|x| x.as_str()) == Some("error") {
                    let msg = v
                        .pointer("/data/message")
                        .and_then(|m| m.as_str())
                        .or_else(|| v.get("message").and_then(|m| m.as_str()))
                        .unwrap_or("");
                    if msg.contains("timed out") || msg.contains("timeout") {
                        saw_timeout_error = true;
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
            Ok(Some(Ok(_))) => continue,
            Err(_) => break,
        }
    }
    assert!(
        saw_timeout_error,
        "idle start wait must emit a terminal error mentioning timeout"
    );

    // Permit released: a fresh upgrade must succeed.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (ws, status) = connect_ws(addr)
        .await
        .expect("upgrade after start timeout should succeed");
    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);
    drop(ws);

    // Avoid leaking a short timeout into any later test in this binary.
    unsafe {
        std::env::remove_var("COUNCIL_WS_START_TIMEOUT_MS");
    }
}
