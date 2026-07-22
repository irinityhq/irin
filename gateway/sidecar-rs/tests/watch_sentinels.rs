//! Phase 2 sentinel implementation tests.

use gateway_sidecar::watch::db::utc_day_bucket;
use gateway_sidecar::watch::sentinels::file_inbox::FileInboxSentinel;
use gateway_sidecar::watch::sentinels::ledger_delta::LedgerDeltaSentinel;
use gateway_sidecar::watch::sentinels::queue_depth::QueueDepthSentinel;
use gateway_sidecar::watch::sentinels::silence::SilenceSentinel;
use gateway_sidecar::watch::Sentinel;
use std::time::Duration;

// For VerifiedExact provenance fixture in t11b (must round-trip via the same
// WorkerProvenanceGuard deserializer used in db.rs list_outbox / get_outbox).
use sovereign_protocol::jcs;
use sovereign_protocol::types::WorkerProvenanceGuard;

/// T6: file-inbox sentinel fires on Create event for a matching pattern.
///
/// The watcher handle is held in scope so it isn't dropped (Drop stops
/// the watcher). Uses a deadline-bounded wait + post-consume drain loop so
/// the test is deterministic regardless of 1s poll phase / 500ms debounce
/// latency on slow CI (or local). Verifies that observe() consumes the
/// settled path (last_path.take) and that the same file is not re-delivered
/// on later observes (runner ticks must not re-fire the same file forever).
#[tokio::test]
async fn t6_file_inbox_fires_on_create_for_matching_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();

    let sentinel = FileInboxSentinel::new(
        "file-inbox-watch",
        "sovereign",
        &inbox,
        vec!["*.pdf".into()],
        Duration::from_millis(500),
    );

    // Start the watcher BEFORE writing the file so the poll catches the
    // Create event. Hold the handle so Drop doesn't stop the watcher.
    let _watcher_handle = sentinel.start_watching(None).unwrap();

    // Small lead time so the first poll-tick happens after the watcher
    // is fully registered.
    tokio::time::sleep(Duration::from_millis(100)).await;

    std::fs::write(inbox.join("test.pdf"), b"hi").unwrap();

    // Wait (with deadline) for the Create+debounce to settle into a path-bearing
    // state. This replaces a brittle fixed sleep so the test is deterministic
    // across slow CI runners (1s poll + 500ms debounce variance) without
    // widening sleeps.
    let expected_path = inbox.join("test.pdf").to_str().unwrap().to_owned();
    let state = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut found = None;
        while tokio::time::Instant::now() < deadline {
            if let Ok(st) = sentinel.observe().await {
                if st
                    .payload
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s == expected_path)
                    .unwrap_or(false)
                {
                    found = Some(st);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        found.expect("timed out waiting for settled file-inbox state after Create")
    };

    assert_eq!(state.payload["path"].as_str().unwrap(), expected_path);

    let reason = sentinel.interesting(&state);
    assert!(reason.is_some(), "expected fire reason for new .pdf");
    assert!(
        reason.unwrap().contains("test.pdf"),
        "reason should mention the filename"
    );

    // Drain loop with deadline: after the first observe consumed the settled
    // path, subsequent observes must not re-surface the *same* file (would
    // mean runner ticks re-fire the same creation forever). We expect the
    // idle/empty-payload state (or at worst a non-matching transient), never
    // the original path again. This verifies consume + dedup without assuming
    // observe() errors on idle (the current contract returns Ok({}) for
    // healthy no-pending).
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    let mut saw_idle = false;
    while tokio::time::Instant::now() < drain_deadline {
        match sentinel.observe().await {
            Ok(st) => {
                if let Some(p) = st.payload.get("path").and_then(|v| v.as_str()) {
                    assert_ne!(
                        p, expected_path,
                        "must not re-deliver the same file path after observe() consumed it"
                    );
                } else {
                    saw_idle = true;
                }
            }
            Err(_) => {
                // Transient meta failure on a just-settled path would be a
                // separate bug, but we still must not loop-forever on the
                // original file; continue draining.
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_idle,
        "expected at least one healthy idle (empty-payload) observe after consume"
    );
}

/// T6b: file-inbox sentinel does NOT fire for a non-matching pattern.
/// Catches the false-positive bug (every file fires regardless of pattern).
#[tokio::test]
async fn t6b_file_inbox_silent_on_non_matching_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();

    let sentinel = FileInboxSentinel::new(
        "file-inbox-watch",
        "sovereign",
        &inbox,
        vec!["*.pdf".into()],
        Duration::from_millis(500),
    );

    let _watcher_handle = sentinel.start_watching(None).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    std::fs::write(inbox.join("notes.txt"), b"hi").unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let state = sentinel.observe().await.unwrap();
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_none(),
        "expected no fire for .txt under *.pdf pattern, got reason: {:?}",
        reason
    );
}

/// T6c: validate_path fails when the inbox directory doesn't exist.
/// Boot-time P0-4 fix: better to crash clearly at startup than silently
/// quarantine after 60s of "no inbox" failures.
#[tokio::test]
async fn t6c_validate_path_fails_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");

    let sentinel = FileInboxSentinel::new(
        "file-inbox-watch",
        "sovereign",
        &missing,
        vec!["*.pdf".into()],
        Duration::from_millis(500),
    );

    let result = sentinel.validate_path();
    assert!(
        result.is_err(),
        "expected validate_path to fail on missing dir"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("missing or unreadable"),
        "expected helpful boot error; got: {}",
        err
    );
}

// Build a minimal ledger.db with one row at `timestamp_secs`.
fn make_ledger_with_event(path: &std::path::Path, timestamp_secs: i64) {
    let conn = rusqlite::Connection::open(path).unwrap();
    // Schema is a minimal subset of audit_events — silence only needs
    // `timestamp` to exist and be queryable. The real ledger.rs schema
    // has more columns; silence's `SELECT MAX(timestamp)` ignores them.
    conn.execute(
        "CREATE TABLE audit_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO audit_events (timestamp) VALUES (?1)",
        rusqlite::params![timestamp_secs],
    )
    .unwrap();
}

/// T7: silence sentinel FIRES when last audit event is older than
/// `threshold_hours` AND the backlog directory contains at least one entry.
#[tokio::test]
async fn t7_silence_fires_with_backlog() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("ledger.db");
    let backlog_path = tmp.path().join("backlog");
    std::fs::create_dir(&backlog_path).unwrap();

    // Event from 48h ago (in unix seconds).
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let old_secs = now_secs - 48 * 3600;
    make_ledger_with_event(&ledger_path, old_secs);

    // Backlog directory has at least one entry.
    std::fs::write(backlog_path.join("pending-task.json"), b"{}").unwrap();

    let sentinel = SilenceSentinel::new(
        "silence-watch",
        "sovereign",
        24,
        &ledger_path,
        &backlog_path,
    );

    let state = sentinel.observe().await.unwrap();
    assert_eq!(state.payload["backlog_count"].as_i64().unwrap(), 1);
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_some(),
        "expected fire when silence > threshold AND backlog present, got: {:?}",
        reason
    );
}

/// T7b: silence sentinel STAYS SILENT when there is no backlog, even though
/// silence exceeds the threshold. Backlog gate is the whole point.
#[tokio::test]
async fn t7b_silence_silent_without_backlog() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("ledger.db");
    let backlog_path = tmp.path().join("backlog");
    std::fs::create_dir(&backlog_path).unwrap();

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let old_secs = now_secs - 48 * 3600;
    make_ledger_with_event(&ledger_path, old_secs);

    // backlog_path is empty — no entries to wait on.

    let sentinel = SilenceSentinel::new(
        "silence-watch",
        "sovereign",
        24,
        &ledger_path,
        &backlog_path,
    );

    let state = sentinel.observe().await.unwrap();
    assert_eq!(state.payload["backlog_count"].as_i64().unwrap(), 0);
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_none(),
        "expected no fire when backlog is empty (silence is fine), got: {:?}",
        reason
    );
}

/// Spin up a tiny axum server that returns a fixed JSON body at `/stats`.
/// Returns (base_url, shutdown_handle). The caller drops the handle to
/// stop the server (abort the task).
async fn spawn_stats_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{routing::get, Router};
    let app = Router::new().route("/stats", get(move || async move { body }));
    // Bind to ephemeral port — the OS picks for us.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Tiny settle so the listener is accepting before the test pokes it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{}", addr), handle)
}

/// T8: queue-depth sentinel FIRES when the extracted value exceeds threshold.
#[tokio::test]
async fn t8_queue_depth_fires_when_threshold_exceeded() {
    let (base, handle) = spawn_stats_server(r#"{"active_total": 5}"#).await;
    let sentinel = QueueDepthSentinel::new(
        "queue-depth-watch",
        "sovereign",
        &format!("{base}/stats"),
        "$.active_total",
        3,
    );

    let state = sentinel.observe().await.unwrap();
    assert_eq!(state.payload["value"].as_i64().unwrap(), 5);
    assert_eq!(state.payload["threshold"].as_i64().unwrap(), 3);
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_some(),
        "expected fire when 5 > 3, got: {:?}",
        reason
    );
    handle.abort();
}

/// T8b: queue-depth sentinel STAYS SILENT when value is at/below threshold.
#[tokio::test]
async fn t8b_queue_depth_silent_below_threshold() {
    let (base, handle) = spawn_stats_server(r#"{"active_total": 2}"#).await;
    let sentinel = QueueDepthSentinel::new(
        "queue-depth-watch",
        "sovereign",
        &format!("{base}/stats"),
        "$.active_total",
        3,
    );

    let state = sentinel.observe().await.unwrap();
    assert_eq!(state.payload["value"].as_i64().unwrap(), 2);
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_none(),
        "expected no fire when 2 <= 3, got: {:?}",
        reason
    );
    handle.abort();
}

/// T8c: valid JSONPath with no match is Fatal (endpoint shape mismatch).
#[tokio::test]
async fn t8c_queue_depth_fatal_when_jsonpath_has_no_match() {
    let (base, handle) = spawn_stats_server(r#"{"active_total": 5}"#).await;
    let sentinel = QueueDepthSentinel::new(
        "queue-depth-watch",
        "sovereign",
        &format!("{base}/stats"),
        "$.missing_counter",
        3,
    );

    let err = sentinel.observe().await.unwrap_err();
    assert!(
        matches!(err, gateway_sidecar::watch::ObserveError::Fatal(ref msg) if msg.contains("resolved to no value")),
        "expected Fatal on jsonpath miss, got {err:?}"
    );
    handle.abort();
}

/// T8d: JSONPath match that is non-numeric is Fatal (operator config mismatch).
#[tokio::test]
async fn t8d_queue_depth_fatal_when_jsonpath_match_is_non_numeric() {
    let (base, handle) = spawn_stats_server(r#"{"active_total": "busy"}"#).await;
    let sentinel = QueueDepthSentinel::new(
        "queue-depth-watch",
        "sovereign",
        &format!("{base}/stats"),
        "$.active_total",
        3,
    );

    let err = sentinel.observe().await.unwrap_err();
    assert!(
        matches!(err, gateway_sidecar::watch::ObserveError::Fatal(ref msg) if msg.contains("resolved to non-numeric")),
        "expected Fatal on non-numeric jsonpath value, got {err:?}"
    );
    handle.abort();
}

async fn temp_watch_db_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("watch.db");
    let db = gateway_sidecar::watch::db::WatchDb::open(&path)
        .await
        .unwrap();
    db.run_migrations().await.unwrap();
    path
}

fn set_day_spend(watch_db: &std::path::Path, day_bucket: &str, settled_usd: f64) {
    let conn = rusqlite::Connection::open(watch_db).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, ?2)
         ON CONFLICT(day_bucket) DO UPDATE SET
           reserved_usd = excluded.reserved_usd,
           settled_usd = excluded.settled_usd",
        rusqlite::params![day_bucket, settled_usd],
    )
    .unwrap();
}

/// T9: ledger-delta sentinel FIRES when spend rises above threshold_pct from baseline.
#[tokio::test]
async fn t9_ledger_delta_fires_on_spend_spike() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let today = utc_day_bucket(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    );

    set_day_spend(&watch_db, &today, 1.0);

    let sentinel = LedgerDeltaSentinel::new(
        "ledger-delta-watch",
        "sovereign",
        &watch_db,
        50.0,
        0.01,
        0.25,
        None,
    );

    let first = sentinel.observe().await.unwrap();
    assert!(
        first.payload["baseline_established"].as_bool().unwrap(),
        "first sample at $1 establishes baseline"
    );
    assert!(
        sentinel.interesting(&first).is_none(),
        "baseline tick must not fire"
    );

    set_day_spend(&watch_db, &today, 2.0);
    let second = sentinel.observe().await.unwrap();
    assert!((second.payload["delta_pct"].as_f64().unwrap() - 100.0).abs() < 1e-6);
    let reason = sentinel.interesting(&second);
    assert!(
        reason.is_some(),
        "100% spike over 50% threshold must fire, got: {:?}",
        reason
    );
}

/// T9b: ledger-delta sentinel STAYS SILENT when delta is below threshold.
#[tokio::test]
async fn t9b_ledger_delta_silent_below_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let today = utc_day_bucket(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    );

    set_day_spend(&watch_db, &today, 1.0);
    let sentinel = LedgerDeltaSentinel::new(
        "ledger-delta-watch",
        "sovereign",
        &watch_db,
        50.0,
        0.01,
        0.25,
        None,
    );
    let _ = sentinel.observe().await.unwrap();

    set_day_spend(&watch_db, &today, 1.2);
    let state = sentinel.observe().await.unwrap();
    assert!(
        sentinel.interesting(&state).is_none(),
        "20% delta under 50% threshold must stay silent, got: {:?}",
        sentinel.interesting(&state)
    );
}

/// T9c: validate_path fails when watch.db is missing (boot fail-fast).
#[tokio::test]
async fn t9c_ledger_delta_validate_path_fails_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-watch.db");
    let sentinel = LedgerDeltaSentinel::new(
        "ledger-delta-watch",
        "sovereign",
        &missing,
        50.0,
        0.01,
        0.25,
        None,
    );
    let err = sentinel.validate_path().unwrap_err().to_string();
    assert!(
        err.contains("missing") || err.contains("no-watch.db"),
        "expected missing-path error, got: {err}"
    );
}

fn seed_pending_escalation(
    watch_db: &std::path::Path,
    tenant: &str,
    id: &str,
    status: &str,
    created_at_ms: i64,
) {
    let conn = rusqlite::Connection::open(watch_db).unwrap();
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES (?1, ?2, 'file-inbox-watch', '{}', ?3, ?4)",
        rusqlite::params![id, tenant, status, created_at_ms],
    )
    .unwrap();
}

/// T10: anomaly-watch FIRES when failure rate spikes above EWMA baseline.
#[tokio::test]
async fn t10_anomaly_fires_on_error_rate_spike() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    for i in 0..5 {
        seed_pending_escalation(
            &watch_db,
            "sovereign",
            &format!("ok-{i}"),
            "outbox_written",
            now_ms - 60_000,
        );
    }

    let sentinel = gateway_sidecar::watch::sentinels::anomaly::AnomalySentinel::new(
        "anomaly-watch",
        "sovereign",
        gateway_sidecar::watch::sentinels::anomaly::AnomalyConfig {
            watch_db_path: watch_db.clone(),
            window_ms: 900_000,
            threshold_pct: 50.0,
            min_samples: 5,
            min_failures: 2,
            min_error_rate: 0.25,
            ewma_alpha: 0.3,
            consecutive_windows_required: 1,
        },
    );

    let _ = sentinel.observe().await.unwrap();

    for i in 0..3 {
        seed_pending_escalation(
            &watch_db,
            "sovereign",
            &format!("fail-{i}"),
            "failed",
            now_ms - 30_000,
        );
    }
    seed_pending_escalation(
        &watch_db,
        "sovereign",
        "fail-extra",
        "dead_lettered",
        now_ms - 20_000,
    );
    seed_pending_escalation(
        &watch_db,
        "sovereign",
        "ok-extra",
        "dismissed",
        now_ms - 10_000,
    );

    let state = sentinel.observe().await.unwrap();
    assert!(
        sentinel.interesting(&state).is_some(),
        "4/10 failures after 0% baseline should fire, got: {:?}",
        state.payload
    );
}

/// T10b: anomaly-watch STAYS SILENT when failure rate is healthy.
#[tokio::test]
async fn t10b_anomaly_silent_on_healthy_rate() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    for i in 0..8 {
        seed_pending_escalation(
            &watch_db,
            "sovereign",
            &format!("ok-{i}"),
            "outbox_written",
            now_ms - i * 1000,
        );
    }
    seed_pending_escalation(&watch_db, "sovereign", "one-fail", "failed", now_ms - 500);

    let sentinel = gateway_sidecar::watch::sentinels::anomaly::AnomalySentinel::new(
        "anomaly-watch",
        "sovereign",
        gateway_sidecar::watch::sentinels::anomaly::AnomalyConfig {
            watch_db_path: watch_db.clone(),
            window_ms: 900_000,
            threshold_pct: 50.0,
            min_samples: 5,
            min_failures: 2,
            min_error_rate: 0.25,
            ewma_alpha: 0.3,
            consecutive_windows_required: 2,
        },
    );

    let state = sentinel.observe().await.unwrap();
    assert!(
        sentinel.interesting(&state).is_none(),
        "1/9 failure rate should stay silent, got: {:?}",
        sentinel.interesting(&state)
    );
}

/// T11: completion-verify-watch FIRES on acked with unverified provenance.
#[tokio::test]
async fn t11_completion_verify_fires_on_unverified_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let tenant = "sovereign";
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let conn = rusqlite::Connection::open(&watch_db).unwrap();

    conn.execute(
        "INSERT INTO pending_escalations (id, tenant, sentinel_name, envelope_json, status, created_at_ms) VALUES ('esc-none', ?1, 'test', '{}', 'council_response_staged', ?2)",
        rusqlite::params![tenant, now_ms - 2_000],
    ).unwrap();
    conn.execute(
        "INSERT INTO pending_escalations (id, tenant, sentinel_name, envelope_json, status, created_at_ms) VALUES ('esc-opaque', ?1, 'test', '{}', 'council_response_staged', ?2)",
        rusqlite::params![tenant, now_ms - 2_000],
    ).unwrap();

    conn.execute(
        "INSERT INTO directive_outbox (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, acked_at_ms) VALUES ('dir-none', 'esc-none', ?1, 'acked', 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?2, ?3, ?4)",
        rusqlite::params![tenant, now_ms - 1_000, now_ms + 3_600_000, now_ms],
    ).unwrap();

    conn.execute(
        "INSERT INTO directive_outbox (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, acked_at_ms, claim_handle) VALUES ('dir-opaque', 'esc-opaque', ?1, 'acked', 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?2, ?3, ?4, 'w-opaque-123')",
        rusqlite::params![tenant, now_ms - 500, now_ms + 3_600_000, now_ms],
    ).unwrap();

    let sentinel =
        gateway_sidecar::watch::sentinels::completion_verify::CompletionVerifySentinel::new(
            "completion-verify-watch",
            tenant,
            &watch_db,
        );

    let state = sentinel.observe().await.unwrap();
    let reason = sentinel.interesting(&state);
    assert!(reason.is_some(), "should fire for unverified");

    let esc = sentinel.escalate(state, reason.unwrap()).await.unwrap();
    assert!(format!("{:?}", esc.urgency).contains("Medium"));
    assert!(
        esc.state.payload["unverified_acked_count"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
}

/// T11b: completion-verify-watch silent on VerifiedExact.
#[tokio::test]
async fn t11b_completion_verify_silent_on_verified_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let tenant = "sovereign";
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let conn = rusqlite::Connection::open(&watch_db).unwrap();

    conn.execute(
        "INSERT INTO pending_escalations (id, tenant, sentinel_name, envelope_json, status, created_at_ms) VALUES ('esc-v', ?1, 'test', '{}', 'council_response_staged', ?2)",
        rusqlite::params![tenant, now_ms - 2_000],
    ).unwrap();

    // Construct via the real type + JCS (canonical) so db.rs deserializes it as
    // WorkerProvenanceStatus::VerifiedExact rather than falling back to OpaqueHandleOnly.
    let verified_guard = WorkerProvenanceGuard {
        status: sovereign_protocol::types::WorkerProvenanceStatus::VerifiedExact,
        fabrication_guard: true,
        opaque_handle: Some("w-verified-42".to_string()),
    };
    let verified_json = jcs::to_jcs_string(&verified_guard).expect("jcs serialize VerifiedExact");

    conn.execute(
        "INSERT INTO directive_outbox (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, acked_at_ms, claim_handle, worker_provenance) VALUES ('dir-verified', 'esc-v', ?1, 'acked', 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![tenant, now_ms - 200, now_ms + 3_600_000, now_ms, "w-verified-42", verified_json],
    ).unwrap();

    let sentinel =
        gateway_sidecar::watch::sentinels::completion_verify::CompletionVerifySentinel::new(
            "completion-verify-watch",
            tenant,
            &watch_db,
        );

    let state = sentinel.observe().await.unwrap();
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_none(),
        "VerifiedExact must not fire, got reason: {:?} payload: {:?}",
        reason,
        state.payload
    );
}

/// T11c: completion-verify-watch sees unverified ack buried past page 1 (>50 acked rows).
/// Seeds >50 acked (50 verified + 1 old unverified), run observe, assert count >0
/// and the buried id is in the sample. Multi-page scan (cursor) required to reach it.
/// (Early-break on short pages still reaches it.)
#[tokio::test]
async fn t11c_completion_verify_sees_buried_unverified_via_pagination() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = temp_watch_db_path(&tmp).await;
    let tenant = "sovereign";
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let conn = rusqlite::Connection::open(&watch_db).unwrap();

    // one old unverified first (lowest ts, satisfies created_at non-regression on outbox inserts)
    let esc_old = "esc-old-unv";
    let old_t = now_ms - 100_000;
    conn.execute(
        "INSERT INTO pending_escalations (id, tenant, sentinel_name, envelope_json, status, created_at_ms) VALUES (?1, ?2, 'test', '{}', 'council_response_staged', ?3)",
        rusqlite::params![esc_old, tenant, now_ms - 20_000],
    ).unwrap();
    conn.execute(
        "INSERT INTO directive_outbox (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, acked_at_ms) VALUES ('dir-old-unv', ?1, ?2, 'acked', 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?3, ?4, ?5)",
        rusqlite::params![esc_old, tenant, old_t, old_t + 3_600_000, old_t],
    ).unwrap();

    // 50 recent VerifiedExact rows with *increasing* created_at (older verifieds inserted before newer)
    // total >50 acked (50+1); unverified is older => requires page 2+ via cursor to discover.
    let verified_guard = WorkerProvenanceGuard {
        status: sovereign_protocol::types::WorkerProvenanceStatus::VerifiedExact,
        fabrication_guard: true,
        opaque_handle: Some("w-verified-page".to_string()),
    };
    let verified_json = jcs::to_jcs_string(&verified_guard).expect("jcs serialize VerifiedExact");

    let v_base = now_ms - 5_100;
    for j in 0..50 {
        let esc = format!("esc-vp-{}", j);
        let dir = format!("dir-vp-{}", j);
        let t = v_base + (j as i64 * 100); // increasing ts, all newer than old_t
        conn.execute(
            "INSERT INTO pending_escalations (id, tenant, sentinel_name, envelope_json, status, created_at_ms) VALUES (?1, ?2, 'test', '{}', 'council_response_staged', ?3)",
            rusqlite::params![esc, tenant, now_ms - 10_000],
        ).unwrap();
        conn.execute(
            "INSERT INTO directive_outbox (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms, acked_at_ms, claim_handle, worker_provenance) VALUES (?1, ?2, ?3, 'acked', 'Act', 'execute', '{}', '{}', 'sig', 'kid', ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![dir, esc, tenant, t, t + 3_600_000, t, format!("w-vp-{}", j), verified_json],
        ).unwrap();
    }

    let sentinel =
        gateway_sidecar::watch::sentinels::completion_verify::CompletionVerifySentinel::new(
            "completion-verify-watch",
            tenant,
            &watch_db,
        );

    let state = sentinel.observe().await.unwrap();
    let count = state.payload["unverified_acked_count"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        count > 0,
        "multi-page scan must surface the buried unverified (got {})",
        count
    );

    let has_buried = state
        .payload
        .get("unverified_acked")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|e| e["id"].as_str() == Some("dir-old-unv")))
        .unwrap_or(false);
    assert!(
        has_buried,
        "the buried id must be in the sample thanks to pagination beyond 50"
    );

    let reason = sentinel.interesting(&state);
    assert!(reason.is_some(), "should fire on the unverified");
}
