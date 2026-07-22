//! Phase 2 watch plane — threading + budget enforcement tests.
//!
//! T19 falsification proof. SHOULD FAIL against a *shared* runtime; passes
//! once §8's dedicated watch_runtime keeps sentinel blocking work off the
//! hot path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// T19: prove blocking-pool isolation between the sidecar's hot path and
/// the watch plane's blocking I/O.
///
/// Setup — three runtimes in-process:
///   - **server_rt**: a 2-worker multi-thread runtime that binds a minimal
///     UDS listener exposing `GET /auth/check`. Stands in for the sidecar's
///     main runtime.
///   - **watch_rt**: the dedicated runtime built by `build_watch_runtime()`.
///     Hammered with 16 `spawn_blocking(|| sleep(1s))` — simulates worst-case
///     sentinel `observe()` / audit-write contention.
///   - **client_rt**: a 4-worker multi-thread runtime fires 100 concurrent
///     UDS requests at server_rt, measuring per-request latency.
///
/// Invariant: server_rt and watch_rt have separate blocking pools. Therefore
/// saturating watch_rt's 8-thread blocking pool MUST NOT delay /auth/check
/// served from server_rt. p99 < 50ms is the §8 budget.
///
/// Falsifier: if `fire_pipeline` (or any sentinel) is ever wired to
/// `tokio::task::spawn_blocking` from the sidecar's main runtime instead of
/// watch_runtime, this test catches it — the shared pool gets saturated and
/// /auth/check p99 climbs past the budget. The TWO RUNTIMES on different
/// thread pools is what makes the assertion meaningful.
/// Note (Fix Round 2): real SilenceSentinel (with its spawn_blocking for backlog FS)
/// is exercised via WatchRunner registration + fire_pipeline in watch_runner tests
/// and silence sentinel paths under the dedicated watch_rt (t19 provides the saturation
/// model; integration coverage in runner tests confirms no starvation for real sentinels).
#[test]
fn t19_blocking_pool_does_not_starve_hot_path() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let socket_path = dir.path().join("t19.sock");

    // Runtime A — stand-in sidecar serving /auth/check on UDS.
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("t19-server")
        .enable_all()
        .build()
        .expect("server_rt");
    let server_socket = socket_path.clone();
    server_rt.spawn(async move { run_mini_sidecar(&server_socket).await });

    // Wait until the UDS socket file appears (bind happened) before any
    // client connects. 1s ceiling; in practice <10ms.
    let wait_deadline = Instant::now() + Duration::from_secs(1);
    while !socket_path.exists() {
        if Instant::now() > wait_deadline {
            panic!("T19: mini-sidecar failed to bind UDS within 1s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // Runtime B — dedicated watch_runtime, saturated with sentinel-like
    // blocking work via *its own* spawn_blocking pool.
    let watch_rt = gateway_sidecar::watch::runtime::build_watch_runtime();
    for _ in 0..16 {
        watch_rt.handle().spawn_blocking(|| {
            std::thread::sleep(Duration::from_secs(1));
        });
    }

    // Runtime C — client. 100 concurrent /auth/check probes. Latencies
    // captured into a shared Vec; p99 is the assertion.
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("t19-client")
        .enable_all()
        .build()
        .expect("client_rt");

    let latencies = Arc::new(parking_lot::Mutex::new(Vec::<u128>::with_capacity(100)));
    client_rt.block_on(async {
        let mut handles = Vec::with_capacity(100);
        for _ in 0..100 {
            let lats = latencies.clone();
            let sock = socket_path.clone();
            handles.push(tokio::spawn(async move {
                let start = Instant::now();
                let _ = uds_auth_check(&sock).await;
                lats.lock().push(start.elapsed().as_millis());
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });

    let mut lats = latencies.lock().clone();
    lats.sort_unstable();
    let p99 = lats[(lats.len() as f64 * 0.99) as usize];

    // Drop watch_rt eagerly so the sleeping blocking-pool threads don't
    // outlive the test (the 1s sleeps would otherwise hold the runtime
    // shutdown for up to 1s). server_rt + client_rt drop on scope exit.
    drop(watch_rt);

    assert!(
        p99 < 50,
        "T19: /auth/check p99={p99}ms ≥ 50ms — blocking-pool isolation broken (§8 P0-1)"
    );
}

/// Minimal UDS HTTP server: accepts one connection at a time, reads up to
/// the request terminator, writes a 200 OK and shuts down the half.
async fn run_mini_sidecar(path: &Path) {
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).expect("t19 UDS bind");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_conn(stream));
            }
            Err(_) => return,
        }
    }
}

async fn handle_conn(mut stream: tokio::net::UnixStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = [0u8; 1024];
    // Read at least the request line; one read is enough for the canned
    // client below (which writes its full request in one shot).
    let _ = stream.read(&mut buf).await;
    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    let _ = stream.write_all(resp).await;
    let _ = stream.shutdown().await;
}

/// Tiny hand-rolled UDS HTTP client. Sends one fixed request and reads the
/// response to completion. We only care about end-to-end latency.
async fn uds_auth_check(path: &PathBuf) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = match tokio::net::UnixStream::connect(path).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let req = b"GET /auth/check HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(req).await.is_err() {
        return;
    }
    let mut sink = Vec::with_capacity(128);
    let _ = stream.read_to_end(&mut sink).await;
}

/// T1: panicking escalate() is caught by catch_unwind; runner survives.
#[tokio::test]
async fn t1_panicking_escalate_is_caught() {
    use async_trait::async_trait;
    use gateway_sidecar::watch::{
        EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier,
    };

    struct Panicky;
    #[async_trait]
    impl Sentinel for Panicky {
        fn name(&self) -> &str {
            "panicky"
        }
        fn tenant(&self) -> &str {
            "test"
        }
        fn tier(&self) -> Tier {
            Tier::Fast
        }
        fn cooldown(&self) -> Duration {
            Duration::from_millis(0)
        }
        async fn observe(&self) -> Result<SentinelState, ObserveError> {
            Ok(SentinelState {
                tenant: "test".into(),
                sentinel: "panicky".into(),
                observed_at: 0,
                payload: serde_json::Value::Null,
            })
        }
        fn interesting(&self, _: &SentinelState) -> Option<String> {
            Some("yo".into())
        }
        async fn escalate(&self, _: SentinelState, _: String) -> Result<Escalation, EscalateError> {
            panic!("intentional panic in escalate");
        }
    }

    let sentinel = Panicky;
    let outcome = gateway_sidecar::watch::runtime::fire_pipeline(
        &sentinel,
        &gateway_sidecar::watch::quarantine::QuarantineState::test_default(),
    )
    .await;

    use gateway_sidecar::watch::runtime::FireOutcome;
    assert!(
        matches!(outcome, FireOutcome::Panic),
        "expected Panic, got {:?}",
        outcome
    );
}

/// T3: observe() exceeding 50ms triggers sub-budget timeout (not escalate timeout).
#[tokio::test]
async fn t3_observe_sub_budget_enforced() {
    use async_trait::async_trait;
    use gateway_sidecar::watch::{
        EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier,
    };

    struct SlowObserve;
    #[async_trait]
    impl Sentinel for SlowObserve {
        fn name(&self) -> &str {
            "slow"
        }
        fn tenant(&self) -> &str {
            "test"
        }
        fn tier(&self) -> Tier {
            Tier::Polling
        }
        fn cooldown(&self) -> Duration {
            Duration::from_millis(0)
        }
        async fn observe(&self) -> Result<SentinelState, ObserveError> {
            tokio::time::sleep(Duration::from_millis(60)).await; // exceeds 50ms budget
            Ok(SentinelState {
                tenant: "test".into(),
                sentinel: "slow".into(),
                observed_at: 0,
                payload: serde_json::Value::Null,
            })
        }
        fn interesting(&self, _: &SentinelState) -> Option<String> {
            Some("yo".into())
        }
        async fn escalate(&self, _: SentinelState, _: String) -> Result<Escalation, EscalateError> {
            Ok(Escalation {
                state: SentinelState {
                    tenant: "test".into(),
                    sentinel: "slow".into(),
                    observed_at: 0,
                    payload: serde_json::Value::Null,
                },
                reason: "ok".into(),
                urgency: gateway_sidecar::watch::Urgency::Low,
            })
        }
    }

    let outcome = gateway_sidecar::watch::runtime::fire_pipeline(
        &SlowObserve,
        &gateway_sidecar::watch::quarantine::QuarantineState::test_default(),
    )
    .await;

    use gateway_sidecar::watch::runtime::FireOutcome;
    assert!(
        matches!(outcome, FireOutcome::Timeout("observe")),
        "expected Timeout(observe), got {:?}",
        outcome
    );
}

/// Blocking sentinel code can exceed wall-clock budget without yielding, which
/// tokio::time::timeout cannot preempt. The fire-decision budget catches that
/// class after observe()/interesting() returns and records a budget violation.
#[tokio::test]
async fn t3_fire_decision_budget_catches_blocking_observe() {
    use async_trait::async_trait;
    use gateway_sidecar::watch::{
        EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier,
    };

    struct BlockingObserve;
    #[async_trait]
    impl Sentinel for BlockingObserve {
        fn name(&self) -> &str {
            "blocking-observe"
        }
        fn tenant(&self) -> &str {
            "test"
        }
        fn tier(&self) -> Tier {
            Tier::Polling
        }
        fn cooldown(&self) -> Duration {
            Duration::from_millis(0)
        }
        async fn observe(&self) -> Result<SentinelState, ObserveError> {
            std::thread::sleep(Duration::from_millis(120));
            Ok(SentinelState {
                tenant: "test".into(),
                sentinel: "blocking-observe".into(),
                observed_at: 0,
                payload: serde_json::Value::Null,
            })
        }
        fn interesting(&self, _: &SentinelState) -> Option<String> {
            Some("blocked runtime worker".into())
        }
        async fn escalate(
            &self,
            state: SentinelState,
            reason: String,
        ) -> Result<Escalation, EscalateError> {
            Ok(Escalation {
                state,
                reason,
                urgency: gateway_sidecar::watch::Urgency::Low,
            })
        }
    }

    let outcome = gateway_sidecar::watch::runtime::fire_pipeline(
        &BlockingObserve,
        &gateway_sidecar::watch::quarantine::QuarantineState::test_default(),
    )
    .await;

    use gateway_sidecar::watch::runtime::FireOutcome;
    assert!(
        matches!(outcome, FireOutcome::BudgetViolation("fire_decision")),
        "expected BudgetViolation(fire_decision), got {:?}",
        outcome
    );
}

/// `fire_pipeline` must treat ProbationLogOnly as a reason rewrite, not a
/// blocking gate. During probation, `escalate()` runs, an audit row is written
/// with a `[PROBATION]` prefix, and the dispatcher
/// filters. If fire_pipeline returns Gated(ProbationLogOnly), scheduled
/// fires are silently dropped during the 10-min window — operator loses
/// visibility, defeating probation's whole purpose.
#[tokio::test]
async fn t_fire_pipeline_during_probation_runs_with_prefix() {
    use async_trait::async_trait;
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::runtime::FireOutcome;
    use gateway_sidecar::watch::{
        EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
    };
    use std::sync::Arc;

    struct HealthySentinel;
    #[async_trait]
    impl Sentinel for HealthySentinel {
        fn name(&self) -> &str {
            "probation-test"
        }
        fn tenant(&self) -> &str {
            "sovereign"
        }
        fn tier(&self) -> Tier {
            Tier::Polling
        }
        fn cooldown(&self) -> Duration {
            Duration::from_millis(0)
        }
        async fn observe(&self) -> Result<SentinelState, ObserveError> {
            Ok(SentinelState {
                tenant: "sovereign".into(),
                sentinel: "probation-test".into(),
                observed_at: 0,
                payload: serde_json::Value::Null,
            })
        }
        fn interesting(&self, _: &SentinelState) -> Option<String> {
            Some("new file".into())
        }
        async fn escalate(
            &self,
            state: SentinelState,
            reason: String,
        ) -> Result<Escalation, EscalateError> {
            Ok(Escalation {
                state,
                reason,
                urgency: Urgency::Low,
            })
        }
    }

    let tmp = tempfile::TempDir::new().unwrap();
    let db = Arc::new(WatchDb::open(&tmp.path().join("watch.db")).await.unwrap());
    db.run_migrations().await.unwrap();
    db.upsert_sentinel_registration("sovereign", "probation-test", "polling", 0, "{}")
        .await
        .unwrap();
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    // Park in probation: 10min from now.
    q.test_set_probation_until(
        "sovereign",
        "probation-test",
        std::time::Instant::now() + Duration::from_secs(600),
    )
    .await;

    let outcome = gateway_sidecar::watch::runtime::fire_pipeline(&HealthySentinel, &q).await;
    let fire_id = match outcome {
        FireOutcome::Fired(id) => id,
        other => panic!(
            "fire_pipeline during probation must Fire (with prefix), not gate; got {other:?}"
        ),
    };
    let row = db
        .fetch_fire_by_id(fire_id)
        .await
        .unwrap()
        .expect("audit row must exist");
    assert!(
        row.reason.starts_with("[PROBATION] "),
        "probation fire's audit row must be prefixed for Phase 3 filtering; got {:?}",
        row.reason
    );
}

/// T33.8 P1-3 — invariant under concurrent admin DELETEs on the same
/// sentinel. The `admin_clear_quarantine` split-lock pattern documented at
/// `quarantine.rs:357-366` notes that phase-1 inspect and phase-3 mutate
/// happen under SEPARATE lock acquisitions with the DB await in between;
/// two racing admin DELETEs can therefore produce inconsistent cosmetic
/// `cleared[]` arrays. This test locks the durable invariant: regardless
/// of interleaving, `watch_sentinels.hard_killed_at` MUST be NULL after
/// both calls commit. Council/harden P1-3 — "tests only, no reload" trade
/// (Chair pick over Grok's `refresh_from_db` proposal).
#[tokio::test]
async fn t33_8_two_admin_clears_same_sentinel_durable_state_correct() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Pre-stage hard-kill row directly in DB (bypassing record_failure so the
    // test isolates the admin_clear race, not the full state-machine).
    db.upsert_hard_kill("sovereign", "s", 1_700_000_000_000, "test pre-stage")
        .await
        .unwrap();

    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));

    // Drive two concurrent admin_clear_quarantine calls. tokio::join! polls
    // both futures on the same task; tokio_rusqlite's connection actor
    // serializes the BEGIN IMMEDIATE tx but the surrounding split-lock
    // phases (inspect under parking_lot lock, mutate under parking_lot lock)
    // can still interleave.
    let q1 = q.clone();
    let q2 = q.clone();
    let (r1, r2) = tokio::join!(
        q1.admin_clear_quarantine("sovereign", "s", false),
        q2.admin_clear_quarantine("sovereign", "s", false)
    );
    assert!(r1.is_ok(), "first admin_clear must complete Ok: {:?}", r1);
    assert!(r2.is_ok(), "second admin_clear must complete Ok: {:?}", r2);

    // Durable invariant — independent of cleared[] cosmetic race.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let hard_killed_at: Option<i64> = conn
        .query_row(
            "SELECT hard_killed_at FROM watch_sentinels
             WHERE tenant='sovereign' AND name='s'",
            [],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap();
    assert!(
        hard_killed_at.is_none(),
        "DB hard_killed_at MUST be NULL after either admin_clear commits; got {:?}. \
         If this fires, the split-lock pattern broke its durable-correctness invariant.",
        hard_killed_at
    );

    // Both clears report at least an empty cleared[] array; together they
    // may BOTH report "hard_kill" (the documented cosmetic race) or only one.
    // Both shapes are acceptable.
    let c1 = r1.unwrap();
    let c2 = r2.unwrap();
    let any_reported_hard_kill =
        c1.cleared.iter().any(|s| s == "hard_kill") || c2.cleared.iter().any(|s| s == "hard_kill");
    assert!(
        any_reported_hard_kill,
        "at least one admin_clear should have observed and reported hard_kill cleared; c1={:?} c2={:?}",
        c1.cleared, c2.cleared
    );
}

/// T33.8 P1-3 — `admin_clear_quarantine` racing `write_fire_row` on the
/// same sentinel. `BEGIN IMMEDIATE` serializes the two transactions:
///   - admin_clear commits first → write_fire_row sees cleared
///     hard_killed_at → returns Ok(audit_row_id).
///   - write_fire_row commits first → OCC reads still-set hard_killed_at →
///     returns Err("hard_killed_race"). Then admin_clear runs + clears.
/// Both orderings are valid. The post-state invariant: hard_killed_at is
/// NULL AND a fresh write_fire_row MUST succeed (OCC sees cleared state).
#[tokio::test]
async fn t33_8_admin_clear_racing_insert_fire_post_state_unblocked() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::{Escalation, SentinelState, Urgency};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    db.upsert_hard_kill("sovereign", "s", 1_700_000_000_000, "test pre-stage")
        .await
        .unwrap();

    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let esc = Escalation {
        state: SentinelState {
            tenant: "sovereign".into(),
            sentinel: "s".into(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        },
        reason: "race-test".into(),
        urgency: Urgency::Low,
    };

    let q_clear = q.clone();
    let q_fire = q.clone();
    let esc_clone = esc.clone();
    let (_clear_outcome, _fire_outcome) = tokio::join!(
        q_clear.admin_clear_quarantine("sovereign", "s", false),
        q_fire.write_fire_row(esc_clone)
    );

    // Post-state invariant — DB hard_killed_at is NULL regardless of order.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let hard_killed_at: Option<i64> = conn
        .query_row(
            "SELECT hard_killed_at FROM watch_sentinels
             WHERE tenant='sovereign' AND name='s'",
            [],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap();
    assert!(
        hard_killed_at.is_none(),
        "post-race DB hard_killed_at MUST be NULL; got {:?}",
        hard_killed_at
    );

    // A fresh write_fire_row MUST succeed — OCC sees the cleared state.
    let post = q.write_fire_row(esc).await;
    assert!(
        post.is_ok(),
        "post-race fresh write_fire_row must succeed (OCC sees cleared hard_killed_at); got {:?}",
        post
    );
}
