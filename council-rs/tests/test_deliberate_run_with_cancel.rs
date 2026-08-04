//! Characterization tests for `engine::deliberate::run_with_cancel`.
//!
//! PR7 gate: bind orchestration contracts before any phase extraction.
//! No live providers — mock seats/roles only; env isolated per test.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use council_rs::config::Config;
use council_rs::engine::context::RequestContext;
use council_rs::engine::deliberate;
use council_rs::mode::Mode;
use council_rs::types::{
    Cabinet, Chair, ExecutionRoute, RoleCascadeStep, RoleDefinition, RolesConfig, Seat,
    SessionOrigin,
};
use tokio_util::sync::CancellationToken;

/// Serialize tests that mutate process env (sessions dir, evidence switches).
fn env_lock() -> MutexGuard<'static, ()> {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn mock_step(model: &str) -> RoleCascadeStep {
    RoleCascadeStep {
        provider: "mock".into(),
        model: model.into(),
        max_tokens: 256,
    }
}

fn mock_roles() -> RolesConfig {
    let step = mock_step("mock-role");
    let validator = mock_step("mock-claim-validator");
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
            cascade: vec![validator],
        },
        scope_auditor: RoleDefinition {
            description: "test auditor".into(),
            cascade: vec![step],
        },
    }
}

fn mock_seat(name: &str, model: &str) -> Seat {
    Seat {
        name: name.into(),
        provider: "mock".into(),
        model: model.into(),
        system: "You are a mock seat.".into(),
    }
}

fn mock_config(name: &str, rounds: u32, seats: Vec<Seat>) -> Config {
    let mut cabinets = HashMap::new();
    cabinets.insert(
        name.into(),
        Cabinet {
            hash: String::new(),
            name: name.into(),
            description: "characterization cabinet".into(),
            rounds,
            seats,
            chair: Chair {
                name: "chair".into(),
                provider: "mock".into(),
                model: "mock-chair".into(),
                system: None,
                thinking_effort: None,
            },
            local_code_only: false,
            synthesis_mode: Default::default(),
        },
    );
    Config {
        cabinets,
        models: council_rs::types::ModelRegistry {
            models: HashMap::new(),
        },
        roles: mock_roles(),
        tera: tera::Tera::default(),
        base_dir: PathBuf::from("."),
    }
}

struct SessionDirs {
    root: PathBuf,
    sessions: PathBuf,
}

impl SessionDirs {
    fn install() -> Self {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let sessions = root.join("sessions");
        let runs = root.join("runs");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&runs).unwrap();
        // Force no-spend evidence path (no native web gather).
        unsafe {
            std::env::set_var("COUNCIL_SESSIONS_DIR", sessions.to_str().unwrap());
            std::env::set_var("COUNCIL_RUNS_DIR", runs.to_str().unwrap());
            std::env::set_var("COUNCIL_SHELDON_WEB_EVIDENCE", "off");
        }
        Self { root, sessions }
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn list_json(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn load_session(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn api_ctx(parent: &str) -> RequestContext {
    RequestContext {
        parent_request_id: Some(parent.into()),
        via_gateway: Some(false),
        council_auto_escalate: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn run_with_cancel_happy_path_binds_session_contract() {
    let _guard = env_lock();
    let dirs = SessionDirs::install();
    // Two polar seats so the keyword judge does not early-converge (score 0.5).
    let config = mock_config(
        "quick",
        2,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );

    let session = deliberate::run_with_cancel(
        &config,
        "quick",
        "Hello 2+2",
        "Context",
        Mode::TearDown,
        true, // blind — no precedent index I/O
        false,
        false,
        None,
        "best",
        false,
        "mock",
        false,
        SessionOrigin::Api,
        api_ctx("parent-req-happy"),
        None,
        None,
    )
    .await
    .expect("happy path");

    assert_eq!(session.rounds.len(), 2, "exact planned round count (no early stop)");
    assert!(session.synthesis.is_some(), "chair synthesis required");
    assert_eq!(session.synthesis_model.as_deref(), Some("mock-chair"));
    assert_eq!(session.origin, SessionOrigin::Api);
    assert_eq!(session.execution_route, ExecutionRoute::Direct);
    assert_eq!(session.parent_request_id.as_deref(), Some("parent-req-happy"));
    assert_eq!(session.tier, "best");
    assert!(!session.specops_triggered);
    assert!(!session.session_id.is_empty());

    let saved = list_json(&dirs.sessions);
    assert_eq!(saved.len(), 1, "exactly one canonical session file");
    let disk = load_session(&saved[0]);
    assert_eq!(disk["session_id"], session.session_id);
    assert_eq!(disk["origin"], "api");
    assert_eq!(disk["execution_route"], "direct");
    assert_eq!(disk["parent_request_id"], "parent-req-happy");
    assert_eq!(disk["tier"], "best");
    assert!(disk["synthesis"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(disk["rounds"].as_array().map(|a| a.len()), Some(2));
    assert!(
        list_json(&dirs.sessions.join("_cancelled")).is_empty(),
        "happy path must not write cancelled diagnostics"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_pre_cancelled_writes_only_api_cancelled_partial() {
    let _guard = env_lock();
    let dirs = SessionDirs::install();
    let config = mock_config(
        "quick",
        2,
        vec![mock_seat("seat_a", "mock-model")],
    );
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = deliberate::run_with_cancel(
        &config,
        "quick",
        "pre-cancelled topic",
        "Context",
        Mode::TearDown,
        true,
        false,
        false,
        None,
        "best",
        false,
        "mock",
        false,
        SessionOrigin::Api,
        api_ctx("parent-req-cancel"),
        None,
        Some(cancel),
    )
    .await
    .expect_err("pre-cancelled must Err");

    assert!(
        err.to_string().contains("cancelled"),
        "error message must name cancellation, got: {err}"
    );

    let canonical = list_json(&dirs.sessions);
    assert!(
        canonical.is_empty(),
        "pre-cancelled must not write a canonical session, found {canonical:?}"
    );

    let cancelled = list_json(&dirs.sessions.join("_cancelled"));
    assert_eq!(cancelled.len(), 1, "exactly one _cancelled diagnostic");
    let partial = load_session(&cancelled[0]);
    assert_eq!(partial["origin"], "api_cancelled");
    assert!(partial["synthesis"].is_null());
    assert_eq!(partial["parent_request_id"], "parent-req-cancel");
    assert_eq!(partial["tier"], "best");
    assert_eq!(
        partial["rounds"].as_array().map(|a| a.len()),
        Some(0),
        "pre-round cancel has no completed rounds"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_zero_budget_ends_after_round_one() {
    let _guard = env_lock();
    let dirs = SessionDirs::install();
    let config = mock_config(
        "budgeted",
        3,
        vec![mock_seat("seat_a", "mock-model")],
    );

    let session = deliberate::run_with_cancel(
        &config,
        "budgeted",
        "budget topic",
        "Context",
        Mode::TearDown,
        true,
        false,
        false,
        Some(0.0),
        "best",
        false,
        "mock",
        false,
        SessionOrigin::Api,
        api_ctx("parent-req-budget"),
        None,
        None,
    )
    .await
    .expect("zero-budget run");

    assert_eq!(session.rounds.len(), 1, "must stop after first round");
    let budget = session.budget.expect("budget record required");
    assert!(budget.paused, "paused must be true");
    assert_eq!(budget.action_taken.as_deref(), Some("end_early"));
    assert!((budget.max_usd - 0.0).abs() < f64::EPSILON);
    assert!(session.synthesis.is_some(), "still synthesizes after early end");
    assert_eq!(list_json(&dirs.sessions).len(), 1, "canonical session saved");

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_terminating_round_skips_gate_redaction() {
    let _guard = env_lock();
    let dirs = SessionDirs::install();
    // Two seats so validate_round does not skip for insufficient responses.
    let config = mock_config(
        "gated",
        3,
        vec![
            mock_seat("seat_a", "mock-gated-seat"),
            mock_seat("seat_b", "mock-gated-seat"),
        ],
    );

    let session = deliberate::run_with_cancel(
        &config,
        "gated",
        "gate placement topic",
        // Non-empty context supplies repo evidence so the mock validator is reached.
        "fixture repo context for no-spend validation",
        Mode::TearDown,
        true,
        false,
        false,
        Some(0.0), // budget terminates after round 1
        "best",
        true,  // validate
        "mock",
        true,  // validate_gate
        SessionOrigin::Api,
        api_ctx("parent-req-gate"),
        None,
        None,
    )
    .await
    .expect("validate+gate terminating run");

    assert_eq!(session.rounds.len(), 1);
    let round = &session.rounds[0];
    assert!(
        round.validation_report.is_some(),
        "terminating round must still run validation when enabled"
    );
    // Gate redaction only applies on *continuing* intermediate rounds.
    // Budget termination makes round 1 terminating — full claim text must remain.
    for resp in &round.responses {
        assert!(
            resp.text.contains("UNIQUE_CONTRADICTED_CLAIM_XYZ_12345"),
            "terminating round must not apply gate redaction; got: {}",
            resp.text
        );
        assert!(
            !resp.text.contains("REDACTED"),
            "no REDACTED marker on terminating round"
        );
    }
    assert!(session.synthesis.is_some());
    assert_eq!(list_json(&dirs.sessions).len(), 1);

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_api_default_suppresses_specops() {
    let _guard = env_lock();
    let dirs = SessionDirs::install();
    let config = mock_config(
        "specops",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    );

    // API origin + council_auto_escalate=false is the product default from
    // POST /api/deliberate. SpecOps must not fire even if a grok CLI is present.
    let session = deliberate::run_with_cancel(
        &config,
        "specops",
        "no-spend api specops check",
        "Context",
        Mode::TearDown,
        true,
        false,
        false,
        None,
        "best",
        false,
        "mock",
        false,
        SessionOrigin::Api,
        api_ctx("parent-req-specops"),
        None,
        None,
    )
    .await
    .expect("api run");

    assert!(
        !session.specops_triggered,
        "API default must suppress SpecOps auto-escalation"
    );
    assert_eq!(session.specops_cost_usd, 0.0);
    assert_eq!(session.origin, SessionOrigin::Api);

    dirs.cleanup();
}
