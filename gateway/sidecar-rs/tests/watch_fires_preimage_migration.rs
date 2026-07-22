//! W3 item 3 — the watch_fires `preimage_version` ADD COLUMN migration.
//!
//! A pre-W3 watch.db has a `watch_fires` table with NO `preimage_version`
//! column and NO append-only triggers (W3 added both). On upgrade,
//! `run_migrations` must:
//!   1. ALTER TABLE ... ADD COLUMN preimage_version INTEGER NOT NULL DEFAULT 3,
//!      backfilling every legacy row to version 3 (the original 6-field
//!      preimage — the 8 live canary rows were written this way);
//!   2. add the append-only triggers via `schema_v1()`'s CREATE ... IF NOT
//!      EXISTS (idempotent over the existing table);
//!   3. leave every legacy row verifiable: a v3-backfilled row's stored hash
//!      still recomputes under the 6-field preimage (envelope_json NOT hashed).
//!
//! A fresh v4 fire then chains on top and the mixed chain verifies — the
//! version tag, not byte-equality, is the per-row discriminator.
//!
//! This is the additive sibling to arm_audit's rebuild migration test; the
//! watch_fires path is a plain ADD COLUMN (no table rebuild), so there is no
//! frozen backup table and no trigger drop/recreate hazard to exercise here.

use gateway_sidecar::watch::db::{watch_distinct_genesis, WatchDb};
use sha2::{Digest, Sha256};

/// The PRE-W3 watch_fires shape: the W3 column and the W3 append-only triggers
/// are BOTH absent (a real upgraded watch.db carried neither). Indices match
/// schema_v1 so the migration adds only the column + triggers.
const OLD_WATCH_FIRES_SCHEMA: &str = "
CREATE TABLE watch_fires (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant                  TEXT NOT NULL,
    sentinel                TEXT NOT NULL,
    fired_at                INTEGER NOT NULL,
    state_json              TEXT NOT NULL,
    reason                  TEXT NOT NULL,
    prev_hash               TEXT NOT NULL,
    hash                    TEXT NOT NULL UNIQUE,
    envelope_json           TEXT NOT NULL,
    envelope_schema_version INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX idx_watch_fires_tenant_fired ON watch_fires(tenant, fired_at DESC);
CREATE INDEX idx_watch_fires_sentinel ON watch_fires(sentinel);
";

/// Compute a legacy v3 watch_fires hash exactly as the pre-W3 code did:
/// the 6-field length-prefixed preimage, with envelope_json NOT included.
/// (compute_watch_fire_preimage is pub(crate), so the integration test inlines
/// the format — the same inline the mixed-chain test in watch_chain.rs uses.)
fn legacy_v3_hash(
    tenant: &str,
    sentinel: &str,
    fired_at: i64,
    state_json: &str,
    reason: &str,
    prev_hash: &str,
) -> String {
    let fired_at_str = fired_at.to_string();
    let preimage = format!(
        "{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}",
        tenant.len(),
        tenant,
        sentinel.len(),
        sentinel,
        fired_at_str.len(),
        fired_at_str,
        state_json.len(),
        state_json,
        reason.len(),
        reason,
        prev_hash.len(),
        prev_hash,
    );
    hex::encode(Sha256::digest(preimage.as_bytes()))
}

fn column_names(path: &std::path::Path, table: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    cols
}

/// Seed a single legacy v3 row in the OLD shape: a real chained row whose hash
/// is the 6-field preimage digest, with a non-empty envelope_json that the v3
/// scheme deliberately does NOT hash.
fn seed_old_db_one_v3_row(path: &std::path::Path) -> (String, i64) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(OLD_WATCH_FIRES_SCHEMA).unwrap();

    let tenant = "sovereign";
    let sentinel = "s1";
    let fired_at: i64 = 1_747_166_531_000;
    let state_json = "{}";
    let reason = "legacy-v3";
    let prev_hash = watch_distinct_genesis();
    let hash = legacy_v3_hash(tenant, sentinel, fired_at, state_json, reason, &prev_hash);

    conn.execute(
        "INSERT INTO watch_fires
           (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash,
            envelope_json, envelope_schema_version)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,1)",
        rusqlite::params![
            tenant,
            sentinel,
            fired_at,
            state_json,
            reason,
            prev_hash,
            hash,
            // A legacy row DID carry an envelope_json value; v3 does not hash it,
            // so the post-migration v3 verify must ignore it entirely.
            r#"{"legacy":"ignored-by-v3"}"#,
        ],
    )
    .unwrap();
    (tenant.to_string(), fired_at)
}

/// Upgrade path: ADD COLUMN backfills the legacy row to v3, triggers get armed,
/// and the legacy row still verifies under the 6-field preimage.
#[tokio::test]
async fn w3_migration_adds_preimage_version_and_backfills_legacy_to_v3() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    let (tenant, _) = seed_old_db_one_v3_row(&path);

    // Pre-migration: the column does not exist.
    assert!(
        !column_names(&path, "watch_fires").contains(&"preimage_version".to_string()),
        "fixture must start on the pre-W3 schema (no preimage_version column)"
    );

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();

    // 1. Column exists after migration.
    assert!(
        column_names(&path, "watch_fires").contains(&"preimage_version".to_string()),
        "migration must ADD the preimage_version column"
    );

    // 2. The legacy row was backfilled to the DEFAULT (v3), not left NULL/4.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let ver: i64 = conn
        .query_row(
            "SELECT preimage_version FROM watch_fires WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ver, 3, "legacy rows must backfill to v3, never to v4");

    // 3. The legacy row still verifies — verify_chain dispatches it as v3 and
    //    does NOT pull envelope_json into the preimage.
    let verdict = db.verify_chain(&tenant).await.unwrap();
    assert!(
        verdict.ok,
        "backfilled legacy v3 row must still verify; got {verdict:?}"
    );
    assert_eq!(verdict.rows_walked, 1);
}

/// After migration the append-only triggers are armed (W3 item 2 also lands on
/// the upgrade path, not just on fresh DBs).
#[tokio::test]
async fn w3_migration_arms_append_only_triggers_on_upgrade() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    seed_old_db_one_v3_row(&path);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let blocked_update = conn.execute("UPDATE watch_fires SET reason='forged' WHERE id=1", []);
    assert!(
        blocked_update.is_err(),
        "upgrade must arm trg_watch_fires_no_update"
    );
    let blocked_delete = conn.execute("DELETE FROM watch_fires WHERE id=1", []);
    assert!(
        blocked_delete.is_err(),
        "upgrade must arm trg_watch_fires_no_delete"
    );
}

/// A fresh v4 fire chains onto the backfilled legacy v3 row, and the mixed-
/// version chain verifies end to end — the true upgrade-then-run scenario.
#[tokio::test]
async fn w3_migration_then_fresh_v4_fire_mixed_chain_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    let (tenant, fired_at) = seed_old_db_one_v3_row(&path);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();

    // insert_fire reads the chain tip's hash as prev_hash, so this v4 row links
    // onto the migrated v3 row.
    db.insert_fire(&tenant, "s1", fired_at + 1, "{}", "v4-row", r#"{"e":1}"#, 1)
        .await
        .unwrap()
        .expect("not hard-killed");

    // The new row is v4 (envelope hashed); the old row stays v3.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let v4_ver: i64 = conn
        .query_row(
            "SELECT preimage_version FROM watch_fires WHERE id=2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v4_ver, 4, "fresh fires after migration must be v4");

    let verdict = db.verify_chain(&tenant).await.unwrap();
    assert!(
        verdict.ok,
        "mixed v3(legacy)->v4(fresh) chain must verify after migration; got {verdict:?}"
    );
    assert_eq!(verdict.rows_walked, 2);
}

/// Idempotency: a second boot over the already-migrated DB does not re-ALTER
/// (the PRAGMA table_info guard suppresses a duplicate-column error) and the
/// chain stays verifiable.
#[tokio::test]
async fn w3_migration_is_idempotent_across_boots() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("watch.db");
    let (tenant, _) = seed_old_db_one_v3_row(&path);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();
    drop(db);

    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations()
        .await
        .expect("second boot must not re-run the ADD COLUMN");

    let verdict = db.verify_chain(&tenant).await.unwrap();
    assert!(verdict.ok, "chain must stay verifiable across boots");
    assert_eq!(verdict.rows_walked, 1);
}
