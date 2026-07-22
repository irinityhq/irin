//! Phase 2 quarantine state machine tests — including the P0-3c hard-kill race.
//!
//! T21 tests the OCC invariant that defends against the race: hard-kill
//! is checked INSIDE the BEGIN IMMEDIATE tx that writes the fire row, so
//! once hard_killed_at is set on watch_sentinels, no fire-write can
//! commit. T21 verifies this property directly by pre-staging the
//! hard-kill marker before calling write_fire_row.

use std::sync::Arc;

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;

/// T21: hard-kill set on a sentinel BEFORE write_fire_row is called must
///      result in (a) Err("hard_killed_race") returned, (b) no row in
///      watch_fires. Restructured from the plan's barrier-race shape —
///      that formulation tested TOCTOU between an in-process is_blocked
///      check and the insert, but the OCC inside the same BEGIN IMMEDIATE
///      tx makes that race impossible by construction. We test the
///      stronger property the OCC enforces: hard-kill committed → no fire
///      row can commit, regardless of any race ordering.
#[tokio::test]
async fn t21_hard_kill_blocks_fire_row_under_occ() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::{Escalation, SentinelState, Urgency};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Pre-stage the hard-kill marker for ("test", "flaky-test"). This is
    // the post-race state — what the OCC check inside insert_fire will see.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, hard_killed_at)
             VALUES ('flaky-test', 'test', 'fast', 0, '{}', 1, ?1)",
            rusqlite::params![1_000_000_000_000_i64],
        )
        .unwrap();
    }

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    let esc = Escalation {
        state: SentinelState {
            tenant: "test".into(),
            sentinel: "flaky-test".into(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        },
        reason: "would-fire-but-hard-killed".into(),
        urgency: Urgency::Low,
    };

    // ASSERT (a): write_fire_row returns Err with the hard-kill marker.
    let result = q.write_fire_row(esc).await;
    assert!(
        matches!(&result, Err(e) if e == "hard_killed_race"),
        "expected Err(\"hard_killed_race\"); got {:?}",
        result
    );

    // ASSERT (b): no row landed in watch_fires.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM watch_fires", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "OCC must have suppressed the fire-row INSERT");
}

/// T21 positive control: no hard-kill marker → write_fire_row succeeds and
/// the fire-row IS inserted. Catches the failure mode where the OCC check
/// accidentally suppresses every insert (the inverse bug of the race).
#[tokio::test]
async fn t21b_no_hard_kill_allows_fire_row() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::{Escalation, SentinelState, Urgency};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    let esc = Escalation {
        state: SentinelState {
            tenant: "test".into(),
            sentinel: "healthy".into(),
            observed_at: 1,
            payload: serde_json::Value::Null,
        },
        reason: "ok".into(),
        urgency: Urgency::Low,
    };

    let id = q.write_fire_row(esc).await.expect("should insert");
    assert!(id > 0, "expected non-zero fire-row id");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM watch_fires", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "fire-row should be in watch_fires");
}

// --- T4 + T5a: in-memory quarantine state machine (quarantine-state) ---

use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
use std::time::Duration;

/// T4: cooldown — fire at t=0, drop at t=2s (cooldown 5s), fire at t=6s.
#[tokio::test]
async fn t4_cooldown_drop_then_fire() {
    let cfg = QuarantineConfig::test_with_cooldown(Duration::from_secs(5));
    let q = QuarantineState::new_in_memory(cfg);

    q.note_fire("sovereign", "s1").await;
    assert!(
        q.in_cooldown("sovereign", "s1", Duration::from_secs(2))
            .await
    );
    assert!(
        !q.in_cooldown("sovereign", "s1", Duration::from_secs(6))
            .await
    );
}

/// T5a (subset — full machine in the full state machine): backoff doubling on consecutive failures.
#[tokio::test]
async fn t5a_backoff_doubling() {
    let cfg = QuarantineConfig::default();
    let q = QuarantineState::new_in_memory(cfg);

    // 2 consecutive failures → cycle 0 (60s).
    q.record_failure("sovereign", "flaky").await;
    q.record_failure("sovereign", "flaky").await;
    let r = q
        .get_state("sovereign", "flaky")
        .await
        .expect("quarantine row exists");
    assert_eq!(r.cycle_count, 0);
    assert_eq!(r.duration_ms, 60_000);

    // simulate quarantine expiry + 2 more failures → cycle 1 (5min).
    q.test_advance_past_quarantine("sovereign", "flaky").await;
    q.record_failure("sovereign", "flaky").await;
    q.record_failure("sovereign", "flaky").await;
    let r = q
        .get_state("sovereign", "flaky")
        .await
        .expect("still quarantined");
    assert_eq!(r.cycle_count, 1);
    assert_eq!(r.duration_ms, 300_000);
}

/// T5b: hysteresis — single success does NOT full-reset.
#[tokio::test]
async fn t5b_single_success_does_not_full_reset() {
    let cfg = QuarantineConfig::default();
    let q = QuarantineState::new_in_memory(cfg);

    q.record_failure("sovereign", "s").await;
    q.record_failure("sovereign", "s").await; // cycle 0
    q.test_advance_past_quarantine("sovereign", "s").await;
    q.record_success("sovereign", "s").await; // 1 success

    let r = q
        .get_state("sovereign", "s")
        .await
        .expect("still quarantined");
    assert_eq!(r.consecutive_successes, 1); // not 3, so no full reset
    assert!(q.get_state("sovereign", "s").await.is_some());
}

/// T5c: hysteresis — N=3 successes AND ≥2× backoff elapsed → full reset.
#[tokio::test]
async fn t5c_three_successes_plus_elapsed_clears_state() {
    let cfg = QuarantineConfig::default();
    let q = QuarantineState::new_in_memory(cfg);

    q.record_failure("sovereign", "s").await;
    q.record_failure("sovereign", "s").await; // cycle 0 (60s)
    q.test_advance_past_quarantine("sovereign", "s").await;

    // Push last_quarantine_end far enough into the past that 2× backoff
    // elapsed (2 * 60_000ms = 120_000ms) is satisfied.
    q.test_set_last_quarantine_end(
        "sovereign",
        "s",
        std::time::Instant::now() - std::time::Duration::from_millis(120_001),
    )
    .await;

    q.record_success("sovereign", "s").await;
    q.record_success("sovereign", "s").await;
    q.record_success("sovereign", "s").await; // 3rd success + elapsed satisfied

    assert!(
        q.get_state("sovereign", "s").await.is_none(),
        "expected full reset after N=3 + 2x backoff"
    );
}

/// T5d: hard-kill at 5 cycles within 1h window.
#[tokio::test]
async fn t5d_hard_kill_at_5_cycles() {
    let cfg = QuarantineConfig::default();
    let q = QuarantineState::new_in_memory(cfg);

    for _ in 0..5 {
        q.record_failure("sovereign", "s").await;
        q.record_failure("sovereign", "s").await;
        q.test_advance_past_quarantine("sovereign", "s").await;
    }

    let r = q.get_state("sovereign", "s").await.expect("row exists");
    assert!(
        r.hard_killed_at.is_some(),
        "expected hard-kill at cycle 5; got cycle_count={}, hard_killed_at={:?}",
        r.cycle_count,
        r.hard_killed_at
    );
}

/// T33.6 P0-2 — runtime hard-kill (5 cycles in 1h window) MUST persist to
/// `watch_sentinels.hard_killed_at`. Without this, restart wipes the
/// hard-kill, OCC stops rejecting fires, and the sentinel silently
/// re-emits audit rows for a now-known-bad source.
///
/// Regression guard: `WatchDb::upsert_hard_kill` had ZERO callers in `src/`;
/// `rec.hard_killed_at = Some(...)` in `quarantine.rs:178` was in-memory
/// only. Cavecrew-investigator confirmed via grep + Read.
#[tokio::test]
async fn t33_6_runtime_hard_kill_persists_to_db() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    // Drive 5 cycles in window (same shape as t5d).
    for _ in 0..5 {
        q.record_failure("sovereign", "s").await;
        q.record_failure("sovereign", "s").await;
        q.test_advance_past_quarantine("sovereign", "s").await;
    }

    let r = q.get_state("sovereign", "s").await.expect("row exists");
    assert!(
        r.hard_killed_at.is_some(),
        "precondition: in-memory hard-kill should fire at cycle 5; got cycle_count={} hard_killed_at={:?}",
        r.cycle_count,
        r.hard_killed_at
    );

    // The actual property under test: DB-side persistence.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let dbside: Option<i64> = conn
        .query_row(
            "SELECT hard_killed_at FROM watch_sentinels
             WHERE tenant=?1 AND name=?2",
            rusqlite::params!["sovereign", "s"],
            |r| r.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten();
    assert!(
        dbside.is_some(),
        "watch_sentinels.hard_killed_at MUST be NOT NULL after runtime hard-kill — got {dbside:?}; runtime hard-kill is in-memory only and won't survive a restart"
    );
}

/// T33.6 P0-2 — after runtime hard-kill, recreating `QuarantineState` (e.g.,
/// process restart) MUST keep the OCC gate engaged: `insert_fire` returns
/// `Err("hard_killed_race")` because `watch_sentinels.hard_killed_at` is
/// still set on disk.
///
/// This is the durability claim: the wall-line "hard_killed sentinels stay
/// dead across restart" hinges on the DB persistence T33.6 introduces.
#[tokio::test]
async fn t33_6_hard_kill_survives_state_recreation() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::{Escalation, SentinelState, Urgency};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Phase 1 — drive runtime hard-kill via record_failure.
    {
        let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
        for _ in 0..5 {
            q.record_failure("sovereign", "s").await;
            q.record_failure("sovereign", "s").await;
            q.test_advance_past_quarantine("sovereign", "s").await;
        }
        assert!(
            q.get_state("sovereign", "s")
                .await
                .unwrap()
                .hard_killed_at
                .is_some(),
            "precondition: in-memory hard-kill set"
        );
    } // QuarantineState dropped, in-memory state gone. DB must carry the state.

    // Phase 2 — fresh QuarantineState, same DB. Fire MUST be rejected by OCC.
    let q2 = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    let esc = Escalation {
        state: SentinelState {
            tenant: "sovereign".into(),
            sentinel: "s".into(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        },
        reason: "post-restart fire should be rejected".into(),
        urgency: Urgency::Low,
    };
    let result = q2.write_fire_row(esc).await;
    assert!(
        matches!(&result, Err(e) if e == "hard_killed_race"),
        "post-restart insert_fire must hit OCC and return hard_killed_race; got {result:?}. Without DB persistence the OCC sees hard_killed_at=NULL and accepts the fire — sentinel silently un-quarantined across restart."
    );
}

/// T33.7 P1-5 — `watch_sentinels.probation_until` must be hydrated into the
/// in-memory `QuarantineState.records` on boot so `is_blocked()` returns
/// `ProbationLogOnly` and `fire_pipeline` applies the `[PROBATION] ` reason
/// prefix on every scheduled fire during the residual window.
///
/// Regression guard (P1-5, /// independent verification): the DB column has zero readers in `src/`;
/// post-restart `is_blocked` consults memory only and returns `None`;
/// scheduled fires silently lose the prefix until the wall-clock window
/// expires. Audit rows for known-recovering sentinels mix with normal fires.
#[tokio::test]
async fn t33_7_probation_hydrates_from_db_on_boot() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::runtime::QuarantineGate;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Pre-stage a sentinel row with probation_until set 10 min in the future.
    // hard_killed_at is NULL — this is the "admin cleared, probation window
    // active, then process restarted" state.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let probation_until_ms = now_ms + 600_000;
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, probation_until)
             VALUES ('s', 'sovereign', 'polling', 0, '{}', 1, ?1)",
            rusqlite::params![probation_until_ms],
        )
        .unwrap();
    }

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    // Before hydrate — in-memory records map is empty; is_blocked returns
    // None even though the DB has an active probation window.
    let pre_gate = q.is_blocked("sovereign", "s").await;
    assert!(
        pre_gate.is_none(),
        "precondition: is_blocked should be None before hydrate (in-memory empty), got {pre_gate:?}"
    );

    // The actual property under test: after hydrate, is_blocked returns
    // ProbationLogOnly for the row whose probation_until is in the future.
    let hydrated = q.hydrate_probation_from_db().await.unwrap();
    assert_eq!(hydrated, 1, "expected exactly 1 row hydrated");

    let post_gate = q.is_blocked("sovereign", "s").await;
    assert!(
        matches!(post_gate, Some(QuarantineGate::ProbationLogOnly)),
        "post-hydrate is_blocked must return ProbationLogOnly so fire_pipeline applies the [PROBATION] prefix; got {post_gate:?}"
    );
}

/// T33.7 P1-5 — hydrate must skip rows whose `probation_until` is already in
/// the past, AND skip rows that are hard-killed (hard-kill is independently
/// gated by OCC; hydrating both would create a split-brain where is_blocked
/// returns ProbationLogOnly but OCC rejects every fire).
#[tokio::test]
async fn t33_7_probation_hydrate_skips_expired_and_hard_killed() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // Expired probation — must be skipped.
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, probation_until)
             VALUES ('expired', 'sovereign', 'polling', 0, '{}', 1, ?1)",
            rusqlite::params![now_ms - 10_000],
        )
        .unwrap();
        // Hard-killed AND probation set — hydrate must skip (OCC owns the gate).
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled,
                 hard_killed_at, probation_until)
             VALUES ('hardkilled', 'sovereign', 'polling', 0, '{}', 1, ?1, ?2)",
            rusqlite::params![now_ms, now_ms + 600_000],
        )
        .unwrap();
        // Active probation — must be hydrated.
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, probation_until)
             VALUES ('active', 'sovereign', 'polling', 0, '{}', 1, ?1)",
            rusqlite::params![now_ms + 600_000],
        )
        .unwrap();
    }

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    let hydrated = q.hydrate_probation_from_db().await.unwrap();
    assert_eq!(
        hydrated, 1,
        "only the active row should hydrate (expired and hard-killed skipped)"
    );
    assert!(q.is_blocked("sovereign", "active").await.is_some());
    assert!(q.is_blocked("sovereign", "expired").await.is_none());
    // Hard-killed not hydrated (no in-memory record), so is_blocked is None
    // from memory — but a real fire would still be rejected by the DB-side OCC.
    assert!(q.is_blocked("sovereign", "hardkilled").await.is_none());
}

/// T5e: probation state after admin-clear (Grok EB).
#[tokio::test]
async fn t5e_probation_window_after_admin_clear() {
    let cfg = QuarantineConfig::default();
    let q = QuarantineState::new_in_memory(cfg);

    // Hard-kill the sentinel.
    for _ in 0..5 {
        q.record_failure("sovereign", "s").await;
        q.record_failure("sovereign", "s").await;
        q.test_advance_past_quarantine("sovereign", "s").await;
    }
    assert!(q
        .get_state("sovereign", "s")
        .await
        .unwrap()
        .hard_killed_at
        .is_some());

    // Admin clears hard-kill — sentinel enters probation.
    let _ = q.admin_clear_quarantine("sovereign", "s", false).await;

    let r = q
        .get_state("sovereign", "s")
        .await
        .expect("row still exists");
    assert!(r.probation_until.is_some(), "expected probation_until set");
    assert!(
        r.hard_killed_at.is_none(),
        "expected hard_killed_at cleared"
    );
    // Verify the gate now returns ProbationLogOnly, not HardKilled.
    use gateway_sidecar::watch::runtime::QuarantineGate;
    let gate = q.is_blocked("sovereign", "s").await;
    assert!(matches!(gate, Some(QuarantineGate::ProbationLogOnly)));
}

/// T33.P0-B (review) — `watch_sentinels.hard_killed_at` MUST
/// be hydrated into the in-memory `QuarantineState.records` on boot so
/// `is_blocked()` returns `HardKilled` at the gate (step 3 of fire_pipeline),
/// not silently `None`. Without this, `runner_loop` post-restart drives
/// `observe()` + `interesting()` + `escalate()` for a known-bad sentinel and
/// only the OCC in `insert_fire` rejects the write — work is wasted, audit
/// rows are touched, and the gate / OCC layers disagree on sentinel status.
///
/// Mirror of T33.7 probation hydrate, with stronger invariant: hard-kill
/// hydrate failure on boot is fail-closed (caller exits 1). Probation
/// hydrate stays log-and-continue (bifurcated hydration policy from
/// the invariant, durability invariant).
#[tokio::test]
async fn t33_p0b_hard_kill_hydrates_from_db_on_boot() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::runtime::QuarantineGate;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Pre-stage a sentinel row with hard_killed_at set. This is the "runtime
    // hard-kill fired, then process restarted" state — durable on disk,
    // absent from memory until hydrate runs.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, hard_killed_at)
             VALUES ('s', 'sovereign', 'polling', 0, '{}', 1, ?1)",
            rusqlite::params![now_ms],
        )
        .unwrap();
    }

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    // Before hydrate — is_blocked returns None even though DB has hard-kill.
    let pre_gate = q.is_blocked("sovereign", "s").await;
    assert!(
        pre_gate.is_none(),
        "precondition: is_blocked should be None before hydrate (in-memory empty), got {pre_gate:?}"
    );

    // Property under test: hydrate_hard_kill_from_db reads watch.db and
    // mirrors the hard-kill into in-memory records.
    let hydrated = q.hydrate_hard_kill_from_db().await.unwrap();
    assert_eq!(hydrated, 1, "expected exactly 1 hard-killed row hydrated");

    let post_gate = q.is_blocked("sovereign", "s").await;
    assert!(
        matches!(post_gate, Some(QuarantineGate::HardKilled)),
        "post-hydrate is_blocked must return HardKilled so fire_pipeline gate stops the work before observe/escalate; got {post_gate:?}"
    );
}

/// T33.P0-B — hydrate must skip rows whose `hard_killed_at IS NULL` (the
/// only rows it cares about are durable hard-kills; probation rows are
/// handled by the separate `hydrate_probation_from_db` path).
#[tokio::test]
async fn t33_p0b_hard_kill_hydrate_skips_non_killed() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // Healthy sentinel — must be skipped.
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled)
             VALUES ('healthy', 'sovereign', 'polling', 0, '{}', 1)",
            rusqlite::params![],
        )
        .unwrap();
        // Probation-only (no hard-kill) — must be skipped by the hard-kill
        // hydrate (the probation hydrate path picks this row up).
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, probation_until)
             VALUES ('probation', 'sovereign', 'polling', 0, '{}', 1, ?1)",
            rusqlite::params![now_ms + 600_000],
        )
        .unwrap();
        // Hard-killed — must be hydrated.
        conn.execute(
            "INSERT INTO watch_sentinels
                (name, tenant, tier, cooldown_ms, config_json, enabled, hard_killed_at)
             VALUES ('killed', 'sovereign', 'polling', 0, '{}', 1, ?1)",
            rusqlite::params![now_ms],
        )
        .unwrap();
    }

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    let hydrated = q.hydrate_hard_kill_from_db().await.unwrap();
    assert_eq!(
        hydrated, 1,
        "only the hard-killed row should hydrate (healthy and probation-only skipped)"
    );
    use gateway_sidecar::watch::runtime::QuarantineGate;
    assert!(matches!(
        q.is_blocked("sovereign", "killed").await,
        Some(QuarantineGate::HardKilled)
    ));
    assert!(q.is_blocked("sovereign", "healthy").await.is_none());
    // Probation row is not hydrated by hard-kill path; is_blocked stays None
    // until the probation hydrate path runs separately.
    assert!(q.is_blocked("sovereign", "probation").await.is_none());
}

/// T33.P0-A — `record_failure` and `admin_clear_quarantine` running
/// concurrently on the same (tenant, sentinel) at the hard-kill threshold
/// must not produce a DB-vs-memory split-brain.
///
/// Regression guard (P0-A, ):
/// both paths take phase-1 inspect, release the in-memory lock across the
/// SQLite await, then phase-3 mirror. The cross-await release lets the
/// admin clear land BETWEEN record_failure's DB hard-kill upsert and its
/// in-memory mirror — final state: DB cleared, memory still hard-killed
/// (or the reverse, depending on interleaving).
///
/// Pattern (named-key serialization lane, cf. Kafka partition locks):
/// per-(tenant, name) `op_lock` acquired for the FULL phase1-3 span on
/// both paths. Race becomes impossible by construction.
///
/// Test runs the race in a loop on a multi-thread runtime to surface the
/// race window probabilistically — even one split-brain outcome across the
/// loop is a defect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t33r_record_failure_racing_admin_clear_no_split_brain() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    for iter in 0..50 {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("watch.db");
        let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
        db.run_migrations().await.unwrap();

        let q = Arc::new(QuarantineState::new_with_db(
            QuarantineConfig::default(),
            db.clone(),
        ));

        // Drive 4 cycles deterministically. On the 5th cycle's 2nd
        // record_failure (raced below), `consecutive_fails` crosses
        // `fails_to_trigger = 2` AND `cycle_count + 1 >= 5`, triggering
        // hard-kill — same shape as `t5d_hard_kill_at_5_cycles`.
        for _ in 0..4 {
            q.record_failure("sovereign", "s").await;
            q.record_failure("sovereign", "s").await;
            q.test_advance_past_quarantine("sovereign", "s").await;
        }
        // First fail of cycle 5 — does not cross fails_to_trigger=2.
        q.record_failure("sovereign", "s").await;

        // RACE: hard-kill-triggering record_failure ⋈ admin_clear_quarantine.
        let q1 = q.clone();
        let q2 = q.clone();
        let (_, ac) = tokio::join!(
            async move { q1.record_failure("sovereign", "s").await },
            async move {
                q2.admin_clear_quarantine("sovereign", "s", false)
                    .await
                    .unwrap()
            },
        );
        let _ = ac;

        // Invariant: after both ops complete, the DB column and the
        // in-memory record MUST agree on hard-kill state. Split-brain
        // either direction is the defect.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let db_hard_killed: Option<i64> = conn
            .query_row(
                "SELECT hard_killed_at FROM watch_sentinels
                 WHERE tenant=?1 AND name=?2",
                rusqlite::params!["sovereign", "s"],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten();
        let mem_hard_killed = q
            .get_state("sovereign", "s")
            .await
            .and_then(|r| r.hard_killed_at)
            .is_some();
        assert_eq!(
            db_hard_killed.is_some(),
            mem_hard_killed,
            "iter {iter}: split-brain after race — db.hard_killed_at = {db_hard_killed:?}, mem.hard_killed_at.is_some() = {mem_hard_killed}"
        );
    }
}

/// T33.P1-A — `is_blocked` MUST stamp `last_quarantine_end` once on natural
/// expiry so `record_success`'s hysteresis check can engage.
///
/// Natural-expiry regression guard:
/// `record_success`'s hysteresis gate is `consecutive_successes >= N AND
/// last_quarantine_end.elapsed() >= 2× duration_ms`. The stamp is set ONLY
/// by `test_advance_past_quarantine` (test-only) and admin_clear paths. A
/// sentinel whose quarantine window expires naturally (clock runs past
/// `quarantined_until` without an admin clear) has `last_quarantine_end =
/// None` forever, so hysteresis never engages — three successes do
/// nothing, the record orphans, and the next `record_failure` advances
/// the cycle count instead of starting fresh. The wall-line "thought is
/// rare → action is final" depends on quarantine actually clearing under
/// recovery; without this fix, recovery is impossible without admin
/// intervention even when the sentinel is healthy.
///
/// Fix: upgrade `is_blocked` to mut lock; when `quarantined_until <= now`
/// AND `last_quarantine_end.is_none()` AND `duration_ms > 0` (record has
/// actually been quarantined at least once), stamp once with `now`.
#[tokio::test]
async fn t33n_natural_expiry_engages_hysteresis() {
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    // Short-backoff config: 10ms quarantine, 3 successes, 2× elapsed.
    // Lets the test drive a real natural-expiry path in ~50ms without
    // any test-only mutators that pre-stamp `last_quarantine_end`.
    let cfg = QuarantineConfig {
        cooldown: std::time::Duration::from_millis(0),
        fails_to_trigger: 2,
        backoff_ms_per_cycle: vec![10, 20, 40, 80],
        hard_kill_after_cycles: 100, // out of reach for this test
        hard_kill_window_ms: 3_600_000,
        hysteresis_successes: 3,
        hysteresis_elapsed_mult: 2,
        probation_ms: 600_000,
    };
    let q = QuarantineState::new_in_memory(cfg);

    // Trigger quarantine: 2nd failure crosses fails_to_trigger=2, sets
    // duration_ms=10 and quarantined_until = now + 10ms.
    q.record_failure("sovereign", "s").await;
    q.record_failure("sovereign", "s").await;

    // Precondition: in-quarantine, hysteresis stamp NOT set yet.
    let pre = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        pre.duration_ms >= 10,
        "precondition: quarantine triggered (duration_ms={})",
        pre.duration_ms,
    );
    assert!(
        pre.last_quarantine_end.is_none(),
        "precondition: last_quarantine_end must be None before natural expiry"
    );

    // Sleep well past the 10ms window — natural expiry.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // is_blocked must (a) return None (window elapsed) AND (b) stamp
    // last_quarantine_end so hysteresis can engage on subsequent successes.
    let blocked = q.is_blocked("sovereign", "s").await;
    assert!(
        blocked.is_none(),
        "post-expiry: is_blocked must return None; got {blocked:?}"
    );
    let post_check = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        post_check.last_quarantine_end.is_some(),
        "is_blocked must stamp last_quarantine_end on natural expiry so hysteresis can engage; got None"
    );

    // Sleep past 2× duration_ms (20ms) since the stamp.
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    // 3 successes + elapsed >= 2× duration_ms → record removed.
    q.record_success("sovereign", "s").await;
    q.record_success("sovereign", "s").await;
    q.record_success("sovereign", "s").await;

    let final_state = q.get_state("sovereign", "s").await;
    assert!(
        final_state.is_none(),
        "after natural expiry + 3 successes + 2× elapsed, hysteresis must clear the record; still present: {final_state:?}"
    );
}

/// T33.P1-A negative control — `is_blocked` MUST NOT stamp
/// `last_quarantine_end` on a record that has never been quarantined
/// (duration_ms == 0). The fresh-record entry that `record_failure` inserts
/// on the very first call (before fails_to_trigger crosses) has
/// `quarantined_until = Instant::now()` (immediately stale) but
/// `duration_ms = 0` — stamping would mark a never-quarantined sentinel
/// as recovering, triggering trivial hysteresis on the next 3 successes.
#[tokio::test]
async fn t33n_natural_expiry_skips_never_quarantined() {
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let q = QuarantineState::new_in_memory(QuarantineConfig::default());

    // Single failure — does not cross fails_to_trigger=2. Record exists
    // with duration_ms=0, quarantined_until = Instant::now() (stale by µs).
    q.record_failure("sovereign", "s").await;
    let pre = q.get_state("sovereign", "s").await.expect("record exists");
    assert_eq!(pre.duration_ms, 0, "precondition: never quarantined");
    assert!(pre.last_quarantine_end.is_none(), "precondition: no stamp");

    let blocked = q.is_blocked("sovereign", "s").await;
    assert!(blocked.is_none(), "never quarantined → not blocked");

    let post = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        post.last_quarantine_end.is_none(),
        "is_blocked must NOT stamp last_quarantine_end on a never-quarantined record (duration_ms=0); got Some"
    );
}

/// T33.P1-D — when `db.upsert_hard_kill` fails inside `record_failure`,
/// `persist_failures_total` MUST increment AND the in-memory record MUST
/// carry `pending_hard_kill_persist = Some(_)` so `is_blocked` can fail
/// closed via the new HardKilled gate. Without this, the safety ladder
/// silently loses ground: cycle threshold crossed, DB persist failed,
/// nothing visible, OCC accepts fires.
///
/// Driving the failure: use a WatchDb opened but NOT migrated — the
/// `watch_sentinels` table doesn't exist, so `upsert_hard_kill`'s INSERT
/// returns `Err("no such table")`. Real production fault scenarios (disk
/// full, IO error, schema mismatch) all surface as the same Err path.
#[tokio::test]
async fn t33_p1d_persist_failure_sets_pending_and_bumps_counter() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    // Intentionally NOT calling run_migrations — schema absent → upsert_hard_kill Err.
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());

    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());
    assert_eq!(q.persist_failures_total(), 0, "precondition: counter at 0");

    // Drive 5 cycles into the hard-kill threshold; on the 5th cycle the
    // persist attempt fires and fails because the schema is absent.
    for _ in 0..5 {
        q.record_failure("sovereign", "s").await;
        q.record_failure("sovereign", "s").await;
        q.test_advance_past_quarantine("sovereign", "s").await;
    }

    assert!(
        q.persist_failures_total() >= 1,
        "persist_failures_total must increment on upsert_hard_kill Err; got {}",
        q.persist_failures_total()
    );

    let rec = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        rec.pending_hard_kill_persist.is_some(),
        "pending_hard_kill_persist MUST be Some after persist failure; got None"
    );
    assert!(
        rec.hard_killed_at.is_none(),
        "hard_killed_at MUST stay None when persist failed (DB-first invariant)"
    );
}

/// T33.P1-D — a record with `pending_hard_kill_persist = Some(_)` MUST
/// be treated as `HardKilled` by `is_blocked`. Fail-closed safety ladder.
#[tokio::test]
async fn t33_p1d_pending_persist_blocks_via_is_blocked() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
    use gateway_sidecar::watch::runtime::QuarantineGate;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    for _ in 0..5 {
        q.record_failure("sovereign", "s").await;
        q.record_failure("sovereign", "s").await;
        q.test_advance_past_quarantine("sovereign", "s").await;
    }

    let blocked = q.is_blocked("sovereign", "s").await;
    assert!(
        matches!(blocked, Some(QuarantineGate::HardKilled)),
        "pending_hard_kill_persist must surface as HardKilled in is_blocked; got {blocked:?}"
    );
}

// T33.P1-D's `t33_p1d_record_success_clears_pending` removed in T33.P0.1:
// it encoded the wrong invariant. The corrected invariant is
// "record_success MUST NOT clear pending" — pending is the in-process
// fail-closed flag for "DB upsert failed", and clearing it on a successful
// tick silently drops the safety ladder. See
// `t33_p01_record_success_does_not_clear_pending_hard_kill_persist` for the
// corrected invariant.

/// T33.P1-D — `admin_clear_quarantine` clears `pending_hard_kill_persist`.
/// Operator override resolves the limbo even when the DB persist still
/// hasn't succeeded — admin clear UPDATEs `hard_killed_at = NULL`
/// (phase 2 of admin_clear), making the pending limbo logically void.
#[tokio::test]
async fn t33_p1d_admin_clear_clears_pending() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    // Migrate THIS time — admin_clear must succeed (it needs schema). The
    // pending state is planted via direct field write since here the
    // persist would have succeeded under normal flow.
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    // Seed a record. Then poison hard_killed_at to None + plant pending
    // directly by driving through the failure path.
    q.record_failure("sovereign", "s").await; // creates record, consecutive_fails=1
    q.test_advance_past_quarantine("sovereign", "s").await;
    // Force the pending state by toggling DB to fail then driving the
    // hard-kill cycle. Easiest: open a separate un-migrated DB just for
    // this state-induction.
    drop(q);

    let tmp2 = tempfile::tempdir().unwrap();
    let db_path2 = tmp2.path().join("watch.db");
    let db2 = std::sync::Arc::new(WatchDb::open(&db_path2).await.unwrap()); // no migrations
    let q2 = QuarantineState::new_with_db(QuarantineConfig::default(), db2.clone());
    for _ in 0..5 {
        q2.record_failure("sovereign", "s").await;
        q2.record_failure("sovereign", "s").await;
        q2.test_advance_past_quarantine("sovereign", "s").await;
    }
    let pre = q2.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        pre.pending_hard_kill_persist.is_some(),
        "precondition: pending set"
    );

    // Now we need a migrated DB for admin_clear to work. Re-attach to db2 —
    // run_migrations is idempotent over CREATE TABLE IF NOT EXISTS.
    db2.run_migrations().await.unwrap();

    let outcome = q2
        .admin_clear_quarantine("sovereign", "s", false)
        .await
        .expect("admin_clear should succeed against migrated db");
    // Either way: post-call pending must be None.
    let _ = outcome;

    let post = q2.get_state("sovereign", "s").await;
    // Record may have been removed if it had no probation_until + no
    // last_quarantine_end. Either no record OR pending cleared is fine.
    if let Some(rec) = post {
        assert!(
            rec.pending_hard_kill_persist.is_none(),
            "admin_clear must clear pending_hard_kill_persist; still {:?}",
            rec.pending_hard_kill_persist
        );
    }
}

/// T33.P1-D — when `pending_hard_kill_persist.is_some()`, the rolling
/// `cycles_window_start` MUST NOT reset on window expiry. Resetting would
/// paper over the safety-ladder gap: cycle_count → 0 effectively hides
/// the prior 5-in-window from the next hard-kill check, even though the
/// DB never recorded the previous hard-kill. Fail-closed.
#[tokio::test]
async fn t33_p1d_pending_inhibits_window_reset() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    // Short hard_kill_window so we can observe a "post-window" failure
    // without sleeping long enough to break test wall-clock budget.
    // hard_kill_window_ms=100 → after 200ms, the window has elapsed.
    let cfg = QuarantineConfig {
        cooldown: std::time::Duration::from_millis(0),
        fails_to_trigger: 2,
        backoff_ms_per_cycle: vec![1, 2, 4, 8],
        hard_kill_after_cycles: 100, // out of reach so we don't auto-hard-kill
        hard_kill_window_ms: 100,    // window expires fast
        hysteresis_successes: 3,
        hysteresis_elapsed_mult: 2,
        probation_ms: 600_000,
    };

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = std::sync::Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations → persist fails
    let q = QuarantineState::new_with_db(cfg, db.clone());

    // Drive into pending state: with hard_kill_after_cycles=100 we won't
    // actually hit hard-kill, so we plant pending via a different path —
    // crank hard_kill_after_cycles down then drive 5 cycles.
    // Re-instantiate with the threshold low enough to fire.
    drop(q);
    let cfg2 = QuarantineConfig {
        cooldown: std::time::Duration::from_millis(0),
        fails_to_trigger: 2,
        backoff_ms_per_cycle: vec![1, 2, 4, 8],
        hard_kill_after_cycles: 5,
        hard_kill_window_ms: 100,
        hysteresis_successes: 3,
        hysteresis_elapsed_mult: 2,
        probation_ms: 600_000,
    };
    let q = QuarantineState::new_with_db(cfg2, db.clone());
    for _ in 0..5 {
        q.record_failure("sovereign", "s").await;
        q.record_failure("sovereign", "s").await;
        q.test_advance_past_quarantine("sovereign", "s").await;
    }
    let pre = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        pre.pending_hard_kill_persist.is_some(),
        "precondition: pending set"
    );
    let cycle_count_before_window_expiry = pre.cycle_count;

    // Wait past hard_kill_window_ms so the next record_failure's "in_window"
    // branch evaluates to false. Without P1-D, the window-reset path would
    // zero cycle_count + cycles_window_start. With P1-D, those mutations
    // are gated on pending.is_none() → no reset.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    q.record_failure("sovereign", "s").await;
    q.record_failure("sovereign", "s").await;

    let post = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        post.pending_hard_kill_persist.is_some(),
        "pending must remain set across the window-expiry record_failure"
    );
    assert!(
        post.cycle_count >= cycle_count_before_window_expiry,
        "cycle_count must NOT reset under pending; was {} now {}",
        cycle_count_before_window_expiry,
        post.cycle_count,
    );
}

/// T33.P0.1 — `record_success` MUST NOT clear `pending_hard_kill_persist`.
///
/// The current `record_success` clears pending
/// unconditionally on a successful tick. Combined with the pipeline
/// ordering bug (observe→interesting→gate), a sentinel whose
/// `interesting()` returns None triggers `FireOutcome::Uninteresting`,
/// `handle_fire_outcome` routes to `record_success`, and pending is
/// silently cleared. The fail-closed safety ladder is lost without any
/// DB persist ever succeeding.
///
/// Pending is the in-process flag that the DB upsert failed; clearing it
/// requires either (a) a successful persist (record_failure's Ok arm) or
/// (b) admin_clear_quarantine. A `record_success` tick is NEITHER.
#[tokio::test]
async fn t33_p01_record_success_does_not_clear_pending_hard_kill_persist() {
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let q = QuarantineState::new_in_memory(QuarantineConfig::test_with_cooldown(
        std::time::Duration::from_millis(0),
    ));
    q.test_set_pending_hard_kill_persist("sovereign", "s", std::time::Instant::now())
        .await;

    q.record_success("sovereign", "s").await;

    let post = q
        .get_state("sovereign", "s")
        .await
        .expect("record must still exist after record_success under pending");
    assert!(
        post.pending_hard_kill_persist.is_some(),
        "record_success must NOT clear pending_hard_kill_persist (fail-closed ladder)"
    );
}

/// T33.P0.1 — `record_success` MUST NOT remove the record (via hysteresis
/// `should_remove`) while `pending_hard_kill_persist` is set. Even if every
/// other recovery criterion is met (consecutive_successes >= threshold AND
/// elapsed >= 2× duration_ms), pending blocks removal — losing the record
/// silently drops the pending flag too, since is_blocked returns None for
/// absent records.
#[tokio::test]
async fn t33_p01_record_success_does_not_remove_record_with_pending() {
    use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};

    let q = QuarantineState::new_in_memory(QuarantineConfig::test_with_cooldown(
        std::time::Duration::from_millis(0),
    ));

    // Seed via natural failure path: drive 2 fails to build a quarantine
    // record with duration_ms > 0. (db=None → record_failure's Ok shortcut,
    // no pending; we add pending manually after.)
    q.record_failure("sovereign", "s").await;
    q.record_failure("sovereign", "s").await;

    // Force the elapsed gate to past so hysteresis can engage.
    q.test_set_last_quarantine_end(
        "sovereign",
        "s",
        std::time::Instant::now() - std::time::Duration::from_secs(120),
    )
    .await;

    // Manually stamp pending — simulates "we'd have hard-killed but DB
    // upsert failed and we left a pending flag." This is the load-bearing
    // fail-closed flag.
    q.test_set_pending_hard_kill_persist("sovereign", "s", std::time::Instant::now())
        .await;

    // Drive enough successes to clear the hysteresis bar.
    for _ in 0..5 {
        q.record_success("sovereign", "s").await;
    }

    let post = q.get_state("sovereign", "s").await;
    assert!(
        post.is_some(),
        "record MUST NOT be removed while pending_hard_kill_persist is set"
    );
    let rec = post.unwrap();
    assert!(
        rec.pending_hard_kill_persist.is_some(),
        "pending must still be set after hysteresis-eligible successes"
    );
}

// ---------------------------------------------------------------------------
// p0a-four-eyes (the dual-custody invariant / Blind spot 3) — distinct-principal
// stage->confirm arming flow. The tests below prove:
//   * same-principal confirm is rejected (the core four-eyes invariant),
//   * two distinct principals arm successfully,
//   * a stale stage past ARM_STAGE_TTL cannot be confirmed,
//   * a confirm bound to the wrong stage_id nonce is rejected (design-review
//     amendment: confirm must ratify the exact stage it intends),
//   * /disarm stays single-principal (fast kill never blocks on a second
//     signature — canary abort table #11).
// ---------------------------------------------------------------------------

async fn four_eyes_fixture() -> (
    tempfile::TempDir,
    Arc<gateway_sidecar::watch::db::WatchDb>,
    Arc<QuarantineState>,
    Arc<gateway_sidecar::watch::api::ArmPrincipals>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(
        gateway_sidecar::watch::db::WatchDb::open(&db_path)
            .await
            .unwrap(),
    );
    db.run_migrations().await.unwrap();
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let principals = Arc::new(gateway_sidecar::watch::api::ArmPrincipals::parse(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
    ));
    (tmp, db, q, principals)
}

async fn stage_id_from(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["stage_id"]
        .as_str()
        .expect("stage response must carry stage_id")
        .to_string()
}

/// dual-custody-local-attest (spec §2/§9): the second custody domain is the
/// ENCLAVE KEY, not a second token — a bearer-only confirm body (no
/// credential fields) must be a 400 and must never arm, even from a second
/// principal. (The retired same-principal 403 is superseded by the attest
/// ceremony tests, where the staging principal CAN confirm with a valid
/// signature.)
#[tokio::test]
async fn test_arm_confirm_bearer_only_body_is_rejected() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{admin_arm_confirm_json, admin_arm_stage_json};

    let (_tmp, db, q, principals) = four_eyes_fixture().await;

    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let stage_id = stage_id_from(stage_resp).await;

    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("alice:tok_alpha_0001".to_string()),
        Some(serde_json::json!({ "stage_id": stage_id })),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        Arc::new(gateway_sidecar::watch::attest::AttestKeyRegistry::unloaded()),
        true,
    )
    .await;
    assert_eq!(
        confirm_resp.status(),
        StatusCode::BAD_REQUEST,
        "bearer-only confirm body must be rejected (400: credential fields required)"
    );

    // Producer must NOT be armed, and no confirm row may exist.
    assert!(
        q.producer_kill_state.lock().is_none(),
        "producer must not be armed by tokens alone (spec §2)"
    );
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        !rows.iter().any(|r| r.action == "confirm"),
        "no confirm row may exist after a bearer-only body; got {rows:?}"
    );
}

/// Stage by alice, confirm by bob -> armed. Audit carries both ceremony
/// halves with the correct principals.
#[tokio::test]
async fn test_arm_two_principals_happy_path() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{
        admin_arm_confirm_json, admin_arm_stage_json, admin_disarm_producer_json,
    };

    let (_tmp, db, q, principals) = four_eyes_fixture().await;

    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(stage_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["staged_by"], "alice");
    assert!(v["expires_in_ms"].as_i64().unwrap() > 0);
    let stage_id = v["stage_id"].as_str().unwrap().to_string();
    let challenge = arm_attest_common::b64d(v["challenge"].as_str().unwrap());

    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(confirm_resp.status(), StatusCode::OK);

    assert!(
        q.producer_kill_state.lock().is_some(),
        "producer_kill_state must be Some after a confirmed arm"
    );

    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.action == "stage" && r.principal == "alice"),
        "audit must record stage by alice; got {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.action == "confirm" && r.principal == "bob"),
        "audit must record confirm by bob; got {rows:?}"
    );

    // Cleanup: disarm so the spawned cdc_sweep_loop drains.
    let disarm_resp = admin_disarm_producer_json(
        q.clone(),
        "unused_admin_token".to_string(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;
    assert_eq!(disarm_resp.status(), StatusCode::OK);
}

/// A stage older than its TTL cannot be confirmed — 410 Gone, not armed.
#[tokio::test]
async fn test_arm_stage_ttl_expiry() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{admin_arm_confirm_json, admin_arm_stage_json};

    let (_tmp, _db, q, principals) = four_eyes_fixture().await;

    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(20),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let (stage_id, challenge) = arm_attest_common::stage_fields(stage_resp).await;

    tokio::time::sleep(Duration::from_millis(60)).await;

    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(
        confirm_resp.status(),
        StatusCode::GONE,
        "expired stage must be 410 Gone"
    );
    assert!(
        q.producer_kill_state.lock().is_none(),
        "producer must not be armed via an expired stage"
    );
}

/// Design-review amendment — confirm must present the stage_id nonce echoed
/// at stage time; a mismatched nonce is rejected so a confirm can never
/// ratify a different/older stage than the one intended.
#[tokio::test]
async fn test_arm_confirm_rejects_mismatched_stage_id() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{
        admin_arm_confirm_json, admin_arm_stage_json, admin_disarm_producer_json,
    };

    let (_tmp, db, q, principals) = four_eyes_fixture().await;

    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let (stage_id, challenge) = arm_attest_common::stage_fields(stage_resp).await;

    // Wrong nonce -> 409, not armed, stage NOT consumed.
    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(
            "deadbeefdeadbeefdeadbeefdeadbeef",
            &challenge,
        )),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(
        confirm_resp.status(),
        StatusCode::CONFLICT,
        "mismatched stage_id must be rejected"
    );
    assert!(q.producer_kill_state.lock().is_none());
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.action == "confirm_rejected" && r.principal == "bob"),
        "audit must record the mismatched-nonce rejection; got {rows:?}"
    );

    // The correct nonce still works afterwards — a mismatch must not
    // destroy the legitimate stage.
    let confirm_ok = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(confirm_ok.status(), StatusCode::OK);

    // Cleanup.
    let disarm_resp = admin_disarm_producer_json(
        q.clone(),
        "unused_admin_token".to_string(),
        principals.clone(),
        Some("alice:tok_alpha_0001".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;
    assert_eq!(disarm_resp.status(), StatusCode::OK);
}

/// /disarm stays single-principal — a kill-switch that requires two humans
/// is a safety regression. One valid principal bearer disarms in one call,
/// and the kill is still audited.
#[tokio::test]
async fn test_disarm_single_principal_still_works() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{
        admin_arm_confirm_json, admin_arm_stage_json, admin_disarm_producer_json,
    };

    let (_tmp, db, q, principals) = four_eyes_fixture().await;

    // Arm via the full two-principal ceremony.
    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let (stage_id, challenge) = arm_attest_common::stage_fields(stage_resp).await;
    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(confirm_resp.status(), StatusCode::OK);
    assert!(q.producer_kill_state.lock().is_some());

    // Disarm with ONE principal bearer — no second signature required.
    let disarm_resp = admin_disarm_producer_json(
        q.clone(),
        "unused_admin_token".to_string(),
        principals.clone(),
        Some("alice:tok_alpha_0001".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;
    assert_eq!(
        disarm_resp.status(),
        StatusCode::OK,
        "single-principal disarm must succeed (fast kill)"
    );
    assert!(q.producer_kill_state.lock().is_none());

    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.action == "disarm" && r.principal == "alice"),
        "audit must record the disarm by alice; got {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// single-writer (single-writer invariant) — single-writer enforced in code via the
// singleton `writer_claim` row (the SQLite advisory-lock equivalent):
//   * a second live writer is refused at the DB level AND at the arm path,
//   * a crashed writer (stale heartbeat) can be taken over,
//   * a live heartbeat keeps the claim indefinitely,
//   * a holder whose heartbeat affects 0 rows self-disarms (fail-closed),
//   * the CHECK(singleton=1) + PK makes a second claim row physically
//     impossible,
//   * the arming-authorization runbook (max-loss formula, canary abort
//     table, DB-unavailable=fail-closed) exists and is CI-enforced.
// Single-writer assumes a single shared watch.db — two sidecars pointed at
// DIFFERENT db files would both believe they are sole writer (declared
// topology, documented in the runbook).
// ---------------------------------------------------------------------------

const TEST_STALE_MS: i64 = 90_000;

async fn writer_claim_fixture() -> (tempfile::TempDir, Arc<gateway_sidecar::watch::db::WatchDb>) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(
        gateway_sidecar::watch::db::WatchDb::open(&db_path)
            .await
            .unwrap(),
    );
    db.run_migrations().await.unwrap();
    (tmp, db)
}

fn real_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// single-writer invariant core proof — instance A holds a FRESH claim; instance B is
/// refused at the DB level, and B's arm path (the four-eyes ceremony in
/// this process) returns 409 and does NOT arm.
#[tokio::test]
async fn test_second_instance_refuses_to_arm() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{admin_arm_confirm_json, admin_arm_stage_json};

    let (_tmp, db, q, principals) = four_eyes_fixture().await;
    let now = real_now_ms();

    // Instance A (a different sidecar process) acquires the writer claim.
    assert!(
        db.try_acquire_writer_claim("instance-a", now, TEST_STALE_MS)
            .await
            .unwrap(),
        "first claim on an empty table must succeed"
    );

    // Instance B (different uuid, A still fresh) is refused at the DB level.
    assert!(
        !db.try_acquire_writer_claim("instance-b", now + 1, TEST_STALE_MS)
            .await
            .unwrap(),
        "a second writer must be refused while the holder's heartbeat is fresh"
    );

    // B's arm path: this process IS the second writer (its process uuid is
    // not "instance-a"), so a full four-eyes ceremony must end in 409 and
    // the producer must NOT be armed.
    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let (stage_id, challenge) = arm_attest_common::stage_fields(stage_resp).await;

    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(
        confirm_resp.status(),
        StatusCode::CONFLICT,
        "arm must be refused (409) while another live writer holds the claim"
    );
    let bytes = axum::body::to_bytes(confirm_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        body.contains("writer"),
        "refusal must name the single-writer claim; got {body}"
    );
    assert!(
        q.producer_kill_state.lock().is_none(),
        "producer must NOT be armed when the writer claim is held elsewhere"
    );

    // The claim is untouched — still instance A's.
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("instance-a"));
}

/// Crash-recovery path — A claims then stops heartbeating; once the claim
/// is past WRITER_CLAIM_STALE_MS, B's acquire matches the stale predicate
/// and takes over.
#[tokio::test]
async fn test_stale_writer_takeover() {
    let (_tmp, db) = writer_claim_fixture().await;
    let t0 = 1_000_000_000i64;

    assert!(db
        .try_acquire_writer_claim("instance-a", t0, TEST_STALE_MS)
        .await
        .unwrap());

    // Exactly AT the stale boundary the claim is still considered live
    // (heartbeat_at_ms < now - stale is strict).
    assert!(
        !db.try_acquire_writer_claim("instance-b", t0 + TEST_STALE_MS, TEST_STALE_MS)
            .await
            .unwrap(),
        "claim exactly at the stale boundary must still refuse"
    );

    // Past the boundary the takeover succeeds.
    assert!(
        db.try_acquire_writer_claim("instance-b", t0 + TEST_STALE_MS + 1, TEST_STALE_MS)
            .await
            .unwrap(),
        "stale claim must be taken over by a new instance"
    );
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("instance-b"));
}

/// Graceful-shutdown lifecycle (smoke fix — CI regression): A acquires,
/// then RELEASES its own claim; a fresh instance B re-acquires IMMEDIATELY
/// (same clock, well inside the stale window) — no 90s stale wait. This is
/// the exact restart-inside-stale-window case the CI smoke hit.
#[tokio::test]
async fn test_release_writer_claim_allows_immediate_reacquire() {
    let (_tmp, db) = writer_claim_fixture().await;
    let t0 = 1_000_000_000i64;

    assert!(db
        .try_acquire_writer_claim("instance-a", t0, TEST_STALE_MS)
        .await
        .unwrap());

    // Before release, a different instance at the SAME clock is refused
    // (the claim is fresh — this is what bricked the smoke).
    assert!(
        !db.try_acquire_writer_claim("instance-b", t0 + 1, TEST_STALE_MS)
            .await
            .unwrap(),
        "B must be refused while A's fresh claim is held"
    );

    // A releases its own claim on graceful shutdown.
    assert!(
        db.release_writer_claim("instance-a").await.unwrap(),
        "release of our own held claim must report a deleted row"
    );
    assert!(
        db.writer_claim_holder().await.unwrap().is_none(),
        "the claim row must be gone after release"
    );

    // B re-acquires immediately — NO stale wait (t0 + 2, far inside STALE).
    assert!(
        db.try_acquire_writer_claim("instance-b", t0 + 2, TEST_STALE_MS)
            .await
            .unwrap(),
        "a fresh instance must re-acquire immediately after a graceful release"
    );
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("instance-b"));
}

/// Fencing-safe release — a DEPOSED instance must NOT release its successor's
/// claim. B has taken over (B's uuid in the row); A's late release with its
/// OWN (stale) uuid matches 0 rows and B's claim survives untouched.
#[tokio::test]
async fn test_release_writer_claim_wrong_uuid_is_noop() {
    let (_tmp, db) = writer_claim_fixture().await;
    let t0 = 1_000_000_000i64;

    // A claims, goes stale, B takes over — the row now carries B's uuid.
    assert!(db
        .try_acquire_writer_claim("instance-a", t0, TEST_STALE_MS)
        .await
        .unwrap());
    assert!(db
        .try_acquire_writer_claim("instance-b", t0 + TEST_STALE_MS + 1, TEST_STALE_MS)
        .await
        .unwrap());

    // A (deposed) issues a graceful release with ITS uuid — fencing guard
    // makes this a no-op; B's claim is untouched.
    assert!(
        !db.release_writer_claim("instance-a").await.unwrap(),
        "releasing with a non-holder uuid must affect 0 rows"
    );
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(
        holder.map(|(u, _)| u).as_deref(),
        Some("instance-b"),
        "the successor's claim must survive a deposed instance's release"
    );

    // Release on an empty table (claim never held) is also a no-op.
    let (_tmp2, db2) = writer_claim_fixture().await;
    assert!(
        !db2.release_writer_claim("ghost").await.unwrap(),
        "release with no claim row must be a no-op, not an error"
    );
}

/// Liveness keeps the lock — A claims and heartbeats; B is repeatedly
/// refused even long after A's ORIGINAL claim time would have gone stale.
#[tokio::test]
async fn test_heartbeat_keeps_claim_fresh() {
    let (_tmp, db) = writer_claim_fixture().await;
    let t0 = 1_000_000_000i64;

    assert!(db
        .try_acquire_writer_claim("instance-a", t0, TEST_STALE_MS)
        .await
        .unwrap());

    // A heartbeats every 30s for 5 ticks (t0+30s .. t0+150s).
    for k in 1..=5i64 {
        assert!(
            db.heartbeat_writer_claim("instance-a", t0 + k * 30_000)
                .await
                .unwrap(),
            "holder's heartbeat must keep affecting 1 row"
        );
        // B is refused after every heartbeat.
        assert!(
            !db.try_acquire_writer_claim("instance-b", t0 + k * 30_000 + 1, TEST_STALE_MS)
                .await
                .unwrap(),
            "B must be refused while A heartbeats (tick {k})"
        );
    }

    // t0+160s is far past t0+STALE (90s), but A's last heartbeat (t0+150s)
    // is only 10s old — B must still be refused. Liveness, not boot time,
    // keeps the lock.
    assert!(!db
        .try_acquire_writer_claim("instance-b", t0 + 160_000, TEST_STALE_MS)
        .await
        .unwrap());
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("instance-a"));
}

/// Fail-closed on lost claim — A arms (claim under this process's uuid),
/// B takes over after the claim goes stale; A's next heartbeat affects
/// 0 rows -> A detects the loss and self-disarms.
#[tokio::test]
async fn test_lost_claim_self_disarms() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{
        admin_arm_confirm_json, admin_arm_stage_json, writer_claim_heartbeat_step,
    };

    let (_tmp, db, q, principals) = four_eyes_fixture().await;

    // Arm via the full ceremony — acquires the writer claim under this
    // process's instance uuid.
    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let (stage_id, challenge) = arm_attest_common::stage_fields(stage_resp).await;
    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(confirm_resp.status(), StatusCode::OK);
    assert!(q.producer_kill_state.lock().is_some());

    let our_uuid = gateway_sidecar::watch::db::process_instance_uuid();
    let now = real_now_ms();
    let stale = gateway_sidecar::watch::db::writer_claim_stale_ms();

    // B takes over after our claim goes stale (simulated future clock).
    assert!(
        db.try_acquire_writer_claim("instance-b", now + stale + 1, stale)
            .await
            .unwrap(),
        "takeover of the stale claim must succeed"
    );

    // A's next heartbeat affects 0 rows -> loss detected -> fail-closed
    // self-disarm (producer drained, kill state cleared, audit row written).
    let still_held = writer_claim_heartbeat_step(&q, &db, our_uuid, now + stale + 2).await;
    assert!(!still_held, "heartbeat after takeover must report loss");
    assert!(
        q.producer_kill_state.lock().is_none(),
        "lost writer claim must self-disarm the producer (fail-closed)"
    );
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        rows.iter().any(|r| r.action == "disarm"
            && r.detail
                .as_deref()
                .unwrap_or("")
                .contains("writer claim lost")),
        "audit must record the self-disarm with its cause; got {rows:?}"
    );
}

/// The physical single-row invariant — CHECK(singleton=1) + PRIMARY KEY
/// reject both a second row (singleton=2) and a duplicate singleton=1.
#[tokio::test]
async fn test_singleton_check_constraint_enforced() {
    let (tmp, db) = writer_claim_fixture().await;
    assert!(db
        .try_acquire_writer_claim("instance-a", 1_000, TEST_STALE_MS)
        .await
        .unwrap());

    // Raw second connection to the same file — attempt to violate the
    // invariant below the application layer.
    let raw = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();

    let second_row = raw.execute(
        "INSERT INTO writer_claim (singleton, instance_uuid, boot_at_ms, heartbeat_at_ms)
         VALUES (2, 'rogue', 1, 1)",
        [],
    );
    let err = format!(
        "{:?}",
        second_row.expect_err("singleton=2 must be rejected")
    );
    assert!(
        err.contains("CHECK") || err.contains("constraint"),
        "singleton=2 must fail the CHECK constraint; got {err}"
    );

    let dup_row = raw.execute(
        "INSERT INTO writer_claim (singleton, instance_uuid, boot_at_ms, heartbeat_at_ms)
         VALUES (1, 'rogue', 1, 1)",
        [],
    );
    let err = format!(
        "{:?}",
        dup_row.expect_err("duplicate singleton=1 must be rejected")
    );
    assert!(
        err.contains("UNIQUE") || err.contains("constraint"),
        "duplicate singleton=1 must fail the PK; got {err}"
    );

    // The legitimate holder is untouched.
    let holder = db.writer_claim_holder().await.unwrap();
    assert_eq!(holder.map(|(u, _)| u).as_deref(), Some("instance-a"));
}

/// The arming runbook must preserve the operator-facing safety contract.
#[test]
fn test_runbook_arming_authorization_exists() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/runbooks/arming-authorization.md");
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("docs/runbooks/arming-authorization.md must exist: {e}"));
    for marker in [
        "max_loss = charge_unit",
        "| Trigger | Action | Who |",
        "DB-unavailable = fail-closed",
        "Signature expiry",
        "Partial-deliberation cost",
    ] {
        assert!(
            body.contains(marker),
            "arming-authorization runbook missing required marker: {marker}"
        );
    }
}

/// a full four-eyes ceremony that
/// ends in a 409 writer-claim refusal must leave a `confirm_rejected` row in
/// the audit chain naming the refusal — a bare staged ceremony with nothing
/// armed and no rejection row reads as "in flight" forever during forensics.
#[tokio::test]
async fn test_confirm_writer_claim_conflict_appends_rejection_audit_row() {
    use axum::http::StatusCode;
    use gateway_sidecar::watch::api::{admin_arm_confirm_json, admin_arm_stage_json};

    let (_tmp, db, q, principals) = four_eyes_fixture().await;
    let now = real_now_ms();

    // A different live instance holds the writer claim.
    assert!(db
        .try_acquire_writer_claim("instance-a", now, TEST_STALE_MS)
        .await
        .unwrap());

    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let (stage_id, challenge) = arm_attest_common::stage_fields(stage_resp).await;

    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(confirm_resp.status(), StatusCode::CONFLICT);

    let rows = db.list_arm_audit().await.unwrap();
    let rejection = rows
        .iter()
        .find(|r| r.action == "confirm_rejected" && r.principal == "bob")
        .unwrap_or_else(|| panic!("409 arm refusal must append confirm_rejected; got {rows:?}"));
    assert!(
        rejection
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("arm_producer_start refused"),
        "rejection detail must name the refusal; got {rejection:?}"
    );
}

/// the writer-claim heartbeat loop
/// actually refreshes heartbeat_at_ms while armed, and exits once the
/// producer is disarmed (so the claim goes stale and another instance can
/// take over). Uses a ms-scale period — no wall-clock minutes involved.
#[tokio::test]
async fn test_writer_claim_heartbeat_loop_refreshes_and_exits_on_disarm() {
    use gateway_sidecar::watch::api::writer_claim_heartbeat_loop;

    let (_tmp, db, q, _principals) = four_eyes_fixture().await;
    let now = real_now_ms();

    // Armed state: kill channels present.
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *q.producer_kill_state.lock() = Some((kill_tx, ack_rx));

    // This process holds the claim under uuid "hb-instance".
    assert!(db
        .try_acquire_writer_claim("hb-instance", now, TEST_STALE_MS)
        .await
        .unwrap());
    let (_, hb0) = db.writer_claim_holder().await.unwrap().unwrap();

    let handle = tokio::spawn(writer_claim_heartbeat_loop(
        q.clone(),
        db.clone(),
        "hb-instance".to_string(),
        Duration::from_millis(20),
        None,
    ));

    // Wait (bounded) for at least one refresh past hb0.
    let mut advanced = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (holder, hb) = db.writer_claim_holder().await.unwrap().unwrap();
        assert_eq!(
            holder, "hb-instance",
            "heartbeat must not change the holder"
        );
        if hb > hb0 {
            advanced = true;
            break;
        }
    }
    assert!(advanced, "heartbeat_at_ms must advance while armed");

    // Disarm — the loop must notice on its next tick and exit.
    *q.producer_kill_state.lock() = None;
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("heartbeat loop must exit after disarm")
        .unwrap();
}

/// Graceful-exit RELEASE (smoke fix — CI regression): when the heartbeat
/// loop exits on DISARM, it releases our writer claim so the row is GONE — a
/// restart inside the stale window re-acquires immediately. Proven via the
/// loop glue (writer_claim_heartbeat_loop) end-to-end, then a fresh-instance
/// re-acquire at the SAME clock with no stale wait.
#[tokio::test]
async fn test_writer_claim_heartbeat_loop_releases_on_disarm_exit() {
    use gateway_sidecar::watch::api::writer_claim_heartbeat_loop;

    let (_tmp, db, q, _principals) = four_eyes_fixture().await;
    let now = real_now_ms();

    // Armed state + our claim under uuid "hb-release".
    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *q.producer_kill_state.lock() = Some((kill_tx, ack_rx));
    assert!(db
        .try_acquire_writer_claim("hb-release", now, TEST_STALE_MS)
        .await
        .unwrap());

    let handle = tokio::spawn(writer_claim_heartbeat_loop(
        q.clone(),
        db.clone(),
        "hb-release".to_string(),
        Duration::from_millis(20),
        None,
    ));

    // Disarm — the loop must release our claim on its graceful exit.
    *q.producer_kill_state.lock() = None;
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("heartbeat loop must exit after disarm")
        .unwrap();

    // The claim row is gone (released), NOT merely left to go stale.
    assert!(
        db.writer_claim_holder().await.unwrap().is_none(),
        "disarm exit must RELEASE the claim, not leave it for the stale window"
    );

    // A fresh instance re-acquires immediately at the same clock — the whole
    // point of the fix (no 90s brick on restart).
    assert!(
        db.try_acquire_writer_claim("hb-successor", now + 1, TEST_STALE_MS)
            .await
            .unwrap(),
        "successor must re-acquire immediately after the graceful release"
    );
}

/// Graceful-exit RELEASE via the SHUTDOWN signal path (runner shutdown):
/// the loop exits on the watch-channel shutdown and releases our claim, same
/// as the disarm path. Covers the second graceful exit branch.
#[tokio::test]
async fn test_writer_claim_heartbeat_loop_releases_on_shutdown_signal() {
    use gateway_sidecar::watch::api::writer_claim_heartbeat_loop;

    let (_tmp, db, q, _principals) = four_eyes_fixture().await;
    let now = real_now_ms();

    let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
    let (_ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *q.producer_kill_state.lock() = Some((kill_tx, ack_rx));
    assert!(db
        .try_acquire_writer_claim("hb-shutdown", now, TEST_STALE_MS)
        .await
        .unwrap());

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(writer_claim_heartbeat_loop(
        q.clone(),
        db.clone(),
        "hb-shutdown".to_string(),
        Duration::from_millis(20),
        Some(shutdown_rx),
    ));

    // Signal graceful shutdown (producer stays armed — this is the runner
    // shutdown path, distinct from disarm).
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("heartbeat loop must exit on shutdown signal")
        .unwrap();

    assert!(
        db.writer_claim_holder().await.unwrap().is_none(),
        "shutdown-signal exit must RELEASE the claim"
    );
    assert!(
        db.try_acquire_writer_claim("hb-successor", now + 1, TEST_STALE_MS)
            .await
            .unwrap(),
        "successor must re-acquire immediately after graceful shutdown release"
    );
}
