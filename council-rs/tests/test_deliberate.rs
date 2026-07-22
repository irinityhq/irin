use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use council_rs::config::Config;
use council_rs::engine::deliberate;
use council_rs::mode::Mode;
use council_rs::types::{Cabinet, Chair, Seat};

#[tokio::test]
async fn test_mock_deliberate() {
    let mut cabinets = HashMap::new();

    let seat = Seat {
        name: "test_seat".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        system: "You are a mock seat.".into(),
    };

    let chair = Chair {
        name: "chair".into(),
        provider: "mock".into(),
        model: "mock-chair".into(),
        system: None,
        thinking_effort: None,
    };

    let cabinet = Cabinet {
        hash: String::new(),
        name: "quick".into(),
        description: "quick test".into(),
        rounds: 2,
        seats: vec![seat],
        chair,
        local_code_only: false,
        synthesis_mode: Default::default(),
    };

    cabinets.insert("quick".into(), cabinet);

    let config = Config {
        cabinets,
        models: council_rs::types::ModelRegistry {
            models: HashMap::new(),
        },
        roles: council_rs::types::RolesConfig::default(),
        tera: tera::Tera::default(),
        base_dir: PathBuf::from("."),
    };

    let test_dir = env::temp_dir().join("council-rs-smoke-test");
    let sessions_dir = test_dir.join("sessions");
    let runs_dir = test_dir.join("runs");
    let _ = fs::remove_dir_all(&test_dir); // clean up any old test run
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::create_dir_all(&runs_dir).unwrap();

    unsafe {
        env::set_var("COUNCIL_SESSIONS_DIR", sessions_dir.to_str().unwrap());
        env::set_var("COUNCIL_RUNS_DIR", runs_dir.to_str().unwrap());
    }

    let session = deliberate::run(
        &config,
        "quick",
        "Hello 2+2",
        "Context",
        Mode::TearDown,
        false,  // blind
        false,  // frame_check
        false,  // verbose
        None,   // budget_max_usd
        "best", // tier
        false,  // validate
        "mock", // validate_provider
        false,  // validate_gate
    )
    .await
    .expect("run deliberate");

    assert!(!session.rounds.is_empty(), "Expected at least 1 round");
    assert!(session.synthesis.is_some(), "Expected synthesis from chair");
    assert_eq!(session.synthesis_model.as_deref(), Some("mock-chair"));

    let _ = fs::remove_dir_all(&test_dir);
}
