use gateway_sidecar::watch::db::WatchDb;
use rusqlite::params;
use std::env;
use std::process::Command;
use std::sync::Arc;

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;

/// atomic spend ledger BEHAVIOR CHANGE (design-review must-fix): this is a rewrite, not a
/// re-seed, of the old multiprocess spend-cap proof. The old test asserted that a crashed
/// worker's claim PERMANENTLY consumed $1.00 (successful_claims==0). The whole point of the
/// p0c ledger RELEASE-on-abandon is to make that budget reclaimable. So we prove two phases
/// across real OS processes whose "crash" is `std::process::exit(99)` — NOT a literal
/// SIGKILL
/// has the same no-settle/no-release crash semantics for this proof — the claim tx
/// committed and no cleanup code runs). Children map a refused/SQLITE_BUSY claim
/// (busy_timeout=50ms) to exit-1 "reject":
///   (a) immediately after the crash, with NO sweep, the crashed worker's reservation still
///       holds the budget -> a fresh claim at the cap is REFUSED.
///   (b) AFTER a sweep tick releases that reservation, exactly ONE more claim gets through
///       (release works cross-process), and a second is refused.
///
/// Determinism: the day cap is 50.0 and we pin WATCH_MAX_FANOUT_COST_USD=5.0 in every process.
/// We seed settled=45.0 so the headroom is exactly one ceiling (5.0). The crash child reserves
/// that last ceiling and dies before settling.
///
/// CITATION RULE : this multiprocess two-phase test and the in-process
/// `test_atomic_ledger_n_writer_race_cap_n_minus_1` (channel-serialized exact-N-1) are a
/// PAIR — cite both together as the ruling :5763 cap-under-concurrency evidence.
// CROWN JEWEL: proves the spend_ledger enforces the daily cap as defense-in-depth BELOW single-writer (concurrent child processes, distinct uuids, one db). Do NOT add a reserve-ownership gate (instance_id==writer_claim.holder) to satisfy the v2 stale-claim smoke — it refuses every child here and collapses this proof. Invariant: caps enforce at ledger, ownership at claim — never collapse the two.
#[tokio::test]
async fn test_falsification_multiprocess_spend_cap() {
    // Pin the ceiling for every process spawned from this binary (parent + children inherit).
    env::set_var("WATCH_MAX_FANOUT_COST_USD", "5.0");

    if env::var("IS_CHILD").is_ok() {
        // --- CHILD PROCESS LOGIC ---
        let db_path = env::var("DB_PATH").unwrap();
        let db = Arc::new(WatchDb::open(std::path::Path::new(&db_path)).await.unwrap());

        // Attested-arm: the reserve re-verifies the arm signature against the boot
        // registry — the child must publish the SAME fixed test registry the
        // parent used to sign the active_arm, else every child fail-closes.
        arm_attest_common::publish_test_boot_registry();

        let claim_res = db.claim_next_queued_or_failed().await;

        // Crash simulation: exit(99) without settle/release (no cleanup runs).
        if env::var("CRASH_AFTER_CLAIM").is_ok()
            && claim_res.is_ok()
            && claim_res.as_ref().unwrap().is_some()
        {
            // Crash before completing the task (before settle/release)!
            std::process::exit(99);
        }

        if let Ok(Some(_)) = claim_res {
            std::process::exit(0); // Success (claimed)
        } else {
            std::process::exit(1); // Rejected/None/Err
        }
    }

    // --- PARENT PROCESS LOGIC ---
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_multiprocess.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    // Attested-arm: the reserve fail-closes without an active_arm; the parent stamps
    // an ambient-transparent ceiling (persisted in the shared db file) so the
    // child processes' claims behave exactly as the legacy spend-cap proof.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Insert several claimable escalations.
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

    // p0c re-seed: seed the serialized spend_ledger so headroom is exactly one 5.0 ceiling.
    // cap=50.0, settled=45.0 -> the crash child's single ceiling reservation lands at the cap.
    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 45.0)",
        params![today],
    )
    .unwrap();

    let exe = env::current_exe().unwrap();

    // 1. Spawn a crashed worker that claims 1 task (reserving the last 5.0 ceiling) but exits 99.
    let mut crash_child = Command::new(&exe)
        .env("IS_CHILD", "1")
        .env("CRASH_AFTER_CLAIM", "1")
        .env("DB_PATH", db_path.to_str().unwrap())
        .env("WATCH_MAX_FANOUT_COST_USD", "5.0")
        .arg("--exact")
        .arg("test_falsification_multiprocess_spend_cap")
        .spawn()
        .unwrap();

    let _crash_status = crash_child.wait().unwrap();

    // The crashed worker reserved 5.0 -> reserved+settled == 50.0 == cap. Confirm the ledger.
    let reserved_after_crash: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        (reserved_after_crash - 5.0).abs() < 1e-9,
        "crashed worker must have reserved one 5.0 ceiling, got {}",
        reserved_after_crash
    );

    // --- PHASE (a): NO sweep yet. The crashed reservation still holds the budget. ---
    // Spawn 10 fresh claimers across OS processes; ALL must be refused because the day cap
    // is fully consumed by the (un-released) crashed reservation.
    let mut children = vec![];
    for _ in 0..10 {
        let child = Command::new(&exe)
            .env("IS_CHILD", "1")
            .env("DB_PATH", db_path.to_str().unwrap())
            .env("WATCH_MAX_FANOUT_COST_USD", "5.0")
            .arg("--exact")
            .arg("test_falsification_multiprocess_spend_cap")
            .spawn()
            .unwrap();
        children.push(child);
    }
    let (mut success_a, mut reject_a) = (0, 0);
    for mut child in children {
        if child.wait().unwrap().success() {
            success_a += 1
        } else {
            reject_a += 1
        }
    }
    assert_eq!(
        success_a, 0,
        "PHASE (a): with no sweep, the crashed reservation holds the cap -> no claim succeeds"
    );
    assert_eq!(reject_a, 10, "PHASE (a): all 10 claims refused at the cap");

    // --- PHASE (b): a sweep tick releases the crashed (lease-expired) reservation. ---
    // Force the crashed row's lease to be expired, then sweep (cross-process release).
    conn.execute(
        "UPDATE pending_escalations SET claimed_until_ms = 0 WHERE status = 'claimed'",
        [],
    )
    .unwrap();
    let swept = db.sweep_phantom_claims().await.unwrap();
    assert!(
        swept >= 1,
        "sweep must reclaim the crashed worker's expired claim"
    );
    let reserved_after_sweep: f64 = conn
        .query_row(
            "SELECT reserved_usd FROM spend_ledger WHERE day_bucket = ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        reserved_after_sweep.abs() < 1e-9,
        "sweep must release the crashed reservation cross-process, got {}",
        reserved_after_sweep
    );

    // Now exactly one more claim should get through (headroom = one ceiling again); a second refused.
    let mut children2 = vec![];
    for _ in 0..10 {
        let child = Command::new(&exe)
            .env("IS_CHILD", "1")
            .env("DB_PATH", db_path.to_str().unwrap())
            .env("WATCH_MAX_FANOUT_COST_USD", "5.0")
            .arg("--exact")
            .arg("test_falsification_multiprocess_spend_cap")
            .spawn()
            .unwrap();
        children2.push(child);
    }
    let (mut success_b, mut reject_b) = (0, 0);
    for mut child in children2 {
        if child.wait().unwrap().success() {
            success_b += 1
        } else {
            reject_b += 1
        }
    }
    assert_eq!(
        success_b, 1,
        "PHASE (b): released budget lets exactly one more claim through"
    );
    assert_eq!(
        reject_b, 9,
        "PHASE (b): the remaining nine are refused at the cap"
    );
}
