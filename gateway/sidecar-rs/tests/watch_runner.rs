//! T23 — WatchRunner: per-sentinel spawn loop on watch_runtime.
//!
//! Verifies that WatchRunner::start drives fire_pipeline at the sentinel's
//! cooldown for Polling/Deep tier, accepts external kicks for Fast tier,
//! and shuts down cleanly when the shutdown signal is dropped.

use async_trait::async_trait;
use gateway_sidecar::watch::quarantine::QuarantineState;
use gateway_sidecar::watch::runner::WatchRunner;
use gateway_sidecar::watch::runtime::build_watch_runtime;
use gateway_sidecar::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;

/// Counts observe() invocations so we can assert the runner actually drives.
struct CountingSentinel {
    name: String,
    tenant: String,
    tier: Tier,
    cooldown: Duration,
    observes: Arc<AtomicU32>,
    fires: Arc<AtomicU32>,
}

#[async_trait]
impl Sentinel for CountingSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        self.tier
    }
    fn cooldown(&self) -> Duration {
        self.cooldown
    }
    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        self.observes.fetch_add(1, Ordering::SeqCst);
        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        })
    }
    fn interesting(&self, _: &SentinelState) -> Option<String> {
        Some("test".into())
    }
    async fn escalate(&self, state: SentinelState, _: String) -> Result<Escalation, EscalateError> {
        self.fires.fetch_add(1, Ordering::SeqCst);
        Ok(Escalation {
            state,
            reason: "test".into(),
            urgency: Urgency::Low,
        })
    }
}

#[test]
fn t23a_polling_sentinel_runs_on_cooldown() {
    let rt = build_watch_runtime();
    let observes = Arc::new(AtomicU32::new(0));
    let fires = Arc::new(AtomicU32::new(0));
    let sentinel = Arc::new(CountingSentinel {
        name: "polling-test".into(),
        tenant: "test".into(),
        tier: Tier::Polling,
        cooldown: Duration::from_millis(10),
        observes: observes.clone(),
        fires: fires.clone(),
    }) as Arc<dyn Sentinel>;
    let quarantine = Arc::new(QuarantineState::test_default());

    let handles = WatchRunner::start(rt.handle().clone(), vec![sentinel], quarantine);

    // Give the runner time to fire several times at 10ms cooldown.
    std::thread::sleep(Duration::from_millis(100));

    handles.shutdown();

    rt.block_on(handles.join_all());

    let n = observes.load(Ordering::SeqCst);
    assert!(
        n >= 3,
        "expected ≥3 observes in 100ms with 10ms cooldown, got {n}"
    );
    let f = fires.load(Ordering::SeqCst);
    assert!(
        f >= 3,
        "expected ≥3 fires (interesting always returns Some), got {f}"
    );
}

#[test]
fn t23b_fast_sentinel_only_fires_on_external_kick() {
    let rt = build_watch_runtime();
    let observes = Arc::new(AtomicU32::new(0));
    let fires = Arc::new(AtomicU32::new(0));
    let sentinel = Arc::new(CountingSentinel {
        name: "fast-test".into(),
        tenant: "test".into(),
        tier: Tier::Fast,
        cooldown: Duration::from_millis(10), // ignored for Fast tier
        observes: observes.clone(),
        fires: fires.clone(),
    }) as Arc<dyn Sentinel>;
    let quarantine = Arc::new(QuarantineState::test_default());

    let handles = WatchRunner::start(rt.handle().clone(), vec![sentinel], quarantine);

    // Wait — no kicks; Fast tier must NOT fire on its own.
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        observes.load(Ordering::SeqCst),
        0,
        "Fast tier MUST NOT fire without an external kick"
    );

    // Now kick 3 times.
    let kick = handles
        .kick_sender("fast-test")
        .expect("kick sender for fast-test");
    rt.block_on(async {
        kick.send(()).await.unwrap();
        kick.send(()).await.unwrap();
        kick.send(()).await.unwrap();
    });

    // Allow the runner task to drain and process.
    std::thread::sleep(Duration::from_millis(100));

    handles.shutdown();
    rt.block_on(handles.join_all());

    let n = observes.load(Ordering::SeqCst);
    assert!(
        (3..=4).contains(&n),
        "expected ~3 observes after 3 kicks, got {n}"
    );
}

#[test]
fn t23c_shutdown_drops_all_loops_cleanly() {
    let rt = build_watch_runtime();
    let observes = Arc::new(AtomicU32::new(0));
    let fires = Arc::new(AtomicU32::new(0));
    let s1 = Arc::new(CountingSentinel {
        name: "s1".into(),
        tenant: "test".into(),
        tier: Tier::Polling,
        cooldown: Duration::from_millis(5),
        observes: observes.clone(),
        fires: fires.clone(),
    }) as Arc<dyn Sentinel>;
    let s2 = Arc::new(CountingSentinel {
        name: "s2".into(),
        tenant: "test".into(),
        tier: Tier::Deep,
        cooldown: Duration::from_millis(5),
        observes: observes.clone(),
        fires: fires.clone(),
    }) as Arc<dyn Sentinel>;
    let quarantine = Arc::new(QuarantineState::test_default());

    let handles = WatchRunner::start(rt.handle().clone(), vec![s1, s2], quarantine);

    std::thread::sleep(Duration::from_millis(50));
    handles.shutdown();
    // join_all MUST return — if shutdown signal weren't honored, this hangs.
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), handles.join_all())
            .await
            .expect("WatchRunner::join_all hung after shutdown");
    });
}

/// T33.5 — Sentinel that always returns `EscalateError::Transient`. Used to
/// drive `record_failure` through the runner loop without touching observe()
/// or interesting() error paths.
struct EscalateFailingSentinel {
    name: String,
    tenant: String,
}

#[async_trait]
impl Sentinel for EscalateFailingSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Fast
    }
    fn cooldown(&self) -> Duration {
        Duration::from_millis(10) // ignored — Fast tier
    }
    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        })
    }
    fn interesting(&self, _: &SentinelState) -> Option<String> {
        Some("test".into())
    }
    async fn escalate(&self, _: SentinelState, _: String) -> Result<Escalation, EscalateError> {
        Err(EscalateError::Transient(
            "T33.5 RED — wiring under test".into(),
        ))
    }
}

/// T33.5 P0-1 — runner_loop must wire `FireOutcome::EscalateErr` to
/// `QuarantineState::record_failure`. With `fails_to_trigger = 2` (default),
/// 2 consecutive escalate-failures must push the sentinel into `Quarantined`.
///
/// Regression guard: `record_failure`/`record_success` have ZERO callers in
/// `src/`. The state machine is admin-only theater until this test is GREEN.
#[test]
fn t33_5_runner_wires_escalate_err_to_record_failure() {
    let rt = build_watch_runtime();
    let sentinel = Arc::new(EscalateFailingSentinel {
        name: "fail-test".into(),
        tenant: "test".into(),
    }) as Arc<dyn Sentinel>;
    let quarantine = Arc::new(QuarantineState::test_default());

    let handles = WatchRunner::start(rt.handle().clone(), vec![sentinel], quarantine.clone());

    // Drive 2 failures via external kicks.
    let kick = handles.kick_sender("fail-test").expect("kick sender");
    rt.block_on(async {
        kick.send(()).await.unwrap();
        kick.send(()).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(100));

    // Drain & shut down before assertion so failures are committed.
    handles.shutdown();
    rt.block_on(handles.join_all());

    // After 2 failures the state machine MUST have an entry in Quarantined.
    let state = rt.block_on(quarantine.get_state("test", "fail-test"));
    let rec =
        state.expect("record_failure never wired — no quarantine record after 2 escalate errors");
    assert!(
        rec.duration_ms > 0,
        "expected quarantine cycle triggered after 2 fails, got cycle={} duration_ms={}",
        rec.cycle_count,
        rec.duration_ms
    );

    let gate = rt.block_on(quarantine.is_blocked("test", "fail-test"));
    assert!(
        matches!(
            gate,
            Some(gateway_sidecar::watch::runtime::QuarantineGate::Quarantined)
        ),
        "expected QuarantineGate::Quarantined after 2 fails, got {gate:?}"
    );
}

/// T33.5 P0-1 — runner_loop must wire `FireOutcome::Fired` to
/// `QuarantineState::record_success`. After seeding a record via direct
/// `record_failure`, one successful fire via the runner must bump
/// `consecutive_successes` and decrement `consecutive_fails`.
#[test]
fn t33_5_runner_wires_fired_to_record_success() {
    let rt = build_watch_runtime();
    let observes = Arc::new(AtomicU32::new(0));
    let fires = Arc::new(AtomicU32::new(0));
    let sentinel = Arc::new(CountingSentinel {
        name: "succ-test".into(),
        tenant: "test".into(),
        tier: Tier::Fast,
        cooldown: Duration::from_millis(10),
        observes: observes.clone(),
        fires: fires.clone(),
    }) as Arc<dyn Sentinel>;
    let quarantine = Arc::new(QuarantineState::test_default());

    // Seed a record with one prior failure so the state machine has something
    // to observe `record_success` mutating.
    rt.block_on(quarantine.record_failure("test", "succ-test"));
    let before = rt
        .block_on(quarantine.get_state("test", "succ-test"))
        .expect("seed record_failure");
    assert_eq!(before.consecutive_fails, 1);
    assert_eq!(before.consecutive_successes, 0);

    let handles = WatchRunner::start(rt.handle().clone(), vec![sentinel], quarantine.clone());

    let kick = handles.kick_sender("succ-test").expect("kick sender");
    rt.block_on(async {
        kick.send(()).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(100));

    handles.shutdown();
    rt.block_on(handles.join_all());

    assert_eq!(
        fires.load(Ordering::SeqCst),
        1,
        "exactly one Fired outcome expected"
    );

    let after = rt
        .block_on(quarantine.get_state("test", "succ-test"))
        .expect("record should still exist");
    assert_eq!(
        after.consecutive_successes, 1,
        "Fired → record_success NOT wired; consecutive_successes did not advance"
    );
    assert_eq!(
        after.consecutive_fails, 0,
        "record_success should have decremented consecutive_fails from 1 → 0"
    );
}

/// Minimal Sentinel stub for `handle_fire_outcome` unit tests. The dispatch
/// only reads `name()` and `tenant()`; observe/interesting/escalate are
/// never invoked. Lets us exercise every FireOutcome variant without
/// driving the full fire_pipeline (which can't easily produce
/// AuditWriteErr / Timeout("audit") without contrived DB plumbing).
struct StubSentinel {
    name: String,
    tenant: String,
}

#[async_trait]
impl Sentinel for StubSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Fast
    }
    fn cooldown(&self) -> Duration {
        Duration::from_millis(0)
    }
    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        unreachable!("StubSentinel.observe must not be called by handle_fire_outcome")
    }
    fn interesting(&self, _: &SentinelState) -> Option<String> {
        unreachable!("StubSentinel.interesting must not be called by handle_fire_outcome")
    }
    async fn escalate(&self, _: SentinelState, _: String) -> Result<Escalation, EscalateError> {
        unreachable!("StubSentinel.escalate must not be called by handle_fire_outcome")
    }
}

/// T33.P1-B — `FireOutcome::AuditWriteErr` MUST NOT call `record_failure`.
///
/// Regression guard (P1-B, /// circuit-breaker fault-domain split): the original T33.5 wiring routed
/// AuditWriteErr / AuditWorkerCrashed / Timeout("audit") to record_failure.
/// Those outcomes mean the sentinel itself fired correctly but the audit
/// pipeline (SQLite, worker thread) failed — punishing a healthy sentinel
/// for downstream infra trouble is the wrong invariant. Correct mapping:
/// warn + bump `audit_infra_errors_total` (no quarantine effect).
///
/// Pre-existing T33.5 tests (`t33_5_runner_wires_escalate_err_to_record_failure`
/// + `t33_5_runner_wires_fired_to_record_success`) cover correct invariants
/// (EscalateErr → record_failure, Fired → record_success); they do NOT
/// encode the wrong AuditWriteErr routing — that invariant lived only in
/// `runner.rs` source. These P1-B tests cover the previously-uncovered
/// variants directly through the extracted `handle_fire_outcome` fn.
#[tokio::test]
async fn t33_p1b_audit_write_err_does_not_quarantine_healthy_sentinel() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::FireOutcome;

    let s = StubSentinel {
        name: "audit-flaky".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    let before = q.audit_infra_errors_total();
    handle_fire_outcome(FireOutcome::AuditWriteErr("disk full".into()), &s, &q).await;
    let after = q.audit_infra_errors_total();

    assert_eq!(
        after,
        before + 1,
        "AuditWriteErr must bump audit_infra_errors_total; before={before} after={after}"
    );
    assert!(
        q.get_state("sovereign", "audit-flaky").await.is_none(),
        "AuditWriteErr MUST NOT call record_failure — healthy sentinel quarantined for audit-infra trouble"
    );
}

/// T33.P1-B — `FireOutcome::AuditWorkerCrashed` MUST NOT call
/// `record_failure`. Same fault-domain split as AuditWriteErr.
#[tokio::test]
async fn t33_p1b_audit_worker_crashed_does_not_quarantine_healthy_sentinel() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::FireOutcome;

    let s = StubSentinel {
        name: "worker-flaky".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    handle_fire_outcome(FireOutcome::AuditWorkerCrashed, &s, &q).await;

    assert_eq!(q.audit_infra_errors_total(), 1);
    assert!(
        q.get_state("sovereign", "worker-flaky").await.is_none(),
        "AuditWorkerCrashed MUST NOT call record_failure"
    );
}

/// T33.P1-B — `FireOutcome::Timeout("audit")` MUST NOT call `record_failure`.
/// Other Timeout phases (observe|interesting|escalate|total) DO call
/// record_failure — that's the discriminator the fault-domain split uses.
#[tokio::test]
async fn t33_p1b_timeout_audit_does_not_quarantine_healthy_sentinel() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::FireOutcome;

    let s = StubSentinel {
        name: "audit-slow".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    handle_fire_outcome(FireOutcome::Timeout("audit"), &s, &q).await;

    assert_eq!(q.audit_infra_errors_total(), 1);
    assert!(
        q.get_state("sovereign", "audit-slow").await.is_none(),
        "Timeout(\"audit\") MUST NOT call record_failure"
    );
}

/// T33.P1-B negative control — `FireOutcome::Timeout("escalate")` (the
/// sentinel's escalate() stalled) MUST still call `record_failure`. The
/// fault-domain split discriminates by phase string; only "audit" is
/// infra. Verifies the chair-prescribed Timeout(_) catch-all routes
/// non-audit phases correctly.
#[tokio::test]
async fn t33_p1b_timeout_escalate_still_quarantines() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::FireOutcome;

    let s = StubSentinel {
        name: "escalate-slow".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    // 2 timeouts at fails_to_trigger=2 → record_failure → enter quarantine.
    handle_fire_outcome(FireOutcome::Timeout("escalate"), &s, &q).await;
    handle_fire_outcome(FireOutcome::Timeout("escalate"), &s, &q).await;

    let rec = q
        .get_state("sovereign", "escalate-slow")
        .await
        .expect("Timeout(\"escalate\") → record_failure → record exists");
    assert!(
        rec.duration_ms > 0,
        "2× Timeout(\"escalate\") must trigger quarantine; got duration_ms={}",
        rec.duration_ms,
    );
    assert_eq!(
        q.audit_infra_errors_total(),
        0,
        "Timeout(\"escalate\") is NOT audit-infra — counter must stay 0"
    );
}

/// Budget violations are sentinel execution failures, not audit-infra faults.
/// They reuse the existing failure/quarantine path so repeated budget overruns
/// cannot stay silently healthy.
#[tokio::test]
async fn t33_budget_violation_quarantines_like_other_sentinel_failures() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::FireOutcome;

    let s = StubSentinel {
        name: "budget-breaker".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    handle_fire_outcome(FireOutcome::BudgetViolation("fire_decision"), &s, &q).await;
    handle_fire_outcome(FireOutcome::BudgetViolation("fire_decision"), &s, &q).await;

    let rec = q
        .get_state("sovereign", "budget-breaker")
        .await
        .expect("BudgetViolation -> record_failure -> record exists");
    assert!(
        rec.duration_ms > 0,
        "2x BudgetViolation must trigger quarantine; got duration_ms={}",
        rec.duration_ms,
    );
    assert_eq!(
        q.audit_infra_errors_total(),
        0,
        "BudgetViolation is not audit-infra"
    );
}

/// T33.P1-B — `FireOutcome::Uninteresting` MUST call `record_success`. A
/// successful observe + None from interesting() IS a healthy tick; the
/// original T33.5 wiring treated this as a no-op, so healthy-but-bored
/// sentinels with prior failures could never engage hysteresis (council
/// P1-B reasoning: an Uninteresting tick is exactly the recovery signal
/// hysteresis needs).
#[tokio::test]
async fn t33_p1b_uninteresting_records_success() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::FireOutcome;

    let s = StubSentinel {
        name: "healthy-quiet".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    // Seed one prior failure so record_success has observable effect.
    q.record_failure("sovereign", "healthy-quiet").await;
    let before = q
        .get_state("sovereign", "healthy-quiet")
        .await
        .expect("seed");
    assert_eq!(before.consecutive_fails, 1);
    assert_eq!(before.consecutive_successes, 0);

    handle_fire_outcome(FireOutcome::Uninteresting, &s, &q).await;

    let after = q
        .get_state("sovereign", "healthy-quiet")
        .await
        .expect("record exists");
    assert_eq!(
        after.consecutive_successes, 1,
        "Uninteresting → record_success must advance consecutive_successes"
    );
    assert_eq!(
        after.consecutive_fails, 0,
        "record_success decrements consecutive_fails 1 → 0"
    );
}

/// riders (B) — sweep_phantom_claims must have a RUNTIME caller (closes the
/// engine-fact "zero runtime callers" gap; ruling acceptance for the riders
/// item). The runner spawns a phantom-sweep maintenance loop next to
/// pruning_loop whenever a durable WatchDb is wired; its boot sweep reclaims
/// an expired 'claimed' row to 'failed', releases the spend ledger
/// reservation, and bumps the p0b lease_expired_during_deliberation counter
/// — all WITHOUT any test code calling sweep_phantom_claims directly.
/// Producer/dispatcher gates stay default-OFF; no sentinels are registered.
#[test]
fn test_phantom_sweep_wired_into_runner_loop() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;

    let rt = build_watch_runtime();
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_phantom_wiring.db");

    // Seed: one escalation, claimed (reserves the p0c estimate), then the
    // holder "dies" — we force the lease to expire so only a runtime sweep
    // can reclaim it.
    let db = Arc::new(rt.block_on(async {
        let db = WatchDb::open(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
        // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
        arm_attest_common::arm_db_for_reserve_test(&db).await;
        db
    }));
    rt.block_on(async {
        db.insert_pending_escalation_with_causal_dedup(
            "phantom-1",
            "tenant_pw",
            "sentinel_pw",
            "{}",
            "dig-pw-1",
            100,
            0,
        )
        .await
        .unwrap();
    });
    let dead_claim = rt
        .block_on(db.claim_next_queued_or_failed())
        .unwrap()
        .expect("row must be claimable");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.busy_timeout(Duration::from_millis(500)).unwrap();
    conn.execute(
        "UPDATE pending_escalations SET claimed_until_ms = 0 WHERE id = 'phantom-1'",
        [],
    )
    .unwrap();

    // The p0c reservation is held by the dead claim before the runner starts.
    let reserved_before: f64 = conn
        .query_row("SELECT reserved_usd FROM spend_ledger", [], |r| r.get(0))
        .unwrap();
    assert!(
        reserved_before > 0.0,
        "dead claim must hold a reservation before the sweep, got {reserved_before}"
    );

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    // No sentinels; producer gate is default-OFF — only the maintenance
    // loops (pending-retry, pruning, phantom sweep) spawn.
    let handles = WatchRunner::start(rt.handle().clone(), vec![], Arc::clone(&quarantine));

    // The phantom loop's boot sweep must flip the row promptly (no test code
    // calls sweep_phantom_claims — this is the runtime-caller proof).
    let mut status = String::new();
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        status = conn
            .query_row(
                "SELECT status FROM pending_escalations WHERE id = 'phantom-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if status == "failed" {
            break;
        }
    }
    assert_eq!(
        status, "failed",
        "runtime phantom sweep never reclaimed the expired claim — sweep_phantom_claims has no runtime caller"
    );

    // p0c reservation released by the runtime sweep.
    let reserved_after: f64 = conn
        .query_row("SELECT reserved_usd FROM spend_ledger", [], |r| r.get(0))
        .unwrap();
    assert!(
        reserved_after.abs() < 1e-9,
        "p0c reservation must be released by the runtime sweep, got {reserved_after}"
    );

    // p0b counter bumped exactly once (the dead claim was a real in-flight
    // claim: claim_token set, attempts > 0).
    assert_eq!(
        quarantine.lease_expired_during_deliberation(),
        1,
        "runtime sweep must bump lease_expired_during_deliberation for the in-flight dead claim"
    );

    // The swept row is reclaimable with a fresh fencing token.
    let reclaim = rt
        .block_on(db.claim_next_queued_or_failed())
        .unwrap()
        .expect("swept row must be reclaimable");
    assert_eq!(reclaim.id, "phantom-1");
    assert_ne!(
        reclaim.claim_token, dead_claim.claim_token,
        "fresh fencing token on reclaim"
    );

    handles.shutdown();
    rt.block_on(handles.join_all());
}

/// T33.P1-B — `FireOutcome::Gated(_)` MUST be a no-op. The sentinel was
/// already gated (Quarantined / HardKilled / ProbationLogOnly) before
/// escalate ran; calling record_failure or record_success would double-count.
#[tokio::test]
async fn t33_p1b_gated_is_no_op() {
    use gateway_sidecar::watch::runner::handle_fire_outcome;
    use gateway_sidecar::watch::runtime::{FireOutcome, QuarantineGate};

    let s = StubSentinel {
        name: "blocked".into(),
        tenant: "sovereign".into(),
    };
    let q = QuarantineState::test_default();

    // Seed a record with one prior failure so we can detect mutation.
    q.record_failure("sovereign", "blocked").await;
    let before = q.get_state("sovereign", "blocked").await.expect("seed");

    handle_fire_outcome(FireOutcome::Gated(QuarantineGate::Quarantined), &s, &q).await;

    let after = q
        .get_state("sovereign", "blocked")
        .await
        .expect("record exists");
    assert_eq!(
        after.consecutive_fails, before.consecutive_fails,
        "Gated must NOT call record_failure"
    );
    assert_eq!(
        after.consecutive_successes, before.consecutive_successes,
        "Gated must NOT call record_success"
    );
    assert_eq!(q.audit_infra_errors_total(), 0);
}

// ===========================================================================
// boot-time producer claim check + env-arm
// audit, factored into `boot_producer_claim_check_and_audit` so deleting the
// enforcement would fail THESE tests instead of leaving CI green.
// ===========================================================================

/// Foreign LIVE writer claim → the boot path must refuse: returns false,
/// clears producer_kill_state (so /watch surfaces report unarmed truthfully),
/// and appends NO boot_env_arm audit row.
#[tokio::test]
async fn test_boot_arm_claim_check_refuses_on_foreign_claim() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;
    use gateway_sidecar::watch::runner::boot_producer_claim_check_and_audit_with_mode;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_boot_refuse.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // A DIFFERENT instance holds a FRESH claim.
    assert!(db
        .try_acquire_writer_claim("foreign-instance", now_ms, 90_000)
        .await
        .unwrap());

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    // Simulate the boot path having pre-set the kill state (as start() does).
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    let ok =
        boot_producer_claim_check_and_audit_with_mode(&quarantine, &db, now_ms + 1, Some("LIVE"))
            .await;
    assert!(
        !ok,
        "boot arm must REFUSE while a foreign live writer holds the claim"
    );
    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "kill state must be cleared on refusal (unarmed truthfully)"
    );
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        !rows.iter().any(|r| r.action == "boot_env_arm"),
        "NO boot_env_arm row may exist after a refusal; got {rows:?}"
    );

    // The foreign claim is untouched.
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("foreign-instance"));
}

/// Free claim → the boot path acquires it under this process's uuid AND
/// appends the `boot_env_arm` audit row (the env-path arm
/// bypassed the four-eyes ceremony with ZERO audit evidence).
#[tokio::test]
async fn test_boot_arm_claim_check_succeeds_and_appends_boot_env_arm_audit() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;
    use gateway_sidecar::watch::runner::boot_producer_claim_check_and_audit_with_mode;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_boot_arm.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    let ok =
        boot_producer_claim_check_and_audit_with_mode(&quarantine, &db, now_ms, Some("LIVE")).await;
    assert!(ok, "boot arm must proceed on a free claim");
    assert!(
        quarantine.producer_kill_state.lock().is_some(),
        "kill state stays armed on success"
    );

    let rows = db.list_arm_audit().await.unwrap();
    let boot_row = rows
        .iter()
        .find(|r| r.action == "boot_env_arm")
        .expect("boot env-arm must append a boot_env_arm audit row");
    assert_eq!(boot_row.principal, "env(WATCH_PRODUCER_ENABLED)");
    assert!(
        boot_row
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("WATCH_DISPATCHER_GATEWAY_KEY"),
        "audit detail must name the env gate pair; got {boot_row:?}"
    );
    assert!(
        boot_row
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("execution_mode=LIVE"),
        "audit detail must record the EXECUTION_MODE value seen; got {boot_row:?}"
    );

    // The claim is now ours.
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(
        holder.map(|(u, _)| u).as_deref(),
        Some(gateway_sidecar::watch::db::process_instance_uuid())
    );
}

#[test]
fn test_boot_env_arm_gate_requires_exact_live_execution_mode() {
    use gateway_sidecar::watch::runner::producer_gate_armed_from;

    assert!(
        producer_gate_armed_from(Some("true"), true, Some("LIVE")),
        "WATCH_PRODUCER_ENABLED=true + key + EXECUTION_MODE=LIVE must arm"
    );
    assert!(
        !producer_gate_armed_from(Some("true"), true, None),
        "absent EXECUTION_MODE must fail closed"
    );
    for mode in ["live", "true", "other", ""] {
        assert!(
            !producer_gate_armed_from(Some("true"), true, Some(mode)),
            "EXECUTION_MODE={mode:?} must not arm; only exact LIVE is allowed"
        );
    }
}

/// Boot claim ATTEMPT outcomes (restart regression): a
/// foreign live claim yields `RefusedForeignClaim { holder }` (carrying the
/// holder uuid for the WARN log), and once the foreign claim is RELEASED the
/// very next attempt yields `Acquired`. This is the refuse→succeed transition
/// the retry loop is built on, proven without wall-clock via release.
#[tokio::test]
async fn test_boot_producer_claim_attempt_refuse_then_acquire_after_release() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;
    use gateway_sidecar::watch::runner::{boot_producer_claim_attempt, BootClaimOutcome};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_boot_retry.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // A foreign instance holds a FRESH claim.
    assert!(db
        .try_acquire_writer_claim("foreign-live", now_ms, 90_000)
        .await
        .unwrap());

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    // Attempt 1: refused, holder reported.
    let out = boot_producer_claim_attempt(&quarantine, &db, now_ms + 1).await;
    assert_eq!(
        out,
        BootClaimOutcome::RefusedForeignClaim {
            holder: Some("foreign-live".to_string())
        },
        "a foreign live claim must refuse with the holder uuid"
    );
    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "kill state cleared on refusal (unarmed truthfully)"
    );

    // Foreign instance gracefully releases (the lifecycle fix).
    assert!(db.release_writer_claim("foreign-live").await.unwrap());

    // Attempt 2: now acquires + audits — no stale wait needed.
    let out = boot_producer_claim_attempt(&quarantine, &db, now_ms + 2).await;
    assert_eq!(
        out,
        BootClaimOutcome::Acquired,
        "after release the next attempt must acquire"
    );
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        rows.iter().any(|r| r.action == "boot_env_arm"),
        "a boot_env_arm audit row must append on the SUCCESSFUL attempt only"
    );
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(
        holder.map(|(u, _)| u).as_deref(),
        Some(gateway_sidecar::watch::db::process_instance_uuid())
    );
}

/// Boot claim ATTEMPT succeeds via STALE TAKEOVER (the case that should have
/// recovered the smoke at ~90s): a foreign claim held at t0, attempted past
/// the stale window, is taken over — `Acquired`. Clock injected, no wall-clock.
#[tokio::test]
async fn test_boot_producer_claim_attempt_acquires_after_stale_takeover() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;
    use gateway_sidecar::watch::runner::{boot_producer_claim_attempt, BootClaimOutcome};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_boot_stale.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let t0 = 1_000_000_000i64;
    let stale = gateway_sidecar::watch::db::writer_claim_stale_ms();

    assert!(db
        .try_acquire_writer_claim("dead-foreign", t0, stale)
        .await
        .unwrap());

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    // Inside the stale window: refused.
    let out = boot_producer_claim_attempt(&quarantine, &db, t0 + 1).await;
    assert!(matches!(out, BootClaimOutcome::RefusedForeignClaim { .. }));

    // Past the stale window (what a one-shot refusal failed to revisit): the
    // dead foreign claim is taken over.
    let out = boot_producer_claim_attempt(&quarantine, &db, t0 + stale + 1).await;
    assert_eq!(out, BootClaimOutcome::Acquired);
}

/// Boot RETRY LOOP exits cleanly on the runner shutdown signal (SIGTERM
/// territory in prod). A foreign live claim is held the whole time; the loop
/// keeps refusing, then the shutdown channel fires and the loop returns
/// `false` (producer never spawns) — it does NOT hang forever.
#[tokio::test]
async fn test_boot_producer_claim_retry_loop_exits_on_shutdown() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;
    use gateway_sidecar::watch::runner::boot_producer_claim_retry_loop;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_boot_loop_shutdown.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Foreign claim held fresh for the whole test (never released, never stale
    // within the test window).
    assert!(db
        .try_acquire_writer_claim("forever-foreign", now_ms, 90_000)
        .await
        .unwrap());

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let q = Arc::clone(&quarantine);
    let dbc = Arc::clone(&db);
    let handle = tokio::spawn(async move {
        boot_producer_claim_retry_loop(&q, &dbc, Duration::from_millis(20), &mut shutdown_rx).await
    });

    // Let it refuse a couple of times, then signal shutdown.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown_tx.send(true).unwrap();

    let acquired = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("retry loop must exit promptly on shutdown")
        .unwrap();
    assert!(
        !acquired,
        "loop must return false (never acquired) when shutdown fires first"
    );
    // The foreign claim is untouched — we never two-live-writers.
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("forever-foreign"));
}

/// Boot RETRY LOOP acquires once the foreign holder RELEASES (the smoke fix
/// end-to-end through the loop glue): the loop refuses while the foreign claim
/// is held, a concurrent release clears it, and the loop's next attempt
/// acquires and returns `true`.
#[tokio::test]
async fn test_boot_producer_claim_retry_loop_acquires_after_release() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::QuarantineConfig;
    use gateway_sidecar::watch::runner::boot_producer_claim_retry_loop;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_boot_loop_release.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    assert!(db
        .try_acquire_writer_claim("releasing-foreign", now_ms, 90_000)
        .await
        .unwrap());

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let q = Arc::clone(&quarantine);
    let dbc = Arc::clone(&db);
    let handle = tokio::spawn(async move {
        boot_producer_claim_retry_loop(&q, &dbc, Duration::from_millis(15), &mut shutdown_rx).await
    });

    // Let it refuse at least once, then the foreign holder releases.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(db.release_writer_claim("releasing-foreign").await.unwrap());

    let acquired = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("retry loop must acquire promptly after release")
        .unwrap();
    assert!(
        acquired,
        "loop must return true once the foreign claim is released"
    );
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(
        holder.map(|(u, _)| u).as_deref(),
        Some(gateway_sidecar::watch::db::process_instance_uuid())
    );
}

/// cdc_sweep_tick
/// must refuse to sweep while a DIFFERENT instance holds a LIVE writer
/// claim, and proceed once that claim is stale (refusing there would
/// deadlock recovery) or absent.
#[tokio::test]
async fn test_cdc_sweep_tick_refuses_under_foreign_live_claim() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::runner::cdc_sweep_tick;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_fence_tick.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // One committed fire the sweep would normally enqueue.
    db.insert_fire(
        "tenant_fence",
        "s_fence",
        now_ms,
        "{}",
        "fence test",
        "{}",
        1,
    )
    .await
    .unwrap()
    .expect("fire inserted");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let pending_count = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM pending_escalations", [], |r| r.get(0))
            .unwrap()
    };

    let (_shut_tx, mut shut_rx) = tokio::sync::watch::channel(false);
    let mut poison = std::collections::HashMap::new();

    // (a) Foreign LIVE claim (fresh heartbeat) → tick refused, nothing enqueued.
    assert!(db
        .try_acquire_writer_claim("foreign-live", now_ms, 90_000)
        .await
        .unwrap());
    let res = cdc_sweep_tick(&db, &mut shut_rx, None, &mut poison).await;
    assert!(
        res.is_none(),
        "deposed producer's tick must refuse under a foreign LIVE claim"
    );
    assert_eq!(
        pending_count(&conn),
        0,
        "no escalation may be enqueued by a deposed producer"
    );

    // (b) Make the foreign claim STALE → tick proceeds (recovery not deadlocked).
    conn.execute(
        "UPDATE writer_claim SET heartbeat_at_ms = ?1 WHERE singleton = 1",
        rusqlite::params![now_ms - gateway_sidecar::watch::db::writer_claim_stale_ms() - 60_000],
    )
    .unwrap();
    let res2 = cdc_sweep_tick(&db, &mut shut_rx, None, &mut poison).await;
    assert!(
        res2.is_some(),
        "a STALE foreign claim must not block the sweep"
    );
    assert_eq!(
        pending_count(&conn),
        1,
        "the fire is enqueued once the claim is stale"
    );
}
