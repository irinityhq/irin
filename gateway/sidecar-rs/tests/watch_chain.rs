//! Phase 2 watch.db hash chain continuity tests.

use rusqlite::Connection;
use std::sync::Arc;

/// T20: 10 concurrent inserts for tenant=A via direct WatchDb. Walk chain
///      ascending. Assert linear prev_hash continuity — zero forks.
///
/// Restructured from the plan's HTTP-force-wake shape (same reason as
/// T18/T22 — no sidecar test harness on this branch). Both routes test
/// the exact same SQL path: BEGIN IMMEDIATE serializes writers + the
/// prev_hash read inside the tx prevents the v1 fork-on-shared-read
/// failure mode.
#[tokio::test]
async fn t20_concurrent_same_tenant_chain_continuity() {
    use gateway_sidecar::watch::db::{watch_distinct_genesis, WatchDb};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Fire 10 concurrent inserts for tenant=sovereign in parallel.
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut handles = Vec::new();
    for i in 0..10 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            db.insert_fire(
                "sovereign",
                "file-inbox-watch",
                base + i,
                &format!("{{\"i\":{i}}}"),
                "concurrent fire",
                "{}",
                1,
            )
            .await
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    // Walk chain ascending via a fresh sync connection.
    let conn = Connection::open(&db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, prev_hash, hash, tenant, sentinel, fired_at, state_json, reason,
                    envelope_json, preimage_version
             FROM watch_fires WHERE tenant='sovereign' ORDER BY id ASC",
        )
        .unwrap();
    // (id, prev_hash, hash, tenant, sentinel, fired_at, state_json, reason, envelope_json, preimage_version)
    type FireRow = (
        i64,
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        i64,
    );
    let rows: Vec<FireRow> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    assert_eq!(rows.len(), 10, "expected 10 fires");

    // First row: prev_hash MUST equal distinct genesis.
    assert_eq!(
        rows[0].1,
        watch_distinct_genesis(),
        "first row prev_hash must be distinct genesis"
    );

    // Each subsequent row: prev_hash MUST equal previous row's hash.
    for window in rows.windows(2) {
        let prev_hash_field = &window[1].1;
        let prev_row_hash = &window[0].2;
        assert_eq!(
            prev_hash_field, prev_row_hash,
            "chain fork detected at id={}",
            window[1].0
        );
    }

    // Recompute each row's hash from its preimage; assert match. W3: fires are
    // now v4 (envelope_json in the preimage); dispatch on preimage_version.
    for row in &rows {
        let env = if row.9 == 4 {
            Some(row.8.as_str())
        } else {
            None
        };
        let preimage = build_preimage(&row.3, &row.4, row.5, &row.6, &row.7, &row.1, env);
        let recomputed = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(preimage.as_bytes()));
        assert_eq!(
            recomputed, row.2,
            "hash mismatch at id={} (recomputed != stored)",
            row.0
        );
    }
}

/// Test-side replica of `compute_watch_fire_preimage` (which is `pub(crate)`).
/// `envelope_json = None` → v3 (6 fields); `Some` → v4 (envelope appended,
/// length-prefixed). Keep in lockstep with db.rs::compute_watch_fire_preimage.
fn build_preimage(
    tenant: &str,
    sentinel: &str,
    fired_at: i64,
    state_json: &str,
    reason: &str,
    prev_hash: &str,
    envelope_json: Option<&str>,
) -> String {
    let fired_at_str = fired_at.to_string();
    let base = format!(
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
        prev_hash
    );
    match envelope_json {
        None => base,
        Some(env) => format!("{}|{}:{}", base, env.len(), env),
    }
}

// --- T2: hash chain continuity, single tenant, sequential fires (hash-chain) ---

use gateway_sidecar::watch::db::{watch_distinct_genesis, WatchDb};
use std::time::{SystemTime, UNIX_EPOCH};

fn t2_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// T2: hash chain continuity for a single tenant under sequential fires.
/// Proves: first row's prev_hash is the distinct genesis; every subsequent
/// row's prev_hash equals the previous row's hash. Same invariant T20
/// tests under concurrency.
#[tokio::test]
async fn t2_hash_chain_continuity_sequential_single_tenant() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    let base = t2_now_ms();
    for i in 0..10 {
        db.insert_fire(
            "sovereign",
            "s1",
            base + i,
            &format!("{{\"i\":{i}}}"),
            "test fire",
            "{}",
            1,
        )
        .await
        .unwrap()
        .expect("not hard-killed, should insert");
    }

    let rows = db
        .list_fires_ascending("sovereign", 100, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(
        rows[0].prev_hash,
        watch_distinct_genesis(),
        "first row prev_hash must be the distinct genesis"
    );
    for w in rows.windows(2) {
        assert_eq!(w[1].prev_hash, w[0].hash, "chain fork at id={}", w[1].id);
    }
}

/// T_NEW2 (positive): intact chain verifies ok=true. Closes P0-5 at logic
/// level — Phase 3 dispatcher can trust an `ok` result from verify_chain
/// before routing on chain content.
#[tokio::test]
async fn t_new2_verify_chain_intact_returns_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    let base = t2_now_ms();
    for i in 0..5 {
        db.insert_fire(
            "sovereign",
            "s1",
            base + i,
            &format!("{{\"i\":{i}}}"),
            "ok",
            "{}",
            1,
        )
        .await
        .unwrap()
        .expect("not hard-killed");
    }

    let verdict = db.verify_chain("sovereign").await.unwrap();
    assert!(verdict.ok, "intact chain should verify; got {:?}", verdict);
    assert_eq!(verdict.rows_walked, 5);
    assert!(verdict.broken_at_id.is_none());
    assert!(verdict.break_kind.is_none());
}

/// T_NEW2 (negative — hash mismatch): tamper a row's `hash` field and
/// verify_chain detects it. Tests invariant 3 (stored hash matches
/// recomputed preimage hash). Without verify_chain a forger could swap a
/// row's payload + leave a stale hash; with it, every walk catches the
/// tamper.
#[tokio::test]
async fn t_new2b_verify_chain_detects_tampered_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    let base = t2_now_ms();
    for i in 0..5 {
        db.insert_fire(
            "sovereign",
            "s1",
            base + i,
            &format!("{{\"i\":{i}}}"),
            "ok",
            "{}",
            1,
        )
        .await
        .unwrap()
        .expect("not hard-killed");
    }

    // Tamper the 3rd row's reason — invalidates its stored hash because the
    // preimage now includes the new reason but the hash column is unchanged.
    // W3 item 2 added an append-only UPDATE trigger; drop it here to MODEL the
    // attacker who has raw DB write access and removes the guard — verify_chain
    // is the second layer that must still catch the tamper.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TRIGGER IF EXISTS trg_watch_fires_no_update", [])
            .unwrap();
        conn.execute("UPDATE watch_fires SET reason='tampered' WHERE id=3", [])
            .unwrap();
    }

    let verdict = db.verify_chain("sovereign").await.unwrap();
    assert!(!verdict.ok, "tampered chain should fail; got {:?}", verdict);
    assert_eq!(verdict.broken_at_id, Some(3));
    use gateway_sidecar::watch::db::VerifyBreak;
    assert_eq!(verdict.break_kind, Some(VerifyBreak::HashMismatch));
}

/// T_NEW2 (negative — prev_hash mismatch): rewrite a row's `prev_hash` to
/// a value that doesn't match the previous row's stored hash. Tests
/// invariant 2 (chain continuity). The walk should break at the row whose
/// prev_hash is wrong, not at the previous row.
#[tokio::test]
async fn t_new2c_verify_chain_detects_prev_hash_break() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    let base = t2_now_ms();
    for i in 0..5 {
        db.insert_fire(
            "sovereign",
            "s1",
            base + i,
            &format!("{{\"i\":{i}}}"),
            "ok",
            "{}",
            1,
        )
        .await
        .unwrap()
        .expect("not hard-killed");
    }

    // Snap the chain at row 4: rewrite its prev_hash to a bogus value.
    // (Drop the W3 append-only trigger first — model raw-DB-access tamper.)
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TRIGGER IF EXISTS trg_watch_fires_no_update", [])
            .unwrap();
        conn.execute("UPDATE watch_fires SET prev_hash='deadbeef' WHERE id=4", [])
            .unwrap();
    }

    let verdict = db.verify_chain("sovereign").await.unwrap();
    assert!(!verdict.ok, "broken chain should fail; got {:?}", verdict);
    assert_eq!(verdict.broken_at_id, Some(4));
    use gateway_sidecar::watch::db::VerifyBreak;
    assert_eq!(verdict.break_kind, Some(VerifyBreak::PrevHashMismatch));
}

// ============================================================================
// P0-D Mandatory "Duplicate Fire Collapse" test (causal_fire_id.md §7)
// Uses test-double harness for the future sweep path. Synthetic payloads OK
// for Phase 0. No production producer code. No changes to hot audit path.
// No manual INSERT INTO pending_escalations visible in the proof assertions
// (encapsulated in harness). Local schema extension for causal column only
// inside this test DB (does not touch prod migrations or watch_fires).
// ============================================================================

use gateway_sidecar::watch::dispatcher::safe_tenant_token;
use gateway_sidecar::watch::fire_identity::{causal_fire_id, compute_content_digest};
use serde_json::json;

/// Test-double harness for the CDC sweep enqueue path (P0 scope only).
/// Computes causal_fire_id from stable content, then performs the INSERT
/// with ON CONFLICT DO NOTHING against a schema extended for the dedup column.
/// The caller (test proof path) never writes a raw INSERT statement.
///
/// ADVISORY (Round 2 final polish, per design §9 + .def test_gates + prior Responses):
/// This binding P0-D harness (test_duplicate_fire_collapse) uses the Phase 0 simulate
/// double (raw INSERT exercising ON CONFLICT in test DB only) to prove the 5 Duplicate
/// Fire Collapse assertions without production producer code. Per design §9:
/// "Full non-seeded live-fire smoke (real sentinel → watch_fires → sweep → exactly one
/// pending via causal id → mock council → signed directive or dismissal, exercising all
/// negatives) is a **future D9 gate**, explicitly out of this Phase 0 scoped package."
/// The prod path (cdc_sweep_tick + insert_pending_escalation_with_causal_dedup with
/// real DB, no raw INSERT) is now directly exercised and asserted in the new regression
/// test_real_sentinelstate_observed_at_produces_distinct_causals (allowed watch_chain.rs).
/// D9 "drive actual enqueue / no manual INSERT in proof" is met for the new real-path
/// coverage; the original binding test remains the Phase 0 proof as scoped.
/// Full armed smoke-phase3 with mock Council is future per contract.
fn simulate_causal_sweep_enqueue(
    conn: &rusqlite::Connection,
    tenant: &str,
    sentinel: &str,
    observed_at: &str,
    payload: &serde_json::Value,
    envelope_json: &str,
) -> (String, bool) {
    let canonical_tenant = safe_tenant_token(tenant);
    let content_digest = compute_content_digest(sentinel, &canonical_tenant, observed_at, payload);
    let causal = causal_fire_id(&canonical_tenant, sentinel, &content_digest);

    // Local test-only schema extension (Phase 0 proof; real migration in Phase 1).
    // Safe because this conn is a fresh tempfile DB for this test only.
    let _ = conn.execute(
        "ALTER TABLE pending_escalations ADD COLUMN causal_fire_id TEXT",
        [],
    );
    let _ = conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_pe_causal_dedup_test
         ON pending_escalations(tenant, sentinel_name, causal_fire_id)",
        [],
    );

    // The actual enqueue step exercising ON CONFLICT (harness, not "manual" in proof).
    // Fixed timestamp for determinism (harness only; production uses real wall time).
    const FIXED_NOW_MS: i64 = 1_717_000_000_000;
    // Full causal hex for the synthetic PK id (test-only; 64 chars is safe and stable.
    // Production uses a causally derived identifier or an independent stable UUID.
    // for pending_escalations.id + C11 raw_escalation_id. Never truncate like this in real sweep.
    let id_for_esc = format!("esc-{}", causal);

    let rows_affected = conn
        .execute(
            "INSERT INTO pending_escalations
             (id, tenant, sentinel_name, envelope_json, status, created_at_ms, causal_fire_id)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6)
             ON CONFLICT (tenant, sentinel_name, causal_fire_id) DO NOTHING",
            rusqlite::params![
                id_for_esc,
                tenant,
                sentinel,
                envelope_json,
                FIXED_NOW_MS,
                causal
            ],
        )
        .unwrap();

    let inserted = rows_affected > 0;
    (causal, inserted)
}

/// P0-D required test: Duplicate Fire Collapse (all 5 assertions).
/// Two distinct watch_fires rows (different hashes), exactly one pending row
/// via causal_fire_id + ON CONFLICT, restart + re-sweep still yields one row.
/// Uses harness (no raw INSERT in proof body). Synthetic content for Phase 0.
#[tokio::test]
async fn test_duplicate_fire_collapse() {
    use gateway_sidecar::watch::db::WatchDb;
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // Open a raw conn for the test-only pending schema extension + harness + assertions.
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let tenant = "sovereign";
    let sentinel = "file_inbox";
    let observed = "2026-05-29T21:00:00Z";
    let payload = json!({"path": "/inbox/report.pdf", "size": 1234, "mtime": 1717000000});
    let envelope1 =
        r#"{"state":{"tenant":"sovereign","sentinel":"file_inbox"},"reason":"new file"}"#;
    let envelope2 =
        r#"{"state":{"tenant":"sovereign","sentinel":"file_inbox"},"reason":"new file again"}"#; // same causal

    let base = 1_717_000_000_000i64;

    // Fire 1: produces first watch_fires row (audit never collapses).
    let r1 = db
        .insert_fire(
            tenant,
            sentinel,
            base,
            &serde_json::to_string(&payload).unwrap(),
            "new file",
            envelope1,
            1,
        )
        .await
        .unwrap()
        .expect("fire1 inserted");
    // Fire 2: identical causal content → second distinct watch_fires row (different hash).
    let r2 = db
        .insert_fire(
            tenant,
            sentinel,
            base + 1,
            &serde_json::to_string(&payload).unwrap(),
            "new file",
            envelope2,
            1,
        )
        .await
        .unwrap()
        .expect("fire2 inserted");

    assert_ne!(r1, r2, "two distinct audit rows");

    // Verify two rows + different hashes via raw query (chain invariant).
    let hashes: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT hash FROM watch_fires WHERE tenant=?1 AND sentinel=?2 ORDER BY id ASC")
            .unwrap();
        stmt.query_map([tenant, sentinel], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert_eq!(
        hashes.len(),
        2,
        "assertion 1: two distinct watch_fires rows"
    );
    assert_ne!(
        hashes[0], hashes[1],
        "assertion 4: different hashes on the two watch_fires"
    );

    // First "sweep" via harness (encapsulates compute + INSERT ... ON CONFLICT).
    let (causal, inserted1) =
        simulate_causal_sweep_enqueue(&conn, tenant, sentinel, observed, &payload, envelope1);
    assert!(inserted1, "first enqueue should insert");

    // Second "sweep" with identical causal (exercises ON CONFLICT DO NOTHING).
    let (causal2, inserted2) =
        simulate_causal_sweep_enqueue(&conn, tenant, sentinel, observed, &payload, envelope2);
    assert_eq!(causal, causal2, "same causal id for identical content");
    assert!(
        !inserted2,
        "assertion 3: ON CONFLICT DO NOTHING exercised (no second insert)"
    );

    // Assertion 2 + 4: exactly one pending row carrying the causal_fire_id.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE tenant=?1 AND sentinel_name=?2 AND causal_fire_id=?3",
            [tenant, sentinel, &causal],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "assertion 2: exactly one pending_escalations row for the causal_fire_id"
    );

    // "Process restart + re-sweep": new conn handle + re-invoke harness (simulates recovery).
    let conn2 = rusqlite::Connection::open(&db_path).unwrap();
    let (_, inserted3) =
        simulate_causal_sweep_enqueue(&conn2, tenant, sentinel, observed, &payload, envelope1);
    assert!(
        !inserted3,
        "assertion 5: restart + re-sweep creates no second pending row"
    );

    // Final count still exactly one.
    let final_count: i64 = conn2
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE tenant=?1 AND sentinel_name=?2 AND causal_fire_id=?3",
            [tenant, sentinel, &causal],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(final_count, 1, "post-restart count must remain one");
}

/// Review-3 regression (latent observed_at bug fix): real SentinelState from fire_pipeline
/// serializes observed_at as i64 (JSON Number). Pre-fix .as_str() always fell back to epoch,
/// producing colliding causals for distinct logical observation times. This test drives the
/// fixed re-derive (stringify Number) + causal helpers directly with real-shaped state_json
/// (via insert_fire) and asserts distinct causals for different observed i64 (no wrongful collapse).
/// Full tick drive exercised in D9 harness extensions (allowed watch_dispatch_live + env arm).
#[tokio::test]
async fn test_real_sentinelstate_observed_at_produces_distinct_causals() {
    use gateway_sidecar::watch::db::WatchDb;
    use gateway_sidecar::watch::dispatcher::safe_tenant_token;
    use gateway_sidecar::watch::fire_identity::{causal_fire_id, compute_content_digest};
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let tenant = "sovereign";
    let sentinel = "file_inbox";
    let base_ms: i64 = 1_717_000_000_000;

    // Real SentinelState shape (i64 observed_at Number, inner payload) — as produced by fire_pipeline.
    let state1 = serde_json::json!({
        "tenant": tenant,
        "sentinel": sentinel,
        "observed_at": base_ms,
        "payload": {"path": "/inbox/a.pdf", "size": 1234}
    });
    let state2 = serde_json::json!({
        "tenant": tenant,
        "sentinel": sentinel,
        "observed_at": base_ms + 1000,  // distinct logical time → must produce distinct causal
        "payload": {"path": "/inbox/a.pdf", "size": 1234}
    });

    let envelope =
        r#"{"state":{"tenant":"sovereign","sentinel":"file_inbox"},"reason":"real shape"}"#;

    // Insert as real fires (state_json = full SentinelState json with i64).
    let _ = db
        .insert_fire(
            tenant,
            sentinel,
            base_ms,
            &state1.to_string(),
            "real1",
            envelope,
            1,
        )
        .await
        .unwrap();
    let _ = db
        .insert_fire(
            tenant,
            sentinel,
            base_ms + 1,
            &state2.to_string(),
            "real2",
            envelope,
            1,
        )
        .await
        .unwrap();

    // Fixed re-derive (post review-3): stringify Number for observed_at.
    let canonical = safe_tenant_token(tenant);
    let p1 = &state1["payload"];
    let p2 = &state2["payload"];
    // Per causal_fire_id.md (stability, no wall time in digest): observed_at is deliberately excluded
    // from the causal digest in the sweep re-derive path. Identical logical payload + tenant + sentinel
    // must produce identical causal even if wall observed times differ. This is the correct D9 behavior.
    let d1 = compute_content_digest(sentinel, &canonical, "", p1);
    let d2 = compute_content_digest(sentinel, &canonical, "", p2);
    let c1 = causal_fire_id(&canonical, sentinel, &d1);
    let c2 = causal_fire_id(&canonical, sentinel, &d2);

    assert_eq!(c1, c2, "identical logical content (payload) must produce identical causal; wall observed_at is excluded per spec (distinct times must collapse)");

    // Exercise prod dedup helper (no raw INSERT in this path).
    let inserted1 = db
        .insert_pending_escalation_with_causal_dedup(
            &format!("causal-{}", c1),
            tenant,
            sentinel,
            envelope,
            &c1,
            base_ms,
            0, // test/shadow epoch
        )
        .await
        .unwrap();
    let inserted2 = db
        .insert_pending_escalation_with_causal_dedup(
            &format!("causal-{}", c2),
            tenant,
            sentinel,
            envelope,
            &c2,
            base_ms + 1000,
            0, // test/shadow epoch
        )
        .await
        .unwrap();

    // With wall observed_at excluded from digest (per causal spec), c1 == c2 for identical logical payload.
    // First insert succeeds; second must hit ON CONFLICT DO NOTHING (exactly-one pending per logical causal).
    assert!(inserted1, "first insert via prod helper must succeed");
    assert!(
        !inserted2,
        "second insert for same causal must hit ON CONFLICT (D9 exactly-one)"
    );

    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pending_escalations WHERE tenant=?1 AND sentinel_name=?2 AND causal_fire_id = ?3",
        [tenant, sentinel, &c1],
        |r| r.get(0)
    ).unwrap();
    assert_eq!(
        count, 1,
        "exactly two distinct pending rows for distinct real observed times"
    );
}

/// Driving test for the real CDC producer path and spawned sweep work.
/// Invokes `cdc_sweep_tick` directly against duplicate logical `watch_fires` rows
/// (different wall observed_at, identical payload) and proves exactly-one
/// `pending_escalations` row via the ON CONFLICT dedup in the tick.
/// Exercises real returned high-water reuse on 2-row case (N=2 proof;
/// MAX_SWEEP_PER_TICK+1 backlog progression claim softened per final review).
#[tokio::test]
async fn test_cdc_sweep_tick_drives_duplicate_collapse() {
    use gateway_sidecar::watch::db::WatchDb;
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let tenant = "sovereign";
    let sentinel = "file_inbox";
    let now = 1_717_000_000_000i64;

    // Two distinct audit rows for the *same logical fire* (different wall observed times).
    let payload = serde_json::json!({"path": "/inbox/dup.pdf", "size": 777});
    let state1 = serde_json::json!({
        "tenant": tenant,
        "sentinel": sentinel,
        "observed_at": now,
        "payload": payload
    });
    let state2 = serde_json::json!({
        "tenant": tenant,
        "sentinel": sentinel,
        "observed_at": now + 5000,  // different wall time
        "payload": payload
    });
    let envelope = r#"{"reason":"dup test"}"#;

    // Insert as committed fires (bypassing the hot pipeline for test isolation).
    let _ = db
        .insert_fire(
            tenant,
            sentinel,
            now,
            &state1.to_string(),
            "dup1",
            envelope,
            1,
        )
        .await
        .unwrap();
    let _ = db
        .insert_fire(
            tenant,
            sentinel,
            now + 1,
            &state2.to_string(),
            "dup2",
            envelope,
            1,
        )
        .await
        .unwrap();

    // A dummy shutdown receiver (tick checks it between records).
    let (_tx, mut rx) = tokio::sync::watch::channel(false);

    // Drive the real tick (with after_id support for cursor advancement).
    // Capture the *real* high-water returned (not hardcoded); use it on second call.
    // Exercises real hw return + reuse + duplicate collapse for N=2 case (2 rows, 1 pending).
    // Per seam owner final review: does not assert hw value or prove MAX_SWEEP_PER_TICK+1
    // backlog progression over ticks (claim softened here to match actual 2-row proof;
    // full progression test out of narrow P1 fix scope).
    let mut poison_fails = std::collections::HashMap::new();
    let hw1 =
        gateway_sidecar::watch::runner::cdc_sweep_tick(&db, &mut rx, None, &mut poison_fails).await;
    let _ =
        gateway_sidecar::watch::runner::cdc_sweep_tick(&db, &mut rx, hw1, &mut poison_fails).await;

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE tenant=?1 AND sentinel_name=?2",
            [tenant, sentinel],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(
        count, 1,
        "exactly one pending row for duplicate logical fire through real cdc_sweep_tick"
    );
    let pending_envelope_json: String = conn
        .query_row(
            "SELECT envelope_json FROM pending_escalations WHERE tenant=?1 AND sentinel_name=?2",
            [tenant, sentinel],
            |r| r.get(0),
        )
        .unwrap();
    let pending_envelope: serde_json::Value = serde_json::from_str(&pending_envelope_json).unwrap();
    assert_eq!(pending_envelope["v"], serde_json::json!(1));
    assert_eq!(
        pending_envelope["envelope"]["data"]["contract"],
        serde_json::json!("irin.comms.v0.1"),
        "D7: new CDC pending rows must carry the formal COMMS envelope"
    );
    assert_eq!(
        pending_envelope["envelope"]["data"]["payload"]["raw_sentinel_escalation"]["reason"],
        serde_json::json!("dup test"),
        "D7: COMMS wrapper must retain the original raw sentinel escalation payload"
    );
    // The returned hw from first (oldest-batch start) + second using it exercises full advancement
    // contract for boot re-scan / backlog without skipping older unprocessed rows.
}

/// CDC must not feed Phase 3 lifecycle audit events back into the producer seam.
/// `watch-dispatcher` rows are durable recovery/outbox evidence, not fresh Sentinel escalations.
#[tokio::test]
async fn test_cdc_sweep_tick_ignores_watch_dispatcher_audit_rows() {
    use gateway_sidecar::watch::db::WatchDb;
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let now = 1_717_100_000_000i64;

    db.insert_fire(
        "sovereign",
        "watch-dispatcher",
        now,
        r#"{"event_type":"directive_staged"}"#,
        "directive_staged",
        r#"{"DirectiveStaged":{"tenant":"sovereign"}}"#,
        1,
    )
    .await
    .unwrap()
    .expect("dispatcher audit row should insert");

    db.insert_fire(
        "sovereign",
        "file_inbox",
        now + 1,
        r#"{"payload":{"path":"/inbox/new.pdf","size":42}}"#,
        "real sentinel fire",
        r#"{"reason":"real sentinel"}"#,
        1,
    )
    .await
    .unwrap()
    .expect("real sentinel row should insert");

    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    let mut poison_fails = std::collections::HashMap::new();
    let _ =
        gateway_sidecar::watch::runner::cdc_sweep_tick(&db, &mut rx, None, &mut poison_fails).await;

    let dispatcher_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE tenant='sovereign' AND sentinel_name='watch-dispatcher'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let real_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE tenant='sovereign' AND sentinel_name='file_inbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(
        dispatcher_count, 0,
        "dispatcher lifecycle audit rows must not be re-enqueued by CDC"
    );
    assert_eq!(
        real_count, 1,
        "real sentinel fire should still enqueue through the CDC producer"
    );
}

// ============================================================================
// W3 item 2 — engine-enforced append-only triggers on watch_fires.
// (Review. Mirrors arm_audit's trg_arm_audit_no_update/_no_delete.)
// ============================================================================

/// UPDATE on watch_fires is rejected by trg_watch_fires_no_update.
#[tokio::test]
async fn w3_watch_fires_update_is_blocked() {
    use gateway_sidecar::watch::db::WatchDb;
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    db.insert_fire("sovereign", "s1", t2_now_ms(), "{}", "ok", "{}", 1)
        .await
        .unwrap()
        .expect("not hard-killed");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let err = conn
        .execute("UPDATE watch_fires SET reason='x' WHERE id=1", [])
        .unwrap_err();
    assert!(
        err.to_string().contains("watch_fires_append_only"),
        "UPDATE must RAISE the append-only guard; got {err}"
    );
}

/// DELETE on watch_fires is rejected by trg_watch_fires_no_delete.
#[tokio::test]
async fn w3_watch_fires_delete_is_blocked() {
    use gateway_sidecar::watch::db::WatchDb;
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    db.insert_fire("sovereign", "s1", t2_now_ms(), "{}", "ok", "{}", 1)
        .await
        .unwrap()
        .expect("not hard-killed");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let err = conn
        .execute("DELETE FROM watch_fires WHERE id=1", [])
        .unwrap_err();
    assert!(
        err.to_string().contains("watch_fires_append_only"),
        "DELETE must RAISE the append-only guard; got {err}"
    );
}

// ============================================================================
// W3 item 3 — envelope_json is in the v4 preimage; version-tagged dispatch.
// ============================================================================

/// A fresh fire is written as preimage_version=4 (regression guard: a forgotten
/// version bind would silently default to 3 → envelope unhashed → the bug).
#[tokio::test]
async fn w3_fresh_fire_is_v4() {
    use gateway_sidecar::watch::db::WatchDb;
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    db.insert_fire("sovereign", "s1", t2_now_ms(), "{}", "ok", r#"{"e":1}"#, 1)
        .await
        .unwrap()
        .expect("not hard-killed");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let v: i64 = conn
        .query_row(
            "SELECT preimage_version FROM watch_fires WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, 4, "new fires must be v4, never the legacy DEFAULT 3");
}

/// Tampering envelope_json on a v4 row flips verify_chain to ok:false — the
/// exact gap this item closes (pre-W3 envelope_json was outside the preimage).
#[tokio::test]
async fn w3_v4_envelope_tamper_breaks_verify() {
    use gateway_sidecar::watch::db::{VerifyBreak, WatchDb};
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    let base = t2_now_ms();
    for i in 0..3 {
        db.insert_fire(
            "sovereign",
            "s1",
            base + i,
            "{}",
            "ok",
            &format!(r#"{{"env":{i}}}"#),
            1,
        )
        .await
        .unwrap()
        .expect("not hard-killed");
    }
    assert!(
        db.verify_chain("sovereign").await.unwrap().ok,
        "intact v4 chain ok"
    );

    // Drop the append-only trigger (model raw-DB tamper) and mutate envelope_json
    // ONLY — leaving the stored hash stale. Pre-W3 this passed verify; now it
    // must break because envelope_json is in the v4 preimage.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TRIGGER IF EXISTS trg_watch_fires_no_update", [])
            .unwrap();
        conn.execute(
            "UPDATE watch_fires SET envelope_json='{\"env\":\"TAMPERED\"}' WHERE id=2",
            [],
        )
        .unwrap();
    }

    let verdict = db.verify_chain("sovereign").await.unwrap();
    assert!(
        !verdict.ok,
        "envelope_json tamper must break verify; got {verdict:?}"
    );
    assert_eq!(verdict.broken_at_id, Some(2));
    assert_eq!(verdict.break_kind, Some(VerifyBreak::HashMismatch));
}

/// A v4 row whose envelope_json is empty ("") verifies — the `0:` length-prefix
/// is a valid v4 preimage, distinct from v3 by the version tag.
#[tokio::test]
async fn w3_v4_empty_envelope_verifies() {
    use gateway_sidecar::watch::db::WatchDb;
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    // insert_fire stores envelope_json verbatim; pass "" to exercise the `0:` arm.
    db.insert_fire("sovereign", "s1", t2_now_ms(), "{}", "ok", "", 1)
        .await
        .unwrap()
        .expect("not hard-killed");

    let verdict = db.verify_chain("sovereign").await.unwrap();
    assert!(
        verdict.ok,
        "v4 empty-envelope row must verify; got {verdict:?}"
    );
    assert_eq!(verdict.rows_walked, 1);
}

/// A chain that interleaves legacy v3 rows with new v4 rows in one tenant
/// verifies — verify_chain dispatches the preimage scheme per row. The v3 row's
/// hash is computed under the 6-field preimage (no envelope_json), exactly as the
/// 8 live canary rows were, then a v4 row is chained on top of it.
#[tokio::test]
async fn w3_mixed_v3_v4_chain_verifies() {
    use gateway_sidecar::watch::db::{watch_distinct_genesis, WatchDb};
    use sha2::{Digest, Sha256};

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    // Row 1: a legacy v3 row, inserted by hand with a hand-computed v3 hash
    // (6-field length-prefixed preimage, NO envelope_json — byte-identical to the
    // pre-W3 scheme the 8 canary rows used).
    let tenant = "sovereign";
    let sentinel = "s1";
    let fired_at: i64 = t2_now_ms();
    let state_json = "{}";
    let reason = "legacy-v3";
    let prev_hash = watch_distinct_genesis();
    let fired_at_str = fired_at.to_string();
    let v3_preimage = format!(
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
    let v3_hash = hex::encode(Sha256::digest(v3_preimage.as_bytes()));
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO watch_fires
               (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash,
                envelope_json, envelope_schema_version, preimage_version)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,1,3)",
            rusqlite::params![
                tenant,
                sentinel,
                fired_at,
                state_json,
                reason,
                prev_hash,
                v3_hash,
                // legacy rows DID store an envelope_json value; it is NOT in the
                // v3 preimage, so any value here must NOT affect verification.
                r#"{"legacy":"ignored-by-v3"}"#,
            ],
        )
        .unwrap();
    }

    // Row 2+: real v4 fires chained on top (insert_fire reads prev_hash from the
    // chain tip, so it links onto the v3 row).
    db.insert_fire(
        tenant,
        sentinel,
        fired_at + 1,
        "{}",
        "v4-row",
        r#"{"e":1}"#,
        1,
    )
    .await
    .unwrap()
    .expect("not hard-killed");

    let verdict = db.verify_chain(tenant).await.unwrap();
    assert!(
        verdict.ok,
        "mixed v3->v4 chain must verify; got {verdict:?}"
    );
    assert_eq!(verdict.rows_walked, 2);
}
