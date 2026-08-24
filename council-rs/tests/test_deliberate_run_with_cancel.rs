//! Characterization tests for `engine::deliberate::run_with_cancel`.
//!
//! PR7 gate: bind orchestration contracts before any phase extraction.
//! No live providers — mock seats/roles only; env isolated per test.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use council_rs::config::Config;
use council_rs::engine::context::RequestContext;
use council_rs::engine::deliberate;
use council_rs::mode::Mode;
use council_rs::types::{
    Cabinet, Chair, ExecutionRoute, RoleCascadeStep, RoleDefinition, RolesConfig, Seat,
    SessionOrigin,
};
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

/// Serialize tests that mutate process env (sessions dir, evidence switches).
/// Async-aware so clippy await_holding_lock stays clean under ship-check.
async fn env_lock() -> MutexGuard<'static, ()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(())).lock().await
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

struct FailingGeminiCli {
    root: PathBuf,
    call_log: PathBuf,
    previous_path: Option<OsString>,
    previous_log: Option<OsString>,
}

impl FailingGeminiCli {
    fn install() -> Self {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let binary = root.join("gemini");
        let call_log = root.join("calls.log");
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then exit 0; fi\nprintf 'call\\n' >> \"$COUNCIL_TEST_PROVIDER_CALL_LOG\"\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();

        let previous_path = std::env::var_os("PATH");
        let previous_log = std::env::var_os("COUNCIL_TEST_PROVIDER_CALL_LOG");
        let mut paths = vec![root.clone()];
        if let Some(path) = &previous_path {
            paths.extend(std::env::split_paths(path));
        }
        unsafe {
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
            std::env::set_var("COUNCIL_TEST_PROVIDER_CALL_LOG", &call_log);
        }

        Self {
            root,
            call_log,
            previous_path,
            previous_log,
        }
    }

    fn call_count(&self) -> usize {
        fs::read_to_string(&self.call_log)
            .map(|calls| calls.lines().count())
            .unwrap_or(0)
    }
}

impl Drop for FailingGeminiCli {
    fn drop(&mut self) {
        unsafe {
            match &self.previous_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            match &self.previous_log {
                Some(path) => std::env::set_var("COUNCIL_TEST_PROVIDER_CALL_LOG", path),
                None => std::env::remove_var("COUNCIL_TEST_PROVIDER_CALL_LOG"),
            }
        }
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
async fn budget_signal_empty_on_guard_miss() {
    let _guard = env_lock().await;
    let previous_guard = std::env::var_os("HERMES_BUDGET_GUARD_SCRIPT");
    let missing_guard = tempfile::tempdir()
        .unwrap()
        .path()
        .join("missing-budget-guard");
    unsafe {
        std::env::set_var("HERMES_BUDGET_GUARD_SCRIPT", &missing_guard);
    }

    let (signal, tier) = deliberate::fetch_budget_signal(Some("default"), Some("test-task"));

    unsafe {
        match previous_guard {
            Some(value) => std::env::set_var("HERMES_BUDGET_GUARD_SCRIPT", value),
            None => std::env::remove_var("HERMES_BUDGET_GUARD_SCRIPT"),
        }
    }
    let prompt = deliberate::build_round_prompt(
        "Budget guard miss",
        "",
        "",
        "",
        &[],
        &signal,
        &mock_seat("seat_a", "mock-model"),
        1,
    );

    assert!(signal.is_empty(), "guard miss must emit no budget signal");
    assert_eq!(tier, "UNKNOWN");
    assert!(!prompt.contains("REMAINING_USD"));
    assert!(!prompt.contains("$7"));
}

#[tokio::test]
async fn run_with_cancel_all_failed_seats_skips_chair_and_writes_partial() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let provider = FailingGeminiCli::install();
    let mut config = mock_config(
        "all-fail",
        1,
        vec![
            mock_seat("seat_a", "mock-seat-a"),
            mock_seat("seat_b", "mock-seat-b"),
        ],
    );
    let cabinet = config.cabinets.get_mut("all-fail").unwrap();
    for seat in &mut cabinet.seats {
        seat.provider = "gemini_cli".into();
    }
    cabinet.chair.provider = "gemini_cli".into();

    let err = deliberate::run_with_cancel(
        &config,
        "all-fail",
        "all providers fail",
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
        api_ctx("parent-req-all-fail"),
        None,
        None,
    )
    .await
    .expect_err("zero usable seat responses must fail before chair synthesis");

    assert!(err.to_string().contains("all seats failed"), "{err:#}");
    assert_eq!(
        provider.call_count(),
        2,
        "chair transport must not be called"
    );
    assert!(list_json(&dirs.sessions).is_empty());
    let partials = list_json(&dirs.sessions.join("_cancelled"));
    assert_eq!(partials.len(), 1, "failed cabinet writes one partial");
    let partial = load_session(&partials[0]);
    assert!(partial["synthesis"].is_null());
    assert_eq!(partial["chair_cost_usd"], 0.0);

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_unavailable_provider_fails_before_any_seat_call() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let provider = FailingGeminiCli::install();
    let mut config = mock_config("unavailable", 1, vec![mock_seat("seat_a", "mock-seat-a")]);
    let cabinet = config.cabinets.get_mut("unavailable").unwrap();
    cabinet.seats[0].provider = "gemini_cli".into();
    cabinet.chair.provider = "nope".into();

    let err = deliberate::run_with_cancel(
        &config,
        "unavailable",
        "unavailable provider",
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
        api_ctx("parent-req-unavailable"),
        None,
        None,
    )
    .await
    .expect_err("unavailable chair must fail before seat fan-out");

    assert!(
        err.to_string()
            .contains("provider unavailable: Chair (nope)"),
        "{err:#}"
    );
    assert_eq!(provider.call_count(), 0, "no seat transport may run");
    assert!(list_json(&dirs.sessions).is_empty());
    assert!(list_json(&dirs.sessions.join("_cancelled")).is_empty());

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_redacts_seat_and_chair_secrets_without_indexing() {
    const MOCK_SLACK_TOKEN: &str = concat!("xoxb-", "0000000000FAKEFIXTURE");

    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let mut config = mock_config(
        "redaction",
        1,
        vec![mock_seat("seat_a", "mock-slack-token")],
    );
    config.cabinets.get_mut("redaction").unwrap().chair.model = "mock-slack-token".into();

    deliberate::run_with_cancel(
        &config,
        "redaction",
        "redact mock provider secret",
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
        api_ctx("parent-req-redaction"),
        None,
        None,
    )
    .await
    .expect("redaction run");

    let saved = list_json(&dirs.sessions);
    assert_eq!(saved.len(), 1, "exactly one canonical session file");
    let body = fs::read_to_string(&saved[0]).unwrap();
    assert!(
        !body.contains(MOCK_SLACK_TOKEN),
        "secret reached session file"
    );
    assert!(
        body.contains("[REDACTED:secret]"),
        "secret was not redacted"
    );
    let disk = load_session(&saved[0]);
    let seat_text = disk["rounds"][0]["responses"][0]["text"]
        .as_str()
        .expect("persisted seat text");
    assert!(!seat_text.contains(MOCK_SLACK_TOKEN));
    assert!(seat_text.contains("[REDACTED:secret]"));
    let synthesis = disk["synthesis"].as_str().expect("persisted synthesis");
    assert!(!synthesis.contains(MOCK_SLACK_TOKEN));
    assert!(synthesis.contains("[REDACTED:secret]"));
    assert!(
        !dirs.sessions.join("index.jsonl").exists(),
        "engine persistence must remain write-only"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_happy_path_binds_session_contract() {
    let _guard = env_lock().await;
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

    assert_eq!(
        session.rounds.len(),
        2,
        "exact planned round count (no early stop)"
    );
    assert!(session.synthesis.is_some(), "chair synthesis required");
    assert_eq!(session.synthesis_model.as_deref(), Some("mock-chair"));
    assert_eq!(session.origin, SessionOrigin::Api);
    assert_eq!(session.execution_route, ExecutionRoute::Direct);
    assert_eq!(
        session.parent_request_id.as_deref(),
        Some("parent-req-happy")
    );
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
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = mock_config("quick", 2, vec![mock_seat("seat_a", "mock-model")]);
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
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = mock_config("budgeted", 3, vec![mock_seat("seat_a", "mock-model")]);

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
    assert!(
        session.synthesis.is_some(),
        "still synthesizes after early end"
    );
    assert_eq!(
        list_json(&dirs.sessions).len(),
        1,
        "canonical session saved"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn run_with_cancel_terminating_round_skips_gate_redaction() {
    let _guard = env_lock().await;
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
        true, // validate
        "mock",
        true, // validate_gate
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
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    // Force Grok capability true. Without this, enable_specops is false for
    // lack of capability and the API-origin guard is never exercised.
    let prev_xai = std::env::var("XAI_API_KEY").ok();
    unsafe {
        std::env::set_var("XAI_API_KEY", "pr7-fixture-xai-capability-only");
    }

    // Non-converging early exit (polar seats + zero budget) so final_converged
    // is false — SpecOps would fire without the API-origin guard.
    let config = mock_config(
        "specops",
        3,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );

    // API origin + council_auto_escalate=false is the product default from
    // POST /api/deliberate.
    let session = deliberate::run_with_cancel(
        &config,
        "specops",
        "no-spend api specops check",
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
        api_ctx("parent-req-specops"),
        None,
        None,
    )
    .await
    .expect("api run");

    assert_eq!(session.rounds.len(), 1, "budget ends after round one");
    assert!(
        !session.rounds[0].converged,
        "must leave non-converged so SpecOps would otherwise be eligible"
    );
    assert!(
        !session.specops_triggered,
        "API default must suppress SpecOps even when Grok capability is present"
    );
    assert_eq!(session.specops_cost_usd, 0.0);
    assert_eq!(session.origin, SessionOrigin::Api);

    match prev_xai {
        Some(v) => unsafe { std::env::set_var("XAI_API_KEY", v) },
        None => unsafe { std::env::remove_var("XAI_API_KEY") },
    }
    dirs.cleanup();
}

/// B-07: the engine must judge the final round too. The `round_num <
/// cabinet.rounds` gate sent the last round past the judge, so its
/// convergence score and assessment were the skipped placeholder while the
/// stream core judged every round.
#[tokio::test]
async fn engine_judges_final_round() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
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
        true,
        false,
        false,
        None,
        "best",
        false,
        "mock",
        false,
        SessionOrigin::Api,
        api_ctx("parent-req-final-judge"),
        None,
        None,
    )
    .await
    .expect("two-round run");
    dirs.cleanup();

    assert_eq!(session.rounds.len(), 2);
    let first = &session.rounds[0];
    let last = &session.rounds[1];
    // Polar mock seats score 0.5 from the judge; the skipped placeholder is 1.0.
    assert!(
        first.convergence_score < 1.0,
        "round 1 judged (score {})",
        first.convergence_score
    );
    assert_eq!(
        last.convergence_score, first.convergence_score,
        "final round must be judged like every other round, got score {}",
        last.convergence_score
    );
    assert_eq!(last.judge_provider, first.judge_provider);
    assert_eq!(
        last.judge_assessment.is_some(),
        first.judge_assessment.is_some(),
        "final round carries a judge assessment like every other round"
    );
}
