//! T33.P1-D — `GET /watch/stats` JSON scrape surface.
//!
//! Invariant B4 named the silent-unscrape risk: counters
//! increment internally but nothing exports them. This test closes the gap
//! by hitting the HTTP endpoint and asserting both counter names + values
//! appear in the JSON response — the same surface the Lua poller will
//! scrape to emit `gw_watch_audit_infra_errors_total` and
//! `gw_watch_persist_failures_total` on /metrics.
//!
//! Endpoint shape matches the `/council/stats` precedent (council.rs:347 +
//! main.rs:1558). Sidecar exposes JSON state; Lua owns Prometheus formatting.

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    routing::get,
    Json, Router,
};
use gateway_sidecar::watch::api::{build_watch_stats, WatchStats};
use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
use std::sync::Arc;
use tower::ServiceExt;

/// Shared state for the test router — quarantine always, durable db only for
/// the tests that exercise the spend gauge (p0d).
#[derive(Clone)]
struct StatsState {
    q: Arc<QuarantineState>,
    db: Option<Arc<WatchDb>>,
}

/// Build a minimal `/watch/stats` route. watch telemetry: now calls the SAME
/// `build_watch_stats` the main.rs handler uses (no mirror drift — the test
/// exercises the real stats assembly, including the spend gauge db read).
fn router_with_db(q: Arc<QuarantineState>, db: Option<Arc<WatchDb>>) -> Router {
    Router::new()
        .route(
            "/watch/stats",
            get(|State(s): State<StatsState>| async move {
                Json(build_watch_stats(&s.q, s.db.as_deref()).await)
            }),
        )
        .with_state(StatsState { q, db })
}

fn router(q: Arc<QuarantineState>) -> Router {
    router_with_db(q, None)
}

async fn scrape(app: Router) -> serde_json::Value {
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&body).expect("valid JSON")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn temp_watch_db(tmp: &tempfile::TempDir) -> Arc<WatchDb> {
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    db
}

/// T33.P1-D — GET /watch/stats returns 200 with both counter fields. Asserts
/// the scrape surface NAMES the two metrics so the Lua poller can find
/// them. Without this endpoint, the counters silently increment with no
/// scrape surface — exactly the silent-unscrape gap council B4 worried about.
#[tokio::test]
async fn t33_p1d_watch_stats_endpoint_exposes_both_counters() {
    let q = Arc::new(QuarantineState::test_default());
    let app = router(q.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");

    // Field names — these are the contract the Lua poller scrapes by.
    // Lua poller emits gw_watch_audit_infra_errors_total +
    // gw_watch_persist_failures_total on /metrics from these fields.
    assert!(
        v.get("audit_infra_errors_total").is_some(),
        "JSON must carry `audit_infra_errors_total` field for Lua scrape; got {v}"
    );
    assert!(
        v.get("persist_failures_total").is_some(),
        "JSON must carry `persist_failures_total` field for Lua scrape; got {v}"
    );
    assert_eq!(v["audit_infra_errors_total"], 0);
    assert_eq!(v["persist_failures_total"], 0);
}

/// T33.P1-D — counter increments are visible through the scrape surface.
/// Closes the silent-unscrape gap in the strict sense: a value that
/// increments internally MUST surface through the JSON endpoint, otherwise
/// scrapers can't detect it.
#[tokio::test]
async fn t33_p1d_watch_stats_reflects_internal_counter_increments() {
    let q = Arc::new(QuarantineState::test_default());

    // Bump both counters via their respective entry points.
    q.bump_audit_infra_errors();
    q.bump_audit_infra_errors();
    q.bump_audit_infra_errors();
    q.test_bump_persist_failures();
    q.test_bump_persist_failures();

    let app = router(q.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let stats: WatchStats = serde_json::from_slice(&body).expect("WatchStats JSON");

    assert_eq!(
        stats.audit_infra_errors_total, 3,
        "audit_infra_errors_total must reflect 3 internal bumps; scrape surface silent if not"
    );
    assert_eq!(
        stats.persist_failures_total, 2,
        "persist_failures_total must reflect 2 internal bumps; scrape surface silent if not"
    );
}

/// `pending_pending_records` surfaces the snapshot count of records parked in
/// `pending_hard_kill_persist = Some(_)` limbo. Seeds 3 records (2 with
/// pending stamped, 1 clean), asserts the JSON scrape surface shows
/// exactly 2. This is the visibility gauge that pairs with the silent-fail
/// hard-kill ladder: without this, ops can see persist FAILURES
/// (counter) but cannot see how many records are CURRENTLY parked
/// waiting for retry.
#[tokio::test]
async fn h1_shim_watch_stats_exposes_pending_pending_records_snapshot() {
    let q = Arc::new(QuarantineState::test_default());
    let now = std::time::Instant::now();

    // 2 records parked in pending_hard_kill_persist = Some(_) limbo
    q.test_set_pending_hard_kill_persist("tenant-a", "sentinel-1", now)
        .await;
    q.test_set_pending_hard_kill_persist("tenant-b", "sentinel-2", now)
        .await;
    // 1 clean record (pending_hard_kill_persist remains None — the
    // test_set_quarantined_until helper inserts a healthy-shape record)
    q.test_set_quarantined_until("tenant-c", "sentinel-3", now)
        .await;

    let app = router(q.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let stats: WatchStats = serde_json::from_slice(&body).expect("WatchStats JSON");

    assert_eq!(
        stats.pending_pending_records, 2,
        "pending_pending_records must reflect 2 parked records out of 3 seeded; got {stats:?}"
    );
}

/// H1 metrics contract — the HTTP scrape JSON must expose the retry-failure
/// counter and oldest-pending-age gauge that Lua maps to Prometheus.
#[tokio::test]
async fn h1_watch_stats_endpoint_exposes_pending_retry_and_age_fields() {
    let q = Arc::new(QuarantineState::test_default());
    q.test_set_pending_hard_kill_persist("tenant-a", "sentinel-1", std::time::Instant::now())
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let app = router(q.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");

    assert!(
        v.get("pending_retry_failures_total").is_some(),
        "JSON must carry `pending_retry_failures_total` for Lua scrape; got {v}"
    );
    assert!(
        v.get("pending_oldest_age_ms").is_some(),
        "JSON must carry `pending_oldest_age_ms` for Lua scrape; got {v}"
    );
    assert_eq!(v["pending_retry_failures_total"], 0);
    assert!(
        v["pending_oldest_age_ms"].as_u64().unwrap() >= 10,
        "pending_oldest_age_ms must reflect the parked row age; got {}",
        v["pending_oldest_age_ms"]
    );
}

/// lease liveness (telemetry invariant / lease-loss path) — the lost-deliberation-lease
/// counter must surface through the same JSON scrape surface, named exactly
/// `lease_expired_during_deliberation`, so the Lua poller can emit
/// `gw_watch_lease_expired_during_deliberation_total` on /metrics. Same
/// silent-unscrape bar as the B4 counters: increments internally MUST be
/// visible to scrapers.
#[tokio::test]
async fn p0b_watch_stats_exposes_lease_expired_during_deliberation() {
    let q = Arc::new(QuarantineState::test_default());

    q.bump_lease_expired_during_deliberation();
    q.bump_lease_expired_during_deliberation();

    let app = router(q.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");

    assert!(
        v.get("lease_expired_during_deliberation").is_some(),
        "JSON must carry `lease_expired_during_deliberation` for Lua scrape; got {v}"
    );
    assert_eq!(
        v["lease_expired_during_deliberation"], 2,
        "counter must reflect 2 internal bumps through the scrape surface"
    );
}

/// watch telemetry (telemetry invariant) — the four new metric families MUST be named on
/// the scrape surface: dup-charge alarm, spend-vs-cap gauge pair, kill-switch
/// latency, recon divergence, plus the p0b lease counter (already shipped as
/// `lease_expired_during_deliberation` — the Lua poller appends `_total`).
#[tokio::test]
async fn p0d_watch_stats_exposes_four_new_metrics() {
    let q = Arc::new(QuarantineState::test_default());
    let v = scrape(router(q)).await;

    for field in [
        "dup_charge_alarm_total",
        "spend_today_usd",
        "spend_cap_usd",
        "kill_switch_latency_ms",
        "kill_switch_latency_max_ms",
        "recon_divergence_total",
        "recon_cap_breach_total",
        "lease_expired_during_deliberation",
        "directive_ttl_expired_total",
        "directive_max_delivery_exceeded_total",
        "directive_clock_skew_rejected_total",
    ] {
        assert!(
            v.get(field).is_some(),
            "JSON must carry `{field}` for Lua scrape; got {v}"
        );
    }
    assert_eq!(v["dup_charge_alarm_total"], 0);
    assert_eq!(v["recon_divergence_total"], 0);
    assert_eq!(v["recon_cap_breach_total"], 0);
    assert_eq!(v["kill_switch_latency_ms"], 0, "no disarm yet -> 0");
    // The cap gauge is the boot-resolved day cap — wired, not hardcoded.
    assert_eq!(
        v["spend_cap_usd"].as_f64().unwrap(),
        gateway_sidecar::watch::db::daily_spend_cap(),
        "spend_cap_usd must surface daily_spend_cap()"
    );
    // No durable db wired -> the spend gauge reads 0.0 (never an error).
    assert_eq!(v["spend_today_usd"].as_f64().unwrap(), 0.0);
}

/// watch telemetry (telemetry invariant) — `spend_today_usd` reads the spend ledger
/// (reserved + settled for today's UTC bucket) via the re-pointed
/// `get_daily_council_spend`. Proves the previously-dead fn is wired to the
/// gauge: seed the ledger out-of-band, scrape, and match.
#[tokio::test]
async fn p0d_spend_vs_cap_gauge_reads_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;

    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms());
    let conn = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 2.5, 7.25)",
        rusqlite::params![today],
    )
    .unwrap();

    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));
    let v = scrape(router_with_db(q, Some(db))).await;

    let spend = v["spend_today_usd"].as_f64().unwrap();
    assert!(
        (spend - 9.75).abs() < 1e-9,
        "spend_today_usd must read reserved+settled from spend_ledger (2.5+7.25=9.75); got {spend}"
    );
    assert_eq!(
        v["spend_cap_usd"].as_f64().unwrap(),
        gateway_sidecar::watch::db::daily_spend_cap()
    );
}

/// watch telemetry (telemetry invariant) — the idempotency-dedup MISS detector. A second
/// realized-cost settle for the SAME escalation id is impossible while the
/// OCC claim_token fence holds, so any occurrence is the alarm. Force the
/// impossible state (row re-claimed after settle, fresh token), settle again,
/// and assert the alarm fires through the dispatcher's wiring + scrape surface.
#[tokio::test]
async fn p0d_dup_charge_alarm_fires_on_double_settle() {
    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    // Attested-arm: this ledger test claims real spend directly; stamp an
    // ambient-transparent active_arm so the reserve does not fail-closed.
    arm_attest_common::arm_db_for_reserve_test(&db).await;

    db.insert_pending_escalation_with_causal_dedup(
        "esc-dup-1",
        "tenant-a",
        "sentinel-x",
        "{}",
        "dig-dup-1",
        now_ms(),
        0,
    )
    .await
    .unwrap();

    let claim = db
        .claim_next_queued_or_failed()
        .await
        .unwrap()
        .expect("claim");
    let council_json = r#"{"body":"{}","headers":{"x-total-cost-usd":"2.50"}}"#;

    // First settle — legitimate, no dup.
    let report1 = db
        .store_council_response_and_stage(
            &claim.tenant,
            &claim.id,
            council_json,
            &claim.claim_token,
        )
        .await
        .unwrap();
    assert!(
        !report1.dup_realized_cost,
        "first settle must NOT report a dup realized cost"
    );
    gateway_sidecar::watch::dispatcher::note_settle_report(
        Some(&q),
        &claim.tenant,
        &claim.id,
        &report1,
    )
    .await;
    assert_eq!(q.dup_charge_alarm_total(), 0);

    // Force the impossible: the settled row goes back to 'claimed' with a
    // fresh token (simulated OCC-fence breach / re-claim after settle).
    let conn = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();
    conn.execute(
        "UPDATE pending_escalations
         SET status = 'claimed', claim_token = 'feedfacefeedface', claimed_until_ms = ?1
         WHERE tenant = ?2 AND id = ?3",
        rusqlite::params![now_ms() + 150_000, claim.tenant, claim.id],
    )
    .unwrap();

    // Second settle for the SAME escalation id — realized cost written twice.
    let report2 = db
        .store_council_response_and_stage(
            &claim.tenant,
            &claim.id,
            council_json,
            "feedfacefeedface",
        )
        .await
        .unwrap();
    assert!(
        report2.dup_realized_cost,
        "second realized-cost settle for the same (tenant,id) MUST report dup"
    );
    gateway_sidecar::watch::dispatcher::note_settle_report(
        Some(&q),
        &claim.tenant,
        &claim.id,
        &report2,
    )
    .await;

    assert_eq!(
        q.dup_charge_alarm_total(),
        1,
        "dup-charge alarm must increment"
    );
    let v = scrape(router_with_db(q, Some(db))).await;
    assert_eq!(
        v["dup_charge_alarm_total"], 1,
        "alarm must surface on /watch/stats; got {v}"
    );
}

/// watch telemetry (telemetry invariant) — kill-switch latency: wall time from the disarm
/// signal (`tx.send(true)`) to the drain ack, recorded on every disarm.
/// Sub-millisecond drains round up to 1ms so "a disarm happened" is always
/// distinguishable from "no disarm yet" (0).
#[tokio::test]
async fn p0d_kill_switch_latency_recorded_on_disarm() {
    use gateway_sidecar::watch::api::{
        admin_arm_confirm_json, admin_arm_stage_json, admin_disarm_producer_json, ArmPrincipals,
    };

    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    // Arm via the p0a four-eyes ceremony (alice stages, bob confirms).
    let principals = Arc::new(ArmPrincipals::parse(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
    ));
    let stage_resp = admin_arm_stage_json(
        Arc::clone(&q),
        principals.clone(),
        std::time::Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let bytes = to_bytes(stage_resp.into_body(), 64 * 1024).await.unwrap();
    let stage_v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let stage_id = stage_v["stage_id"].as_str().unwrap().to_string();
    let challenge = arm_attest_common::b64d(stage_v["challenge"].as_str().unwrap());
    let arm_resp = admin_arm_confirm_json(
        Arc::clone(&q),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(arm_resp.status(), StatusCode::OK);

    // Disarm — the instrumented leg.
    let disarm_resp = admin_disarm_producer_json(
        Arc::clone(&q),
        "test_admin_token".to_string(),
        principals,
        Some("test_admin_token".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;
    assert_eq!(disarm_resp.status(), StatusCode::OK);

    let v = scrape(router_with_db(Arc::clone(&q), Some(db))).await;
    let last = v["kill_switch_latency_ms"].as_u64().unwrap();
    let max = v["kill_switch_latency_max_ms"].as_u64().unwrap();
    assert!(last > 0, "disarm must record a non-zero latency; got {v}");
    assert!(max >= last, "max must dominate last; got {v}");
}

/// watch telemetry (telemetry invariant) — out-of-band reconciliation, file-import
/// fallback. Local ledger settled=10.0; operator-dropped export says 25.0;
/// divergence 15.0 > threshold 1.0 -> recon_alarm row + counter bump.
#[tokio::test]
async fn p0d_recon_file_import_divergence_alarms() {
    use gateway_sidecar::watch::recon::{run_recon_once, FileImportRecon};

    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms());
    let conn = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 10.0)",
        rusqlite::params![today],
    )
    .unwrap();

    let import_path = tmp.path().join("provider_costs.json");
    std::fs::write(&import_path, format!(r#"{{"{today}": 25.0}}"#)).unwrap();

    let source = FileImportRecon::new(&import_path);
    let outcome = run_recon_once(&db, &q, &source, 1.0).await.unwrap();
    assert!(
        outcome.alarmed,
        "15.0 divergence over 1.0 threshold must alarm"
    );
    assert!(
        (outcome.divergence_usd - 15.0).abs() < 1e-9,
        "got {outcome:?}"
    );

    let alarms = db.list_recon_alarms().await.unwrap();
    assert_eq!(alarms.len(), 1, "exactly one recon_alarm row");
    let a = &alarms[0];
    assert_eq!(a.day_bucket, today);
    assert!((a.local_usd - 10.0).abs() < 1e-9);
    assert!((a.external_usd - 25.0).abs() < 1e-9);
    assert!((a.divergence_usd - 15.0).abs() < 1e-9);
    assert_eq!(a.source, "file_import");

    assert_eq!(q.recon_divergence_total(), 1);
    let v = scrape(router_with_db(q, Some(db))).await;
    assert_eq!(
        v["recon_divergence_total"], 1,
        "recon divergence must surface on /watch/stats; got {v}"
    );
}

/// watch telemetry — recon within threshold writes NO alarm row and bumps
/// nothing: local 10.0 vs external 10.4 with threshold 1.0.
#[tokio::test]
async fn p0d_recon_within_threshold_no_alarm() {
    use gateway_sidecar::watch::recon::{run_recon_once, FileImportRecon};

    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    let today = gateway_sidecar::watch::db::utc_day_bucket(now_ms());
    let conn = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 10.0)",
        rusqlite::params![today],
    )
    .unwrap();

    // CSV fallback shape this time — both import shapes are first-class.
    let import_path = tmp.path().join("provider_costs.csv");
    std::fs::write(&import_path, format!("day,usd\n{today},10.4\n")).unwrap();

    let source = FileImportRecon::new(&import_path);
    let outcome = run_recon_once(&db, &q, &source, 1.0).await.unwrap();
    assert!(
        !outcome.alarmed,
        "0.4 divergence under 1.0 threshold must NOT alarm"
    );

    assert!(db.list_recon_alarms().await.unwrap().is_empty());
    assert_eq!(q.recon_divergence_total(), 0);
}

/// watch telemetry — the recon loop is default-OFF: no RECON_CADENCE_SECS, no
/// spawn (no surprise provider API calls). Pure-fn config test so parallel
/// tests never touch process-global env.
#[test]
fn p0d_recon_config_default_off_and_source_selection() {
    use gateway_sidecar::watch::recon::{recon_config_from_values, ReconSourceKind};

    // Cadence unset -> OFF, even with sources available.
    assert!(recon_config_from_values(None, None, Some("/tmp/x.json"), true, true).is_none());
    // Garbage / non-positive cadence -> OFF.
    assert!(
        recon_config_from_values(Some("nope"), None, Some("/tmp/x.json"), true, true).is_none()
    );
    assert!(recon_config_from_values(Some("0"), None, Some("/tmp/x.json"), true, true).is_none());

    // File import is the robust default source when a path is configured.
    let cfg = recon_config_from_values(Some("3600"), None, Some("/var/recon/c.csv"), true, true)
        .expect("file-import config");
    assert_eq!(cfg.cadence.as_secs(), 3600);
    assert!(matches!(cfg.source, ReconSourceKind::FileImport(_)));
    // H7a: auto_disarm propagates from the param (default ON).
    assert!(cfg.auto_disarm);

    // Provider usage is the best-effort fallback when only a key is present.
    let cfg = recon_config_from_values(Some("60"), Some("2.5"), None, true, true)
        .expect("provider-usage config");
    assert!((cfg.threshold_usd - 2.5).abs() < 1e-9);
    assert!(matches!(cfg.source, ReconSourceKind::ProviderUsage));

    // H7a: auto_disarm=false (rollback) propagates to page-only config.
    let cfg = recon_config_from_values(Some("60"), None, Some("/var/recon/c.csv"), true, false)
        .expect("page-only config");
    assert!(!cfg.auto_disarm);

    // Cadence set but NO source at all -> OFF (warned, not spawned).
    assert!(recon_config_from_values(Some("60"), None, None, false, true).is_none());
}

/// a failing spend-gauge DB
/// read must not silently scrape as 0.0/uninitialized. The stats build
/// reports zeros AND bumps `spend_gauge_read_failures_total`, which is
/// surfaced on /watch/stats for the Lua poller.
#[tokio::test]
async fn p0d_spend_gauge_read_failure_bumps_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    // Break the gauge source out from under the stats builder.
    let conn = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();
    conn.execute("DROP TABLE spend_ledger", []).unwrap();

    let v = scrape(router_with_db(q.clone(), Some(Arc::clone(&db)))).await;
    assert_eq!(
        v["spend_today_usd"], 0.0,
        "broken gauge must read 0.0, not poison the scrape; got {v}"
    );
    assert_eq!(
        q.spend_gauge_read_failures_total(),
        1,
        "gauge read failure must be counted"
    );
    // Counter must itself be on the scrape surface (next scrape carries it).
    let v2 = scrape(router_with_db(q, Some(db))).await;
    assert_eq!(
        v2["spend_gauge_read_failures_total"], 2,
        "failure counter must surface on /watch/stats (and the second scrape fails again); got {v2}"
    );
}

/// settles that land
/// just before UTC midnight reconcile when the external source reports
/// yesterday's bucket. `run_recon_for_day` audits an explicit (non-today)
/// bucket and the alarm row carries THAT bucket.
#[tokio::test]
async fn p0d_recon_yesterday_lookback_alarms_on_closed_bucket() {
    use gateway_sidecar::watch::recon::{run_recon_for_day, FileImportRecon};

    let tmp = tempfile::tempdir().unwrap();
    let db = temp_watch_db(&tmp).await;
    let q = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    let yesterday = gateway_sidecar::watch::db::utc_day_bucket(now_ms() - 86_400_000);
    let conn = rusqlite::Connection::open(tmp.path().join("watch.db")).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 4.0)",
        rusqlite::params![yesterday],
    )
    .unwrap();

    let import_path = tmp.path().join("provider_costs.json");
    std::fs::write(&import_path, format!(r#"{{"{yesterday}": 9.0}}"#)).unwrap();

    let source = FileImportRecon::new(&import_path);
    let outcome = run_recon_for_day(&db, &q, &source, 1.0, &yesterday)
        .await
        .unwrap();
    assert!(
        outcome.alarmed,
        "5.0 divergence on the closed bucket must alarm"
    );

    let alarms = db.list_recon_alarms().await.unwrap();
    assert_eq!(alarms.len(), 1);
    assert_eq!(
        alarms[0].day_bucket, yesterday,
        "alarm row must carry the looked-back bucket, not today"
    );
    assert_eq!(q.recon_divergence_total(), 1);
}
