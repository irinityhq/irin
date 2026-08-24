//! Characterization tests for the CLI deliberation path (`council_rs::cli`).
//!
//! PR8 gate: bind flag routing, mode/cabinet resolution, mock full-run output
//! contracts (synthesis, origin, index, flight record), smoke routing, and
//! `--then-tear-down` phase-2 shape — without live providers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use council_rs::cli::{
    self, DeliberationCliArgs, resolve_cabinet_name, resolve_cabinet_override,
    resolve_direct_fire_slug, resolve_mode, should_frame_check, smoke_default_model,
};
use council_rs::config::Config;
use council_rs::mode::Mode;
use council_rs::types::{Cabinet, Chair, RoleCascadeStep, RoleDefinition, RolesConfig, Seat};
use tokio::sync::{Mutex, MutexGuard};

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
            description: "cli characterization cabinet".into(),
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
    runs: PathBuf,
}

impl SessionDirs {
    fn install() -> Self {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let sessions = root.join("sessions");
        let runs = root.join("runs");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&runs).unwrap();
        unsafe {
            std::env::set_var("COUNCIL_SESSIONS_DIR", sessions.to_str().unwrap());
            std::env::set_var("COUNCIL_RUNS_DIR", runs.to_str().unwrap());
            std::env::set_var("COUNCIL_SHELDON_WEB_EVIDENCE", "off");
        }
        Self {
            root,
            sessions,
            runs,
        }
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn base_args(topic: &str, cabinet: &str) -> DeliberationCliArgs {
    DeliberationCliArgs {
        topic: Some(topic.into()),
        context: vec![],
        map: None,
        quiet: true,
        smoke_provider: None,
        smoke_model: None,
        contrarian: false,
        munger: false,
        kiss_review: false,
        specops: false,
        premortem: false,
        wargame: false,
        quick: false,
        heritage: false,
        warroom: false,
        reflection: false,
        duo: false,
        triad: None,
        cabinet: cabinet.into(),
        harden: false,
        pathfind: false,
        then_tear_down: false,
        blind: true,
        no_frame_check: true,
        budget: None,
        tier: "best".into(),
        validate: false,
        validate_provider: "mock".into(),
        validate_gate: false,
    }
}

fn list_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    if !dir.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
    out.sort();
    out
}

// ── Pure plan resolution ─────────────────────────────────────────────

#[test]
fn cabinet_shortcuts_and_precedence() {
    assert_eq!(
        resolve_cabinet_override(true, true, false, false, false, false, None)
            .unwrap()
            .as_deref(),
        Some("wargame"),
        "wargame wins over quick"
    );
    assert_eq!(
        resolve_cabinet_override(false, true, false, false, false, false, None)
            .unwrap()
            .as_deref(),
        Some("quick")
    );
    assert_eq!(
        resolve_cabinet_override(false, false, true, false, false, false, None)
            .unwrap()
            .as_deref(),
        Some("heritage")
    );
    assert_eq!(
        resolve_cabinet_override(false, false, false, true, false, false, None)
            .unwrap()
            .as_deref(),
        Some("warroom")
    );
    assert_eq!(
        resolve_cabinet_override(false, false, false, false, true, false, None)
            .unwrap()
            .as_deref(),
        Some("reflection")
    );
    assert_eq!(
        resolve_cabinet_override(false, false, false, false, false, true, None)
            .unwrap()
            .as_deref(),
        Some("duo")
    );
    assert_eq!(
        resolve_cabinet_override(
            false,
            false,
            false,
            false,
            false,
            false,
            Some("architecture")
        )
        .unwrap()
        .as_deref(),
        Some("triad-architecture")
    );
    assert!(
        resolve_cabinet_override(false, false, false, false, false, false, Some("nope")).is_err()
    );
    assert_eq!(
        resolve_cabinet_override(false, false, false, false, false, false, None).unwrap(),
        None
    );

    assert_eq!(
        resolve_cabinet_name(Some("warroom"), Some("from-file"), "standard"),
        "warroom"
    );
    assert_eq!(
        resolve_cabinet_name(None, Some("from-file"), "standard"),
        "from-file"
    );
    assert_eq!(resolve_cabinet_name(None, None, "standard"), "standard");
}

#[test]
fn mode_precedence_and_harden_conflict() {
    assert_eq!(resolve_mode(false, false, false).unwrap(), Mode::TearDown);
    assert_eq!(resolve_mode(false, true, false).unwrap(), Mode::Pathfind);
    assert_eq!(resolve_mode(false, false, true).unwrap(), Mode::Pathfind);
    assert_eq!(resolve_mode(true, false, false).unwrap(), Mode::Harden);
    assert_eq!(
        resolve_mode(true, true, false).unwrap(),
        Mode::Harden,
        "harden wins over pathfind"
    );
    let err = resolve_mode(true, false, true).unwrap_err().to_string();
    assert!(
        err.contains("--harden cannot be combined with --then-tear-down"),
        "got: {err}"
    );
}

#[test]
fn frame_check_skip_rules() {
    assert!(should_frame_check(false, false, false));
    assert!(!should_frame_check(true, false, false));
    assert!(!should_frame_check(false, true, false));
    assert!(!should_frame_check(false, false, true));
}

#[test]
fn direct_fire_slug_flag_order() {
    assert_eq!(
        resolve_direct_fire_slug(false, false, false, false, false),
        None
    );
    assert_eq!(
        resolve_direct_fire_slug(true, true, false, false, false),
        Some("premortem"),
        "premortem wins over contrarian"
    );
    assert_eq!(
        resolve_direct_fire_slug(false, true, false, false, false),
        Some("contrarian")
    );
    assert_eq!(
        resolve_direct_fire_slug(false, false, true, false, false),
        Some("munger")
    );
    assert_eq!(
        resolve_direct_fire_slug(false, false, false, true, false),
        Some("kiss")
    );
    assert_eq!(
        resolve_direct_fire_slug(false, false, false, false, true),
        Some("specops")
    );
}

#[test]
fn smoke_default_model_known_and_unknown() {
    assert_eq!(smoke_default_model("claude"), Some("claude-opus-4-6"));
    assert_eq!(smoke_default_model("mock"), None);
}

// ── Full CLI path (mock seats) ───────────────────────────────────────

#[tokio::test]
async fn cli_full_deliberation_prints_indexes_and_flight_records() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = Arc::new(mock_config(
        "cli-mock",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let args = base_args("CLI happy path topic", "cli-mock");

    cli::run_deliberation_cli(args, config, false, None)
        .await
        .expect("mock CLI deliberation");

    let sessions = list_with_ext(&dirs.sessions, "json");
    assert_eq!(sessions.len(), 1, "exactly one canonical session");
    let disk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&sessions[0]).unwrap()).unwrap();
    assert_eq!(disk["origin"], "cli");
    assert_eq!(disk["cabinet_name"], "cli-mock");
    assert_eq!(disk["tier"], "best");
    assert_eq!(disk["mode"], "teardown");
    assert!(
        disk["synthesis"].as_str().is_some_and(|s| !s.is_empty()),
        "synthesis required"
    );
    assert_eq!(disk["rounds"].as_array().map(|a| a.len()), Some(1));

    let index = dirs.sessions.join("index.jsonl");
    assert!(index.exists(), "precedent index must be written");
    let index_body = fs::read_to_string(&index).unwrap();
    assert!(
        index_body.contains("CLI happy path topic"),
        "index must include topic"
    );
    let session_id = disk["session_id"].as_str().unwrap();
    let indexed = index_body
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["id"] == session_id)
        .count();
    assert_eq!(indexed, 1, "CLI must index the session exactly once");

    let flights = list_with_ext(&dirs.runs, "md");
    assert_eq!(flights.len(), 1, "exactly one flight record");
    let flight = fs::read_to_string(&flights[0]).unwrap();
    assert!(flight.contains("CLI happy path topic"));
    assert!(flight.contains("cli-mock") || flight.contains("tear_down") || flight.contains("Mode"));

    dirs.cleanup();
}

#[tokio::test]
async fn cli_then_tear_down_runs_two_sessions_and_indexes_both() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = Arc::new(mock_config(
        "cli-mock",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let mut args = base_args("pathfind then tear down", "cli-mock");
    args.then_tear_down = true;

    cli::run_deliberation_cli(args, config, false, None)
        .await
        .expect("then-tear-down");

    let sessions = list_with_ext(&dirs.sessions, "json");
    assert_eq!(sessions.len(), 2, "phase1 + phase2 sessions");

    let mut modes = Vec::new();
    for path in &sessions {
        let disk: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        modes.push(disk["mode"].as_str().unwrap_or("").to_string());
        assert_eq!(disk["origin"], "cli");
    }
    modes.sort();
    assert_eq!(
        modes,
        vec!["pathfind".to_string(), "teardown".to_string()],
        "one pathfind then one teardown"
    );

    let index_body = fs::read_to_string(dirs.sessions.join("index.jsonl")).unwrap();
    let index_lines = index_body.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(index_lines, 2, "both sessions indexed");

    let flights = list_with_ext(&dirs.runs, "md");
    assert_eq!(flights.len(), 2, "flight record per phase");

    dirs.cleanup();
}

#[tokio::test]
async fn cli_smoke_provider_mock_returns_without_session() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = Arc::new(mock_config(
        "cli-mock",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let mut args = base_args("smoke ping", "cli-mock");
    args.smoke_provider = Some("mock".into());
    args.smoke_model = Some("mock-model".into());

    cli::run_deliberation_cli(args, config, false, None)
        .await
        .expect("mock smoke");

    assert!(
        list_with_ext(&dirs.sessions, "json").is_empty(),
        "smoke must not write a session"
    );
    assert!(
        !dirs.sessions.join("index.jsonl").exists(),
        "smoke must not index"
    );
    assert!(
        list_with_ext(&dirs.runs, "md").is_empty(),
        "smoke must not write flight records"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn cli_smoke_empty_provider_bails() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = Arc::new(mock_config(
        "cli-mock",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let mut args = base_args("topic", "cli-mock");
    args.smoke_provider = Some("   ".into());

    let err = cli::run_deliberation_cli(args, config, false, None)
        .await
        .expect_err("empty smoke provider");
    assert!(
        err.to_string()
            .contains("--smoke-provider requires a provider name"),
        "got: {err}"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn cli_harden_then_tear_down_rejected_before_run() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = Arc::new(mock_config(
        "cli-mock",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let mut args = base_args("topic", "cli-mock");
    args.harden = true;
    args.then_tear_down = true;

    let err = cli::run_deliberation_cli(args, config, false, None)
        .await
        .expect_err("harden+then_tear_down");
    assert!(
        err.to_string()
            .contains("--harden cannot be combined with --then-tear-down"),
        "got: {err}"
    );
    assert!(
        list_with_ext(&dirs.sessions, "json").is_empty(),
        "no session on plan rejection"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn cli_cabinet_shortcut_routes_to_named_cabinet() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    // Register as "warroom" so --warroom shortcut hits it.
    let config = Arc::new(mock_config(
        "warroom",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let mut args = base_args("warroom shortcut", "standard");
    args.warroom = true;

    cli::run_deliberation_cli(args, config, false, None)
        .await
        .expect("warroom shortcut");

    let sessions = list_with_ext(&dirs.sessions, "json");
    assert_eq!(sessions.len(), 1);
    let disk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&sessions[0]).unwrap()).unwrap();
    assert_eq!(disk["cabinet_name"], "warroom");

    dirs.cleanup();
}

#[tokio::test]
async fn cli_loaded_external_cabinet_key_used_when_no_shortcut() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    let config = Arc::new(mock_config(
        "from-yaml-stem",
        1,
        vec![mock_seat("seat_a", "mock-model")],
    ));
    let args = base_args("external key", "standard");

    cli::run_deliberation_cli(args, config, false, Some("from-yaml-stem".into()))
        .await
        .expect("external cabinet key");

    let disk: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&list_with_ext(&dirs.sessions, "json")[0]).unwrap(),
    )
    .unwrap();
    assert_eq!(disk["cabinet_name"], "from-yaml-stem");

    dirs.cleanup();
}
