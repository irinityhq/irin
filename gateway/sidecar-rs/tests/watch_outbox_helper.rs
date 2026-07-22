//! Phase 3a shared outbox insert helper tests.

use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::dispatcher::{
    directive_clock_skew_rejected_total, max_allowed_skew_ms,
};
use gateway_sidecar::watch::outbox::{
    outbox_insert_with_skew_normalize, DirectiveOutboxRow, OutboxAuditEvent, OutboxError,
};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// Stage-to-expiry window the test fixtures model (mirrors the dispatcher default).
const TEST_WINDOW_MS: i64 = 90_000;

async fn migrated_db_path() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    drop(db);
    (tmp, db_path)
}

fn open_checked(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn
}

fn insert_pending(conn: &Connection, tenant: &str, id: &str) {
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES (?1, ?2, 'queue-depth-watch', '{}', 'council_response_staged', 1000)",
        params![id, tenant],
    )
    .unwrap();
}

/// Build a directive row whose `created_at_ms` and `expires_at_ms` come from a SINGLE
/// stage-time sample (`created_at_ms`, `created_at_ms + TEST_WINDOW_MS`) — the production
/// single-clock-sample contract the helper relies on.
fn row(id: &str, tenant: &str, in_response_to: &str, created_at_ms: i64) -> DirectiveOutboxRow {
    DirectiveOutboxRow {
        id: id.to_string(),
        in_response_to: in_response_to.to_string(),
        tenant: tenant.to_string(),
        status: "staged".to_string(),
        verdict: "Act".to_string(),
        authority: "recommend".to_string(),
        envelope_json: "{}".to_string(),
        envelope_json_canonical: "{}".to_string(),
        signature_b64: "sig".to_string(),
        signing_kid: "sidecar-v1-test".to_string(),
        council_session_id: Some("sess-test".to_string()),
        council_cost_usd: Some(0.01),
        created_at_ms,
        expires_at_ms: created_at_ms + TEST_WINDOW_MS,
    }
}

#[test]
fn helper_normalizes_backward_skew_and_emits_audit() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_tmp, db_path) = rt.block_on(migrated_db_path());
    let mut conn = open_checked(&db_path);
    insert_pending(&conn, "acme", "esc-old");
    insert_pending(&conn, "acme", "esc-new");

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    let mut audit = Vec::new();
    let inserted = outbox_insert_with_skew_normalize(
        &tx,
        row("dir-old", "acme", "esc-old", 1_000_000),
        &mut audit,
    )
    .unwrap();
    assert_eq!(inserted, "dir-old");
    tx.commit().unwrap();

    // dir-new stages 5ms in the past (backward skew) relative to the prior row.
    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    audit.clear();
    let inserted = outbox_insert_with_skew_normalize(
        &tx,
        row("dir-new", "acme", "esc-new", 999_995),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    assert_eq!(inserted, "dir-new");
    let created_at: i64 = conn
        .query_row(
            "SELECT created_at_ms FROM directive_outbox WHERE id = 'dir-new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created_at, 1_000_001);
    assert_eq!(
        audit,
        vec![
            OutboxAuditEvent::DirectiveClockSkewNormalized {
                directive_id: "dir-new".to_string(),
                tenant: "acme".to_string(),
                original_ms: 999_995,
                normalized_ms: 1_000_001,
            },
            OutboxAuditEvent::DirectiveStaged {
                tenant: "acme".to_string(),
                directive_id: "dir-new".to_string(),
                in_response_to: "esc-new".to_string(),
            }
        ]
    );
}

#[test]
fn helper_preserves_monotonic_host_time_without_skew_audit() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_tmp, db_path) = rt.block_on(migrated_db_path());
    let mut conn = open_checked(&db_path);
    insert_pending(&conn, "acme", "esc-old");
    insert_pending(&conn, "acme", "esc-new");

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    let mut audit = Vec::new();
    outbox_insert_with_skew_normalize(
        &tx,
        row("dir-old", "acme", "esc-old", 1_000_000),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    audit.clear();
    let inserted = outbox_insert_with_skew_normalize(
        &tx,
        row("dir-new", "acme", "esc-new", 1_000_010),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    assert_eq!(inserted, "dir-new");
    let created_at: i64 = conn
        .query_row(
            "SELECT created_at_ms FROM directive_outbox WHERE id = 'dir-new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created_at, 1_000_010);
    assert_eq!(
        audit,
        vec![OutboxAuditEvent::DirectiveStaged {
            tenant: "acme".to_string(),
            directive_id: "dir-new".to_string(),
            in_response_to: "esc-new".to_string(),
        }]
    );
}

#[test]
fn helper_recovers_existing_id_on_composite_unique_collision() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_tmp, db_path) = rt.block_on(migrated_db_path());
    let mut conn = open_checked(&db_path);
    insert_pending(&conn, "acme", "esc-dupe");

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    let mut audit = Vec::new();
    outbox_insert_with_skew_normalize(
        &tx,
        row("dir-existing", "acme", "esc-dupe", 1_000_000),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    audit.clear();
    let recovered = outbox_insert_with_skew_normalize(
        &tx,
        row("dir-new-attempt", "acme", "esc-dupe", 1_000_010),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    assert_eq!(recovered, "dir-existing");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE tenant = 'acme' AND in_response_to = 'esc-dupe'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        audit,
        vec![OutboxAuditEvent::OutboxRecoveredFromRestart {
            tenant: "acme".to_string(),
            directive_id: "dir-existing".to_string(),
            in_response_to: "esc-dupe".to_string(),
        }]
    );
}

/// Regression: when backward clock skew normalizes
/// `created_at_ms` forward, `expires_at_ms` must shift by the same delta so the directive's
/// authorization window keeps its full length. created_at_ms and expires_at_ms come from one
/// stage-time sample; if only `created_at_ms` is moved forward, the persisted expiry is left
/// behind, the window silently shrinks, and the T21c TTL fence can sweep a legitimately fresh
/// directive (or it is born already-expired). The normalization delta is >= 0, so the corrected
/// expiry can only restore the intended window, never shorten it.
#[test]
fn helper_shifts_expiry_by_skew_delta_preserving_ttl_window() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_tmp, db_path) = rt.block_on(migrated_db_path());
    let mut conn = open_checked(&db_path);
    insert_pending(&conn, "acme", "esc-old");
    insert_pending(&conn, "acme", "esc-new");

    // Anchor the per-tenant MAX(created_at_ms) at 1_000_000.
    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    let mut audit = Vec::new();
    outbox_insert_with_skew_normalize(
        &tx,
        row("dir-old", "acme", "esc-old", 1_000_000),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    // dir-new stages 5ms in the past. Its expires_at_ms was stamped from the same stage sample
    // (created + TEST_WINDOW_MS), so the helper must shift it by the normalization delta.
    let staged_created = 999_995_i64;
    let expires_in = staged_created + TEST_WINDOW_MS;

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    audit.clear();
    outbox_insert_with_skew_normalize(
        &tx,
        row("dir-new", "acme", "esc-new", staged_created),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    let (created_at, expires_at): (i64, i64) = conn
        .query_row(
            "SELECT created_at_ms, expires_at_ms FROM directive_outbox WHERE id = 'dir-new'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    // created_at normalized forward to prior_max + 1.
    assert_eq!(created_at, 1_000_001);
    let delta = created_at - staged_created; // +6ms
                                             // Expiry shifted by the SAME delta — without the fix it would persist as the raw expires_in.
    assert_eq!(expires_at, expires_in + delta);
    // The full window survives skew normalization.
    assert_eq!(
        expires_at - created_at,
        TEST_WINDOW_MS,
        "window must survive skew normalization"
    );
    assert!(
        expires_at > created_at,
        "a normalized directive must not be born expired"
    );
}

/// Council canary-gate test: when there is no skew (created_at_ms is already monotonic), the
/// normalization delta is 0 and `expires_at_ms` is persisted bitwise-identical to the input —
/// the common path must not perturb the absolute expiry.
#[test]
fn helper_no_skew_preserves_exact_expiry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_tmp, db_path) = rt.block_on(migrated_db_path());
    let mut conn = open_checked(&db_path);
    insert_pending(&conn, "acme", "esc-solo");

    let staged_created = 5_000_000_i64;
    let r = row("dir-solo", "acme", "esc-solo", staged_created);
    let expires_in = r.expires_at_ms;

    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    let mut audit = Vec::new();
    outbox_insert_with_skew_normalize(&tx, r, &mut audit).unwrap();
    tx.commit().unwrap();

    let (created_at, expires_at): (i64, i64) = conn
        .query_row(
            "SELECT created_at_ms, expires_at_ms FROM directive_outbox WHERE id = 'dir-solo'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        created_at, staged_created,
        "no prior row → created_at unchanged"
    );
    assert_eq!(expires_at, expires_in, "delta 0 → expiry bitwise-identical");
    // No skew event on the monotonic common path.
    assert_eq!(
        audit,
        vec![OutboxAuditEvent::DirectiveStaged {
            tenant: "acme".to_string(),
            directive_id: "dir-solo".to_string(),
            in_response_to: "esc-solo".to_string(),
        }]
    );
}

/// P2 / #45 no-clock invariant guard (Council R1 action #4). The insert helper must derive
/// created_at_ms and expires_at_ms from the row's OWN base and read NO clock of its own — the
/// "two clock samples" bug (#45) came from a second `unix_now_ms()` read. Pin the exact
/// signature as a `fn` pointer: re-introducing a `now: i64` / `SystemTime` parameter (a second
/// clock sample) changes this type and fails to COMPILE. Turns the invariant from a comment into
/// an enforced gate.
#[test]
fn helper_signature_reads_no_clock() {
    let _pinned: fn(
        &rusqlite::Transaction<'_>,
        DirectiveOutboxRow,
        &mut Vec<OutboxAuditEvent>,
    ) -> Result<String, OutboxError> = outbox_insert_with_skew_normalize;
}

/// P2 clock-skew circuit-breaker: when a poisoned per-tenant `prior_max` would force a
/// normalization delta past `MAX_ALLOWED_SKEW_MS`, the helper REFUSES to stage the row
/// (`Err(ClockSkewExceeded)`) instead of floating the absolute authorization window forward by
/// the delta. Fail-safe: the row is never inserted (no dispatch, no spend) and the rejection is
/// counted. The expected cap is read from `max_allowed_skew_ms()` so the test is hermetic against
/// any ambient `MAX_ALLOWED_SKEW_MS` env override.
#[test]
fn helper_refuses_to_stage_when_skew_delta_exceeds_cap() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_tmp, db_path) = rt.block_on(migrated_db_path());
    let mut conn = open_checked(&db_path);
    insert_pending(&conn, "acme", "esc-anchor");
    insert_pending(&conn, "acme", "esc-poisoned");

    // Anchor the per-tenant MAX(created_at_ms) at 10_000_000 (the "poison" forward floor).
    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    let mut audit = Vec::new();
    outbox_insert_with_skew_normalize(
        &tx,
        row("dir-anchor", "acme", "esc-anchor", 10_000_000),
        &mut audit,
    )
    .unwrap();
    tx.commit().unwrap();

    let cap = max_allowed_skew_ms();
    // A row staged so far in the past that normalization (to prior_max + 1) overshoots the cap.
    // normalized_at = 10_000_001; pick created so delta = (cap + 1000) > cap.
    let staged_created = 10_000_001 - (cap + 1_000);

    let before = directive_clock_skew_rejected_total();
    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    audit.clear();
    let err = outbox_insert_with_skew_normalize(
        &tx,
        row("dir-poisoned", "acme", "esc-poisoned", staged_created),
        &mut audit,
    )
    .unwrap_err();
    // tx is dropped (rolled back) — the over-skewed row must never persist.
    drop(tx);

    match err {
        OutboxError::ClockSkewExceeded {
            directive_id,
            tenant,
            skew_delta_ms,
            max_skew_ms,
        } => {
            assert_eq!(directive_id, "dir-poisoned");
            assert_eq!(tenant, "acme");
            assert_eq!(max_skew_ms, cap);
            assert_eq!(skew_delta_ms, cap + 1_000);
            assert!(skew_delta_ms > max_skew_ms, "delta must exceed the cap");
        }
        other => panic!("expected ClockSkewExceeded, got {other:?}"),
    }

    // Fail-safe: no insert, no audit event.
    assert!(audit.is_empty(), "a refused stage emits no audit event");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE id = 'dir-poisoned'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "over-skewed directive must not be persisted");

    // Observability: the breaker increments the rejection counter (global atomic — assert a
    // monotonic increase rather than an exact delta, robust to parallel test execution).
    assert!(
        directive_clock_skew_rejected_total() > before,
        "clock-skew rejection must bump the counter"
    );
}
