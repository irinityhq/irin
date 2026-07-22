//! Browser WebSocket clients require the server to echo `Sec-WebSocket-Protocol: council`
//! on the 101 response when the client offers `council` + `token.<secret>`.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn ws_deliberate_upgrade_echoes_council_subprotocol() {
    const TOKEN: &str = "integration-ws-secret";
    // Edition 2024: env mutation is `unsafe` (process-global).
    unsafe {
        std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN);
    }

    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config =
        Arc::new(council_rs::config::Config::load(base).expect("load config for ws upgrade test"));
    let app = council_rs::server::router(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "GET /ws/deliberate HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Protocol: council, token.{TOKEN}\r\n\
         \r\n",
        addr.port()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut buf = vec![0u8; 2048];
    let n = stream.read(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf[..n]);

    assert!(
        response.starts_with("HTTP/1.1 101"),
        "expected 101 Switching Protocols, got:\n{response}"
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("sec-websocket-protocol: council"),
        "101 must negotiate council so browser WebSocket open succeeds, got:\n{response}"
    );
}
