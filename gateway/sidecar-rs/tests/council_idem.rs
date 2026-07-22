//! Falsification tests for Council idempotency persistence.
//!
//! The suite verifies write-ahead ordering and concurrent-writer timeout
//! behavior against the durable SQLite mirror.

use std::time::Duration;

/// The SQLite row must be durable before any
/// in-memory mutation in council_idem_claim. Restructured from the original
/// plan's "spawn sidecar over HTTP and SIGKILL it" shape into a direct
/// invariant test of the durable path the production handler exercises:
/// wrap db.upsert_pending in the same tokio::time::timeout(50ms, ...) the
/// handler does, simulate crash by dropping the in-process DB handle, then
/// reopen via fresh rusqlite::Connection and verify the row is there.
/// This keeps the test focused on the storage contract rather than process
/// orchestration.
#[tokio::test]
async fn t18_write_ahead_ordering_survives_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("council_idem.db");
    let db = CouncilIdemDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    // STEP 1: write-ahead exactly as council_idem_claim's STEP 1 does —
    // tokio::time::timeout(50ms, db.upsert_pending(...)).
    let result = tokio::time::timeout(
        Duration::from_millis(50),
        db.upsert_pending("test-key", "test-idem-1", "abc", "req-1", now_ms()),
    )
    .await;
    assert!(
        matches!(result, Ok(Ok(()))),
        "write-ahead should commit durably within 50ms; got {:?}",
        result
    );

    // STEP 2: simulate sidecar crash — drop the in-process db handle (and
    // any in-memory state it might have buffered). SQLite WAL is durable
    // on disk regardless.
    drop(db);

    // STEP 3: verify the row is there via a fresh independent connection
    // (different from the tokio_rusqlite handle to prove the write
    // committed across handles).
    let rows: Vec<(String, String, String)> = rusqlite::Connection::open(&db_path)
        .unwrap()
        .prepare("SELECT caller_key, idempotency_key, state FROM council_idem")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    // V1 SPEC RESULT: rows.is_empty() — write-after fire-and-forget never ran.
    // V2 SPEC RESULT: exactly one row with ('test-key','test-idem-1','pending').
    assert_eq!(
        rows.len(),
        1,
        "expected 1 Pending row to survive crash (write-ahead invariant)"
    );
    assert_eq!(rows[0].2, "pending");
}

/// T22: 100 concurrent upsert_pending calls against council_idem.db. Assert
///      all succeed within 5s. Proves PRAGMA busy_timeout=50ms (set in
///      CouncilIdemDb::open) absorbs the contention burst without surfacing
///      SQLITE_BUSY to callers.
///
/// Restructured from the original plan's `claim_idempotency()` helper shape
/// for the same reason as T18: no HTTP sidecar harness exists. The contention
/// path under test is identical (tokio_rusqlite + WAL + busy_timeout=50ms).
#[tokio::test]
async fn t22_busy_timeout_handles_concurrent_writers() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("council_idem.db");
    let db = std::sync::Arc::new(CouncilIdemDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(100);
    for i in 0..100 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            db.upsert_pending(
                &format!("c-{i}"),
                &format!("k-{i}"),
                &format!("body-{i}"),
                &format!("req-{i}"),
                now_ms(),
            )
            .await
        }));
    }
    let mut results = Vec::with_capacity(100);
    for h in handles {
        results.push(h.await);
    }
    let elapsed = start.elapsed();

    let successes = results.iter().filter(|r| matches!(r, Ok(Ok(())))).count();
    assert_eq!(
        successes, 100,
        "T22: only {successes}/100 succeeded — SQLITE_BUSY detected (busy_timeout missing)"
    );
    assert!(
        elapsed.as_secs() < 5,
        "T22: 100-writer burst took {:?} — busy_timeout=50ms not enough OR contention is pathological",
        elapsed
    );
}

// --- T13: write-ahead happy path. SQLite row appears via upsert_pending. ---

use gateway_sidecar::council_storage::CouncilIdemDb;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// T13: write-ahead happy path. SQLite row appears in council_idem after
/// upsert_pending — proves the write-ahead helper does what its name says.
/// Together with the call-site tests, this locks in the durability invariant:
/// council.rs calls upsert_pending before the in-memory mutation.
#[tokio::test]
async fn t13_write_ahead_ordering_invariant() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("council_idem.db");
    let db = CouncilIdemDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    db.upsert_pending("caller-1", "idem-1", "body-sha", "req-1", now_ms())
        .await
        .unwrap();

    // Verify row in SQLite via a synchronous connection (different from the
    // tokio_rusqlite one to prove the write committed across handles).
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM council_idem WHERE state='pending' AND caller_key='caller-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

/// T14: startup recovery — Stored within TTL reloaded, Pending dropped, stale
///      grants swept.
#[tokio::test]
async fn t14_startup_recovery_loads_stored_drops_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("council_idem.db");

    // Pre-populate via direct SQL through the same write-ahead helpers.
    {
        let db = CouncilIdemDb::open(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
        let now = now_ms();
        // 5 Stored within TTL.
        for i in 0..5 {
            db.upsert_stored(
                &format!("c{i}"),
                &format!("k{i}"),
                "body",
                "resp-sha",
                "{}",
                "{}",
                &format!("owner-{i}"),
                now,
            )
            .await
            .unwrap();
        }
        // 3 Stored past TTL (insert with stored_at well in the past — ~28h ago).
        for i in 5..8 {
            db.upsert_stored(
                &format!("c{i}"),
                &format!("k{i}"),
                "body",
                "resp-sha",
                "{}",
                "{}",
                &format!("owner-{i}"),
                now - 100_000_000,
            )
            .await
            .unwrap();
        }
        // 4 Pending — must be dropped on recovery (orphaned by restart).
        for i in 8..12 {
            db.upsert_pending(&format!("c{i}"), &format!("k{i}"), "body", "owner", now)
                .await
                .unwrap();
        }
        // 2 stale grants (400s old; grant floor = now - 300s - 30s = now - 330s).
        for i in 0..2 {
            db.record_grant(&format!("c{i}"), &format!("g{i}"), now - 400_000)
                .await
                .unwrap();
        }
    }

    let recovered = CouncilIdemDb::open(&db_path)
        .await
        .unwrap()
        .recover_on_startup()
        .await
        .unwrap();

    assert_eq!(recovered.loaded_stored, 5, "expected 5 Stored within TTL");
    assert_eq!(recovered.dropped_pending, 4, "expected 4 Pending dropped");
    assert_eq!(recovered.stale_grants, 2, "expected 2 stale grants swept");
}

/// The Stored upsert is write-once sticky on
/// `owner_request_id` — a re-store carrying an EMPTY owner must not wipe a
/// previously-persisted non-repudiation `original_request_id`; a non-empty
/// owner does replace it. This guards against an ownerless replay.
#[tokio::test]
async fn a3_empty_owner_restore_does_not_wipe_known_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("council_idem.db");
    let db = CouncilIdemDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    let now = now_ms();

    // First store with a known owner.
    db.upsert_stored("c", "k", "bsha", "rsha", "{}", "{}", "req-original", now)
        .await
        .unwrap();
    // Re-store the SAME key with an EMPTY owner (a replay path that lost it).
    db.upsert_stored("c", "k", "bsha", "rsha2", "{}", "{}", "", now + 1)
        .await
        .unwrap();

    let rows = db.load_stored_rows().await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.caller_key == "c" && r.idempotency_key == "k")
        .expect("row present");
    assert_eq!(
        row.owner_request_id, "req-original",
        "empty owner must NOT wipe the known non-repudiation owner"
    );
    assert_eq!(
        row.response_body_sha256, "rsha2",
        "other fields still update on re-store"
    );

    // A non-empty owner DOES replace the stored one.
    db.upsert_stored("c", "k", "bsha", "rsha3", "{}", "{}", "req-second", now + 2)
        .await
        .unwrap();
    let rows = db.load_stored_rows().await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.caller_key == "c" && r.idempotency_key == "k")
        .expect("row present");
    assert_eq!(
        row.owner_request_id, "req-second",
        "a non-empty owner replaces the stored one"
    );
}
