// These tests intentionally hold a std mutex across awaits to serialize
// process-global env mutation.
#![allow(clippy::await_holding_lock)]

use axum::http::StatusCode;
use gateway_sidecar::watch::{
    api::admin_disarm_producer_json,
    db::WatchDb,
    quarantine::{QuarantineConfig, QuarantineState},
};
use rusqlite::params;
use std::sync::Arc;

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;

/// Serializes tests that read/write the process-global WATCH_MAX_FANOUT_COST_USD env var,
/// so cargo's parallel test threads can't read inconsistent ceilings mid-run.
static FANOUT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn test_falsification_concurrent_spend_cap() {
    let _env = FANOUT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_concurrent.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // 1. Insert 10 pending escalations to be claimed
    for i in 0..10 {
        db.insert_pending_escalation_with_causal_dedup(
            &format!("resp{}", i),
            "tenant-a",
            "sentinel-x",
            "{}",
            &format!("dig{}", i),
            now_ms,
            0,
        )
        .await
        .unwrap();
    }

    // 2. atomic spend ledger ADAPT: re-seed past spend via the new serialized spend_ledger
    // (settled=49.0 in TODAY's UTC bucket) instead of the old 49.00 directive_outbox row.
    // Force the reservation ceiling to 1.0 so this test keeps its 1-success / 9-reject shape
    // (the cap is 50.0; 49.0 settled leaves room for exactly one 1.0 reservation).
    std::env::set_var("WATCH_MAX_FANOUT_COST_USD", "1.0");
    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 49.0)
         ON CONFLICT(day_bucket) DO UPDATE SET settled_usd = 49.0",
        params![today],
    )
    .unwrap();

    // 3. Spawn 10 concurrent claim tasks
    let mut join_handles = vec![];
    for _ in 0..10 {
        let db_clone = Arc::clone(&db);
        join_handles.push(tokio::spawn(async move {
            db_clone.claim_next_queued_or_failed().await
        }));
    }

    let mut successful_claims = 0;
    let mut rejected_claims = 0;

    for handle in join_handles {
        let result = handle.await.unwrap().unwrap();
        if result.is_some() {
            successful_claims += 1;
        } else {
            rejected_claims += 1;
        }
    }

    std::env::remove_var("WATCH_MAX_FANOUT_COST_USD");

    // settled is 49.0, ceiling reservation is 1.0 -> first claim brings reserved+settled to
    // 50.0 (== cap, allowed). Every other claim would push to 51.0 (> cap) -> refused.
    // In-process concurrency shares one tokio-rusqlite connection so claims serialize on the
    // single writer; the cap decision is deterministic (no SQLITE_BUSY in-process).
    assert_eq!(successful_claims, 1, "Exactly 1 claim should succeed");
    assert_eq!(
        rejected_claims, 9,
        "9 claims should be rejected due to spend cap"
    );
}

/// atomic spend ledger (spend-cap invariant: cap=N-1, assert exactly N-1 commit). Seed the
/// spend_ledger so the day cap allows exactly N-1 reservations of the ceiling; spawn
/// N concurrent claims; assert exactly N-1 Some and 1 None and that the ledger reserved
/// the conservative ceiling N-1 times. In-process claims share one tokio-rusqlite
/// connection -> serialized writer -> exact None count (no SQLITE_BUSY split).
///
/// HONESTY HEADER : the exact-N-1 assertion holds BECAUSE
/// all N writers share one tokio-rusqlite connection (channel-serialized) —
/// acceptable under the single-writer topology single-writer invariant enforces in code.
/// Genuine multi-connection SQLite contention is proven at headroom=1 by the
/// two-phase multiprocess test (`watch_falsification_multiprocess.rs`, real
/// OS processes, busy_timeout=50ms). When citing the ruling :5763 precedent,
/// cite BOTH tests together.
#[tokio::test]
async fn test_atomic_ledger_n_writer_race_cap_n_minus_1() {
    let _env = FANOUT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("WATCH_MAX_FANOUT_COST_USD"); // use the default ceiling deterministically
    const N: usize = 8;
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_nwriter.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Pin the ceiling to a clean 5.0 (the default), and size the cap so exactly N-1 fit.
    let ceiling = gateway_sidecar::watch::db::max_fanout_cost_usd();
    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms);

    // Insert N claimable escalations.
    for i in 0..N {
        db.insert_pending_escalation_with_causal_dedup(
            &format!("resp{}", i),
            "tenant-a",
            "sentinel-x",
            "{}",
            &format!("dig{}", i),
            now_ms,
            0,
        )
        .await
        .unwrap();
    }

    // Seed settled so that DAILY_SPEND_CAP - settled leaves room for exactly (N-1) ceilings.
    // cap is 50.0. settled = 50.0 - (N-1)*ceiling. With N=8, ceiling=5.0 -> settled=15.0,
    // headroom=35.0 = 7 ceilings = N-1. The 8th reservation would hit 50.0 + 5.0 > cap.
    const CAP: f64 = 50.0;
    let settled = CAP - (N as f64 - 1.0) * ceiling;
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, ?2)
         ON CONFLICT(day_bucket) DO UPDATE SET settled_usd = ?2",
        params![today, settled],
    )
    .unwrap();

    let mut join_handles = vec![];
    for _ in 0..N {
        let db_clone = Arc::clone(&db);
        join_handles.push(tokio::spawn(async move {
            db_clone.claim_next_queued_or_failed().await
        }));
    }
    let (mut some, mut none) = (0usize, 0usize);
    for h in join_handles {
        match h.await.unwrap().unwrap() {
            Some(_) => some += 1,
            None => none += 1,
        }
    }
    assert_eq!(some, N - 1, "exactly N-1 claims must commit");
    assert_eq!(none, 1, "exactly 1 claim must be refused at the cap");

    // The ledger must show exactly (N-1) ceiling reservations on top of the seeded settled.
    let reserved: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    let expected = (N as f64 - 1.0) * ceiling;
    assert!(
        (reserved - expected).abs() < 1e-9,
        "reserved={} expected={}",
        reserved,
        expected
    );
}

/// atomic spend ledger: abandoned work must NOT permanently consume budget.
/// (1) claim reserves the ceiling; mark_claim_failed backs it out.
/// (2) re-claim, let the lease expire, sweep_phantom_claims backs it out.
#[tokio::test]
async fn test_ledger_release_on_failed_and_sweep() {
    let _env = FANOUT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("WATCH_MAX_FANOUT_COST_USD");
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_release.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms);
    let ceiling = gateway_sidecar::watch::db::max_fanout_cost_usd();
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    db.insert_pending_escalation_with_causal_dedup(
        "rel1",
        "tenant-a",
        "sentinel-x",
        "{}",
        "digrel1",
        now_ms,
        0,
    )
    .await
    .unwrap();

    // (1) claim -> reserved == ceiling; mark_claim_failed -> reserved back to 0.
    let c1 = db
        .claim_next_queued_or_failed()
        .await
        .unwrap()
        .expect("claim 1");
    let r_after_claim: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        (r_after_claim - ceiling).abs() < 1e-9,
        "claim must reserve the ceiling"
    );

    db.mark_claim_failed(&c1.tenant, &c1.id, "boom", &c1.claim_token)
        .await
        .unwrap();
    let r_after_fail: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        r_after_fail.abs() < 1e-9,
        "mark_claim_failed must release the reservation, got {}",
        r_after_fail
    );

    // (2) re-claim, expire lease, sweep -> reserved back to 0 again.
    // mark_claim_failed set next_retry_at_ms = now+30s (backoff); clear it so the row is
    // immediately re-claimable (simulates backoff elapsed) without sleeping in the test.
    conn.execute(
        "UPDATE pending_escalations SET next_retry_at_ms = NULL WHERE id = ?1",
        params![c1.id],
    )
    .unwrap();
    let c2 = db
        .claim_next_queued_or_failed()
        .await
        .unwrap()
        .expect("claim 2");
    let r_after_claim2: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        (r_after_claim2 - ceiling).abs() < 1e-9,
        "re-claim must reserve the ceiling again"
    );

    conn.execute(
        "UPDATE pending_escalations SET claimed_until_ms = 0 WHERE id = ?1",
        params![c2.id],
    )
    .unwrap();
    let swept = db.sweep_phantom_claims().await.unwrap();
    assert_eq!(swept, 1, "sweep must reclaim the expired row");
    let r_after_sweep: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        r_after_sweep.abs() < 1e-9,
        "sweep must release the reservation, got {}",
        r_after_sweep
    );

    // Idempotency: a second sweep does not drive reserved negative.
    let _ = db.sweep_phantom_claims().await.unwrap();
    let r_double: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(r_double >= 0.0, "double-release must not go negative");
}

/// re-claiming a
/// lease-expired 'claimed' row DIRECTLY via claim_next (NO sweep — the
/// dominant production path at the 1s dispatcher tick) must release the
/// prior reservation inside the claim tx. Before the fix, the second claim
/// reserved a second ceiling and overwrote the stamp, orphaning the first
/// 5.0 in spend_ledger for the rest of the UTC day (10 lease expiries
/// bricked the day's budget). Also proves the in-flight reclaim flag that
/// feeds the lease_expired_during_deliberation counter (p0b P1).
#[tokio::test]
async fn test_stale_reclaim_releases_prior_reservation_and_flags_in_flight() {
    let _env = FANOUT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("WATCH_MAX_FANOUT_COST_USD");
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_stale_reclaim.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms);
    let ceiling = gateway_sidecar::watch::db::max_fanout_cost_usd();
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    db.insert_pending_escalation_with_causal_dedup(
        "stale1",
        "tenant-a",
        "sentinel-x",
        "{}",
        "digstale1",
        now_ms,
        0,
    )
    .await
    .unwrap();

    // A fresh (never-claimed) claim is NOT an in-flight reclaim.
    let c1 = db
        .claim_next_queued_or_failed()
        .await
        .unwrap()
        .expect("claim 1");
    assert!(
        !c1.reclaimed_in_flight,
        "fresh claim must not flag in-flight reclaim"
    );

    // Expire the lease without any settle/fail/sweep (dead dispatcher).
    conn.execute(
        "UPDATE pending_escalations SET claimed_until_ms = 0 WHERE id = ?1",
        params![c1.id],
    )
    .unwrap();

    // Direct re-claim (no sweep). Must NOT double-reserve.
    let c2 = db
        .claim_next_queued_or_failed()
        .await
        .unwrap()
        .expect("stale re-claim");
    assert_eq!(c2.id, c1.id);
    assert_ne!(
        c2.claim_token, c1.claim_token,
        "fresh fencing token on reclaim"
    );
    assert!(
        c2.reclaimed_in_flight,
        "reclaiming an expired REAL in-flight claim (non-empty token, attempts > 0) must flag the orphan-charge recon hint"
    );

    let reserved: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        (reserved - ceiling).abs() < 1e-9,
        "stale reclaim must RELEASE the prior reservation before stamping its own: expected exactly one ceiling ({}), got {} (a doubled value is the day-budget leak)",
        ceiling, reserved
    );

    // The reclaimer's own lifecycle still releases cleanly.
    db.mark_claim_failed(&c2.tenant, &c2.id, "boom", &c2.claim_token)
        .await
        .unwrap();
    let r_final: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        r_final.abs() < 1e-9,
        "no orphaned reservation may remain, got {}",
        r_final
    );
}

/// atomic spend ledger (Q5: UTC day bucket). Spend accounted to bucket D must NOT gate
/// claims accounted to bucket D+1 (the rolling-window double-count hazard is gone).
#[tokio::test]
async fn test_ledger_utc_day_bucket_rollover() {
    let _env = FANOUT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("WATCH_MAX_FANOUT_COST_USD");
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_rollover.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms);
    // The day bucket for ~25h ago is a different string (the previous UTC day),
    // unless we are within the first hour of a UTC day; pick a clearly-prior day.
    let yesterday = gateway_sidecar::watch::db::utc_day_bucket(now_ms - 48 * 3600 * 1000);
    assert_ne!(today, yesterday, "two distinct day buckets");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // Saturate the PRIOR day's bucket to the cap. This must not gate today's claim.
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 50.0)",
        params![yesterday],
    )
    .unwrap();

    db.insert_pending_escalation_with_causal_dedup(
        "roll1",
        "tenant-a",
        "sentinel-x",
        "{}",
        "digroll1",
        now_ms,
        0,
    )
    .await
    .unwrap();

    // Today's bucket is empty -> claim must succeed despite yesterday being maxed out.
    let claimed = db.claim_next_queued_or_failed().await.unwrap();
    assert!(
        claimed.is_some(),
        "a maxed PRIOR-day bucket must not gate a TODAY claim"
    );

    // Today's reservation lands in today's bucket, not yesterday's.
    let r_today: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(r_today > 0.0, "reservation must accrue to today's bucket");
    let r_yest: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![yesterday],
            |r| r.get(0),
        )
        .unwrap();
    assert!(r_yest.abs() < 1e-9, "prior-day bucket reserved untouched");
}

#[tokio::test]
async fn test_falsification_producer_panic_drain_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_panic.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: ambient-transparent arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    // Manually arm producer with a fake channel, then immediately drop the receiver side
    // to simulate a panicked/crashed producer loop that didn't ack.
    let (tx, _rx) = tokio::sync::watch::channel(false);
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    *quarantine.producer_kill_state.lock() = Some((tx, ack_rx));

    // Simulate crash by dropping the ack sender
    drop(ack_tx);

    let disarm_resp = admin_disarm_producer_json(
        Arc::clone(&quarantine),
        "test_admin_token".to_string(),
        Arc::new(gateway_sidecar::watch::api::ArmPrincipals::empty()),
        Some("test_admin_token".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;

    // Admin API should NOT return 200 OK! It must return 500.
    assert_eq!(disarm_resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
