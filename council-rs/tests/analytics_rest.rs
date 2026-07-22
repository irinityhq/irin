//! Phase 9 analytics REST integration tests (N03 clusters, N06 PDF export).
//!
//! Zero provider spend — these endpoints are pure local computation over the
//! sessions dir. We boot the real `server::router` against a temp
//! `COUNCIL_SESSIONS_DIR` with a fixture session on disk and exercise the
//! routes over a loopback TCP server with the configured bearer token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::TcpListener;

const TOKEN: &str = "analytics-rest-secret";

/// This test binary mutates process-global env (`COUNCIL_AUTH_TOKEN`,
/// `COUNCIL_SESSIONS_DIR`) that `router()` reads at boot. It is a dedicated
/// binary, but serialize the bodies anyway so the two tests never interleave
/// their sessions dir.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn temp_dir(tag: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let p = std::env::temp_dir().join(format!(
        "council_analytics_{tag}_{}_{:?}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Write a minimal but schema-valid CouncilSession JSON into `dir`.
fn write_fixture_session(dir: &std::path::Path, id: &str, topic: &str, synthesis: &str) {
    let session = json!({
        "session_id": id,
        "topic": topic,
        "cabinet_name": "standard",
        "rounds": [],
        "synthesis": synthesis,
        "synthesis_model": "grok-4.3",
        "total_tokens": 0,
        "total_latency_ms": 0,
        "total_cost_usd": 0.0,
        "specops_triggered": false,
        "specops_cost_usd": 0.0,
        "mode": "teardown",
        "precedent_ids": [],
        "timestamp": "2026-06-06T12:00:00Z",
        "schema_version": 2,
        "tier": "best",
        "context_sources": []
    });
    let fname = format!("council_20260606_120000_{id}.json");
    std::fs::write(
        dir.join(fname),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();
}

async fn boot_council(sessions_dir: &std::path::Path) -> SocketAddr {
    unsafe {
        std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN);
        std::env::set_var("COUNCIL_SESSIONS_DIR", sessions_dir);
    }
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = Arc::new(council_rs::config::Config::load(base).expect("config"));
    let app = council_rs::server::router(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

#[tokio::test]
async fn export_pdf_returns_pdf_for_fixture_and_404_for_unknown() {
    let _guard = ENV_LOCK.lock().await;
    let dir = temp_dir("pdf");
    write_fixture_session(
        &dir,
        "deadbeef0001",
        "Should we migrate auth to passkeys?",
        "## Ruling\n\nShip it incrementally. Confidence HIGH.",
    );
    let addr = boot_council(&dir).await;
    let c = client();

    // Known session → 200 application/pdf with %PDF magic + nonzero length.
    let resp = c
        .post(format!(
            "http://{addr}/api/sessions/deadbeef0001/export/pdf"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "known session should export");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(ct, "application/pdf");
    let disp = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        disp.contains("attachment") && disp.contains("council_deadbeef0001.pdf"),
        "attachment filename, got {disp:?}"
    );
    let bytes = resp.bytes().await.unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "PDF magic");
    assert!(bytes.len() > 200, "nonzero PDF body, got {}", bytes.len());

    // Unknown session → 404.
    let resp = c
        .post(format!("http://{addr}/api/sessions/nope/export/pdf"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "unknown session should 404");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn clusters_empty_index_returns_200_empty() {
    let _guard = ENV_LOCK.lock().await;
    let dir = temp_dir("clusters_empty");
    let addr = boot_council(&dir).await;
    let c = client();

    let resp = c
        .get(format!("http://{addr}/api/clusters"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "empty index still 200");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["clusters"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(body["n_sessions"], 0);
    assert_eq!(body["method"], "kmeans");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn interventions_predict_returns_frequency_when_few_samples() {
    let _guard = ENV_LOCK.lock().await;
    let dir = temp_dir("predict");
    // No intervention log → zero samples → frequency method, probability 0.
    let addr = boot_council(&dir).await;
    let c = client();

    let resp = c
        .get(format!(
            "http://{addr}/api/interventions/predict?convergence=0.4&round=2"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["method"], "frequency");
    assert_eq!(body["n_samples"], 0);
    assert_eq!(body["probability"], 0.0);

    let _ = std::fs::remove_dir_all(&dir);
}
