use gateway_sidecar::watch::db::WatchDb;
use rusqlite::{params, Connection};
use sovereign_protocol::types::{WorkerProvenanceGuard, WorkerProvenanceStatus};

async fn setup_db() -> (tempfile::TempDir, WatchDb, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    (tmp, db, db_path)
}

#[tokio::test]
async fn verify_worker_correlation_fabrication_guard() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-a";

    let conn = Connection::open(&db_path).unwrap();

    // Insert a dummy outbox row representing a directive from Council
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('esc-1', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, council_session_id, council_cost_usd, created_at_ms, expires_at_ms, claim_handle)
         VALUES ('dir-1', 'esc-1', ?1, 'staged', 'Act', 'execute', '{}', '{}', 'sig', 'kid', 'sess-123', 0.12, 1000, 2000, 'w-12345')",
        params![tenant],
    ).unwrap();

    let rec = db.get_outbox(tenant, "dir-1").await.unwrap().unwrap();

    // We can successfully correlate the council session and cost.
    assert_eq!(rec.council_session_id.as_deref(), Some("sess-123"));
    assert_eq!(rec.council_cost_usd, Some(0.12));

    let guard = rec
        .worker_provenance
        .expect("expected a guard since claim_handle is present");
    assert!(guard.fabrication_guard, "Fabrication guard MUST be true");
    assert_eq!(guard.status, WorkerProvenanceStatus::OpaqueHandleOnly);
    assert_eq!(guard.opaque_handle.as_deref(), Some("w-12345"));
}

#[tokio::test]
async fn verify_negative_path_stale_or_missing_provenance() {
    // Tests that the outbox guard gracefully fails when provenance data is missing or stale.
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-a";

    let conn = Connection::open(&db_path).unwrap();

    // Malformed/missing council_session_id and cost, and NO claim_handle
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('esc-2', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms)
         VALUES ('dir-2', 'esc-2', ?1, 'staged', 'Act', 'execute', '{}', '{}', 'sig', 'kid', 1000, 2000)",
        params![tenant],
    ).unwrap();

    let rec = db.get_outbox(tenant, "dir-2").await.unwrap().unwrap();

    // The query should succeed but the provenance fields must safely map to None (no panic).
    assert_eq!(rec.council_session_id, None);
    assert_eq!(rec.council_cost_usd, None);
    assert!(
        rec.worker_provenance.is_none(),
        "worker_provenance should safely be None when no claim_handle"
    );
}

#[tokio::test]
async fn verify_worker_provenance_guard_json_parsing() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-a";

    let conn = Connection::open(&db_path).unwrap();

    // Create an escalation to anchor the directives
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('esc-3', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('esc-4', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .unwrap();

    // 1. Insert VerifiedExact as JSON in claim_handle
    let verified_exact_guard = WorkerProvenanceGuard {
        status: WorkerProvenanceStatus::VerifiedExact,
        fabrication_guard: true,
        opaque_handle: Some("w-999".to_string()),
    };
    let verified_exact_json =
        sovereign_protocol::jcs::to_jcs_string(&verified_exact_guard).unwrap();

    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, claim_handle)
         VALUES ('dir-3', 'esc-3', ?1, 'staged', 'Act', 'execute', '{}', '{}', 'sig', 'kid', 1000, 2000, ?2)",
        params![tenant, verified_exact_json],
    ).unwrap();

    // 2. Insert Unavailable as JSON in claim_handle
    let unavailable_guard = WorkerProvenanceGuard::new_unavailable();
    let unavailable_json = sovereign_protocol::jcs::to_jcs_string(&unavailable_guard).unwrap();

    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, claim_handle)
         VALUES ('dir-4', 'esc-4', ?1, 'staged', 'Act', 'execute', '{}', '{}', 'sig', 'kid', 1000, 2000, ?2)",
        params![tenant, unavailable_json],
    ).unwrap();

    // Verify VerifiedExact parsing
    let rec3 = db.get_outbox(tenant, "dir-3").await.unwrap().unwrap();
    let guard3 = rec3.worker_provenance.expect("expected guard");
    assert!(guard3.fabrication_guard, "Fabrication guard MUST be true");
    assert_eq!(guard3.status, WorkerProvenanceStatus::VerifiedExact);
    assert_eq!(guard3.opaque_handle.as_deref(), Some("w-999"));

    // Verify Unavailable parsing
    let rec4 = db.get_outbox(tenant, "dir-4").await.unwrap().unwrap();
    let guard4 = rec4.worker_provenance.expect("expected guard");
    assert!(guard4.fabrication_guard, "Fabrication guard MUST be true");
    assert_eq!(guard4.status, WorkerProvenanceStatus::Unavailable);
    assert_eq!(guard4.opaque_handle, None);
}

#[tokio::test]
async fn verify_worker_provenance_guard_malformed_json_fallback() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-a";

    let conn = Connection::open(&db_path).unwrap();

    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('esc-5', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .unwrap();

    // Insert malformed JSON guard
    let malformed_json = r#"{"status":"VerifiedExact", "fabrication_guard":true"#; // missing closing brace

    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, claim_handle)
         VALUES ('dir-5', 'esc-5', ?1, 'staged', 'Act', 'execute', '{}', '{}', 'sig', 'kid', 1000, 2000, ?2)",
        params![tenant, malformed_json],
    ).unwrap();

    let rec = db.get_outbox(tenant, "dir-5").await.unwrap().unwrap();
    let guard = rec
        .worker_provenance
        .expect("expected guard fallback to opaque handle");

    // Because parsing fails, it safely treats the whole string as an opaque handle.
    assert!(guard.fabrication_guard, "Fabrication guard MUST be true");
    assert_eq!(guard.status, WorkerProvenanceStatus::OpaqueHandleOnly);
    assert_eq!(guard.opaque_handle.as_deref(), Some(malformed_json));
}
