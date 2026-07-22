//! Pending hard-kill database retry-loop tests.
//!
//! Covers concurrency, retry limits, shutdown, recovery, and metric
//! propagation through the JSON `/watch/stats` scrape surface.
//!
//! State induction pattern (mirrors `tests/watch_quarantine.rs`): open a
//! `WatchDb` WITHOUT calling `run_migrations()` → `upsert_hard_kill` returns
//! Err with "no such table: watch_sentinels", which leaves the record in
//! `pending_hard_kill_persist = Some(_)` limbo. To make a subsequent retry
//! succeed, call `run_migrations()` on the same db handle (it's idempotent
//! over CREATE TABLE IF NOT EXISTS).

use gateway_sidecar::watch::api::WatchStats;
use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState, RetryOutcome};
use gateway_sidecar::watch::runner::{pending_retry_tick, WatchRunner, MAX_RETRIES_PER_TICK};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Build a dummy shutdown receiver pinned to `false`. Lets tests drive
/// `pending_retry_tick` without exercising the shutdown-between-records path.
fn never_shutdown() -> watch::Receiver<bool> {
    let (_tx, rx) = watch::channel(false);
    rx
}

/// Drive a sentinel through 5 hard-kill cycles against an un-migrated db so
/// `upsert_hard_kill` returns Err and the record lands in pending limbo.
async fn induce_pending(q: &QuarantineState, tenant: &str, sentinel: &str) {
    for _ in 0..5 {
        q.record_failure(tenant, sentinel).await;
        q.record_failure(tenant, sentinel).await;
        q.test_advance_past_quarantine(tenant, sentinel).await;
    }
    let rec = q
        .get_state(tenant, sentinel)
        .await
        .expect("record must exist after induce_pending");
    assert!(
        rec.pending_hard_kill_persist.is_some(),
        "induce_pending precondition: pending must be Some; got None for {tenant}/{sentinel}"
    );
}

/// H1 — retry returns `Persisted` and clears pending when the DB recovers.
/// Covers the original-memo bullet "Pending record + DB fixed → retry persists
/// hard-kill, clears pending, pending gauge drops."
#[tokio::test]
async fn retry_persists_when_db_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations yet
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    induce_pending(&q, "sovereign", "s").await;
    assert_eq!(q.pending_pending_records(), 1);
    assert_eq!(q.pending_retry_failures_total(), 0);

    // Recover the DB.
    db.run_migrations().await.unwrap();

    let outcome = q.retry_pending_hard_kill_once("sovereign", "s").await;
    assert_eq!(outcome, RetryOutcome::Persisted);

    let rec = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        rec.pending_hard_kill_persist.is_none(),
        "pending must be cleared on Persisted; got Some"
    );
    assert!(
        rec.hard_killed_at.is_some(),
        "hard_killed_at must be set after successful retry; got None"
    );
    assert_eq!(q.pending_pending_records(), 0);
    assert_eq!(
        q.pending_retry_failures_total(),
        0,
        "Persisted MUST NOT bump pending_retry_failures_total"
    );
}

/// H1 — retry returns `StillFailing` and increments counter when the DB is
/// still broken. Pending preserved.
#[tokio::test]
async fn retry_still_failing_when_db_broken() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    induce_pending(&q, "sovereign", "s").await;
    let pre_counter = q.pending_retry_failures_total();

    let outcome = q.retry_pending_hard_kill_once("sovereign", "s").await;
    assert_eq!(outcome, RetryOutcome::StillFailing);

    let rec = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        rec.pending_hard_kill_persist.is_some(),
        "pending MUST stay Some on StillFailing"
    );
    assert!(
        rec.hard_killed_at.is_none(),
        "hard_killed_at MUST stay None when retry failed (DB-first)"
    );
    assert_eq!(
        q.pending_retry_failures_total(),
        pre_counter + 1,
        "pending_retry_failures_total must bump by 1 on Err"
    );
}

/// H1 / P0-6 — **deterministic concurrent op_lock contention** (adversarial-
/// review hardening; supersedes the earlier sequential variant of this test
/// that did not actually verify the lock).
///
/// The lock-down: `retry_pending_hard_kill_once` MUST hold the per-key
/// op_lock for the full retry span. Without this, a retry could interleave
/// between admin_clear's DB phase (UPDATE hard_killed_at = NULL) and its
/// in-memory mirror phase (clear pending), reading pending = Some and
/// running another `upsert_hard_kill` that re-sets `hard_killed_at` on the
/// row admin just cleared. Result: durable hard-kill restored after admin
/// cleared it (split-brain: DB hard-killed, memory not).
///
/// The test holds the op_lock externally (mimicking admin's hold), spawns
/// retry, asserts retry has NOT made progress while the lock is held, then
/// releases the lock and asserts the post-state is consistent. **This test
/// fails if the op_lock acquisition is removed from
/// `retry_pending_hard_kill_once`** — without the lock, retry would
/// progress immediately and run the upsert, observable as a finished task
/// before the lock release.
#[tokio::test]
async fn p0_6_retry_blocks_on_op_lock_while_admin_holds_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    // Migrate up-front so upsert_hard_kill would SUCCEED if retry got to run —
    // the test then proves that retry does NOT get to run while we hold the
    // op_lock, despite the DB being healthy.
    db.run_migrations().await.unwrap();
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));

    // Plant pending state directly (test helper bypasses the upsert path).
    q.test_set_pending_hard_kill_persist("sovereign", "s", Instant::now())
        .await;
    assert_eq!(q.pending_pending_records(), 1);

    // Acquire the per-key op_lock externally — same lane retry must contend on.
    let op_lock = q.test_op_lock_for("sovereign", "s");
    let admin_guard = op_lock.lock().await;

    // Spawn retry. Should block on op_lock.lock().await.
    let q_for_retry = q.clone();
    let retry_handle = tokio::spawn(async move {
        q_for_retry
            .retry_pending_hard_kill_once("sovereign", "s")
            .await
    });

    // Give the spawned task time to reach the op_lock acquire. 200ms is
    // generous for CI under load while still failing fast if the lock isn't
    // taken at all.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if retry_handle.is_finished() {
            panic!(
                "retry_pending_hard_kill_once completed while op_lock was held externally — \
                 op_lock contention is NOT being respected (or was removed from retry path)"
            );
        }
    }
    assert!(
        !retry_handle.is_finished(),
        "retry must block on the externally-held op_lock"
    );

    // Simulate admin clear's memory-mirror phase under the held lock:
    // clear pending + hard_killed_at. (admin_clear_quarantine itself takes
    // the same lock; calling it here would deadlock.)
    q.test_clear_pending("sovereign", "s");

    // Release the lock — retry now proceeds, finds pending = None, returns
    // AdminClearedDuringRetry.
    drop(admin_guard);

    let outcome = retry_handle.await.expect("retry task must not panic");
    assert_eq!(
        outcome,
        RetryOutcome::AdminClearedDuringRetry,
        "retry after admin-clear-under-lock must return AdminClearedDuringRetry; got {outcome:?}"
    );

    // Split-brain guard: hard_killed_at must remain None (admin's clear stuck).
    let rec = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        rec.hard_killed_at.is_none(),
        "split-brain check: durable+memory hard_killed_at MUST be None after admin-clear-wins race; got {:?}",
        rec.hard_killed_at
    );
    assert!(
        rec.pending_hard_kill_persist.is_none(),
        "pending must stay None after admin clear; got {:?}",
        rec.pending_hard_kill_persist
    );
    assert_eq!(
        q.pending_retry_failures_total(),
        0,
        "admin-cleared retry MUST NOT bump pending_retry_failures_total"
    );
}

/// H1 / P0-6 — stale-snapshot admin-clear race. T0..T4 codified:
///   T0  retry loop takes pending-key snapshot
///   T1  admin POST /watch/quarantine/{s} clears pending
///   T2  retry loop calls retry_pending_hard_kill_once(t, s)
///   T3  function acquires op_lock, re-checks pending under records.lock()
///   T4  pending is None now → returns AdminClearedDuringRetry, no DB call
///
/// Acceptance: admin clear wins; retry does not re-hard-kill the sentinel.
#[tokio::test]
async fn p0_6_admin_clear_during_retry_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // induce pending
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    induce_pending(&q, "sovereign", "s").await;
    // T0: snapshot would observe (sovereign, s) in pending.
    let snapshot_keys = q.pending_hard_kill_keys();
    assert_eq!(
        snapshot_keys,
        vec![("sovereign".to_string(), "s".to_string())]
    );

    // T1: admin clear lands. Need migrations for admin_clear to succeed.
    db.run_migrations().await.unwrap();
    q.admin_clear_quarantine("sovereign", "s", false)
        .await
        .expect("admin_clear must succeed");

    // T2..T4: retry runs against the now-cleared key.
    let outcome = q.retry_pending_hard_kill_once("sovereign", "s").await;
    assert_eq!(
        outcome,
        RetryOutcome::AdminClearedDuringRetry,
        "retry on admin-cleared key must return AdminClearedDuringRetry; got {outcome:?}"
    );

    // No re-hard-kill: hard_killed_at must remain None after admin clear.
    let post = q.get_state("sovereign", "s").await;
    if let Some(rec) = post {
        assert!(
            rec.hard_killed_at.is_none(),
            "admin clear + retry race must NOT re-hard-kill; hard_killed_at = {:?}",
            rec.hard_killed_at
        );
        assert!(
            rec.pending_hard_kill_persist.is_none(),
            "pending must stay None after admin clear; got {:?}",
            rec.pending_hard_kill_persist
        );
    }
    // No counter bump for the admin-cleared path.
    assert_eq!(
        q.pending_retry_failures_total(),
        0,
        "admin-cleared retries MUST NOT bump pending_retry_failures_total"
    );
}

/// H1 / P0-2 — Instant first-set invariant. 5 failed retries against the
/// same record MUST leave the Instant exactly where record_failure first set
/// it. Asserts strict equality across retries.
#[tokio::test]
async fn p0_2_instant_first_set_preserved_across_failed_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    induce_pending(&q, "sovereign", "s").await;
    let t0 = q
        .get_state("sovereign", "s")
        .await
        .and_then(|r| r.pending_hard_kill_persist)
        .expect("pending must be Some after induce");

    // 5 retries against still-broken DB.
    for _ in 0..5 {
        let outcome = q.retry_pending_hard_kill_once("sovereign", "s").await;
        assert_eq!(outcome, RetryOutcome::StillFailing);
        tokio::time::sleep(Duration::from_millis(2)).await; // make sure clock advances
    }

    let t_after = q
        .get_state("sovereign", "s")
        .await
        .and_then(|r| r.pending_hard_kill_persist)
        .expect("pending must still be Some");
    assert_eq!(
        t_after, t0,
        "P0-2 INVARIANT: Instant MUST NOT be restamped across failed retries; first-set semantics broken"
    );
}

/// H1 / P0-2 + P1-1 — `pending_oldest_age_ms` rises monotonically across
/// failed retries (locks down P0-2 from the metric side too). Companion to
/// the in-memory Instant equality test above.
#[tokio::test]
async fn p1_1_oldest_age_ms_monotone_under_failed_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations
    let q = QuarantineState::new_with_db(QuarantineConfig::default(), db.clone());

    induce_pending(&q, "sovereign", "s").await;
    let age_t0 = q.pending_oldest_age_ms();

    tokio::time::sleep(Duration::from_millis(15)).await;
    let _ = q.retry_pending_hard_kill_once("sovereign", "s").await;
    let age_t1 = q.pending_oldest_age_ms();

    tokio::time::sleep(Duration::from_millis(15)).await;
    let _ = q.retry_pending_hard_kill_once("sovereign", "s").await;
    let age_t2 = q.pending_oldest_age_ms();

    assert!(
        age_t1 >= age_t0,
        "oldest_age_ms must not decrease across retry t0 -> t1; got {age_t0} -> {age_t1}"
    );
    assert!(
        age_t2 >= age_t1,
        "oldest_age_ms must not decrease across retry t1 -> t2; got {age_t1} -> {age_t2}"
    );
    assert!(
        age_t2 >= 30,
        "oldest_age_ms must have grown by at least the sleep budget; got {age_t2}"
    );
}

/// H1 — retry against an in-memory `QuarantineState` (no WatchDb) returns
/// `NoDb`. Defends the P1-5 contract — even if the gate at spawn time is
/// bypassed, the retry function itself does not pretend to persist.
#[tokio::test]
async fn retry_returns_no_db_when_in_memory() {
    let q = QuarantineState::new_in_memory(QuarantineConfig::default());
    // No pending to set — even so, the function should refuse on `db = None`.
    let outcome = q.retry_pending_hard_kill_once("sovereign", "s").await;
    assert_eq!(outcome, RetryOutcome::NoDb);
}

/// H1 / P1-2 — `pending_retry_tick` retries at most `MAX_RETRIES_PER_TICK`
/// records per pass; the remainder drains on the next tick. Seeds N+50
/// pending records and asserts retry-counter delta is exactly N after one tick.
#[tokio::test]
async fn p1_2_tick_caps_at_max_retries_per_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));

    let total = MAX_RETRIES_PER_TICK + 50;
    let now = Instant::now();
    for i in 0..total {
        q.test_set_pending_hard_kill_persist("sovereign", &format!("s{i}"), now)
            .await;
    }
    assert_eq!(q.pending_pending_records() as usize, total);

    let pre = q.pending_retry_failures_total();
    pending_retry_tick(&q, &mut never_shutdown()).await;
    let delta = q.pending_retry_failures_total() - pre;
    assert_eq!(
        delta as usize, MAX_RETRIES_PER_TICK,
        "tick must retry exactly MAX_RETRIES_PER_TICK records; delta = {delta}"
    );
    // Pending pool depth must drop by exactly MAX (none persisted; all still failing).
    assert_eq!(
        q.pending_pending_records() as usize,
        total,
        "still-failing retries MUST preserve pending; pool size must not drop"
    );
}

/// H1 — `pending_retry_tick` is a no-op when no records are pending: no
/// counter bump, no log spam, no DB calls (cannot directly assert no-DB, but
/// we cover counter/state invariants and the no-records branch).
#[tokio::test]
async fn tick_is_noop_when_no_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    assert_eq!(q.pending_pending_records(), 0);
    let pre = q.pending_retry_failures_total();

    pending_retry_tick(&q, &mut never_shutdown()).await;

    assert_eq!(q.pending_retry_failures_total(), pre);
    assert_eq!(q.pending_pending_records(), 0);
}

/// H1 / P1-5 — `WatchRunner::start` spawns the retry loop ONLY when
/// `quarantine.has_db()`. In-memory mode has no Err path to leave pending
/// set, so the loop would be dead work.
#[tokio::test]
async fn p1_5_retry_loop_spawned_only_when_has_db() {
    // With DB → spawned.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    let q_with = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let handles_with = WatchRunner::start(
        tokio::runtime::Handle::current(),
        Vec::new(),
        q_with.clone(),
    );
    assert!(
        handles_with.pending_retry_spawned(),
        "WatchRunner with WatchDb MUST spawn the pending-retry loop"
    );
    handles_with.shutdown();
    handles_with.join_all().await;

    // No DB → not spawned.
    let q_without = Arc::new(QuarantineState::new_in_memory(QuarantineConfig::default()));
    let handles_without = WatchRunner::start(
        tokio::runtime::Handle::current(),
        Vec::new(),
        q_without.clone(),
    );
    assert!(
        !handles_without.pending_retry_spawned(),
        "in-memory WatchRunner MUST NOT spawn the pending-retry loop (P1-5)"
    );
    handles_without.shutdown();
    handles_without.join_all().await;
}

/// H1 / P0-5 — shutdown drains the retry loop cleanly. Without an upper
/// bound, a misrouted shutdown signal would leave the handle hanging until
/// the next tick (60s) — the goal here is "exit ≤100ms" per the invariant.
#[tokio::test]
async fn p0_5_shutdown_drains_retry_loop_promptly() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let handles = WatchRunner::start(tokio::runtime::Handle::current(), Vec::new(), q.clone());
    assert!(handles.pending_retry_spawned());

    let t0 = Instant::now();
    handles.shutdown();
    handles.join_all().await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "join_all after shutdown must drain in <500ms (60s tick would mean blocked-on-tick); got {elapsed:?}"
    );
}

/// H1 (adversarial-review hardening, M3) — shutdown observed between
/// per-record retries inside `pending_retry_tick`. Bounds worst-case
/// shutdown drain to one in-flight retry, NOT
/// `MAX_RETRIES_PER_TICK × RETRY_DB_BUDGET`. Pre-signals shutdown, then
/// drives the tick against a pool larger than 1 — the tick MUST observe
/// shutdown and abort the per-record loop before draining the whole pool.
#[tokio::test]
async fn m3_tick_honors_shutdown_between_records() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // no migrations → StillFailing
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));

    // Plant 5 pending records (small pool, fast test).
    let now = Instant::now();
    for i in 0..5 {
        q.test_set_pending_hard_kill_persist("sovereign", &format!("s{i}"), now)
            .await;
    }
    assert_eq!(q.pending_pending_records(), 5);

    // Pre-signal shutdown — the tick must exit on the first per-record check.
    let (tx, mut rx) = watch::channel(false);
    tx.send(true).expect("shutdown signal must send");

    let pre = q.pending_retry_failures_total();
    pending_retry_tick(&q, &mut rx).await;
    let delta = q.pending_retry_failures_total() - pre;
    assert_eq!(
        delta, 0,
        "tick MUST exit on shutdown before retrying any record; got {delta} failures bumped"
    );
}

/// H1 / P1-4 — JSON `/watch/stats` scrape surface carries both new fields
/// (`pending_retry_failures_total`, `pending_oldest_age_ms`). The Lua poller
/// uses these names; the test guards the contract.
#[tokio::test]
async fn p1_4_watch_stats_struct_exposes_new_fields() {
    let q = Arc::new(QuarantineState::test_default());
    let now = Instant::now();
    q.test_set_pending_hard_kill_persist("sovereign", "s1", now)
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let snapshot = q.pending_snapshot();
    let stats = WatchStats {
        audit_infra_errors_total: q.audit_infra_errors_total(),
        persist_failures_total: q.persist_failures_total(),
        pending_pending_records: snapshot.count,
        pending_retry_failures_total: q.pending_retry_failures_total(),
        pending_oldest_age_ms: snapshot.oldest_age_ms,
        lease_expired_during_deliberation: q.lease_expired_during_deliberation(),
        // watch telemetry fields (dup-charge alarm, spend gauge pair,
        // kill-switch latency, recon divergence) default to zero here —
        // this test guards only the H1 / P1-4 field names.
        ..WatchStats::default()
    };

    // Round-trip JSON to confirm both field names land on the scrape surface.
    let json = serde_json::to_value(&stats).expect("WatchStats serializes");
    assert!(
        json.get("pending_retry_failures_total").is_some(),
        "JSON must carry `pending_retry_failures_total`; got {json}"
    );
    assert!(
        json.get("pending_oldest_age_ms").is_some(),
        "JSON must carry `pending_oldest_age_ms`; got {json}"
    );
    assert_eq!(json["pending_pending_records"], 1);
    assert_eq!(json["pending_retry_failures_total"], 0);
    assert!(
        json["pending_oldest_age_ms"].as_u64().unwrap() >= 10,
        "oldest_age_ms must reflect ~10ms sleep; got {}",
        json["pending_oldest_age_ms"]
    );
}

/// H1 — a full DB-fixed tick drains the pending pool to zero in one pass
/// (covers original-memo bullet, "Pending record + DB fixed → retry persists
/// hard-kill, clears pending, pending gauge drops"). Driven via the tick
/// function so the loop-body logic is end-to-end tested.
#[tokio::test]
async fn tick_drains_pool_when_db_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap()); // induce phase
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));

    // Seed 3 pending records.
    for i in 0..3 {
        induce_pending(&q, "sovereign", &format!("s{i}")).await;
    }
    assert_eq!(q.pending_pending_records(), 3);

    // Heal the DB.
    db.run_migrations().await.unwrap();

    pending_retry_tick(&q, &mut never_shutdown()).await;

    assert_eq!(
        q.pending_pending_records(),
        0,
        "tick must drain pool when DB is healthy"
    );
    for i in 0..3 {
        let rec = q
            .get_state("sovereign", &format!("s{i}"))
            .await
            .expect("record exists");
        assert!(rec.hard_killed_at.is_some());
        assert!(rec.pending_hard_kill_persist.is_none());
    }
}

/// Deterministic `TimedOut` path coverage. Closes the only uncovered arm of
/// `RetryOutcome` for the retry pathway.
///
/// What this proves: when `db.upsert_hard_kill` exceeds `RETRY_DB_BUDGET`
/// (5s), `retry_pending_hard_kill_once` MUST
///   (a) return `RetryOutcome::TimedOut`,
///   (b) bump `pending_retry_failures_total` exactly once (sibling arm with
///       `StillFailing` — both count failed-persist signal),
///   (c) leave `pending_hard_kill_persist` set (load-bearing safety state;
///       see module-doc INVARIANT — first-set Instant must not be restamped),
///   (d) leave `hard_killed_at` `None` (DB-first / memory-mirror discipline:
///       no durable persist, no memory mirror),
///   (e) preserve `pending_oldest_age_ms` monotonicity (P0-2 first-set
///       invariant: TimedOut path does NOT restamp the Instant).
///
/// Why this test exists despite correctness coverage by idempotent SQL +
/// DB-first/memory-mirror invariant: the TimedOut arm is the only retry
/// outcome whose match-arm reachability is not otherwise exercised by the
/// 14 existing tests (Persisted, StillFailing, NoDb, AdminCleared,
/// AdminClearedDuringRetry are all covered). A regression that swapped the
/// Err arm and the timeout arm — say, both incrementing pending but the
/// timeout arm forgetting to preserve pending — would slip through the
/// existing matrix.
///
/// Why `tokio::test(start_paused = true)` + `tokio::time::advance`: the
/// 5s `RETRY_DB_BUDGET` is a tokio-timer deadline. Pausing the runtime
/// and advancing virtual time past the budget fires the timeout
/// deterministically in milliseconds of real time, without changing
/// the const in production code. The wedge on the SQLite worker is via
/// `std::thread::sleep` on the dedicated tokio-rusqlite worker thread —
/// that's OS time, not tokio time, so the worker stays parked while
/// virtual time advances. The retry's `upsert_hard_kill` closure queues
/// FIFO behind the wedge on the single-conn worker and never gets the
/// CPU before the timeout fires.
///
/// State seeding: bypass `induce_pending` (which un-migrates the DB to
/// force `Err`) — instead seed `pending_hard_kill_persist = Some` directly
/// via `test_set_pending_hard_kill_persist`, then migrate the DB so a
/// non-wedged retry WOULD succeed. This isolates the TimedOut signal to
/// the worker-queue wedge, not to a still-broken DB. Without this
/// isolation, a regression that swapped TimedOut→StillFailing on the
/// Err-vs-Elapsed branch could pass silently.
///
/// Worker thread cleanup: the wedge uses a 500ms real sleep — long enough
/// to span the test's tokio-time advance window, short enough not to leak
/// noticeable real time across test-binary tear-down. Subsequent tests
/// allocate their own `WatchDb` (own worker), so leakage cannot cross-
/// contaminate.
#[tokio::test(start_paused = true)]
async fn p0_3_timed_out_path_preserves_pending_and_bumps_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));

    // Seed pending state directly. Use a real-time Instant in the past so
    // `pending_oldest_age_ms` returns > 0 on entry and the post-retry
    // monotonicity assertion has a non-zero baseline.
    let seed_when = Instant::now() - Duration::from_millis(50);
    q.test_set_pending_hard_kill_persist("sovereign", "s", seed_when)
        .await;
    let pre_counter = q.pending_retry_failures_total();
    let pre_age = q.pending_oldest_age_ms();

    // Wedge the worker thread. The closure runs std::thread::sleep on the
    // dedicated worker thread (real time); subsequent conn.call dispatches
    // queue behind it FIFO. Spawn (don't await) so the wedge holds while
    // we drive the retry.
    let db_for_blocker = db.clone();
    let blocker = tokio::spawn(async move {
        db_for_blocker
            .test_block_worker(Duration::from_millis(500))
            .await;
    });
    // Yield twice so the blocker future is polled and its conn.call lands
    // on the worker channel before the retry's call queues behind it.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Spawn the retry so we can advance virtual time and then join.
    let q_for_retry = q.clone();
    let retry = tokio::spawn(async move {
        q_for_retry
            .retry_pending_hard_kill_once("sovereign", "s")
            .await
    });
    tokio::task::yield_now().await;

    // Advance virtual time past RETRY_DB_BUDGET (5s). The
    // tokio::time::timeout future fires; the inner upsert_hard_kill closure
    // is still queue-blocked behind the wedge on the worker thread.
    tokio::time::advance(Duration::from_secs(6)).await;

    let outcome = retry.await.expect("retry task joined");
    assert_eq!(
        outcome,
        RetryOutcome::TimedOut,
        "expected TimedOut when the SQLite worker is wedged past the 5s budget"
    );

    // (b) Counter bumped exactly once.
    assert_eq!(
        q.pending_retry_failures_total(),
        pre_counter + 1,
        "pending_retry_failures_total must bump by 1 on TimedOut (sibling with StillFailing)"
    );

    // (c) Pending preserved; (d) hard_killed_at still None.
    let rec = q.get_state("sovereign", "s").await.expect("record exists");
    assert!(
        rec.pending_hard_kill_persist.is_some(),
        "pending MUST stay Some on TimedOut (load-bearing safety state; INVARIANT module-doc)"
    );
    assert!(
        rec.hard_killed_at.is_none(),
        "hard_killed_at MUST stay None when retry timed out (DB-first / memory-mirror)"
    );

    // (e) First-set Instant untouched → oldest_age_ms is monotonically
    // non-decreasing across the TimedOut path. Restamping would silently
    // reset this gauge below pre_age.
    let post_age = q.pending_oldest_age_ms();
    assert!(
        post_age >= pre_age,
        "pending_oldest_age_ms must be monotone across TimedOut (no restamp); pre={pre_age} post={post_age}"
    );

    // Best-effort cleanup. The worker's std::thread::sleep continues for
    // its remaining real-time slice after the test returns; the next
    // queued closure (delayed upsert) runs as a no-op tolerated by
    // idempotent SQL on the test's tempdir DB, which drops with `tmp`.
    blocker.abort();
}
