//! dual-custody-local-attest B5 (spec §8, fail-closed migration invariant):
//! the arm_audit CHECK rebuild migration. Rows are copied VERBATIM into a
//! table whose CHECK adds 'stage_rehearsal'/'confirm_rehearsal'; inside the
//! SAME transaction the copy must row-count-match AND pass a full hash-chain
//! verification before commit; any failure rolls back to the intact original
//! (fail-closed — boot aborts rather than running on a suspect chain). The
//! original survives as the frozen arm_audit_pre_attest backup table.

use gateway_sidecar::watch::db::{arm_audit_distinct_genesis, compute_arm_audit_preimage, WatchDb};
use sha2::{Digest, Sha256};

/// CREATE the PRE-B5 arm_audit shape (the 'boot_env_arm' CHECK, no rehearsal
/// actions) with its append-only triggers — what an upgraded watch.db carries.
const OLD_SCHEMA: &str = "
CREATE TABLE arm_audit (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms     INTEGER NOT NULL,
    action    TEXT NOT NULL CHECK(action IN ('stage','confirm','disarm','stage_rejected','confirm_rejected','boot_env_arm')),
    principal TEXT NOT NULL,
    detail    TEXT,
    prev_hash TEXT NOT NULL,
    hash      TEXT NOT NULL UNIQUE
);
CREATE TRIGGER trg_arm_audit_no_update
BEFORE UPDATE ON arm_audit
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'arm_audit_append_only');
END;
CREATE TRIGGER trg_arm_audit_no_delete
BEFORE DELETE ON arm_audit
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'arm_audit_append_only');
END;
";

/// Build a REAL chained history in the old-shape table: every prev_hash
/// links and every hash is the preimage digest, exactly as production wrote
/// them.
fn seed_old_db(path: &std::path::Path, rows: &[(&str, &str, Option<&str>)]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(OLD_SCHEMA).unwrap();
    let mut prev = arm_audit_distinct_genesis();
    for (i, (action, principal, detail)) in rows.iter().enumerate() {
        let at_ms = 1_000 + i as i64;
        let preimage =
            compute_arm_audit_preimage(at_ms, action, principal, detail.unwrap_or(""), &prev);
        let hash = hex::encode(Sha256::digest(preimage.as_bytes()));
        conn.execute(
            "INSERT INTO arm_audit (at_ms, action, principal, detail, prev_hash, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![at_ms, action, principal, detail, prev, hash],
        )
        .unwrap();
        prev = hash;
    }
}

fn arm_audit_sql(path: &std::path::Path, table: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )
    .ok()
}

const HISTORY: &[(&str, &str, Option<&str>)] = &[
    ("stage", "alice", Some("stage_id=aaaa")),
    ("confirm", "bob", Some("stage_id=aaaa")),
    ("disarm", "watch-admin", None),
    ("boot_env_arm", "boot", Some("keyset_hash=unloaded")),
];

/// Happy path: old CHECK → rebuilt with rehearsal actions, rows verbatim,
/// frozen _pre_attest backup kept, append-only re-armed, rehearsal rows now
/// accepted.
#[tokio::test]
async fn test_b5_rebuild_extends_check_and_keeps_frozen_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    seed_old_db(&path, HISTORY);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();

    // New CHECK carries the rehearsal actions.
    let sql = arm_audit_sql(&path, "arm_audit").expect("arm_audit exists");
    assert!(sql.contains("stage_rehearsal") && sql.contains("confirm_rehearsal"));

    // Rows copied verbatim — same count, same hashes, chain still verifies
    // (list_arm_audit reads the rebuilt table).
    let rows = db.list_arm_audit().await.unwrap();
    assert_eq!(rows.len(), HISTORY.len());
    let mut prev = arm_audit_distinct_genesis();
    for (row, (action, principal, _)) in rows.iter().zip(HISTORY) {
        assert_eq!(row.action.as_str(), *action);
        assert_eq!(row.principal.as_str(), *principal);
        assert_eq!(row.prev_hash, prev, "chain must link after rebuild");
        prev = row.hash.clone();
    }

    // A rehearsal action now passes the CHECK and chains onto the history.
    db.append_arm_audit("stage_rehearsal", "alice", Some("rehearsal"))
        .await
        .unwrap();
    let rows = db.list_arm_audit().await.unwrap();
    assert_eq!(rows.last().unwrap().action, "stage_rehearsal");

    // The backup table survives (archive-never-delete) with the FULL
    // original history, and is frozen at the engine level.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n_backup: i64 = conn
        .query_row("SELECT COUNT(*) FROM arm_audit_pre_attest", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n_backup as usize, HISTORY.len());
    let frozen = conn.execute("DELETE FROM arm_audit_pre_attest", []);
    assert!(
        frozen.is_err(),
        "arm_audit_pre_attest must be frozen by triggers"
    );
    let frozen = conn.execute("UPDATE arm_audit_pre_attest SET principal='x'", []);
    assert!(frozen.is_err());

    // The rebuilt live table re-armed its own append-only triggers.
    let blocked = conn.execute("DELETE FROM arm_audit", []);
    assert!(blocked.is_err(), "rebuilt arm_audit must stay append-only");
}

/// Idempotency: a second boot over the migrated DB does NOT rebuild again
/// (no clobber error from the existing backup — needs_rebuild is false).
#[tokio::test]
async fn test_b5_rebuild_is_idempotent_across_boots() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    seed_old_db(&path, HISTORY);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();
    drop(db);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations()
        .await
        .expect("second boot must not attempt a second rebuild");
    assert_eq!(db.list_arm_audit().await.unwrap().len(), HISTORY.len());
}

/// Condition 5 fail-closed: a TAMPERED row (hash does not recompute) aborts
/// the migration, the transaction rolls back, and the original old-CHECK
/// table is left fully intact — the gate stays closed on a suspect chain.
#[tokio::test]
async fn test_b5_rebuild_fails_closed_on_chain_tamper() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    seed_old_db(&path, HISTORY);
    {
        // Tamper AFTER seeding: drop the trigger, rewrite one detail so the
        // stored hash no longer matches its preimage, re-create the trigger.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TRIGGER trg_arm_audit_no_update;
             UPDATE arm_audit SET detail = 'forged' WHERE id = 2;
             CREATE TRIGGER trg_arm_audit_no_update
             BEFORE UPDATE ON arm_audit
             FOR EACH ROW
             BEGIN
                 SELECT RAISE(ABORT, 'arm_audit_append_only');
             END;",
        )
        .unwrap();
    }

    let db = WatchDb::open(&path).await.unwrap();
    let err = db
        .run_migrations()
        .await
        .expect_err("tampered chain must abort the migration");
    assert!(
        err.to_string().contains("hash mismatch"),
        "error must name the chain failure; got: {err}"
    );

    // Rolled back: old table intact (old CHECK, all rows), no backup left.
    let sql = arm_audit_sql(&path, "arm_audit").expect("original table must survive");
    assert!(
        !sql.contains("stage_rehearsal"),
        "rebuild must not have committed"
    );
    assert!(arm_audit_sql(&path, "arm_audit_pre_attest").is_none());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM arm_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n as usize, HISTORY.len());
}

/// Same fail-closed posture for a broken LINK (prev_hash does not point at
/// the predecessor) — distinct failure mode from a hash mismatch.
#[tokio::test]
async fn test_b5_rebuild_fails_closed_on_chain_break() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    // Seed two rows that are INTERNALLY consistent but not linked: row 2's
    // prev_hash points at genesis instead of row 1.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(OLD_SCHEMA).unwrap();
    let genesis = arm_audit_distinct_genesis();
    for (i, action) in ["stage", "disarm"].iter().enumerate() {
        let at_ms = 1_000 + i as i64;
        let preimage = compute_arm_audit_preimage(at_ms, action, "alice", "", &genesis);
        let hash = hex::encode(Sha256::digest(preimage.as_bytes()));
        conn.execute(
            "INSERT INTO arm_audit (at_ms, action, principal, detail, prev_hash, hash)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            rusqlite::params![at_ms, action, "alice", genesis, hash],
        )
        .unwrap();
    }
    drop(conn);

    let db = WatchDb::open(&path).await.unwrap();
    let err = db
        .run_migrations()
        .await
        .expect_err("broken link must abort");
    assert!(
        err.to_string().contains("chain break"),
        "error must name the link failure; got: {err}"
    );
    assert!(arm_audit_sql(&path, "arm_audit_pre_attest").is_none());
}

/// A leftover arm_audit_pre_attest backup blocks a NEW rebuild attempt
/// (archive-never-delete: refuse to clobber; operator must move it out).
#[tokio::test]
async fn test_b5_rebuild_refuses_to_clobber_existing_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    seed_old_db(&path, HISTORY);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE arm_audit_pre_attest (id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let db = WatchDb::open(&path).await.unwrap();
    let err = db
        .run_migrations()
        .await
        .expect_err("existing backup must block the rebuild");
    assert!(
        err.to_string().contains("refusing to clobber"),
        "error must explain the refusal; got: {err}"
    );
    // Original untouched.
    let sql = arm_audit_sql(&path, "arm_audit").unwrap();
    assert!(!sql.contains("stage_rehearsal"));
}

/// Fresh DBs get the extended CHECK straight from schema_v1 — no rebuild, no
/// backup table, rehearsal actions accepted from row one.
#[tokio::test]
async fn test_b5_fresh_db_needs_no_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();
    db.run_migrations().await.unwrap(); // and it stays idempotent

    assert!(arm_audit_sql(&path, "arm_audit_pre_attest").is_none());
    db.append_arm_audit("confirm_rehearsal", "alice", Some("rehearsal"))
        .await
        .unwrap();
    assert_eq!(
        db.list_arm_audit().await.unwrap().last().unwrap().action,
        "confirm_rehearsal"
    );
}
