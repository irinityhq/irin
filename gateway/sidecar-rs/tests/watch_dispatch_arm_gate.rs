//! §7 items 9 + 10 — enqueue backpressure and the recovery disarm re-check.
//!
//! Item 10: every path through `recover_council_response_staged` ends in a
//! SIGNED outbox row, so signing is gated on a currently-valid attested arm
//! (the same `attest::verify_arm_row` decision the spend reserve makes).
//! Refusal parks the row (`RecoveryOutcome::ArmHeld`, SkewHeld shape): stays
//! `council_response_staged`, nothing signed, self-heals under a valid arm.
//!
//! Item 9: `pending_escalations` non-terminal rows are never pruned, so the
//! enqueue refuses (transient error, sweep stalls + retries) once a tenant's
//! non-terminal depth reaches the cap.

use gateway_sidecar::keymgmt::DirectiveSigningKey;
use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::dispatcher::run_boot_hydration_sweep;
use tempfile::TempDir;

mod arm_attest_common;

/// Unarmed fixture — migrations only, NO active_arm row.
async fn unarmed_db() -> (TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    drop(db);
    (tmp, db_path)
}

fn seed_staged_row(db_path: &std::path::Path, id: &str, tenant: &str) {
    let council_response_json = serde_json::json!({
        "body": format!(
            "```json\n{{\"schema\":\"irin.directive.proposal.v1\",\"in_response_to\":\"{id}\",\"authority\":\"recommend\",\"verdict\":\"Dismiss\",\"rationale\":\"no action required\"}}\n```"
        ),
        "headers": {
            "x-council-session-id": "sess-arm-gate",
            "x-total-cost-usd": "0.01"
        }
    })
    .to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, council_response_json, created_at_ms)
         VALUES (?1, ?2, 'test-sentinel', '{}', 'council_response_staged', ?3, 1234567890000)",
        rusqlite::params![id, tenant, council_response_json],
    )
    .unwrap();
}

fn outbox_count(db_path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM directive_outbox", [], |r| r.get(0))
        .unwrap()
}

fn pending_status(db_path: &std::path::Path, tenant: &str, id: &str) -> String {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT status FROM pending_escalations WHERE tenant = ?1 AND id = ?2",
        rusqlite::params![tenant, id],
        |r| r.get(0),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Item 10 — disarm re-check at the recovery sign seam
// ---------------------------------------------------------------------------

/// The core hole from the punch list: a staged row on a DISARMED box must not
/// be signed by boot hydration. It parks (ArmHeld), nothing reaches the
/// outbox, and the staged status is untouched.
#[tokio::test]
async fn t13_unarmed_boot_hydration_parks_staged_row_signs_nothing() {
    let (tmp, db_path) = unarmed_db().await;
    let db = WatchDb::open(&db_path).await.unwrap();
    let identity_path = tmp.path().join("directive_identity.json");
    let (key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();
    seed_staged_row(&db_path, "armgate-001", "sovereign");

    let report = run_boot_hydration_sweep(&db, token, &key).await.unwrap();

    assert_eq!(report.rows_examined, 1);
    assert_eq!(report.arm_held, 1, "row must park, not recover");
    assert_eq!(report.staged_rows_recovered, 0);
    assert_eq!(report.parse_failures, 0, "gate must run BEFORE parse");
    assert_eq!(
        outbox_count(&db_path),
        0,
        "nothing may be signed while disarmed"
    );
    assert_eq!(
        pending_status(&db_path, "sovereign", "armgate-001"),
        "council_response_staged",
        "parked row keeps its status (work product preserved)"
    );
    assert!(
        report
            .audit_events
            .iter()
            .any(|e| e.event_type() == "recovery_arm_held"),
        "park must be auditable"
    );
}

/// Positive control: the identical row recovers under a valid arm.
#[tokio::test]
async fn t13b_armed_boot_hydration_recovers_same_row() {
    let (tmp, db_path) = unarmed_db().await;
    let db = WatchDb::open(&db_path).await.unwrap();
    arm_attest_common::arm_db_for_reserve_test(&db).await;
    let identity_path = tmp.path().join("directive_identity.json");
    let (key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();
    seed_staged_row(&db_path, "armgate-002", "sovereign");

    let report = run_boot_hydration_sweep(&db, token, &key).await.unwrap();

    assert_eq!(report.staged_rows_recovered, 1);
    assert_eq!(report.arm_held, 0);
    assert_eq!(outbox_count(&db_path), 1, "signed directive row expected");
}

/// Self-heal: parked while disarmed, recovered by the next sweep once armed.
/// This is the SkewHeld park contract applied to disarm.
#[tokio::test]
async fn t13c_parked_row_self_heals_on_next_armed_sweep() {
    let (tmp, db_path) = unarmed_db().await;
    let db = WatchDb::open(&db_path).await.unwrap();
    let identity_path = tmp.path().join("directive_identity.json");
    let (key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();
    seed_staged_row(&db_path, "armgate-003", "sovereign");

    let first = run_boot_hydration_sweep(&db, token, &key).await.unwrap();
    assert_eq!(first.arm_held, 1);
    assert_eq!(outbox_count(&db_path), 0);

    arm_attest_common::arm_db_for_reserve_test(&db).await;
    let (key2, token2) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();
    let second = run_boot_hydration_sweep(&db, token2, &key2).await.unwrap();

    assert_eq!(
        second.staged_rows_recovered, 1,
        "parked row completes under arm"
    );
    assert_eq!(second.arm_held, 0);
    assert_eq!(outbox_count(&db_path), 1);
}

/// An arm whose SIGNED spend window has lapsed is a disarm for signing
/// purposes: recovery parks exactly like the no-arm case.
#[tokio::test]
async fn t13d_expired_arm_window_parks_like_disarm() {
    let (tmp, db_path) = unarmed_db().await;
    let db = WatchDb::open(&db_path).await.unwrap();
    // Signed tap 25h ago: past the 24h boot-locked spend window.
    let stale_iat = arm_attest_common::now_ms() - 25 * 60 * 60 * 1000;
    arm_attest_common::sign_and_write_active_arm(&db, 5000, 0, stale_iat, None, None, None).await;
    let identity_path = tmp.path().join("directive_identity.json");
    let (key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();
    seed_staged_row(&db_path, "armgate-004", "sovereign");

    let report = run_boot_hydration_sweep(&db, token, &key).await.unwrap();

    assert_eq!(report.arm_held, 1, "expired window must park, not sign");
    assert_eq!(report.staged_rows_recovered, 0);
    assert_eq!(outbox_count(&db_path), 0);
}

/// A broken wall clock (SystemTime saturated to 0, or pre-epoch) must refuse
/// the arm outright — verify_arm_row checks the clock BEFORE anything else,
/// so an expired arm can never look fresh under now_ms == 0 (fail-closed,
/// regression review).
#[test]
fn t13e_invalid_clock_refuses_arm_before_anything_else() {
    use gateway_sidecar::watch::attest::{verify_arm_row, ActiveArmRow};
    let row = ActiveArmRow {
        build_id: String::new(),
        enabled_surface: String::new(),
        effective_daily_cap_cents: 0,
        tenant: String::new(),
        armed_epoch: 0,
        exp_at_ms: 0,
        challenge_bytes: Vec::new(),
        signature_der: Vec::new(),
        credential_id: String::new(),
        credential_type: String::new(),
        authenticator_data: None,
        client_data_json: None,
    };
    // Clock check precedes the registry check: a None registry would yield
    // "registry_unloaded", so getting "clock_invalid" proves the ordering.
    assert_eq!(verify_arm_row(&row, None, 0), Err("clock_invalid"));
    assert_eq!(verify_arm_row(&row, None, -1), Err("clock_invalid"));
}

// ---------------------------------------------------------------------------
// Item 9 — pending_escalations enqueue backpressure
// ---------------------------------------------------------------------------

async fn enqueue(
    db: &WatchDb,
    id: &str,
    tenant: &str,
    causal: &str,
    cap: i64,
) -> anyhow::Result<bool> {
    db.insert_pending_escalation_with_causal_dedup_capped(
        id,
        tenant,
        "test-sentinel",
        "{}",
        causal,
        1_000,
        0,
        cap,
    )
    .await
}

/// At the cap, a NEW causal is refused with a transient error; depth stays put.
#[tokio::test]
async fn t14_enqueue_refuses_new_causal_at_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    for i in 0..3 {
        assert!(
            enqueue(&db, &format!("e-{i}"), "sovereign", &format!("c-{i}"), 3)
                .await
                .unwrap()
        );
    }
    let err = enqueue(&db, "e-3", "sovereign", "c-3", 3)
        .await
        .expect_err("4th non-terminal row must be refused");
    assert!(
        err.to_string().contains("backpressure"),
        "refusal must be identifiable: {err}"
    );
    // The refusal is NOT a tokio_rusqlite/rusqlite error, so the CDC sweep's
    // classifier (`cdc_error_is_transient`: non-DB errors → transient) stalls
    // and retries instead of counting it toward the poison skip.
    assert!(
        err.downcast_ref::<tokio_rusqlite::Error>().is_none(),
        "cap refusal must classify TRANSIENT in the sweep (no poison-skip drop)"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let depth: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_escalations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(depth, 3);
}

/// A duplicate causal at the cap is benign dedup (Ok(false)), never an error —
/// the sweep cursor must not stall behind an already-enqueued fire.
#[tokio::test]
async fn t14b_duplicate_causal_at_cap_is_dedup_not_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let db = WatchDb::open(&tmp.path().join("watch.db")).await.unwrap();
    db.run_migrations().await.unwrap();

    for i in 0..2 {
        enqueue(&db, &format!("e-{i}"), "sovereign", &format!("c-{i}"), 2)
            .await
            .unwrap();
    }
    let deduped = enqueue(&db, "e-0-again", "sovereign", "c-0", 2)
        .await
        .unwrap();
    assert!(!deduped, "existing causal reports dedup even at the cap");
}

/// Terminal rows do not count toward the cap — the gate tracks live depth,
/// not table history (prune handles history).
#[tokio::test]
async fn t14c_terminal_rows_do_not_count_toward_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    for i in 0..2 {
        enqueue(&db, &format!("e-{i}"), "sovereign", &format!("c-{i}"), 2)
            .await
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE pending_escalations SET status = 'dead_lettered' WHERE id = 'e-0'",
            [],
        )
        .unwrap();
    }
    assert!(
        enqueue(&db, "e-2", "sovereign", "c-2", 2).await.unwrap(),
        "freed capacity (terminal row) must admit a new escalation"
    );
}

/// The cap is per tenant: one tenant at its ceiling cannot starve another.
#[tokio::test]
async fn t14d_cap_is_per_tenant() {
    let tmp = tempfile::tempdir().unwrap();
    let db = WatchDb::open(&tmp.path().join("watch.db")).await.unwrap();
    db.run_migrations().await.unwrap();

    for i in 0..2 {
        enqueue(&db, &format!("a-{i}"), "tenant-a", &format!("ca-{i}"), 2)
            .await
            .unwrap();
    }
    assert!(enqueue(&db, "a-2", "tenant-a", "ca-2", 2).await.is_err());
    assert!(
        enqueue(&db, "b-0", "tenant-b", "cb-0", 2).await.unwrap(),
        "tenant-b must be unaffected by tenant-a's backlog"
    );
}
