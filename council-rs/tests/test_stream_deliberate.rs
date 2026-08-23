//! Characterization tests for `stream::deliberate::run` (PR6).
//!
//! Binds multi-round public event ordering, cancellation, and budget stop at
//! the Rust stream entry — not a browser WebSocket fake. Mock seats/roles only.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use council_rs::config::Config;
use council_rs::mode::Mode;
use council_rs::stream::deliberate::{self, StreamConfig};
use council_rs::stream::events::StreamEvent;
use council_rs::stream::intervention::{Intervention, InterventionQueue};
use council_rs::types::{Cabinet, Chair, RoleCascadeStep, RoleDefinition, RolesConfig, Seat};
use tokio::sync::{Mutex, MutexGuard, mpsc};
use tokio_util::sync::CancellationToken;

static MOCK_GATEWAY_ADDR: OnceLock<std::net::SocketAddr> = OnceLock::new();

async fn mock_gateway_models() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "object": "list",
        "data": [
            {"id": "mock-seat-agree", "ready": true, "transports": ["mock", "grok"]},
            {"id": "mock-seat-disagree", "ready": true, "transports": ["mock", "grok"]},
            {"id": "mock-gated-seat", "ready": true, "transports": ["mock", "grok"]},
            {"id": "mock-chair", "ready": true, "transports": ["mock", "grok"]},
            {"id": "mock-role", "ready": true, "transports": ["mock"]},
            {"id": "mock-claim-validator", "ready": true, "transports": ["mock"]},
            {"id": "grok-4.3", "ready": true, "transports": ["grok_hermes"]}
        ]
    }))
}

async fn mock_gateway_chat(
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let model = body["model"].as_str().unwrap_or("mock-model");
    let content = match model {
        "mock-seat-agree" => "I agree and support this approach.",
        "mock-seat-disagree" => "I disagree and reject this approach.",
        "mock-gated-seat" => {
            "Seat analysis states UNIQUE_CONTRADICTED_CLAIM_XYZ_12345 with certainty."
        }
        "mock-claim-validator" => {
            r#"[{"claim":"UNIQUE_CONTRADICTED_CLAIM_XYZ_12345","seat":"seat_a","verdict":"CONTRADICTED","evidence_citations":["fixture evidence"],"reasoning":"priced fixture","confidence":0.95,"impact":"HIGH"}]"#
        }
        "grok-4.3" => "",
        _ => "Mock response",
    };
    axum::Json(serde_json::json!({
        "id": "chatcmpl-stream-spend-test",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
    }))
}

fn install_mock_gateway() {
    let addr = MOCK_GATEWAY_ADDR.get_or_init(|| {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mock Gateway runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind mock Gateway");
                addr_tx
                    .send(listener.local_addr().expect("mock Gateway address"))
                    .expect("publish mock Gateway address");
                let app = axum::Router::new()
                    .route("/health", axum::routing::get(|| async { "ok" }))
                    .route("/v1/models", axum::routing::get(mock_gateway_models))
                    .route(
                        "/v1/chat/completions",
                        axum::routing::post(mock_gateway_chat),
                    );
                axum::serve(listener, app)
                    .await
                    .expect("serve mock Gateway");
            });
        });
        addr_rx.recv().expect("receive mock Gateway address")
    });
    unsafe {
        std::env::set_var("GATEWAY_URL", format!("http://{addr}"));
        std::env::set_var("GW_API_KEY", "stream-spend-test-key");
    }
}

/// Serialize tests that mutate process env (sessions dir, evidence switches).
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

fn mock_cabinet(name: &str, rounds: u32, seats: Vec<Seat>) -> Cabinet {
    Cabinet {
        hash: String::new(),
        name: name.into(),
        description: "stream characterization cabinet".into(),
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
    }
}

fn priced_mock_models() -> council_rs::types::ModelRegistry {
    // High per-token prices so 10in+10out mock responses exceed a small budget
    // after round 1 (cost = 20 * 1e6 / 1e6 = $20 per seat call).
    let price = council_rs::types::ModelPricing {
        input: 1_000_000.0,
        output: 1_000_000.0,
        cached_input: 0.0,
    };
    let mut models = HashMap::new();
    for id in [
        "mock-seat-agree",
        "mock-seat-disagree",
        "mock-gated-seat",
        "mock-chair",
        "mock-role",
        "mock-claim-validator",
        "mock-model",
        "grok-4.3",
    ] {
        models.insert(
            id.into(),
            council_rs::types::ModelEntry {
                id: id.into(),
                provider: "mock".into(),
                description: "characterization fixture".into(),
                pricing: price.clone(),
            },
        );
    }
    council_rs::types::ModelRegistry { models }
}

fn mock_config() -> Config {
    Config {
        cabinets: HashMap::new(),
        models: priced_mock_models(),
        roles: mock_roles(),
        tera: tera::Tera::default(),
        base_dir: PathBuf::from("."),
    }
}

struct SessionDirs {
    root: PathBuf,
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
        Self { root }
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn saved_session(dirs: &SessionDirs) -> serde_json::Value {
    let path = fs::read_dir(dirs.root.join("sessions"))
        .expect("read sessions dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("council_") && name.ends_with(".json"))
        })
        .expect("saved session");
    serde_json::from_slice(&fs::read(path).expect("read saved session"))
        .expect("parse saved session")
}

/// Base StreamConfig for no-spend mock multi-round characterization.
fn base_stream(cabinet: Cabinet) -> StreamConfig {
    StreamConfig {
        topic: "stream characterization topic".into(),
        cabinet_name: cabinet.name.clone(),
        custom_cabinet: Some(cabinet),
        context: "Context".into(),
        mode: Mode::TearDown,
        blind: true,
        frame_check: false,
        // Disable auto SpecOps: final_conv is never < -1.0.
        auto_specops_threshold: -1.0,
        pause_after_each_round: false,
        validate: false,
        validate_gate: false,
        via_gateway: Some(false),
        budget_max_usd: None,
        tier: "best".into(),
        ..StreamConfig::default()
    }
}

async fn run_stream(
    stream_config: StreamConfig,
    interventions: InterventionQueue,
    cancel: CancellationToken,
) -> Vec<StreamEvent> {
    let config = Arc::new(mock_config());
    let (event_tx, event_rx) = mpsc::channel(256);
    deliberate::run(config, stream_config, event_tx, interventions, cancel).await;
    let mut events = Vec::new();
    let mut event_rx = event_rx;
    while let Some(ev) = event_rx.recv().await {
        events.push(ev);
    }
    events
}

fn event_types(events: &[StreamEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event_type.as_str()).collect()
}

fn first_index(events: &[StreamEvent], ty: &str) -> Option<usize> {
    events.iter().position(|e| e.event_type == ty)
}

fn count_type(events: &[StreamEvent], ty: &str) -> usize {
    events.iter().filter(|e| e.event_type == ty).count()
}

fn round_num_of(ev: &StreamEvent) -> Option<u32> {
    ev.data
        .get("round_num")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
}

fn indices_for_round(events: &[StreamEvent], ty: &str, round: u32) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.event_type == ty && round_num_of(e) == Some(round))
        .map(|(i, _)| i)
        .collect()
}

/// Public multi-round spine must appear in order **per round_num**.
///
/// For each planned round: round_started → all seat_started → all seat_complete
/// → convergence_scored → round_complete, then the next round (if any), then
/// synthesis → session_saved → done. Optional events (info, divergence, chunks)
/// may interleave but must not reorder the spine.
fn assert_ordered_public_spine(
    events: &[StreamEvent],
    planned_rounds: u32,
    seats_per_round: usize,
) {
    let types = event_types(events);
    let has_fatal = events
        .iter()
        .any(|e| e.event_type == "error" && e.data.get("fatal") == Some(&serde_json::json!(true)));
    assert!(
        !has_fatal,
        "fatal stream error during characterization; events={types:?}"
    );

    let session_started = first_index(events, "session_started").expect("session_started");
    let mut prev_boundary = session_started;

    for round in 1..=planned_rounds {
        let starts = indices_for_round(events, "round_started", round);
        assert_eq!(
            starts.len(),
            1,
            "exactly one round_started for round {round}; events={types:?}"
        );
        let r_start = starts[0];
        assert!(
            r_start > prev_boundary,
            "round {round} started after previous boundary; events={types:?}"
        );

        let seat_starts = indices_for_round(events, "seat_started", round);
        let seat_completes = indices_for_round(events, "seat_complete", round);
        assert_eq!(
            seat_starts.len(),
            seats_per_round,
            "round {round}: expected {seats_per_round} seat_started; events={types:?}"
        );
        assert_eq!(
            seat_completes.len(),
            seats_per_round,
            "round {round}: expected {seats_per_round} seat_complete; events={types:?}"
        );

        for &si in &seat_starts {
            assert!(
                si > r_start,
                "round {round}: seat_started after round_started; events={types:?}"
            );
        }
        // Every seat_complete for this round must follow that round's start and
        // precede score/complete — catches round-2 starting mid seat-complete of r1.
        let last_seat_complete = *seat_completes.iter().max().unwrap();
        let first_seat_complete = *seat_completes.iter().min().unwrap();
        assert!(
            first_seat_complete > r_start,
            "round {round}: seat_complete after round_started; events={types:?}"
        );
        for &sc in &seat_completes {
            assert!(
                sc > r_start,
                "round {round}: seat_complete after round_started; events={types:?}"
            );
            // All seat_started for the round precede all seat_complete? Not strictly
            // required under parallel fan-out — seat_complete may interleave seat_started
            // of other seats. Require: every seat_started for the round is before
            // convergence_scored, and every seat_complete before score.
        }

        let scored = indices_for_round(events, "convergence_scored", round);
        assert_eq!(
            scored.len(),
            1,
            "exactly one convergence_scored for round {round}; events={types:?}"
        );
        let score_i = scored[0];
        assert!(
            last_seat_complete < score_i,
            "round {round}: all seat_complete before convergence_scored; last_complete={last_seat_complete} score={score_i}; events={types:?}"
        );
        for &si in &seat_starts {
            assert!(
                si < score_i,
                "round {round}: all seat_started before convergence_scored; events={types:?}"
            );
        }

        let completes = indices_for_round(events, "round_complete", round);
        assert_eq!(
            completes.len(),
            1,
            "exactly one round_complete for round {round}; events={types:?}"
        );
        let r_done = completes[0];
        assert!(
            score_i < r_done,
            "round {round}: score before round_complete; events={types:?}"
        );

        // Next round must not start until this round completes.
        if round < planned_rounds {
            let next = indices_for_round(events, "round_started", round + 1);
            assert_eq!(
                next.len(),
                1,
                "round {} started; events={types:?}",
                round + 1
            );
            assert!(
                r_done < next[0],
                "round {round} complete before round {} start; events={types:?}",
                round + 1
            );
        }

        prev_boundary = r_done;
    }

    let synth_started = first_index(events, "synthesis_started").expect("synthesis_started");
    let synth_complete = first_index(events, "synthesis_complete").expect("synthesis_complete");
    let session_saved = first_index(events, "session_saved").expect("session_saved");
    let done = first_index(events, "done").expect("done");

    assert!(
        prev_boundary < synth_started,
        "rounds before synthesis: {types:?}"
    );
    assert!(synth_started < synth_complete, "synthesis order: {types:?}");
    assert!(
        synth_complete < session_saved,
        "synthesis before save: {types:?}"
    );
    assert!(session_saved < done, "save before done: {types:?}");
    assert_eq!(
        events.last().map(|e| e.event_type.as_str()),
        Some("done"),
        "terminal event must be done; last={:?}",
        events.last().map(|e| &e.event_type)
    );
}

#[tokio::test]
async fn stream_multi_round_emits_ordered_public_events_and_done() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    // Polar seats so keyword judge does not early-converge (score ~0.5).
    let cabinet = mock_cabinet(
        "stream-multi",
        2,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    let stream = base_stream(cabinet);

    let events = tokio::time::timeout(
        Duration::from_secs(60),
        run_stream(stream, InterventionQueue::new(), CancellationToken::new()),
    )
    .await
    .expect("stream multi-round timed out");

    assert_ordered_public_spine(&events, 2, 2);

    let round_starts: Vec<u32> = events
        .iter()
        .filter(|e| e.event_type == "round_started")
        .map(|e| e.data["round_num"].as_u64().unwrap() as u32)
        .collect();
    assert_eq!(round_starts, vec![1, 2], "exactly two planned rounds");

    let round_completes = count_type(&events, "round_complete");
    assert_eq!(round_completes, 2);

    // Two seats × two rounds (also enforced inside assert_ordered_public_spine).
    assert_eq!(count_type(&events, "seat_started"), 4);
    assert_eq!(count_type(&events, "seat_complete"), 4);

    let done = events.iter().find(|e| e.event_type == "done").unwrap();
    assert_eq!(done.data["rounds_run"].as_u64(), Some(2));
    assert!(
        done.data["synthesis"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "done must carry synthesis text"
    );
    assert!(
        !events.iter().any(|e| e.event_type == "budget_paused"),
        "happy path must not budget-pause"
    );
    assert!(
        !events.iter().any(|e| e.event_type == "awaiting_input"),
        "pause_after_each_round=false must skip operator pause"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn stream_pre_cancelled_emits_nothing() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    let cabinet = mock_cabinet(
        "stream-cancel",
        2,
        vec![mock_seat("seat_a", "mock-seat-agree")],
    );
    let cancel = CancellationToken::new();
    cancel.cancel();

    let events = tokio::time::timeout(
        Duration::from_secs(15),
        run_stream(base_stream(cabinet), InterventionQueue::new(), cancel),
    )
    .await
    .expect("pre-cancel timed out");

    assert!(
        events.is_empty(),
        "pre-cancelled stream must emit nothing; got {:?}",
        event_types(&events)
    );

    dirs.cleanup();
}

#[tokio::test]
async fn stream_cancel_mid_run_skips_terminal_done() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    // Barrier: pause after round 1. Fast mock seats finish round 1, then the
    // stream blocks on interventions.wait — cancel there is deterministic and
    // cannot race past synthesis/done the way cancel-after-session_started can.
    let cabinet = mock_cabinet(
        "stream-mid-cancel",
        3,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    let mut stream = base_stream(cabinet);
    stream.pause_after_each_round = true;

    let config = Arc::new(mock_config());
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let interventions = InterventionQueue::new();
    // Hold a sender so the queue stays open while paused (no auto-Continue).
    let _hold_tx = interventions.sender();
    let cancel = CancellationToken::new();
    let cancel_run = cancel.clone();

    let handle = tokio::spawn(async move {
        deliberate::run(config, stream, event_tx, interventions, cancel_run).await;
    });

    let mut events = Vec::new();
    let mut saw_awaiting = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            cancel.cancel();
            panic!(
                "timed out waiting for awaiting_input barrier; events={:?}",
                event_types(&events)
            );
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(ev)) => {
                let is_await = ev.event_type == "awaiting_input";
                events.push(ev);
                if is_await {
                    saw_awaiting = true;
                    // Deterministic mid-run point: round 1 complete, not synthesizing.
                    cancel.cancel();
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                cancel.cancel();
                panic!(
                    "timed out waiting for awaiting_input barrier; events={:?}",
                    event_types(&events)
                );
            }
        }
    }
    assert!(
        saw_awaiting,
        "must reach operator pause before cancel; events={:?}",
        event_types(&events)
    );
    // Drain any residual events after cancel.
    while let Some(ev) = event_rx.recv().await {
        events.push(ev);
    }
    handle.await.expect("run task");

    let types = event_types(&events);
    assert!(
        events.iter().any(|e| e.event_type == "round_complete"),
        "barrier is after round 1 complete; events={types:?}"
    );
    assert!(
        !events.iter().any(|e| e.event_type == "done"),
        "mid-run cancel must not emit terminal done; events={types:?}"
    );
    assert!(
        !events.iter().any(|e| e.event_type == "session_saved"),
        "mid-run cancel must not save a complete session; events={types:?}"
    );
    assert!(
        !events.iter().any(|e| e.event_type == "synthesis_started"),
        "mid-run cancel at pause must not reach synthesis; events={types:?}"
    );

    dirs.cleanup();
}

#[tokio::test]
async fn stream_budget_cap_ends_after_round_one_with_budget_paused() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    let cabinet = mock_cabinet(
        "stream-budget",
        3,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    let mut stream = base_stream(cabinet);
    // Cap below one priced mock seat call ($20) so round 1 spend trips the pause
    // while initial cumulative_spend (0) still lets the phase enter.
    stream.budget_max_usd = Some(1.0);

    let events = tokio::time::timeout(
        Duration::from_secs(60),
        run_stream(stream, InterventionQueue::new(), CancellationToken::new()),
    )
    .await
    .expect("budget stream timed out");

    let types = event_types(&events);
    assert!(
        events.iter().any(|e| e.event_type == "budget_paused"),
        "budget cap must emit budget_paused; events={types:?}"
    );
    assert_eq!(
        count_type(&events, "round_started"),
        1,
        "budget stop after round one; events={types:?}"
    );
    assert_eq!(count_type(&events, "round_complete"), 1);

    let budget = events
        .iter()
        .find(|e| e.event_type == "budget_paused")
        .unwrap();
    assert_eq!(budget.data["round_num"].as_u64(), Some(1));
    assert_eq!(budget.data["action"].as_str(), Some("end_early"));
    assert!((budget.data["max_usd"].as_f64().unwrap_or(-1.0) - 1.0).abs() < f64::EPSILON);
    assert!(
        budget.data["total_cost_usd"].as_f64().unwrap_or(0.0) >= 1.0,
        "spend at pause must be at/above max; data={:?}",
        budget.data
    );

    // Stream still synthesizes and terminates with done (matches CLI path).
    assert!(
        events.iter().any(|e| e.event_type == "synthesis_complete"),
        "budget end still synthesizes; events={types:?}"
    );
    let done = events.iter().find(|e| e.event_type == "done");
    assert!(
        done.is_some(),
        "budget end still emits done; events={types:?}"
    );
    assert_eq!(done.unwrap().data["rounds_run"].as_u64(), Some(1));

    // budget_paused must precede synthesis / done.
    let bp = first_index(&events, "budget_paused").unwrap();
    let synth = first_index(&events, "synthesis_started").unwrap();
    let done_i = first_index(&events, "done").unwrap();
    assert!(bp < synth, "budget_paused before synthesis: {types:?}");
    assert!(synth < done_i, "synthesis before done: {types:?}");

    dirs.cleanup();
}

#[tokio::test]
async fn stream_round_two_budget_stop_keeps_full_evidence_for_chair() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    let cabinet = mock_cabinet(
        "stream-round-two-budget",
        3,
        vec![
            mock_seat("seat_a", "mock-gated-seat"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    let mut stream = base_stream(cabinet);
    stream.validate = true;
    stream.validate_gate = true;
    // Round 2 crosses this cap only when prior-round seat cost is included.
    stream.budget_max_usd = Some(115.0);

    let events = tokio::time::timeout(
        Duration::from_secs(60),
        run_stream(stream, InterventionQueue::new(), CancellationToken::new()),
    )
    .await
    .expect("round-two budget stream timed out");

    assert_eq!(count_type(&events, "round_started"), 2);
    let budget = events
        .iter()
        .find(|event| event.event_type == "budget_paused")
        .expect("round 2 must pause for budget");
    assert_eq!(budget.data["round_num"].as_u64(), Some(2));
    assert_eq!(budget.data["total_cost_usd"].as_f64(), Some(120.0));

    let session = saved_session(&dirs);
    let round_two = session["rounds"]
        .as_array()
        .and_then(|rounds| rounds.get(1))
        .expect("saved round 2");
    assert_eq!(
        round_two["converged"].as_bool(),
        Some(false),
        "round 2 must terminate only for budget; round={round_two:?}"
    );
    assert!(
        round_two["validation_report"].is_array(),
        "round 2 must carry the validator report; round={round_two:?}"
    );
    let seat_a = round_two["responses"]
        .as_array()
        .and_then(|responses| {
            responses
                .iter()
                .find(|response| response["seat_name"] == "seat_a")
        })
        .expect("round 2 seat_a response");
    let text = seat_a["text"].as_str().expect("round 2 seat text");
    assert_eq!(
        text, "Seat analysis states UNIQUE_CONTRADICTED_CLAIM_XYZ_12345 with certainty.",
        "budget-terminating round must reach the Chair un-gated"
    );

    dirs.cleanup();
}

/// Pause after round 1 → operator Continue → round 2 → done.
#[tokio::test]
async fn stream_pause_resume_continue_runs_remaining_rounds() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    let cabinet = mock_cabinet(
        "stream-pause-resume",
        2,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    let mut stream = base_stream(cabinet);
    stream.pause_after_each_round = true;

    let config = Arc::new(mock_config());
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let interventions = InterventionQueue::new();
    let tx = interventions.sender();
    let cancel = CancellationToken::new();
    let cancel_run = cancel.clone();

    let handle = tokio::spawn(async move {
        deliberate::run(config, stream, event_tx, interventions, cancel_run).await;
    });

    let mut events = Vec::new();
    let mut continued = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            cancel.cancel();
            panic!("pause/resume timed out; events={:?}", event_types(&events));
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(ev)) => {
                if ev.event_type == "awaiting_input" && !continued {
                    // Resume after first pause.
                    tx.send(Intervention::Continue).await.unwrap();
                    continued = true;
                }
                events.push(ev);
            }
            Ok(None) => break,
            Err(_) => {
                cancel.cancel();
                panic!(
                    "pause/resume timed out waiting for events; events={:?}",
                    event_types(&events)
                );
            }
        }
    }
    handle.await.expect("run task");

    let types = event_types(&events);
    assert!(
        continued,
        "must have paused for operator input; events={types:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "intervention_received"),
        "must emit intervention_received; events={types:?}"
    );
    assert_eq!(
        count_type(&events, "round_started"),
        2,
        "Continue must allow round 2; events={types:?}"
    );
    assert!(
        events.iter().any(|e| e.event_type == "done"),
        "must reach terminal done; events={types:?}"
    );
    // Ordered: awaiting_input before intervention_received before second round.
    let await_i = first_index(&events, "awaiting_input").unwrap();
    let recv_i = first_index(&events, "intervention_received").unwrap();
    let r2 = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.event_type == "round_started")
        .nth(1)
        .map(|(i, _)| i)
        .expect("second round_started");
    assert!(await_i < recv_i, "pause before receive: {types:?}");
    assert!(recv_i < r2, "receive before round 2: {types:?}");

    dirs.cleanup();
}

#[tokio::test]
async fn stream_manual_specops_cost_counts_toward_spend_and_saved_session() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    install_mock_gateway();

    let mut cabinet = mock_cabinet(
        "stream-manual-specops-cost",
        2,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    for seat in &mut cabinet.seats {
        seat.provider = "grok".into();
    }
    cabinet.chair.provider = "grok".into();
    let mut stream = base_stream(cabinet);
    stream.pause_after_each_round = true;
    stream.via_gateway = Some(true);

    let config = Arc::new(mock_config());
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let interventions = InterventionQueue::new();
    let tx = interventions.sender();
    let cancel = CancellationToken::new();
    let cancel_run = cancel.clone();
    let handle = tokio::spawn(async move {
        deliberate::run(config, stream, event_tx, interventions, cancel_run).await;
    });

    let mut events = Vec::new();
    let mut pauses = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            cancel.cancel();
            panic!(
                "manual SpecOps timed out; events={:?}",
                event_types(&events)
            );
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(event)) => {
                if event.event_type == "awaiting_input" {
                    let action = if pauses == 0 {
                        Intervention::EscalateSpecops
                    } else {
                        Intervention::EndEarly
                    };
                    tx.send(action).await.expect("send intervention");
                    pauses += 1;
                }
                events.push(event);
            }
            Ok(None) => break,
            Err(_) => {
                cancel.cancel();
                panic!(
                    "manual SpecOps timed out waiting for events; events={:?}",
                    event_types(&events)
                );
            }
        }
    }
    handle.await.expect("run task");

    assert_eq!(
        pauses,
        2,
        "manual escalation must re-pause; events={:?}",
        events
            .iter()
            .map(|event| (&event.event_type, &event.data))
            .collect::<Vec<_>>()
    );
    let signal = events
        .iter()
        .find(|event| event.event_type == "specops_signal")
        .expect("manual SpecOps signal");
    let signal_cost = signal.data["cost_usd"]
        .as_f64()
        .expect("manual SpecOps cost");
    assert!(signal_cost > 0.0, "signal={:?}", signal.data);

    let done_spend = events
        .iter()
        .find(|event| event.event_type == "done")
        .and_then(|event| event.data["cumulative_spend_usd"].as_f64())
        .expect("done cumulative spend");
    let session = saved_session(&dirs);
    let saved_specops_cost = session["specops_cost_usd"]
        .as_f64()
        .expect("saved SpecOps cost");
    assert_eq!(saved_specops_cost, signal_cost);
    assert_eq!(done_spend, session["total_cost_usd"].as_f64().unwrap());

    dirs.cleanup();
}

#[tokio::test]
async fn stream_auto_specops_cost_counts_once() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();
    install_mock_gateway();

    let mut cabinet = mock_cabinet(
        "stream-auto-specops-cost",
        1,
        vec![
            mock_seat("seat_a", "mock-seat-agree"),
            mock_seat("seat_b", "mock-seat-disagree"),
        ],
    );
    for seat in &mut cabinet.seats {
        seat.provider = "grok".into();
    }
    cabinet.chair.provider = "grok".into();
    let mut stream = base_stream(cabinet);
    stream.via_gateway = Some(true);
    stream.auto_specops_threshold = 1.0;

    let events = tokio::time::timeout(
        Duration::from_secs(60),
        run_stream(stream, InterventionQueue::new(), CancellationToken::new()),
    )
    .await
    .expect("auto SpecOps stream timed out");

    let signal_cost = events
        .iter()
        .find(|event| event.event_type == "specops_signal")
        .and_then(|event| event.data["cost_usd"].as_f64())
        .expect("priced auto SpecOps signal");
    assert_eq!(signal_cost, 20.0);

    let done_spend = events
        .iter()
        .find(|event| event.event_type == "done")
        .and_then(|event| event.data["cumulative_spend_usd"].as_f64())
        .expect("done cumulative spend");
    let session = saved_session(&dirs);
    assert_eq!(session["specops_cost_usd"].as_f64(), Some(20.0));
    assert_eq!(session["specops_triggered"].as_bool(), Some(true));
    assert_eq!(session["total_cost_usd"].as_f64(), Some(100.0));
    assert_eq!(done_spend, 100.0, "auto SpecOps cost must count once");

    dirs.cleanup();
}

/// Zero budget matches engine run_with_cancel: one round, then budget_paused.
#[tokio::test]
async fn stream_zero_budget_ends_after_round_one_with_budget_paused() {
    let _guard = env_lock().await;
    let dirs = SessionDirs::install();

    let cabinet = mock_cabinet(
        "stream-zero-budget",
        3,
        vec![mock_seat("seat_a", "mock-seat-agree")],
    );
    let mut stream = base_stream(cabinet);
    stream.budget_max_usd = Some(0.0);

    let events = tokio::time::timeout(
        Duration::from_secs(30),
        run_stream(stream, InterventionQueue::new(), CancellationToken::new()),
    )
    .await
    .expect("zero-budget stream timed out");

    let types = event_types(&events);
    assert!(
        events.iter().any(|e| e.event_type == "budget_paused"),
        "zero budget must emit budget_paused; events={types:?}"
    );
    assert_eq!(
        count_type(&events, "round_started"),
        1,
        "zero budget runs exactly one round; events={types:?}"
    );
    assert_eq!(count_type(&events, "round_complete"), 1);

    let budget = events
        .iter()
        .find(|e| e.event_type == "budget_paused")
        .unwrap();
    assert_eq!(budget.data["round_num"].as_u64(), Some(1));
    assert_eq!(budget.data["action"].as_str(), Some("end_early"));
    assert!((budget.data["max_usd"].as_f64().unwrap_or(-1.0) - 0.0).abs() < f64::EPSILON);

    assert!(
        events.iter().any(|e| e.event_type == "synthesis_complete"),
        "zero-budget end still synthesizes; events={types:?}"
    );
    let done = events.iter().find(|e| e.event_type == "done");
    assert!(
        done.is_some(),
        "zero-budget end still emits done; events={types:?}"
    );
    assert_eq!(done.unwrap().data["rounds_run"].as_u64(), Some(1));

    let bp = first_index(&events, "budget_paused").unwrap();
    let synth = first_index(&events, "synthesis_started").unwrap();
    let done_i = first_index(&events, "done").unwrap();
    assert!(bp < synth, "budget_paused before synthesis: {types:?}");
    assert!(synth < done_i, "synthesis before done: {types:?}");

    dirs.cleanup();
}
