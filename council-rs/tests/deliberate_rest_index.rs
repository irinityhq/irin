use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use council_rs::config::Config;
use council_rs::types::{
    Cabinet, Chair, RoleCascadeStep, RoleDefinition, RolesConfig, Seat, SynthesisMode,
};
use serde_json::json;
use tokio::net::TcpListener;

const TOKEN: &str = "deliberate-rest-index-secret";
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

fn test_config(name: &str, synthesis_mode: SynthesisMode) -> Config {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Config::load(base).expect("config");
    config.cabinets.clear();
    config.cabinets.insert(
        name.into(),
        Cabinet {
            hash: String::new(),
            name: name.into(),
            description: "REST index test".into(),
            rounds: 1,
            seats: vec![Seat {
                name: "seat_a".into(),
                provider: "mock".into(),
                model: "mock-seat".into(),
                system: "You are a test seat.".into(),
            }],
            chair: Chair {
                name: "chair".into(),
                provider: "mock".into(),
                model: "mock-chair".into(),
                system: None,
                thinking_effort: None,
            },
            local_code_only: false,
            synthesis_mode,
        },
    );
    config.roles = mock_roles();
    config
}

async fn boot_council(sessions_dir: &std::path::Path, config: Config) -> SocketAddr {
    unsafe {
        std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN);
        std::env::set_var("COUNCIL_SESSIONS_DIR", sessions_dir);
        std::env::set_var("COUNCIL_VIA_GATEWAY", "0");
        std::env::set_var("COUNCIL_SHELDON_WEB_EVIDENCE", "off");
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

async fn post_deliberate(addr: SocketAddr, cabinet_name: &str) -> reqwest::Response {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
        .post(format!("http://{addr}/api/deliberate"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "model": "council-warroom",
            "messages": [{"role": "user", "content": "index this completed session"}],
            "cabinet_name": cabinet_name,
            "blind": true
        }))
        .send()
        .await
        .unwrap()
}

fn index_count(sessions_dir: &std::path::Path, session_id: &str) -> usize {
    std::fs::read_to_string(sessions_dir.join("index.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["id"] == session_id)
        .count()
}

fn saved_session_id(sessions_dir: &std::path::Path) -> String {
    let path = std::fs::read_dir(sessions_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("council_") && name.ends_with(".json"))
        })
        .expect("saved session");
    let session: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    session["session_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn successful_deliberate_indexes_session_exactly_once() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().expect("tempdir");
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let addr = boot_council(&sessions, test_config("rest-index", SynthesisMode::Generic)).await;

    let response = post_deliberate(addr, "rest-index").await;
    assert_eq!(response.status(), 200);
    let session_id = response
        .headers()
        .get("X-Council-Session-Id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(index_count(&sessions, &session_id), 1);
}

#[tokio::test]
async fn malformed_directive_is_unprocessable_and_never_indexed() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().expect("tempdir");
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let addr = boot_council(
        &sessions,
        test_config("rest-malformed", SynthesisMode::DirectiveProposalV1),
    )
    .await;

    let response = post_deliberate(addr, "rest-malformed").await;
    assert_eq!(response.status(), 422);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "malformed_directive_proposal");

    let session_id = saved_session_id(&sessions);
    assert_eq!(index_count(&sessions, &session_id), 0);
}
