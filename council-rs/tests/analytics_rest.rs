//! Phase 9 analytics REST integration tests (N03 clusters, N06 PDF export).
//!
//! Zero provider spend — these endpoints are pure local computation over the
//! sessions dir. We boot the real `server::router` against a temp
//! `COUNCIL_SESSIONS_DIR` with a fixture session on disk and exercise the
//! routes over a loopback TCP server with the configured bearer token.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use council_rs::config::Config;
use council_rs::types::{Cabinet, Chair, RoleCascadeStep, RoleDefinition, RolesConfig, Seat};
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

fn mock_roles() -> RolesConfig {
    let step = RoleCascadeStep {
        provider: "mock".into(),
        model: "mock-role".into(),
        max_tokens: 256,
    };
    RolesConfig {
        convergence_judge: RoleDefinition {
            description: "test judge".into(),
            cascade: vec![step.clone()],
        },
        frame_check: RoleDefinition {
            description: "test frame".into(),
            cascade: vec![step.clone()],
        },
        claim_validator: RoleDefinition {
            description: "test validator".into(),
            cascade: vec![step.clone()],
        },
        scope_auditor: RoleDefinition {
            description: "test auditor".into(),
            cascade: vec![step],
        },
    }
}

fn engine_test_config(name: &str, seat_provider: &str, chair_provider: &str) -> Config {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Config::load(base).expect("config");
    config.cabinets.clear();
    config.cabinets.insert(
        name.into(),
        Cabinet {
            hash: String::new(),
            name: name.into(),
            description: "REST engine classification test".into(),
            rounds: 1,
            seats: vec![Seat {
                name: "seat_a".into(),
                provider: seat_provider.into(),
                model: "mock-seat".into(),
                system: "You are a test seat.".into(),
            }],
            chair: Chair {
                name: "chair".into(),
                provider: chair_provider.into(),
                model: "mock-chair".into(),
                system: None,
                thinking_effort: None,
            },
            local_code_only: false,
            synthesis_mode: Default::default(),
        },
    );
    config.roles = mock_roles();
    config
}

struct FailingGeminiCli {
    root: PathBuf,
    previous_path: Option<OsString>,
}

impl FailingGeminiCli {
    fn install() -> Self {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let binary = root.join("gemini");
        std::fs::write(
            &binary,
            "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then exit 0; fi\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();

        let previous_path = std::env::var_os("PATH");
        let mut paths = vec![root.clone()];
        if let Some(path) = &previous_path {
            paths.extend(std::env::split_paths(path));
        }
        unsafe {
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        }
        Self {
            root,
            previous_path,
        }
    }
}

impl Drop for FailingGeminiCli {
    fn drop(&mut self) {
        unsafe {
            match &self.previous_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn boot_council_with_config(sessions_dir: &std::path::Path, config: Config) -> SocketAddr {
    unsafe {
        std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN);
        std::env::set_var("COUNCIL_SESSIONS_DIR", sessions_dir);
        std::env::set_var("COUNCIL_VIA_GATEWAY", "0");
    }
    let app = council_rs::server::router(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    addr
}

async fn boot_council(sessions_dir: &std::path::Path) -> SocketAddr {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = Config::load(base).expect("config");
    boot_council_with_config(sessions_dir, config).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

async fn post_deliberate(addr: SocketAddr, cabinet_name: &str) -> (u16, Value) {
    let response = client()
        .post(format!("http://{addr}/api/deliberate"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "model": "council-warroom",
            "messages": [{"role": "user", "content": "classify engine failure"}],
            "cabinet_name": cabinet_name,
            "blind": true
        }))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn deliberate_all_seats_failed_maps_to_quorum_failed_502() {
    let _guard = ENV_LOCK.lock().await;
    let dir = temp_dir("all_seats_failed");
    let _provider = FailingGeminiCli::install();
    let config = engine_test_config("all-fail", "gemini_cli", "gemini_cli");
    let addr = boot_council_with_config(&dir, config).await;

    let (status, body) = post_deliberate(addr, "all-fail").await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "quorum_failed");

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn deliberate_provider_unavailable_maps_to_unavailable_503() {
    let _guard = ENV_LOCK.lock().await;
    let dir = temp_dir("provider_unavailable");
    let config = engine_test_config("unavailable", "mock", "nope");
    let addr = boot_council_with_config(&dir, config).await;

    let (status, body) = post_deliberate(addr, "unavailable").await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "council_unavailable");

    let _ = std::fs::remove_dir_all(dir);
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
        .json(&json!({}))
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
        .json(&json!({}))
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
