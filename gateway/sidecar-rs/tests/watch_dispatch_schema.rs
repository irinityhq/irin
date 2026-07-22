//! Phase 3a storage schema tests for dispatcher + durable outbox.
//!
//! C11 invariant: escalation ids are raw per tenant. The schema must allow
//! two tenants to share the same raw escalation id while still rejecting a
//! replay within the same tenant.

use base64::Engine;
use gateway_sidecar::watch::db::{watch_distinct_genesis, WatchDb};
use gateway_sidecar::watch::outbox::DirectiveAuthority;
use rusqlite::{params, Connection, Error};
use std::path::{Path, PathBuf};

mod arm_attest_common;

async fn migrated_db_path() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    db.run_migrations().await.unwrap();
    drop(db);
    (tmp, db_path)
}

fn open_checked(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = ?1
        )",
        params![table],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

fn insert_pending(
    conn: &Connection,
    tenant: &str,
    id: &str,
    directive_id: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, directive_id, created_at_ms)
         VALUES (?1, ?2, 'queue-depth-watch', '{}', 'queued', ?3, 1000)",
        params![id, tenant, directive_id],
    )
}

fn insert_outbox(
    conn: &Connection,
    id: &str,
    tenant: &str,
    in_response_to: &str,
    created_at_ms: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO directive_outbox
            (id, in_response_to, tenant, status, verdict, authority,
             envelope_json, envelope_json_canonical, signature_b64, signing_kid,
             council_session_id, council_cost_usd, created_at_ms, expires_at_ms)
         VALUES
            (?1, ?2, ?3, 'staged', 'Act', ?6,
             '{}', '{}', 'sig', 'sidecar-v1-test',
             'sess-test', 0.01, ?4, ?5)",
        params![
            id,
            in_response_to,
            tenant,
            created_at_ms,
            created_at_ms + 90_000,
            DirectiveAuthority::Recommend.as_str(),
        ],
    )
}

fn assert_constraint(err: Error, context: &str) {
    match err {
        Error::SqliteFailure(code, _) => {
            assert_eq!(
                code.code,
                rusqlite::ErrorCode::ConstraintViolation,
                "{context}: expected SQLite constraint failure, got {code:?}"
            );
        }
        other => panic!("{context}: expected SQLite constraint failure, got {other:?}"),
    }
}

fn assert_constraint_message(err: Error, expected: &str, context: &str) {
    let msg = err.to_string();
    assert!(
        msg.contains(expected),
        "{context}: expected error containing {expected:?}, got {msg:?}"
    );
    assert_constraint(err, context);
}

#[tokio::test]
async fn migration_creates_phase3_dispatch_tables_idempotently() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    assert!(table_exists(&conn, "pending_escalations"));
    assert!(table_exists(&conn, "directive_outbox"));
}

#[tokio::test]
async fn ac7c_pending_pk_is_tenant_scoped() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    insert_pending(&conn, "alpha", "same-001", None).unwrap();
    let same_tenant_replay = insert_pending(&conn, "alpha", "same-001", None)
        .expect_err("same tenant + same raw id must collide");
    assert_constraint(same_tenant_replay, "same tenant replay");

    insert_pending(&conn, "beta", "same-001", None)
        .expect("different tenant + same raw id must insert independently");

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_escalations WHERE id = 'same-001'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn ac18_forward_fk_rejects_dangling_directive_id() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    let err = insert_pending(&conn, "acme", "esc-forward", Some("missing-directive"))
        .expect_err("pending_escalations.directive_id must reference directive_outbox.id");
    assert_constraint(err, "forward FK");
}

#[tokio::test]
async fn ac18_reverse_composite_fk_rejects_dangling_outbox() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    let err = insert_outbox(&conn, "dir-dangling", "acme", "missing-escalation", 1000)
        .expect_err("directive_outbox must reference pending_escalations(tenant, id)");
    assert_constraint(err, "reverse composite FK");
}

#[tokio::test]
async fn ac18_deferred_tx_links_outbox_and_pending() {
    let (_tmp, db_path) = migrated_db_path().await;
    let mut conn = open_checked(&db_path);

    insert_pending(&conn, "acme", "esc-linked", None).unwrap();
    let tx = conn.transaction().unwrap();
    tx.pragma_update(None, "defer_foreign_keys", "ON").unwrap();
    tx.execute(
        "INSERT INTO directive_outbox
            (id, in_response_to, tenant, status, verdict, authority,
             envelope_json, envelope_json_canonical, signature_b64, signing_kid,
             council_session_id, council_cost_usd, created_at_ms, expires_at_ms)
         VALUES
            ('dir-linked', 'esc-linked', 'acme', 'staged', 'Act', 'recommend',
             '{}', '{}', 'sig', 'sidecar-v1-test',
             'sess-test', 0.01, 1000, 91000)",
        [],
    )
    .unwrap();
    tx.execute(
        "UPDATE pending_escalations
         SET directive_id = ?3, status = 'outbox_written'
         WHERE tenant = ?1 AND id = ?2",
        params!["acme", "esc-linked", "dir-linked"],
    )
    .unwrap();
    tx.commit().unwrap();

    let status: String = conn
        .query_row(
            "SELECT status FROM pending_escalations WHERE tenant = ?1 AND id = ?2",
            params!["acme", "esc-linked"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "outbox_written");
}

#[tokio::test]
async fn ac24_outbox_unique_collision_is_tenant_scoped() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    insert_pending(&conn, "alpha", "esc-raw", None).unwrap();
    insert_pending(&conn, "beta", "esc-raw", None).unwrap();
    insert_outbox(&conn, "dir-alpha", "alpha", "esc-raw", 1000).unwrap();

    let same_tenant_collision = insert_outbox(&conn, "dir-alpha-2", "alpha", "esc-raw", 1001)
        .expect_err("same tenant + same in_response_to must hit composite UNIQUE");
    assert_constraint(same_tenant_collision, "same tenant outbox collision");

    insert_outbox(&conn, "dir-beta", "beta", "esc-raw", 1000)
        .expect("different tenant + same raw in_response_to must not collide");

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE in_response_to = 'esc-raw'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn d2b_directive_outbox_rejects_invalid_authority() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    insert_pending(&conn, "acme", "esc-invalid", None).unwrap();
    let err = conn
        .execute(
            "INSERT INTO directive_outbox
                (id, in_response_to, tenant, status, verdict, authority,
                 envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                 council_session_id, council_cost_usd, created_at_ms, expires_at_ms)
             VALUES
                ('dir-invalid', 'esc-invalid', 'acme', 'staged', 'Act', 'invalid',
                 '{}', '{}', 'sig', 'sidecar-v1-test',
                 'sess-test', 0.01, 1000, 91000)",
            [],
        )
        .expect_err("directive_outbox.authority must be enum-constrained");

    assert_constraint(err, "directive_outbox authority check");
}

#[tokio::test]
async fn ac26_signed_outbox_fields_are_immutable() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    insert_pending(&conn, "acme", "esc-immutable", None).unwrap();
    insert_outbox(&conn, "dir-immutable", "acme", "esc-immutable", 1000).unwrap();

    for field in [
        "envelope_json_canonical",
        "signature_b64",
        "signing_kid",
        "in_response_to",
        "created_at_ms",
        "tenant",
        "verdict",
        "authority",
        "envelope_json",
        "council_session_id",
        "council_cost_usd",
    ] {
        let sql =
            format!("UPDATE directive_outbox SET {field} = {field} WHERE id = 'dir-immutable'");
        let err = conn
            .execute(&sql, [])
            .expect_err("signed outbox field update must be rejected");
        assert_constraint_message(err, "directive_outbox_immutable_field", field);
    }

    conn.execute(
        "UPDATE directive_outbox
         SET status = 'acked', acked_at_ms = 2000
         WHERE id = 'dir-immutable'",
        [],
    )
    .expect("mutable outbox status/acked_at_ms update must be allowed");
}

#[tokio::test]
async fn ac32_direct_insert_rejects_created_at_regression_per_tenant() {
    let (_tmp, db_path) = migrated_db_path().await;
    let conn = open_checked(&db_path);

    insert_pending(&conn, "acme", "esc-first", None).unwrap();
    insert_pending(&conn, "acme", "esc-regression", None).unwrap();
    insert_pending(&conn, "beta", "esc-regression", None).unwrap();
    insert_outbox(&conn, "dir-first", "acme", "esc-first", 1000).unwrap();

    let err = insert_outbox(&conn, "dir-regression", "acme", "esc-regression", 999)
        .expect_err("direct insert below tenant max must be rejected");
    assert_constraint_message(
        err,
        "directive_outbox_created_at_regression",
        "created_at regression",
    );

    insert_outbox(&conn, "dir-beta", "beta", "esc-regression", 999)
        .expect("created_at monotonicity is per tenant, not cluster-wide");
}

// ==========================================================================
// Phase 3a.5 Boot Hydration Recovery Tests (narrow seam)
// ==========================================================================

use gateway_sidecar::keymgmt::DirectiveSigningKey;
use gateway_sidecar::watch::dispatcher::{run_boot_hydration_sweep, WatchPhase3AuditEvent};

/// Minimal valid durable envelope for a council_response_staged row (Act proposal).
fn minimal_valid_staged_json(escalation_id: &str, tenant: &str) -> String {
    let proposal = serde_json::json!({
        "schema": "irin.directive.proposal.v1",
        "in_response_to": escalation_id,
        "verdict": "Act",
        "authority": "recommend",
        "job": "example.job",
        "scope": {
            "tenant": tenant,
            "subject": "example.subject",
            "allowed_actions": ["read"]
        },
        "stop_condition": "on_success",
        "return_expectation": "structured",
        "rationale": "test recovery seam"
    });

    let envelope = serde_json::json!({
        "body": proposal.to_string(),
        "headers": {
            "x-council-session-id": "sess-recovery-001",
            "x-total-cost-usd": "0.0017"
        }
    });

    envelope.to_string()
}

/// Invalid cost (negative) envelope.
fn invalid_cost_staged_json(escalation_id: &str) -> String {
    let proposal = serde_json::json!({
        "schema": "irin.directive.proposal.v1",
        "in_response_to": escalation_id,
        "verdict": "Act",
        "authority": "recommend",
        "job": "example.job",
        "scope": {
            "tenant": "acme",
            "subject": "example.subject",
            "allowed_actions": ["read"]
        },
        "stop_condition": "on_success",
        "return_expectation": "structured",
        "rationale": "bad cost fixture"
    });
    serde_json::json!({
        "body": proposal.to_string(),
        "headers": {
            "x-council-session-id": "sess-bad-cost",
            "x-total-cost-usd": "1000000.0"
        }
    })
    .to_string()
}

/// Missing session header envelope.
fn missing_session_staged_json() -> String {
    let proposal = serde_json::json!({
        "schema": "irin.directive.proposal.v1",
        "in_response_to": "esc-no-sess",
        "verdict": "Act",
        "authority": "recommend",
        "job": "example.job",
        "scope": {
            "tenant": "acme",
            "subject": "example.subject",
            "allowed_actions": ["read"]
        },
        "stop_condition": "on_success",
        "return_expectation": "structured",
        "rationale": "missing session fixture"
    });
    serde_json::json!({
        "body": proposal.to_string(),
        "headers": {
            "x-total-cost-usd": "0.003"
            // no x-council-session-id
        }
    })
    .to_string()
}

/// Seed a council_response_staged row using a raw Connection (follows existing test style).
/// Must satisfy all NOT NULL columns (sentinel_name, envelope_json, etc.).
fn seed_staged_row_raw(conn: &Connection, tenant: &str, id: &str, json: &str) {
    conn.execute(
        "INSERT INTO pending_escalations
         (id, tenant, sentinel_name, envelope_json, status, council_response_json, created_at_ms)
         VALUES (?1, ?2, 'queue-depth-watch', '{}', 'council_response_staged', ?3, 1000)",
        params![id, tenant, json],
    )
    .unwrap();
}

#[tokio::test]
async fn recovery_valid_staged_row_transitions_pending_and_creates_outbox() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    let esc_id = "esc-valid-001";
    let tenant = "acme";
    let json = minimal_valid_staged_json(esc_id, tenant);

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(&conn, tenant, esc_id, &json);
    }

    let report = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();

    assert_eq!(report.staged_rows_recovered, 1);
    assert_eq!(report.parse_failures, 0);
    assert_eq!(report.audit_events_bridged, 2);
    // P0-zeta: the two events (resume + staged) are written atomically inside the recovery tx
    assert!(report.audit_events.iter().any(|e| matches!(
        e,
        WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox { .. }
    )));
    assert!(report
        .audit_events
        .iter()
        .any(|e| matches!(e, WatchPhase3AuditEvent::DirectiveStaged { .. })));

    // Assert pending updated via composite key (C11)
    let conn = open_checked(&db_path);
    let (status, canonical): (String, String) = conn
        .query_row(
            "SELECT pe.status, do.envelope_json_canonical
             FROM pending_escalations pe
             JOIN directive_outbox do ON do.tenant = pe.tenant AND do.in_response_to = pe.id
             WHERE pe.tenant = ?1 AND pe.id = ?2",
            params![tenant, esc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "outbox_written");
    let persisted: serde_json::Value = serde_json::from_str(&canonical).unwrap();
    assert_eq!(
        persisted.get("schema").and_then(serde_json::Value::as_str),
        Some("irin.directive.payload.v1")
    );

    let outbox_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE tenant = ?1 AND in_response_to = ?2",
            params![tenant, esc_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outbox_count, 1);
}

/// A clock-skew-poisoned staged row must park (`RecoveryOutcome::SkewHeld`), never dead-letter,
/// across repeated boot sweeps — while sibling staged rows in clean tenants recover normally on
/// the same sweep (non-fatal: one poison row does not abort the batch), and the sweep TERMINATES
/// (keyset cursor advances past the held row instead of spinning on it to the deadline).
#[tokio::test]
async fn p2_skew_poisoned_row_parks_across_sweeps_siblings_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;
    let (key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    // Poison tenant 'acme': a directive_outbox row stamped far in the future makes
    // prior_max = MAX(created_at_ms) for acme exceed any real `now` by >> MAX_ALLOWED_SKEW_MS,
    // so the next acme directive's normalization delta trips the breaker. 'beta' is left clean.
    const FUTURE_MS: i64 = 9_000_000_000_000; // ~year 2255, far beyond now + the 5s cap
    {
        let conn = open_checked(&db_path);
        // The poison directive_outbox row's in_response_to FK must reference an existing
        // pending_escalations row — seed its parent first.
        insert_pending(&conn, "acme", "esc-preexisting", None).unwrap();
        insert_outbox(&conn, "dir-poison", "acme", "esc-preexisting", FUTURE_MS).unwrap();
        seed_staged_row_raw(
            &conn,
            "acme",
            "esc-poison",
            &minimal_valid_staged_json("esc-poison", "acme"),
        );
        seed_staged_row_raw(
            &conn,
            "beta",
            "esc-sibling",
            &minimal_valid_staged_json("esc-sibling", "beta"),
        );
    }

    let report = run_boot_hydration_sweep(&db, token, &key).await.unwrap();

    assert!(report.skew_held >= 1, "acme row must be parked as SkewHeld");
    assert_eq!(
        report.staged_rows_recovered, 1,
        "clean-tenant sibling must still recover — the batch must not abort"
    );
    assert_eq!(report.parse_failures, 0, "skew-hold is not a parse failure");
    assert!(
        !report.deadline_hit,
        "keyset cursor must let the sweep terminate, not spin on the held row"
    );

    // acme parked: status UNCHANGED (non-terminal), last_error sentinel set, NO outbox row.
    {
        let conn = open_checked(&db_path);
        let (status, last_error): (String, Option<String>) = conn
            .query_row(
                "SELECT status, last_error FROM pending_escalations
                 WHERE tenant = 'acme' AND id = 'esc-poison'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "council_response_staged",
            "a held row must NOT be dead-lettered/expired/failed"
        );
        assert!(
            last_error
                .as_deref()
                .unwrap_or("")
                .starts_with("ClockSkewExceeded:"),
            "held row must carry the last_error sentinel, got {last_error:?}"
        );
        let outbox_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM directive_outbox
                 WHERE tenant = 'acme' AND in_response_to = 'esc-poison'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            outbox_count, 0,
            "no directive may be staged for the refused row"
        );
    }

    // Second boot sweep: still parked, still NOT terminal — held across repeated sweeps.
    let (key2, token2) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();
    let report2 = run_boot_hydration_sweep(&db, token2, &key2).await.unwrap();
    assert!(report2.skew_held >= 1, "still parked on the second sweep");
    assert!(!report2.deadline_hit);
    {
        let conn = open_checked(&db_path);
        let status: String = conn
            .query_row(
                "SELECT status FROM pending_escalations
                 WHERE tenant = 'acme' AND id = 'esc-poison'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "council_response_staged",
            "repeated sweeps must never force a skew-held row to a terminal status"
        );
    }
}

#[tokio::test]
async fn recovery_persists_phase3_audit_events_into_watch_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    let esc_id = "esc-audit-chain-001";
    let tenant = "acme";
    let json = minimal_valid_staged_json(esc_id, tenant);

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(&conn, tenant, esc_id, &json);
    }

    let report = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();
    assert_eq!(report.staged_rows_recovered, 1);
    assert_eq!(report.audit_events_bridged, 2);

    let fires = db.list_fires_descending(tenant, 10, None).await.unwrap();
    assert_eq!(fires.len(), 2);

    let newest = &fires[0];
    let oldest = &fires[1];

    assert_eq!(oldest.sentinel, "watch-dispatcher");
    assert_eq!(oldest.reason, "escalation_recovered_resume_outbox");
    assert_eq!(oldest.prev_hash, watch_distinct_genesis());
    assert!(
        oldest
            .state_json
            .starts_with("{\"event_type\":\"escalation_recovered_resume_outbox\""),
        "state_json must keep event_type first for preimage corpus byte identity: {}",
        oldest.state_json
    );
    let oldest_state: serde_json::Value = serde_json::from_str(&oldest.state_json).unwrap();
    assert_eq!(oldest_state["escalation_id"], esc_id);
    assert_eq!(oldest_state["tenant"], tenant);

    assert_eq!(newest.sentinel, "watch-dispatcher");
    assert_eq!(newest.reason, "directive_staged");
    assert_eq!(newest.prev_hash, oldest.hash);
    assert!(
        newest
            .state_json
            .starts_with("{\"event_type\":\"directive_staged\""),
        "state_json must keep event_type first for preimage corpus byte identity: {}",
        newest.state_json
    );
    let newest_state: serde_json::Value = serde_json::from_str(&newest.state_json).unwrap();
    assert_eq!(
        newest_state["directive_id"],
        format!(
            "{}-rec-{esc_id}",
            gateway_sidecar::watch::dispatcher::safe_tenant_token(tenant)
        )
    );
    assert_eq!(newest_state["tenant"], tenant);
    assert_eq!(newest_state["in_response_to"], esc_id);

    let verify = db.verify_chain(tenant).await.unwrap();
    assert!(verify.ok, "phase3 audit rows must verify in watch chain");
    assert_eq!(verify.rows_walked, 2);

    let conn = open_checked(&db_path);
    let schema_versions: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT envelope_schema_version
                 FROM watch_fires
                 WHERE tenant = ?1
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map(params![tenant], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<i64>, _>>()
            .unwrap()
    };
    assert_eq!(schema_versions, vec![3, 3]);
}

#[tokio::test]
async fn recovery_existing_outbox_row_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    let esc_id = "esc-dup-001";
    let tenant = "acme";
    let json = minimal_valid_staged_json(esc_id, tenant);

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(&conn, tenant, esc_id, &json);
    }

    // First sweep
    let r1 = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();
    assert_eq!(r1.staged_rows_recovered, 1);

    // Reset to staged (restart simulation)
    {
        let conn = open_checked(&db_path);
        conn.execute(
            "UPDATE pending_escalations SET status = 'council_response_staged' WHERE tenant = ?1 AND id = ?2",
            params![tenant, esc_id],
        )
        .unwrap();
    }

    // Re-load to obtain a fresh HydrationToken (token is not Copy in this seam)
    let (_key2, token2) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    // Second sweep — must be idempotent via helper's UNIQUE (tenant, in_response_to) recovery
    let r2 = run_boot_hydration_sweep(&db, token2, &_key2).await.unwrap();
    assert_eq!(r2.staged_rows_recovered, 1);
    assert_eq!(r2.unique_collisions, 1);
    assert_eq!(r2.parse_failures, 0);
    assert_eq!(r2.audit_events_bridged, 2);
    // P0-zeta: idempotent recovery still produces the resume + recovered event inside the tx
    assert!(r2.audit_events.iter().any(|e| matches!(
        e,
        WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox { .. }
    )));
    assert!(r2
        .audit_events
        .iter()
        .any(|e| matches!(e, WatchPhase3AuditEvent::OutboxRecoveredFromRestart { .. })));

    let conn = open_checked(&db_path);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE tenant = ?1 AND in_response_to = ?2",
            params![tenant, esc_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn recovery_invalid_cost_increments_parse_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(
            &conn,
            "acme",
            "esc-bad-cost",
            &invalid_cost_staged_json("esc-bad-cost"),
        );
    }

    let report = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();

    assert_eq!(report.staged_rows_recovered, 0);
    assert_eq!(report.parse_failures, 1);
    // P0-gamma + P0-zeta: soft failure must dead-letter the row and write audit atomically
    assert!(report.audit_events_bridged >= 1);

    let conn = open_checked(&db_path);
    let (status, last_error): (String, Option<String>) = conn
        .query_row(
            "SELECT status, last_error FROM pending_escalations WHERE tenant = ?1 AND id = ?2",
            params!["acme", "esc-bad-cost"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "dead_lettered");
    assert!(last_error.as_deref().unwrap_or("").contains("cost"));

    // There should be a directive_parse_failed audit row in watch_fires
    let parse_failed_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM watch_fires WHERE tenant = ?1 AND reason = 'directive_parse_failed'",
            params!["acme"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(parse_failed_count >= 1);
}

#[tokio::test]
async fn recovery_missing_session_header_increments_parse_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(&conn, "acme", "esc-no-sess", &missing_session_staged_json());
    }

    let report = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();

    assert_eq!(report.staged_rows_recovered, 0);
    assert_eq!(report.parse_failures, 1);
    // P0-gamma: missing session must dead-letter + write audit
    assert!(report.audit_events_bridged >= 1);
}

/// P0-zeta regression: if the directive_parse_failed audit insert into watch_fires fails,
/// the dead_lettered transition on pending_escalations must roll back.
/// The row must remain council_response_staged and no outbox row is created.
#[tokio::test]
async fn recovery_dead_letter_audit_failure_rolls_back_transition() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(
            &conn,
            "acme",
            "esc-audit-fail",
            &invalid_cost_staged_json("esc-audit-fail"),
        );

        // Regular trigger (visible to all connections/pool) that aborts any INSERT
        // of a directive_parse_failed audit row.
        conn.execute(
            "CREATE TRIGGER abort_dead_letter_audit
             BEFORE INSERT ON watch_fires
             FOR EACH ROW
             WHEN NEW.reason = 'directive_parse_failed'
             BEGIN
                 SELECT RAISE(ABORT, 'forced audit failure for P0-zeta test');
             END;",
            [],
        )
        .unwrap();
    }

    // The recovery should now fail because the mandatory audit insert aborts.
    let sweep_result = run_boot_hydration_sweep(&db, token, &_key).await;
    assert!(
        sweep_result.is_err(),
        "recovery must fail when audit insert is forced to abort"
    );

    // Verify no partial transition: row must still be council_response_staged
    let conn = open_checked(&db_path);
    let status: String = conn
        .query_row(
            "SELECT status FROM pending_escalations WHERE tenant = ?1 AND id = ?2",
            params!["acme", "esc-audit-fail"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "council_response_staged",
        "row must not have been dead_lettered"
    );

    // No outbox row should exist for this escalation
    let outbox_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM directive_outbox WHERE tenant = ?1 AND in_response_to = ?2",
            params!["acme", "esc-audit-fail"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outbox_count, 0);

    // Clean up the temporary trigger (it would be dropped on connection close anyway,
    // but explicit DROP is clearer for the test).
    let _ = conn.execute("DROP TRIGGER IF EXISTS abort_dead_letter_audit;", []);
}

#[tokio::test]
async fn recovery_signature_b64_verifies_with_directive_signing_key() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    let esc_id = "esc-sig-verify";
    let tenant = "verify-tenant";
    let json = minimal_valid_staged_json(esc_id, tenant);

    {
        let conn = open_checked(&db_path);
        seed_staged_row_raw(&conn, tenant, esc_id, &json);
    }

    let _ = run_boot_hydration_sweep(&db, token, &key).await.unwrap();

    let conn = open_checked(&db_path);
    let (sig_b64, canonical): (String, String) = conn
        .query_row(
            "SELECT signature_b64, envelope_json_canonical
             FROM directive_outbox
             WHERE tenant = ?1 AND in_response_to = ?2",
            params![tenant, esc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    // Real Ed25519 verification using the passed key (P0-epsilon — no global)
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&sig_b64)
        .expect("base64 decode");
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes.try_into().unwrap());

    assert!(
        key.verifying_key()
            .verify_strict(canonical.as_bytes(), &sig)
            .is_ok(),
        "recovered signature must verify with the DirectiveSigningKey passed to the sweep"
    );
}

/// P0-alpha: two tenants with the same raw escalation_id must produce distinct
/// directive_outbox.id during recovery (no global PK collision on "rec-xxx").
#[tokio::test]
async fn recovery_cross_tenant_same_raw_id_produces_distinct_directive_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    let raw_esc = "shared-raw-esc-001";
    let tenant_alpha = "alpha";
    let tenant_beta = "beta";

    // Seed both tenants with the *same* raw escalation id, both staged with valid envelopes.
    {
        let conn = open_checked(&db_path);
        let json_alpha = minimal_valid_staged_json(raw_esc, tenant_alpha);
        seed_staged_row_raw(&conn, tenant_alpha, raw_esc, &json_alpha);

        let json_beta = minimal_valid_staged_json(raw_esc, tenant_beta);
        seed_staged_row_raw(&conn, tenant_beta, raw_esc, &json_beta);
    }

    let report = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();
    assert_eq!(report.staged_rows_recovered, 2);

    // Verify two distinct directive ids in outbox for the same in_response_to but different tenants.
    let conn = open_checked(&db_path);
    let mut stmt = conn
        .prepare(
            "SELECT tenant, id, in_response_to FROM directive_outbox
             WHERE in_response_to = ?1 ORDER BY tenant",
        )
        .unwrap();
    let rows: Vec<(String, String, String)> = stmt
        .query_map([raw_esc], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert_eq!(rows.len(), 2);
    let (t0, id0, in0) = &rows[0];
    let (t1, id1, in1) = &rows[1];
    assert_ne!(t0, t1);
    assert_eq!(in0, raw_esc);
    assert_eq!(in1, raw_esc);
    assert_ne!(
        id0, id1,
        "directive ids must differ to avoid PK collision across tenants"
    );
    assert!(
        id0.contains(&gateway_sidecar::watch::dispatcher::safe_tenant_token(t0))
            || id0.contains("rec-"),
        "id should be tenant-scoped"
    );
}

/// Bulk-recovery test (P0-β residual): after dropping the global ROW_TRANSITION_CAP,
/// the sweep (driven only by 30s deadline + 50-row paging) must recover an arbitrary
/// number of valid staged rows (here 25) with deadline_hit == false.
#[tokio::test]
async fn boot_hydration_bulk_recovery_arbitrary_rows_no_cap() {
    let (tmp, db_path) = migrated_db_path().await;
    let identity_path = tmp.path().join("directive_identity.json");

    let db = WatchDb::open(&db_path).await.unwrap();
    // §7 item 10: recovery re-checks the attested arm at sign time.
    arm_attest_common::arm_db_for_reserve_test(&db).await;
    let (_key, token) = DirectiveSigningKey::load_or_initialize(&identity_path, &db)
        .await
        .unwrap();

    // Seed >=20 valid council_response_staged rows (25 here).
    {
        let conn = open_checked(&db_path);
        for i in 0..25 {
            let esc_id = format!("bulk-rec-{:04}", i);
            let json = minimal_valid_staged_json(&esc_id, "bulk-tenant");
            seed_staged_row_raw(&conn, "bulk-tenant", &esc_id, &json);
        }
    }

    let report = run_boot_hydration_sweep(&db, token, &_key).await.unwrap();

    assert!(
        report.staged_rows_recovered >= 20,
        "sweep must recover many rows (got {}) once global cap removed",
        report.staged_rows_recovered
    );
    assert!(
        !report.deadline_hit,
        "30s deadline must not be hit for 25 fast in-mem rows"
    );
    assert_eq!(
        report.parse_failures, 0,
        "all seeded rows must be valid and recover"
    );
}
