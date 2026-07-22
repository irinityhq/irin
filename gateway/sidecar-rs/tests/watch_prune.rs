//! Prune coverage for `WatchDb::prune_terminal_rows`
//! (src/watch/db.rs:2284, hourly `pruning_loop` in src/watch/runner.rs:1114).
//!
//! `prune_terminal_rows(older_than_ms)` deletes rows whose `created_at_ms <
//! older_than_ms` AND whose `status` is terminal, from two tables:
//!   - `pending_escalations`: terminal set = ('outbox_written','dismissed',
//!     'expired','dead_lettered'). Note: 'failed' is NOT pruned, and the
//!     non-terminal states ('queued','claimed','council_response_staged') are
//!     never pruned regardless of age.
//!   - `directive_outbox`: terminal set (W3 item 1) = ('acked','expired',
//!     'dismissed'). The pre-W3 SQL listed ('acked','nacked','expired') — but
//!     'nacked' is not in the table CHECK (impossible) and 'dismissed' (a real
//!     terminal child state) was wrongly omitted. 'staged' is kept (live child).
//!
//! These tests pin: terminal-only deletion, the non-terminal safety property,
//! the retention-window boundary (`<`, so exactly-at-boundary is kept), and
//! that the hash-chained fire log (`watch_fires`) is never touched by prune
//! (db.rs:1152 contiguity mandate).
//!
//! NOTE: `directive_outbox` carries a FK to `pending_escalations`
//! (tenant, in_response_to) with foreign_keys=ON, so directive rows are inserted
//! against a parent escalation that is deliberately held in a non-prunable state
//! to isolate the directive_outbox prune from the parent prune.

use gateway_sidecar::watch::db::WatchDb;
use rusqlite::params;
use std::sync::Arc;

/// Open a fresh migrated WatchDb backed by a tempdir. Returns the db handle and
/// the on-disk path (for direct rusqlite connections in setup/assertions).
async fn fresh_db() -> (Arc<WatchDb>, tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_prune.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    (db, tmp, db_path)
}

/// Insert a pending_escalation row in an explicit status + created_at_ms.
/// Uses the public causal-dedup insert to create the row (status 'queued',
/// created_at_ms = `created_at_ms`), then forces `status` directly when a
/// terminal/other state is requested. Direct status UPDATE is safe here: the
/// immutability trigger lives on `directive_outbox`, not `pending_escalations`.
fn insert_pe(db_path: &std::path::Path, id: &str, tenant: &str, status: &str, created_at_ms: i64) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO pending_escalations
         (id, tenant, sentinel_name, envelope_json, status, created_at_ms, attempts, replay_epoch)
         VALUES (?1, ?2, 'sentinel-x', '{}', ?3, ?4, 0, 0)",
        params![id, tenant, status, created_at_ms],
    )
    .unwrap();
}

/// Insert a directive_outbox row in `status` at `created_at_ms`, with a parent
/// pending_escalation held in a non-prunable state ('council_response_staged')
/// so the parent is never a prune target and the FK stays satisfied.
fn insert_do_with_parent(
    db_path: &std::path::Path,
    do_id: &str,
    tenant: &str,
    status: &str,
    created_at_ms: i64,
) {
    let parent_id = format!("parent-{do_id}");
    // Parent in a non-terminal, non-prunable state, created old enough that it
    // would be eligible by age but is excluded by status.
    insert_pe(
        db_path,
        &parent_id,
        tenant,
        "council_response_staged",
        created_at_ms,
    );

    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json,
          envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?5, ?6)",
        params![
            do_id,
            parent_id,
            tenant,
            status,
            created_at_ms,
            created_at_ms + 1_000
        ],
    )
    .unwrap();
}

/// COUNT(*) of pending_escalations with this id (0 or 1).
fn pe_exists(db_path: &std::path::Path, id: &str) -> bool {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

/// COUNT(*) of directive_outbox with this id (0 or 1).
fn do_exists(db_path: &std::path::Path, id: &str) -> bool {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

// ---------------------------------------------------------------------------
// 1. Terminal-only: terminal rows older than retention are pruned; non-terminal
//    rows are NEVER pruned regardless of age.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prune_terminal_pending_escalations_only() {
    let (db, _tmp, path) = fresh_db().await;

    // All rows are well past the retention boundary (created at t=1000).
    let old = 1_000i64;
    let cutoff = 1_000_000i64; // older_than_ms; every old row is < cutoff.

    // Terminal states that MUST be pruned.
    for (i, status) in ["outbox_written", "dismissed", "expired", "dead_lettered"]
        .iter()
        .enumerate()
    {
        insert_pe(&path, &format!("term-{i}"), "tenant-a", status, old);
    }

    // 'failed' is terminal in the lifecycle but is intentionally NOT in the
    // prune SQL — it must survive even when old.
    insert_pe(&path, "failed-row", "tenant-a", "failed", old);

    // Non-terminal states must NEVER be pruned, even though they are old.
    for (i, status) in ["queued", "claimed", "council_response_staged"]
        .iter()
        .enumerate()
    {
        insert_pe(&path, &format!("live-{i}"), "tenant-a", status, old);
    }

    let (pe_pruned, do_pruned, _aged) = db.prune_terminal_rows(cutoff).await.unwrap();
    assert_eq!(
        pe_pruned, 4,
        "exactly the 4 terminal pending rows are pruned"
    );
    assert_eq!(do_pruned, 0, "no directive_outbox rows in this fixture");

    // Terminal rows gone.
    for i in 0..4 {
        assert!(
            !pe_exists(&path, &format!("term-{i}")),
            "terminal row {i} pruned"
        );
    }
    // 'failed' survives — not in prune set.
    assert!(
        pe_exists(&path, "failed-row"),
        "'failed' is NOT pruned by current SQL"
    );
    // Non-terminal rows survive regardless of age.
    for i in 0..3 {
        assert!(
            pe_exists(&path, &format!("live-{i}")),
            "non-terminal row {i} kept"
        );
    }
}

#[tokio::test]
async fn test_prune_terminal_directive_outbox_only() {
    let (db, _tmp, path) = fresh_db().await;

    let cutoff = 1_000_000i64;
    // created_at_ms must be monotonic non-decreasing per tenant (insert trigger),
    // so step the timestamps upward; all remain < cutoff.
    let mut t = 1_000i64;

    // W3 item 1 terminal prune set = ('acked','expired','dismissed').
    insert_do_with_parent(&path, "do-acked", "tenant-do", "acked", t);
    t += 1;
    insert_do_with_parent(&path, "do-expired", "tenant-do", "expired", t);
    t += 1;
    insert_do_with_parent(&path, "do-dismissed", "tenant-do", "dismissed", t);
    t += 1;
    // 'staged' is a LIVE child — never pruned even when old.
    insert_do_with_parent(&path, "do-staged", "tenant-do", "staged", t);

    let (pe_pruned, do_pruned, _aged) = db.prune_terminal_rows(cutoff).await.unwrap();
    // Parents are all held in 'council_response_staged' (non-prunable), so no
    // pending rows are pruned even though they are old.
    assert_eq!(
        pe_pruned, 0,
        "parent escalations are non-terminal, never pruned"
    );
    assert_eq!(
        do_pruned, 3,
        "exactly 'acked' + 'expired' + 'dismissed' directive rows pruned (W3)"
    );

    assert!(!do_exists(&path, "do-acked"), "'acked' directive pruned");
    assert!(
        !do_exists(&path, "do-expired"),
        "'expired' directive pruned"
    );
    assert!(
        !do_exists(&path, "do-dismissed"),
        "'dismissed' directive pruned (W3: was wrongly kept before)"
    );
    assert!(do_exists(&path, "do-staged"), "'staged' directive kept");

    // The (non-prunable) parents survive in both kept and pruned-child cases.
    assert!(
        pe_exists(&path, "parent-do-acked"),
        "parent of pruned child kept"
    );
    assert!(
        pe_exists(&path, "parent-do-staged"),
        "parent of kept child kept"
    );
}

// ---------------------------------------------------------------------------
// 2. Retention-window edge: prune predicate is `created_at_ms < older_than_ms`.
//    - exactly at boundary (== older_than_ms): KEPT
//    - 1ms outside (older_than_ms - 1): PRUNED
//    - 1ms inside  (older_than_ms + 1): KEPT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prune_retention_window_boundary_pending() {
    let (db, _tmp, path) = fresh_db().await;

    let cutoff = 500_000i64; // older_than_ms

    // All terminal ('expired') so only age distinguishes them.
    insert_pe(&path, "at-boundary", "tenant-edge", "expired", cutoff); // == cutoff -> kept
    insert_pe(
        &path,
        "one-ms-outside",
        "tenant-edge",
        "expired",
        cutoff - 1,
    ); // < cutoff -> pruned
    insert_pe(&path, "one-ms-inside", "tenant-edge", "expired", cutoff + 1); // > cutoff -> kept

    let (pe_pruned, _do_pruned, _aged) = db.prune_terminal_rows(cutoff).await.unwrap();
    assert_eq!(pe_pruned, 1, "only the strictly-older row is pruned");

    assert!(
        pe_exists(&path, "at-boundary"),
        "exactly-at-boundary row is KEPT (< is strict)"
    );
    assert!(
        !pe_exists(&path, "one-ms-outside"),
        "1ms older than boundary is pruned"
    );
    assert!(
        pe_exists(&path, "one-ms-inside"),
        "1ms newer than boundary is kept"
    );
}

#[tokio::test]
async fn test_prune_retention_window_boundary_directive() {
    let (db, _tmp, path) = fresh_db().await;

    let cutoff = 500_000i64;

    // Monotonic created_at per tenant: insert oldest first.
    insert_do_with_parent(&path, "do-outside", "tenant-edge-do", "acked", cutoff - 1); // pruned
    insert_do_with_parent(&path, "do-at", "tenant-edge-do", "acked", cutoff); // kept
    insert_do_with_parent(&path, "do-inside", "tenant-edge-do", "acked", cutoff + 1); // kept

    let (_pe_pruned, do_pruned, _aged) = db.prune_terminal_rows(cutoff).await.unwrap();
    assert_eq!(
        do_pruned, 1,
        "only the strictly-older directive row is pruned"
    );

    assert!(
        !do_exists(&path, "do-outside"),
        "1ms older than boundary is pruned"
    );
    assert!(
        do_exists(&path, "do-at"),
        "exactly-at-boundary directive is KEPT"
    );
    assert!(
        do_exists(&path, "do-inside"),
        "1ms newer than boundary is kept"
    );
}

// ---------------------------------------------------------------------------
// 3. Fire log (watch_fires) is untouched by prune — hash-chain contiguity
//    mandate (db.rs:1152). Even when every escalation/directive is pruned, the
//    append-only hash-chained fire log keeps every row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prune_never_touches_fire_log() {
    let (db, _tmp, path) = fresh_db().await;

    let cutoff = 1_000_000i64;
    let old = 1_000i64;

    // Build a small hash chain of ancient fires (older than any retention).
    for i in 0..5 {
        let fired_at = old + i; // distinct, ancient
        db.insert_fire(
            "tenant-fire",
            "sentinel-x",
            fired_at,
            "{}",
            &format!("reason-{i}"),
            "{}",
            1,
        )
        .await
        .unwrap()
        .expect("fire inserted (sentinel not hard-killed)");
    }

    // Snapshot the chain (id-ascending) before pruning.
    let before = db
        .list_fires_ascending("tenant-fire", 100, None)
        .await
        .unwrap();
    assert_eq!(before.len(), 5, "5 ancient fires present pre-prune");

    // Also seed prunable terminal rows in both tables so prune has real work.
    insert_pe(&path, "term-pe", "tenant-fire", "expired", old);
    insert_do_with_parent(&path, "term-do", "tenant-fire", "acked", old);

    let (pe_pruned, do_pruned, _aged) = db.prune_terminal_rows(cutoff).await.unwrap();
    assert_eq!(pe_pruned, 1, "the terminal escalation was pruned");
    assert_eq!(do_pruned, 1, "the terminal directive was pruned");

    // Fire log fully intact — same count, same rows, same hash chain.
    let after = db
        .list_fires_ascending("tenant-fire", 100, None)
        .await
        .unwrap();
    assert_eq!(after.len(), 5, "fire log row count unchanged by prune");

    let count_since = db.count_fires_since("tenant-fire", 0).await.unwrap();
    assert_eq!(count_since, 5, "no fire rows removed by prune");

    // Hash-chain bytes are identical (prune must not rewrite or reorder).
    for (b, a) in before.iter().zip(after.iter()) {
        assert_eq!(b.id, a.id, "fire id stable across prune");
        assert_eq!(b.hash, a.hash, "fire hash stable across prune");
        assert_eq!(
            b.prev_hash, a.prev_hash,
            "fire prev_hash stable across prune"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. W3 item 1 — the FK-interaction cases the old suite never exercised
//    (every prior test held parents in 'council_response_staged', so the
//    parent-before-child deferred-FK deadlock could never trigger). These
//    insert a TERMINAL parent linked to a real child and assert the tx commits.
// ---------------------------------------------------------------------------

/// Insert a directive_outbox child whose parent is itself in `parent_status`
/// (so the parent IS a prune candidate by status). Both rows share `created_at`.
fn insert_child_under_parent(
    db_path: &std::path::Path,
    parent_id: &str,
    do_id: &str,
    tenant: &str,
    parent_status: &str,
    child_status: &str,
    created_at_ms: i64,
) {
    insert_pe(db_path, parent_id, tenant, parent_status, created_at_ms);
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json,
          envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?5, ?6)",
        params![
            do_id,
            parent_id,
            tenant,
            child_status,
            created_at_ms,
            created_at_ms + 1_000
        ],
    )
    .unwrap();
}

/// The headline regression: aged 'outbox_written' parent + still-'staged' child.
/// Pre-W3 this deleted the parent first, orphaned the child, and the deferred FK
/// rolled the WHOLE tx back (retention dead from day 8). W3: the parent is NOT
/// deleted (live staged child), the child is NOT deleted, and the tx COMMITS —
/// the stuck pair is reported via the aged_staged alarm count.
#[tokio::test]
async fn test_prune_aged_parent_with_staged_child_commits_keeps_both() {
    let (db, _tmp, path) = fresh_db().await;
    let cutoff = 1_000_000i64;
    let old = 1_000i64;

    insert_child_under_parent(
        &path,
        "pe-stuck",
        "do-staged-stuck",
        "tenant-fk",
        "outbox_written",
        "staged",
        old,
    );

    let (pe_pruned, do_pruned, aged_staged) = db.prune_terminal_rows(cutoff).await.unwrap();

    // The whole point: the tx COMMITTED (no panic / rollback) and neither row
    // was deleted — never delete a live staged child.
    assert_eq!(
        pe_pruned, 0,
        "parent pinned by live staged child is NOT pruned"
    );
    assert_eq!(do_pruned, 0, "staged child is NOT pruned");
    assert_eq!(
        aged_staged, 1,
        "the stuck pair is counted for the retention alarm"
    );
    assert!(pe_exists(&path, "pe-stuck"), "parent kept");
    assert!(do_exists(&path, "do-staged-stuck"), "staged child kept");
}

/// Same pin, parent in 'dead_lettered' — also stays while its child is staged.
#[tokio::test]
async fn test_prune_dead_lettered_parent_with_staged_child_stays() {
    let (db, _tmp, path) = fresh_db().await;
    let cutoff = 1_000_000i64;
    let old = 1_000i64;

    insert_child_under_parent(
        &path,
        "pe-dl",
        "do-staged-dl",
        "tenant-fk2",
        "dead_lettered",
        "staged",
        old,
    );

    let (pe_pruned, do_pruned, aged_staged) = db.prune_terminal_rows(cutoff).await.unwrap();
    assert_eq!(
        pe_pruned, 0,
        "dead_lettered parent pinned by staged child stays"
    );
    assert_eq!(do_pruned, 0, "staged child stays");
    assert_eq!(aged_staged, 1, "counted for the alarm");
    assert!(pe_exists(&path, "pe-dl"));
    assert!(do_exists(&path, "do-staged-dl"));
}

/// A terminal parent whose only child is itself terminal drains fully: the child
/// goes in step 1, then the now-childless parent goes in step 2 — within a
/// single prune call. No alarm (nothing stuck).
#[tokio::test]
async fn test_prune_terminal_parent_terminal_child_drains_in_one_pass() {
    let (db, _tmp, path) = fresh_db().await;
    let cutoff = 1_000_000i64;
    let old = 1_000i64;

    insert_child_under_parent(
        &path,
        "pe-done",
        "do-acked-done",
        "tenant-fk3",
        "outbox_written",
        "acked",
        old,
    );

    let (pe_pruned, do_pruned, aged_staged) = db.prune_terminal_rows(cutoff).await.unwrap();
    assert_eq!(do_pruned, 1, "terminal child pruned (step 1)");
    assert_eq!(
        pe_pruned, 1,
        "now-childless terminal parent pruned (step 2)"
    );
    assert_eq!(aged_staged, 0, "nothing stuck — no alarm");
    assert!(!pe_exists(&path, "pe-done"), "parent drained");
    assert!(!do_exists(&path, "do-acked-done"), "child drained");
}
