//! T31 — `GET /watch/verify-chain/:tenant` HTTP wiring.
//!
//! The chain-walking logic is covered exhaustively by `tests/watch_chain.rs`
//! (T_NEW2 et al). These tests cover the HTTP surface: 200 OK on intact
//! chain, 200 OK with `ok=false` when tampered (the endpoint reports the
//! break, it doesn't 500), and the 5s timeout budget exists.

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;
use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    extract::{Path, State},
    http::{Request, StatusCode},
    routing::{get, post},
    Router,
};
use gateway_sidecar::watch::api::{
    ack_outbox_json, audit_json, claim_outbox_json, clear_quarantine_json, force_wake_json,
    get_outbox_json, heartbeat_outbox_json, list_json, list_outbox_json, nack_outbox_json,
    outbox_pubkey_json, temperature_json, ui_snapshot_json, verify_chain_json,
    watch_set_tenant_policy, worker_ack_outbox_json, ClaimRequest, HeartbeatRequest, NackRequest,
    WorkerAckRequest,
};
use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
use gateway_sidecar::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// Build the route with `Arc<WatchDb>` as state — mirrors what main.rs does
/// inside `watch_verify_chain`, minus the AppState wrapper.
fn router(db: Arc<WatchDb>) -> Router {
    Router::new()
        .route(
            "/watch/verify-chain/{tenant}",
            get(
                |State(db): State<Arc<WatchDb>>, Path(tenant): Path<String>| async move {
                    verify_chain_json(db, tenant).await
                },
            ),
        )
        .with_state(db)
}

// ---------------------------------------------------------------------------
// Gate 4 — exact authenticated Watch UI snapshot
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct UiSnapshotState {
    db: Arc<WatchDb>,
    quarantine: Arc<QuarantineState>,
    admin_token: String,
    canary_tenant: String,
}

fn ui_snapshot_router(state: UiSnapshotState) -> Router {
    Router::new()
        .route(
            "/watch/ui-snapshot/{tenant}",
            get(
                |State(state): State<UiSnapshotState>,
                 Path(tenant): Path<String>,
                 headers: axum::http::HeaderMap| async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .map(str::to_string);
                    ui_snapshot_json(
                        state.db,
                        state.quarantine,
                        state.admin_token,
                        bearer,
                        tenant,
                        &state.canary_tenant,
                    )
                    .await
                },
            ),
        )
        .with_state(state)
}

async fn ui_snapshot_fixture() -> (tempfile::TempDir, UiSnapshotState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Arc::new(WatchDb::open(&tmp.path().join("watch.db")).await.unwrap());
    db.run_migrations().await.unwrap();
    db.upsert_sentinel_registration(
        "configured-canary",
        "safe-watch-name",
        "polling",
        5_000,
        r#"{"path":"/private/secret","provider":"do-not-leak"}"#,
    )
    .await
    .unwrap();
    db.insert_fire(
        "configured-canary",
        "safe-watch-name",
        now_ms(),
        r#"{"payload":"RAW_STATE_MUST_NOT_LEAK"}"#,
        "RAW_REASON_MUST_NOT_LEAK",
        r#"{"envelope":"RAW_ENVELOPE_MUST_NOT_LEAK"}"#,
        1,
    )
    .await
    .unwrap()
    .expect("fire inserted");
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    (
        tmp,
        UiSnapshotState {
            db,
            quarantine,
            admin_token: "snapshot-admin".into(),
            canary_tenant: "configured-canary".into(),
        },
    )
}

#[tokio::test]
async fn gate4_ui_snapshot_rejects_missing_or_wrong_admin_auth() {
    let (_tmp, state) = ui_snapshot_fixture().await;
    for auth in [None, Some("Bearer wrong-token")] {
        let mut request = Request::builder().uri("/watch/ui-snapshot/configured-canary");
        if let Some(value) = auth {
            request = request.header("Authorization", value);
        }
        let response = ui_snapshot_router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn gate4_ui_snapshot_rejects_non_canary_tenant() {
    let (_tmp, state) = ui_snapshot_fixture().await;
    let response = ui_snapshot_router(state)
        .oneshot(
            Request::builder()
                .uri("/watch/ui-snapshot/foreign")
                .header("Authorization", "Bearer snapshot-admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"], "single_tenant_violation");
}

fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys = value
        .as_object()
        .expect("value must be object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

fn assert_no_denied_keys(value: &serde_json::Value) {
    const DENIED: &[&str] = &[
        "config",
        "state_json",
        "reason",
        "payload",
        "provider",
        "model",
        "prompt",
        "credential",
        "token",
        "key",
        "env",
        "path",
        "envelope",
        "mutation",
        "claim_handle",
    ];
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let lowered = key.to_ascii_lowercase();
                assert!(
                    !DENIED.iter().any(|denied| lowered.contains(denied)),
                    "denied key leaked into UI snapshot: {key}"
                );
                assert_no_denied_keys(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_denied_keys(item);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn gate4_ui_snapshot_has_exact_whitelist_and_no_raw_values() {
    let (_tmp, state) = ui_snapshot_fixture().await;
    let response = ui_snapshot_router(state)
        .oneshot(
            Request::builder()
                .uri("/watch/ui-snapshot/configured-canary")
                .header("Authorization", "Bearer snapshot-admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        sorted_keys(&value),
        vec![
            "action_production_armed",
            "budget",
            "canary_tenant",
            "degradation",
            "recent_fires",
            "sentinels",
            "temperature",
            "tenant",
        ]
    );
    assert_eq!(value["action_production_armed"], false);
    assert_eq!(
        sorted_keys(&value["sentinels"][0]),
        vec![
            "cooldown_ms",
            "enabled",
            "fires_last_hour",
            "hard_killed_at",
            "last_fire_at",
            "name",
            "tier",
        ]
    );
    assert_eq!(
        sorted_keys(&value["temperature"]),
        vec!["fires_last_24h", "fires_last_hour", "level", "value"]
    );
    assert_eq!(
        sorted_keys(&value["recent_fires"][0]),
        vec!["fired_at", "id", "sentinel"]
    );
    assert_eq!(
        sorted_keys(&value["budget"]),
        vec!["spend_cap_usd", "spend_today_usd"]
    );
    assert_no_denied_keys(&value);
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    for secret in [
        "/private/secret",
        "do-not-leak",
        "RAW_STATE_MUST_NOT_LEAK",
        "RAW_REASON_MUST_NOT_LEAK",
        "RAW_ENVELOPE_MUST_NOT_LEAK",
        "snapshot-admin",
    ] {
        assert!(!body.contains(secret), "raw value leaked: {secret}");
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn fixture_db_with_n_fires(n: i64) -> (tempfile::TempDir, std::path::PathBuf, Arc<WatchDb>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let base = now_ms();
    for i in 0..n {
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
        .expect("insert_fire should succeed");
    }
    (tmp, db_path, db)
}

#[tokio::test]
async fn t31_verify_chain_intact_returns_200_and_ok_true() {
    let (_tmp, _path, db) = fixture_db_with_n_fires(5).await;
    let app = router(db);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/verify-chain/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ok"], serde_json::Value::Bool(true));
    assert_eq!(v["rows_walked"], serde_json::Value::from(5));
    assert!(
        v.get("broken_at_id").is_none() || v["broken_at_id"].is_null(),
        "intact chain should omit broken_at_id"
    );
}

#[tokio::test]
async fn t31_verify_chain_tampered_returns_200_with_ok_false() {
    let (_tmp, db_path, db) = fixture_db_with_n_fires(5).await;

    // Tamper the 3rd row's stored `reason`, leaving its `hash` column stale.
    // verify_chain MUST detect this as a hash mismatch (invariant 3 — the
    // recomputed preimage hash diverges from the stored hash).
    // W3 item 2 added an append-only UPDATE trigger; drop it first to model the
    // raw-DB-access attacker (verify_chain is the layer that still catches it).
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TRIGGER IF EXISTS trg_watch_fires_no_update", [])
            .unwrap();
        conn.execute("UPDATE watch_fires SET reason='tampered' WHERE id=3", [])
            .unwrap();
    }

    let app = router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/verify-chain/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ok"], serde_json::Value::Bool(false));
    assert!(
        v["broken_at_id"].is_i64(),
        "broken chain MUST report broken_at_id"
    );
    assert!(
        v["break_kind"].is_string(),
        "broken chain MUST report break_kind"
    );
}

/// Router for T27 — `GET /watch/list/{tenant}`.
fn list_router(db: Arc<WatchDb>) -> Router {
    Router::new()
        .route(
            "/watch/list/{tenant}",
            get(
                |State(db): State<Arc<WatchDb>>, Path(tenant): Path<String>| async move {
                    list_json(db, tenant).await
                },
            ),
        )
        .with_state(db)
}

/// Router for T28 — `GET /watch/temperature/{tenant}`.
fn temperature_router(db: Arc<WatchDb>) -> Router {
    Router::new()
        .route(
            "/watch/temperature/{tenant}",
            get(
                |State(db): State<Arc<WatchDb>>, Path(tenant): Path<String>| async move {
                    temperature_json(db, tenant).await
                },
            ),
        )
        .with_state(db)
}

/// Router for T29 — `GET /watch/audit/{tenant}?limit=&before_id=`.
fn audit_router(db: Arc<WatchDb>) -> Router {
    Router::new()
        .route(
            "/watch/audit/{tenant}",
            get(
                |State(db): State<Arc<WatchDb>>,
                 Path(tenant): Path<String>,
                 axum::extract::Query(q): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    let limit = q.get("limit").and_then(|s| s.parse::<i64>().ok());
                    let before_id = q.get("before_id").and_then(|s| s.parse::<i64>().ok());
                    audit_json(db, tenant, limit, before_id).await
                },
            ),
        )
        .with_state(db)
}

#[tokio::test]
async fn t27_list_returns_registered_sentinels() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    db.upsert_sentinel_registration(
        "sovereign",
        "file-inbox-watch",
        "polling",
        5000,
        r#"{"path":"/tmp/inbox"}"#,
    )
    .await
    .unwrap();
    db.upsert_sentinel_registration(
        "sovereign",
        "silence-watch",
        "polling",
        60000,
        r#"{"threshold_hours":1}"#,
    )
    .await
    .unwrap();

    let app = list_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/list/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let arr = v["sentinels"].as_array().expect("sentinels must be array");
    assert_eq!(arr.len(), 2);
    let names: Vec<&str> = arr.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"file-inbox-watch"));
    assert!(names.contains(&"silence-watch"));
}

#[tokio::test]
async fn t27_list_empty_tenant_returns_empty_array() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let app = list_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/list/ghost-tenant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["sentinels"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn t27_list_includes_last_fire_at_and_fires_last_hour() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    db.upsert_sentinel_registration("sovereign", "s1", "polling", 5000, "{}")
        .await
        .unwrap();
    let base = now_ms();
    for i in 0..3 {
        db.insert_fire("sovereign", "s1", base + i, "{}", "r", "{}", 1)
            .await
            .unwrap()
            .expect("insert_fire");
    }
    let app = list_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/list/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let row = &v["sentinels"][0];
    assert_eq!(row["fires_last_hour"], serde_json::Value::from(3));
    assert!(row["last_fire_at"].is_i64(), "last_fire_at must be present");
}

#[tokio::test]
async fn t27_upsert_idempotent_preserves_runtime_state() {
    // Restart safety: second upsert with same (tenant, name) must NOT
    // wipe hard_killed_at / probation_until.
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    db.upsert_sentinel_registration("sovereign", "s1", "polling", 5000, "{}")
        .await
        .unwrap();
    // Plant a hard-kill via the existing helper (mirrors what quarantine
    // module does at runtime).
    db.upsert_hard_kill("sovereign", "s1", 12345, "test-kill")
        .await
        .unwrap();
    // Re-run the registration upsert (simulates a restart).
    db.upsert_sentinel_registration("sovereign", "s1", "polling", 5000, "{}")
        .await
        .unwrap();
    // The hard-kill marker must still be there.
    let rows = db.list_registered("sovereign").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].hard_killed_at,
        Some(12345),
        "restart must preserve hard_killed_at"
    );
}

#[tokio::test]
async fn t28_temperature_zero_when_no_fires() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let app = temperature_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/temperature/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["temperature"], serde_json::Value::from(0.0));
    assert_eq!(v["level"], serde_json::Value::from("cold"));
}

#[tokio::test]
async fn t28_temperature_levels_thresholds() {
    // Formula: clamp01(0.7*fires_1h/5 + 0.3*fires_24h/24).
    // 5 fires in last hour → 0.7*5/5 + 0.3*5/24 = 0.7625 → hot (>= 0.6).
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let base = now_ms();
    for i in 0..5 {
        db.insert_fire("sovereign", "s1", base + i, "{}", "r", "{}", 1)
            .await
            .unwrap()
            .expect("insert_fire");
    }
    let app = temperature_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/temperature/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let t = v["temperature"].as_f64().unwrap();
    assert!((t - 0.7625).abs() < 1e-6, "expected 0.7625, got {t}");
    assert_eq!(v["level"], serde_json::Value::from("hot"));
}

#[tokio::test]
async fn t28_temperature_clamps_at_one() {
    // 100 fires in 1h → unclamped value > 1.0; must clamp to 1.0.
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let base = now_ms();
    for i in 0..100 {
        db.insert_fire("sovereign", "s1", base + i, "{}", "r", "{}", 1)
            .await
            .unwrap()
            .expect("insert_fire");
    }
    let app = temperature_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/temperature/sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["temperature"], serde_json::Value::from(1.0));
    assert_eq!(v["level"], serde_json::Value::from("hot"));
}

#[tokio::test]
async fn t29_audit_returns_descending_with_cursor_pagination() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let base = now_ms();
    for i in 0..10 {
        db.insert_fire("sovereign", "s1", base + i, "{}", "r", "{}", 1)
            .await
            .unwrap()
            .expect("insert_fire");
    }

    let app = audit_router(db.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/audit/sovereign?limit=3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let fires = v["fires"].as_array().expect("fires must be array");
    assert_eq!(fires.len(), 3);
    // Descending order: id 10, 9, 8
    let ids: Vec<i64> = fires.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![10, 9, 8], "must be descending by id");

    // Next page using before_id=8.
    let app2 = audit_router(db);
    let resp2 = app2
        .oneshot(
            Request::builder()
                .uri("/watch/audit/sovereign?limit=3&before_id=8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes2 = to_bytes(resp2.into_body(), 64 * 1024).await.unwrap();
    let v2: serde_json::Value = serde_json::from_slice(&bytes2).unwrap();
    let ids2: Vec<i64> = v2["fires"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids2, vec![7, 6, 5]);
}

#[tokio::test]
async fn t29_audit_limit_capped_at_500() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let base = now_ms();
    for i in 0..3 {
        db.insert_fire("sovereign", "s1", base + i, "{}", "r", "{}", 1)
            .await
            .unwrap()
            .expect("insert_fire");
    }
    let app = audit_router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/audit/sovereign?limit=999999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // Limit applied; we only inserted 3 so we get 3, but the cap means
    // the SQL ran with limit=500. Check via the echo'd applied_limit.
    assert_eq!(v["applied_limit"], serde_json::Value::from(500));
    assert_eq!(v["fires"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// T30 — `POST /watch/force-wake/{sentinel}` admin endpoint.
// ---------------------------------------------------------------------------

/// Inline minimal sentinel for force-wake tests. Returns Polling tier, fires
/// on every `interesting()`, escalates without external side effects. Force-
/// wake bypasses `observe()`/`interesting()`, so the test sentinel doesn't
/// need to do anything clever — `escalate()` is the only path exercised.
struct TestSentinel {
    name: String,
    tenant: String,
}

#[async_trait]
impl Sentinel for TestSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Polling
    }
    fn cooldown(&self) -> Duration {
        Duration::from_secs(5)
    }
    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        })
    }
    fn interesting(&self, _: &SentinelState) -> Option<String> {
        Some("always".into())
    }
    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError> {
        Ok(Escalation {
            state,
            reason,
            urgency: Urgency::Low,
        })
    }
}

type ForceWakeRegistry = Arc<HashMap<(String, String), Arc<dyn Sentinel>>>;

#[derive(Clone)]
struct ForceWakeState {
    db: Arc<WatchDb>,
    registry: ForceWakeRegistry,
    quarantine: Arc<QuarantineState>,
    admin_token: String,
}

async fn force_wake_fixture(
    sentinel_name: &str,
    sentinel_tenant: &str,
    admin_token: &str,
) -> (tempfile::TempDir, ForceWakeState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    db.upsert_sentinel_registration(sentinel_tenant, sentinel_name, "polling", 5000, "{}")
        .await
        .unwrap();
    let sentinel: Arc<dyn Sentinel> = Arc::new(TestSentinel {
        name: sentinel_name.into(),
        tenant: sentinel_tenant.into(),
    });
    let mut reg: HashMap<(String, String), Arc<dyn Sentinel>> = HashMap::new();
    reg.insert(
        (sentinel_tenant.to_string(), sentinel_name.to_string()),
        sentinel,
    );
    let quarantine = Arc::new(QuarantineState::new_with_db(
        gateway_sidecar::watch::quarantine::QuarantineConfig::default(),
        db.clone(),
    ));
    let state = ForceWakeState {
        db,
        registry: Arc::new(reg),
        quarantine,
        admin_token: admin_token.into(),
    };
    (tmp, state)
}

fn force_wake_router(state: ForceWakeState) -> Router {
    Router::new()
        .route(
            "/watch/force-wake/{sentinel}",
            post(
                |State(s): State<ForceWakeState>,
                 Path(sentinel): Path<String>,
                 headers: axum::http::HeaderMap,
                 body: Option<axum::Json<serde_json::Value>>| async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.strip_prefix("Bearer "))
                        .map(|s| s.to_string());
                    let body_val = body.map(|axum::Json(v)| v);
                    force_wake_json(
                        s.db,
                        s.registry,
                        s.quarantine,
                        s.admin_token,
                        bearer,
                        sentinel,
                        body_val,
                    )
                    .await
                },
            ),
        )
        .with_state(state)
}

#[tokio::test]
async fn t30_force_wake_no_auth_returns_401() {
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["title"], serde_json::Value::from("unauthorized"));
}

#[tokio::test]
async fn t30_force_wake_wrong_token_returns_401() {
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer not-the-right-token")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn t30_force_wake_correct_token_does_not_return_401() {
    // Sanity: distinguishes bad-token 401 from a correct-token path. Correct
    // token must pass auth — anything except 401 is acceptable for this test
    // (subsequent tests pin the success / 404 / 409 cases).
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "correct token must pass auth gate"
    );
}

#[tokio::test]
async fn t30_force_wake_unknown_sentinel_returns_404() {
    // The fixture registers "file-inbox-watch"; requesting a different name
    // should miss the registry lookup and return 404.
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/ghost-sentinel")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["detail"], serde_json::Value::from("unknown_sentinel"));
    assert_eq!(v["id"], serde_json::Value::from("ghost-sentinel"));
}

#[tokio::test]
async fn t30_force_wake_tenant_mismatch_returns_404() {
    // Sentinel registered for tenant=sovereign; body specifies tenant=other.
    // (tenant, name) is the lookup key, so a tenant mismatch is also a 404
    // (not a 200 against the wrong tenant's chain).
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"tenant":"other"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t30_force_wake_happy_path_returns_200_and_writes_audit() {
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let db_for_assert = state.db.clone();
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"tenant":"sovereign","reason":"manual smoke test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["fire_id"].is_i64(), "response must include fire_id");
    assert!(v["hash"].is_string(), "response must include hash");
    assert!(v["fired_at"].is_i64(), "response must include fired_at");
    let fire_id = v["fire_id"].as_i64().unwrap();
    // Audit row must exist with the admin-provided reason.
    let row = db_for_assert
        .fetch_fire_by_id(fire_id)
        .await
        .unwrap()
        .expect("audit row must exist after force-wake");
    assert_eq!(row.reason, "manual smoke test");
    assert_eq!(row.sentinel, "file-inbox-watch");
    assert_eq!(row.tenant, "sovereign");
    assert_eq!(row.hash, v["hash"].as_str().unwrap());
}

#[tokio::test]
async fn t30_force_wake_quarantined_returns_409() {
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    // Drive the sentinel into quarantine via 2 recorded failures
    // (default `fails_to_trigger`).
    state
        .quarantine
        .record_failure("sovereign", "file-inbox-watch")
        .await;
    state
        .quarantine
        .record_failure("sovereign", "file-inbox-watch")
        .await;
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["title"], serde_json::Value::from("quarantined"));
}

#[tokio::test]
async fn t30_force_wake_hard_killed_returns_409() {
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    // Cycle the sentinel into hard-kill via the public state machine:
    // alternate record_failure → test_advance_past_quarantine until hard-kill
    // engages. Default config: fails_to_trigger=2, hard_kill_after_cycles=5.
    for _ in 0..20 {
        state
            .quarantine
            .record_failure("sovereign", "file-inbox-watch")
            .await;
        state
            .quarantine
            .test_advance_past_quarantine("sovereign", "file-inbox-watch")
            .await;
        let s = state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .unwrap();
        if s.hard_killed_at.is_some() {
            break;
        }
    }
    let s = state
        .quarantine
        .get_state("sovereign", "file-inbox-watch")
        .await
        .unwrap();
    assert!(s.hard_killed_at.is_some(), "fixture must reach hard-kill");

    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["title"], serde_json::Value::from("hard-killed"));
}

#[tokio::test]
async fn t30_force_wake_uses_default_tenant_and_reason() {
    // Body absent (no content-type, empty body) → tenant defaults to
    // "sovereign", reason defaults to "force-wake (admin)" per spec §4.4.
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let db_for_assert = state.db.clone();
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "empty body must accept defaults"
    );
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let fire_id = v["fire_id"].as_i64().unwrap();
    let row = db_for_assert
        .fetch_fire_by_id(fire_id)
        .await
        .unwrap()
        .expect("audit row must exist");
    assert_eq!(row.tenant, "sovereign", "tenant must default to sovereign");
    assert_eq!(
        row.reason, "force-wake (admin)",
        "reason must default to 'force-wake (admin)'"
    );
}

#[tokio::test]
async fn t30_force_wake_during_probation_runs_with_prefix() {
    // T32 closure: force-wake on a sentinel in the 10-min post-hard-kill
    // probation window must NOT 409 — probation is log-only, not blocking.
    // The fire runs, but the audit row's reason is prefixed with
    // `[PROBATION] ` so Phase 3's dispatcher filters it out per spec §9.2.
    let (_tmp, state) = force_wake_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    // Park the sentinel in the probation window — 10min from now.
    state
        .quarantine
        .test_set_probation_until(
            "sovereign",
            "file-inbox-watch",
            std::time::Instant::now() + std::time::Duration::from_secs(600),
        )
        .await;
    let db_for_assert = state.db.clone();
    let app = force_wake_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/force-wake/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "force-wake during probation must run normally (not 409)"
    );
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let fire_id = v["fire_id"].as_i64().unwrap();
    let row = db_for_assert
        .fetch_fire_by_id(fire_id)
        .await
        .unwrap()
        .expect("audit row must exist");
    assert!(
        row.reason.starts_with("[PROBATION] "),
        "probation force-wake reason must be prefixed for Phase 3 filtering; got {:?}",
        row.reason
    );
    assert!(
        row.reason.contains("force-wake (admin)"),
        "probation force-wake reason must still carry the admin label; got {:?}",
        row.reason
    );
}

#[tokio::test]
async fn t31_verify_chain_unknown_tenant_returns_ok_true_zero_rows() {
    // Empty per-tenant chain is vacuously ok — there's no break to find.
    let (_tmp, _path, db) = fixture_db_with_n_fires(3).await;
    let app = router(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/verify-chain/ghost-tenant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ok"], serde_json::Value::Bool(true));
    assert_eq!(v["rows_walked"], serde_json::Value::from(0));
}

// ─── T32: DELETE /watch/quarantine/{sentinel} ──────────────────────────────

#[derive(Clone)]
struct ClearState {
    db: Arc<WatchDb>,
    registry: ForceWakeRegistry,
    quarantine: Arc<QuarantineState>,
    admin_token: String,
}

async fn clear_fixture(
    sentinel_name: &str,
    sentinel_tenant: &str,
    admin_token: &str,
) -> (tempfile::TempDir, ClearState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    db.upsert_sentinel_registration(sentinel_tenant, sentinel_name, "polling", 5000, "{}")
        .await
        .unwrap();
    let sentinel: Arc<dyn Sentinel> = Arc::new(TestSentinel {
        name: sentinel_name.into(),
        tenant: sentinel_tenant.into(),
    });
    let mut reg: HashMap<(String, String), Arc<dyn Sentinel>> = HashMap::new();
    reg.insert(
        (sentinel_tenant.to_string(), sentinel_name.to_string()),
        sentinel,
    );
    let quarantine = Arc::new(QuarantineState::new_with_db(
        gateway_sidecar::watch::quarantine::QuarantineConfig::default(),
        db.clone(),
    ));
    let state = ClearState {
        db,
        registry: Arc::new(reg),
        quarantine,
        admin_token: admin_token.into(),
    };
    (tmp, state)
}

fn clear_router(state: ClearState) -> Router {
    Router::new()
        .route(
            "/watch/quarantine/{sentinel}",
            axum::routing::delete(
                |State(s): State<ClearState>,
                 Path(sentinel): Path<String>,
                 headers: axum::http::HeaderMap,
                 body: Option<axum::Json<serde_json::Value>>| async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.strip_prefix("Bearer "))
                        .map(|s| s.to_string());
                    let body_val = body.map(|axum::Json(v)| v);
                    clear_quarantine_json(
                        s.registry,
                        s.quarantine,
                        s.admin_token,
                        bearer,
                        sentinel,
                        body_val,
                    )
                    .await
                },
            ),
        )
        .with_state(state)
}

// ─── P1: /watch/outbox surface ─────────────────────────────────────────────

#[derive(Clone)]
struct OutboxState {
    db: Arc<WatchDb>,
    admin_token: String,
    /// Wave-1 single-tenant tripwire: the configured canary tenant the outbox
    /// guard accepts. Defaults to "sovereign" (= production default) via
    /// [`OutboxState::new`]; the env-override test sets it to a non-default
    /// value to prove the guard tracks config, not a hardcoded const.
    canary_tenant: String,
}

impl OutboxState {
    /// Construct with the default canary tenant ("sovereign"), matching the
    /// production default. Tests that exercise the env-configured guard set
    /// `canary_tenant` directly via the struct literal instead.
    fn new(db: Arc<WatchDb>, admin_token: impl Into<String>) -> Self {
        Self {
            db,
            admin_token: admin_token.into(),
            canary_tenant: "sovereign".into(),
        }
    }
}

async fn outbox_fixture() -> (tempfile::TempDir, Arc<WatchDb>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    // Canary tenant id is "sovereign" (= api::CANARY_TENANT_DEFAULT; OutboxState
    // defaults to it). The Wave-1 single-tenant tripwire rejects any other scope
    // on the outbox surface (unless WATCH_CANARY_TENANT reconfigures it), so
    // the primary fixture rows are seeded under "sovereign". The "other" row
    // below is the cross-tenant foil (now used to prove the tripwire fires).
    insert_outbox_fixture_row(
        &conn,
        "sovereign",
        "esc-old",
        "dir-old",
        "staged",
        "Act",
        r#"{"hello":"old"}"#,
        1_000,
        "sig-old",
    );
    insert_outbox_fixture_row(
        &conn,
        "sovereign",
        "esc-new",
        "dir-new",
        "staged",
        "Act",
        r#"{"hello":"new"}"#,
        2_000,
        "sig-new",
    );
    insert_outbox_fixture_row(
        &conn,
        "sovereign",
        "esc-dismiss",
        "dir-dismiss",
        "dismissed",
        "Dismiss",
        r#"{"schema":"irin.directive.payload.v1","verdict":"Dismiss"}"#,
        2_500,
        "sig-dismiss",
    );
    insert_outbox_fixture_row(
        &conn,
        "other",
        "esc-other",
        "dir-other",
        "staged",
        "Act",
        r#"{"hello":"other"}"#,
        3_000,
        "sig-other",
    );
    drop(conn);

    (tmp, db)
}

#[allow(clippy::too_many_arguments)]
fn insert_outbox_fixture_row(
    conn: &rusqlite::Connection,
    tenant: &str,
    escalation_id: &str,
    directive_id: &str,
    status: &str,
    verdict: &str,
    envelope_json: &str,
    created_at_ms: i64,
    signature_b64: &str,
) {
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES (?1, ?2, 'queue-depth-watch', '{}', 'outbox_written', ?3)",
        rusqlite::params![escalation_id, tenant, created_at_ms - 10],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO directive_outbox
            (id, in_response_to, tenant, status, verdict, authority,
             envelope_json, envelope_json_canonical, signature_b64, signing_kid,
             created_at_ms, expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'recommend', ?6, ?6, ?7,
                 'sidecar-v1-test', ?8, ?9)",
        rusqlite::params![
            directive_id,
            escalation_id,
            tenant,
            status,
            verdict,
            envelope_json,
            signature_b64,
            created_at_ms,
            created_at_ms + 90_000,
        ],
    )
    .unwrap();
}

fn outbox_router(state: OutboxState) -> Router {
    Router::new()
        .route(
            "/watch/outbox/pubkey",
            get(|| async move { outbox_pubkey_json().await }),
        )
        .route(
            "/watch/outbox/{tenant}",
            get(
                |State(s): State<OutboxState>,
                 Path(tenant): Path<String>,
                 axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| async move {
                    let status = q.get("status").cloned();
                    let cursor = q.get("cursor").cloned();
                    let limit = q.get("limit").and_then(|v| v.parse::<i64>().ok()).unwrap_or(50);
                    let canary = s.canary_tenant.clone();
                    list_outbox_json(s.db, tenant, status, cursor, limit, true, &canary).await
                },
            ),
        )
        .route(
            "/watch/outbox/{tenant}/{id}",
            get(
                |State(s): State<OutboxState>,
                 Path((tenant, id)): Path<(String, String)>| async move {
                    let canary = s.canary_tenant.clone();
                    get_outbox_json(s.db, tenant, id, true, &canary).await
                },
            ),
        )
        // Driving route: unauthed list -> 401 (the invariant Option 3; the D1
        // projection was removed) + 401 note for the main.rs mutation bearer wrappers.
        .route(
            "/watch/outbox/{tenant}/proj",
            get(
                |State(s): State<OutboxState>, Path(tenant): Path<String>| async move {
                    let canary = s.canary_tenant.clone();
                    list_outbox_json(s.db, tenant, None, None, 1, false, &canary).await
                },
            ),
        )
        .route(
            "/watch/outbox/{id}/ack",
            post(
                |State(s): State<OutboxState>,
                 Path(id): Path<String>,
                 headers: axum::http::HeaderMap| async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .map(|v| v.to_string());
                    let tenant_scope = headers
                        .get("x-tenant-scope")
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.to_string());
                    let canary = s.canary_tenant.clone();
                    ack_outbox_json(s.db, s.admin_token, bearer, id, tenant_scope, &canary).await
                },
            ),
        )
        // N: drive the *real* main.rs auth wrappers (claim/heartbeat/worker_ack/nack).
        // Each closure extracts the bearer (Option<String>) and passes the admin
        // token + bearer to the lib fn, which now owns the constant-time 401 gate —
        // exactly as main's watch_*_outbox handlers now do. 4 distinct routes + oneshots below.
        .route(
            "/watch/outbox/claim",
            post(
                |State(s): State<OutboxState>, headers: axum::http::HeaderMap, axum::Json(req): axum::Json<ClaimRequest>| async move {
                    let tenant_scope = headers.get("x-tenant-scope").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer ")).map(|s| s.to_string());
                    claim_outbox_json(s.db, s.admin_token.clone(), bearer, tenant_scope, req, &s.canary_tenant).await
                },
            ),
        )
        .route(
            "/watch/outbox/{id}/heartbeat",
            post(
                |State(s): State<OutboxState>, Path(id): Path<String>, headers: axum::http::HeaderMap, axum::Json(req): axum::Json<HeartbeatRequest>| async move {
                    let tenant_scope = headers.get("x-tenant-scope").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer ")).map(|s| s.to_string());
                    heartbeat_outbox_json(s.db, s.admin_token.clone(), bearer, tenant_scope, id, req, &s.canary_tenant).await
                },
            ),
        )
        .route(
            "/watch/outbox/{id}/worker_ack",
            post(
                |State(s): State<OutboxState>, Path(id): Path<String>, headers: axum::http::HeaderMap, axum::Json(req): axum::Json<WorkerAckRequest>| async move {
                    let tenant_scope = headers.get("x-tenant-scope").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer ")).map(|s| s.to_string());
                    worker_ack_outbox_json(s.db, s.admin_token.clone(), bearer, tenant_scope, id, req, &s.canary_tenant).await
                },
            ),
        )
        .route(
            "/watch/outbox/{id}/nack",
            post(
                |State(s): State<OutboxState>, Path(id): Path<String>, headers: axum::http::HeaderMap, axum::Json(req): axum::Json<NackRequest>| async move {
                    let tenant_scope = headers.get("x-tenant-scope").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer ")).map(|s| s.to_string());
                    nack_outbox_json(s.db, s.admin_token.clone(), bearer, tenant_scope, id, req, &s.canary_tenant).await
                },
            ),
        )
        // T1 fix proof: a "list authed" route that computes `authed` exactly the
        // way main.rs now does (SHA-256 + constant-time compare of the bearer
        // against the admin token) — a junk bearer fails the compare and gets a
        // 401 (outbox reads are admin-only; no public projection fallback).
        .route(
            "/watch/outbox/{tenant}/authed-list",
            get(
                |State(s): State<OutboxState>, Path(tenant): Path<String>, headers: axum::http::HeaderMap| async move {
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer ")).map(|s| s.to_string());
                    let authed = ct_admin_token_matches(&s.admin_token, bearer.as_deref());
                    let canary = s.canary_tenant.clone();
                    list_outbox_json(s.db, tenant, None, None, 50, authed, &canary).await
                },
            ),
        )
        // T1 fix proof: tenant-policy set route — lib fn now owns the 401 gate.
        .route(
            "/watch/policy/{tenant}",
            post(
                |State(s): State<OutboxState>, Path(tenant): Path<String>, headers: axum::http::HeaderMap, axum::Json(policy): axum::Json<gateway_sidecar::watch::db::TenantPolicy>| async move {
                    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer ")).map(|s| s.to_string());
                    watch_set_tenant_policy(s.db, s.admin_token.clone(), bearer, tenant, policy, &s.canary_tenant).await
                },
            ),
        )
        .with_state(state)
}

/// Test-side replica of `watch::api::admin_token_matches` (which is `pub(crate)`
/// and so not reachable from this integration-test crate). Same algorithm:
/// reject a bearer longer than 128 bytes before hashing, SHA-256 both sides to
/// a fixed-width digest, constant-time compare; empty configured token and a
/// missing bearer both fail closed. This mirrors EXACTLY what main.rs computes
/// for `authed` on the read routes — keep the length guard in lockstep with prod.
fn ct_admin_token_matches(expected: &str, provided: Option<&str>) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    if expected.is_empty() {
        return false;
    }
    let Some(given) = provided else {
        return false;
    };
    if given.len() > 128 {
        return false;
    }
    let expected_digest = Sha256::digest(expected.as_bytes());
    let given_digest = Sha256::digest(given.as_bytes());
    expected_digest.ct_eq(&given_digest).into()
}

fn pct_encode_query_value(raw: &str) -> String {
    raw.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

#[tokio::test]
async fn p1_outbox_list_uses_contract_shape_and_cursor() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, "super-secret"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/sovereign?limit=1&status=staged")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v.get("items").is_none(),
        "contract uses directives, not items"
    );
    let directives = v["directives"].as_array().expect("directives array");
    assert_eq!(directives.len(), 1);
    assert_eq!(directives[0]["id"], "dir-new");
    assert_eq!(directives[0]["envelope"]["hello"], "new");
    assert_eq!(
        directives[0]["envelope_json_canonical"],
        r#"{"hello":"new"}"#
    );
    assert_eq!(directives[0]["signature"]["alg"], "Ed25519");
    assert_eq!(directives[0]["signature"]["kid"], "sidecar-v1-test");
    assert_eq!(directives[0]["signature"]["value"], "sig-new");
    let cursor = v["next_cursor"]
        .as_str()
        .expect("next cursor for second row");

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/watch/outbox/sovereign?limit=1&status=staged&cursor={}",
                    pct_encode_query_value(cursor)
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let directives = v["directives"].as_array().expect("directives array");
    assert_eq!(directives.len(), 1);
    assert_eq!(directives[0]["id"], "dir-old");
    assert!(v["next_cursor"].is_null());
}

#[tokio::test]
async fn p1_outbox_get_is_tenant_scoped() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, "super-secret"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/sovereign/dir-new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Cross-tenant path: under the Wave-1 single-tenant tripwire a non-canary
    // path tenant is rejected with a loud 403 BEFORE the cross-tenant 404 (the
    // gap is now fail-loud, not silent). Pre-tripwire this was a 404.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/other/dir-new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["error"],
        serde_json::Value::from("single_tenant_violation")
    );
}

#[tokio::test]
async fn p1_outbox_ack_uses_tenant_scope_header_and_status_contract() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, "super-secret"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-new/ack")
                .header("authorization", "Bearer super-secret")
                .header("x-tenant-scope", "sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-new/ack")
                .header("authorization", "Bearer super-secret")
                .header("x-tenant-scope", "sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-dismiss/ack")
                .header("authorization", "Bearer super-secret")
                .header("x-tenant-scope", "sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // dir-other is seeded under tenant "other"; ack with the canary scope
    // "sovereign" → ack's own cross-tenant TenantMismatch (403). (ack is not
    // gated by the canary tripwire — that guard covers claim/heartbeat/
    // worker_ack/nack + list/get/tenant-policy, per the Wave-1 ruling.)
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-other/ack")
                .header("authorization", "Bearer super-secret")
                .header("x-tenant-scope", "sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn p1_outbox_pubkey_shape_is_static_route_not_tenant_list() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let identity = tmp.path().join("directive_identity.json");
    let _ = gateway_sidecar::keymgmt::DirectiveSigningKey::load_or_initialize(&identity, &db)
        .await
        .unwrap();

    let app = outbox_router(OutboxState::new(db, "super-secret"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/pubkey")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["alg"], "Ed25519");
    assert_eq!(v["kid_format"], "sidecar-v1-{first8_hex_of_sha256(pubkey)}");
    assert!(v["kid"].as_str().unwrap().starts_with("sidecar-v1-"));
    assert!(v["pubkey_b64"].is_string());
    assert!(
        v.get("directives").is_none(),
        "pubkey must not hit tenant list route"
    );
}

#[tokio::test]
async fn t32_clear_no_auth_returns_401() {
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["title"], serde_json::Value::from("unauthorized"));
}

#[tokio::test]
async fn t32_clear_wrong_token_returns_401() {
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer not-the-right-token")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn t32_clear_unknown_sentinel_returns_404() {
    // Registry miss on the (tenant, sentinel) key — typo'd or stale name.
    // Hybrid 404/200 design: typo gets caught loudly even though the
    // operation itself is idempotent for healthy sentinels.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-imbox-watch") // typo
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["detail"], serde_json::Value::from("unknown_sentinel"));
    assert_eq!(v["id"], serde_json::Value::from("file-imbox-watch"));
}

#[tokio::test]
async fn t32_clear_tenant_mismatch_returns_404() {
    // Sentinel name exists, but the body's tenant doesn't match the
    // registry key. (tenant, name) is the lookup, not name alone.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"tenant": "acme"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["detail"], serde_json::Value::from("unknown_sentinel"));
    assert_eq!(v["tenant"], serde_json::Value::from("acme"));
}

#[tokio::test]
async fn t32_clear_healthy_returns_200_empty_cleared() {
    // Sentinel exists in registry but has no quarantine record. Hybrid
    // design: idempotent 200 with `cleared: []` so cron-style "clear if
    // dirty" workflows compose. probation_until is null.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["tenant"], serde_json::Value::from("sovereign"));
    assert_eq!(v["sentinel"], serde_json::Value::from("file-inbox-watch"));
    assert_eq!(v["cleared"], serde_json::json!([]));
    assert_eq!(v["probation_until"], serde_json::Value::Null);
}

#[tokio::test]
async fn t32_clear_quarantined_returns_200_cleared_quarantine() {
    // Drive sentinel into quarantine via 2 consecutive failures (default
    // `fails_to_trigger`). DELETE must clear and report
    // `cleared: ["quarantine"]`. No hard-kill in the mix, so no probation.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    state
        .quarantine
        .record_failure("sovereign", "file-inbox-watch")
        .await;
    state
        .quarantine
        .record_failure("sovereign", "file-inbox-watch")
        .await;
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["cleared"], serde_json::json!(["quarantine"]));
    assert_eq!(v["probation_until"], serde_json::Value::Null);
}

#[tokio::test]
async fn t32_clear_hard_killed_returns_200_cleared_hard_kill_with_probation() {
    // Drive sentinel to hard-kill via the public state machine (matches
    // the t30_force_wake_hard_killed fixture pattern). Default body
    // `reset_probation: false` so the row enters the 10-min log-only
    // probation window per spec §9.2.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    for _ in 0..20 {
        state
            .quarantine
            .record_failure("sovereign", "file-inbox-watch")
            .await;
        state
            .quarantine
            .test_advance_past_quarantine("sovereign", "file-inbox-watch")
            .await;
        let s = state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .unwrap();
        if s.hard_killed_at.is_some() {
            break;
        }
    }
    assert!(
        state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .unwrap()
            .hard_killed_at
            .is_some(),
        "fixture must reach hard-kill"
    );
    let before_ms = now_ms();

    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let cleared = v["cleared"].as_array().expect("cleared is array");
    assert!(
        cleared.iter().any(|s| s == "hard_kill"),
        "cleared must include hard_kill; got {cleared:?}"
    );
    let probation_until = v["probation_until"].as_i64().expect("probation_until ms");
    // 10-min window per default `probation_ms = 600_000`. Allow ±2s slack.
    assert!(
        probation_until >= before_ms + 600_000 - 2_000,
        "probation_until must be >= now + ~10min; got {probation_until} vs before {before_ms}"
    );
    assert!(
        probation_until <= before_ms + 600_000 + 2_000,
        "probation_until must be <= now + ~10min + slack; got {probation_until} vs before {before_ms}"
    );
}

#[tokio::test]
async fn t32_clear_reset_probation_true_skips_probation() {
    // `reset_probation: true` re-arms the sentinel immediately — no
    // probation window. cleared still includes hard_kill (the actual
    // state that was lifted). probation_until is null.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    for _ in 0..20 {
        state
            .quarantine
            .record_failure("sovereign", "file-inbox-watch")
            .await;
        state
            .quarantine
            .test_advance_past_quarantine("sovereign", "file-inbox-watch")
            .await;
        let s = state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .unwrap();
        if s.hard_killed_at.is_some() {
            break;
        }
    }
    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reset_probation": true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let cleared = v["cleared"].as_array().expect("cleared is array");
    assert!(
        cleared.iter().any(|s| s == "hard_kill"),
        "cleared must include hard_kill; got {cleared:?}"
    );
    assert_eq!(
        v["probation_until"],
        serde_json::Value::Null,
        "reset_probation=true must skip the probation window"
    );
}

#[tokio::test]
async fn t32_clear_hard_killed_then_fire_succeeds() {
    // Semantic test (advisor): proves DELETE actually un-sticks the
    // DB-side hard-kill, not just the in-memory record. Without the
    // `clear_hard_kill_and_set_probation` DB write, insert_fire's OCC
    // tx would still ROLLBACK every fire post-DELETE — silent stealth
    // death.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    // Drive in-memory hard-kill.
    for _ in 0..20 {
        state
            .quarantine
            .record_failure("sovereign", "file-inbox-watch")
            .await;
        state
            .quarantine
            .test_advance_past_quarantine("sovereign", "file-inbox-watch")
            .await;
        if state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .unwrap()
            .hard_killed_at
            .is_some()
        {
            break;
        }
    }
    // Plant a matching DB row — runtime would have written this via the
    // quarantine module during the hard-kill transition; we shortcut.
    state
        .db
        .upsert_hard_kill(
            "sovereign",
            "file-inbox-watch",
            now_ms(),
            "test_5_quarantines_in_1h",
        )
        .await
        .unwrap();
    // Pre-DELETE assertion: insert_fire's OCC tx ROLLBACKs because of the
    // DB-side hard-kill — returns Ok(None).
    let pre = state
        .db
        .insert_fire(
            "sovereign",
            "file-inbox-watch",
            now_ms(),
            "{}",
            "pre-clear-attempt",
            "{}",
            1,
        )
        .await
        .unwrap();
    assert!(
        pre.is_none(),
        "pre-DELETE: DB OCC must reject due to hard_killed_at; got {pre:?}"
    );
    let db_for_post = state.db.clone();

    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reset_probation": true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Post-DELETE assertion: insert_fire's OCC tx ACCEPTS — the DB-side
    // hard_killed_at has been cleared. This is what would silently break
    // if admin_clear_quarantine only mutated in-memory state.
    let post = db_for_post
        .insert_fire(
            "sovereign",
            "file-inbox-watch",
            now_ms(),
            "{}",
            "post-clear-attempt",
            "{}",
            1,
        )
        .await
        .unwrap();
    assert!(
        post.is_some(),
        "post-DELETE: DB OCC must accept after admin clear; got {post:?}"
    );
}

#[tokio::test]
async fn t32_clear_clears_durable_db_when_inmemory_record_absent() {
    // Regression guard: a restart preserves `watch_sentinels.hard_killed_at`
    // but resets in-memory `QuarantineState.records`. Admin DELETE in that
    // state must still issue the durable clear — otherwise insert_fire's
    // OCC keeps rejecting forever despite the 200/cleared:[] response.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    // Plant DB hard-kill WITHOUT touching in-memory records. Mimics the
    // post-restart state.
    state
        .db
        .upsert_hard_kill(
            "sovereign",
            "file-inbox-watch",
            now_ms(),
            "test_durable_hard_kill",
        )
        .await
        .unwrap();
    // Sanity: in-memory record really is absent.
    assert!(
        state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .is_none(),
        "fixture pre-condition: in-memory record must be absent"
    );
    // Sanity: insert_fire is currently rejecting due to DB hard-kill.
    assert!(
        state
            .db
            .insert_fire(
                "sovereign",
                "file-inbox-watch",
                now_ms(),
                "{}",
                "pre",
                "{}",
                1,
            )
            .await
            .unwrap()
            .is_none(),
        "pre-DELETE DB OCC must reject"
    );
    let db_for_post = state.db.clone();

    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Critical assertion: DB-side hard-kill must be cleared even though
    // the in-memory record was absent.
    let post = db_for_post
        .insert_fire(
            "sovereign",
            "file-inbox-watch",
            now_ms(),
            "{}",
            "post",
            "{}",
            1,
        )
        .await
        .unwrap();
    assert!(
        post.is_some(),
        "post-DELETE DB OCC must accept — durable hard-kill clear must fire \
         even when in-memory record is absent"
    );
}

#[tokio::test]
async fn t32_clear_db_only_hard_kill_enters_probation_by_default() {
    // When in-memory state is absent but the DB has hard_killed_at set after a
    // restart, DELETE with the default `reset_probation:false` must establish
    // the 10-minute probation window. Otherwise the sentinel goes immediately live, bypassing
    // the operator-visibility guarantee that probation exists for.
    let (_tmp, state) = clear_fixture("file-inbox-watch", "sovereign", "super-secret").await;
    state
        .db
        .upsert_hard_kill(
            "sovereign",
            "file-inbox-watch",
            now_ms(),
            "test_durable_hard_kill_probation",
        )
        .await
        .unwrap();
    assert!(
        state
            .quarantine
            .get_state("sovereign", "file-inbox-watch")
            .await
            .is_none(),
        "fixture pre-condition: in-memory record must be absent"
    );
    let before_ms = now_ms();
    let q_for_post = state.quarantine.clone();

    let app = clear_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/watch/quarantine/file-inbox-watch")
                .header("authorization", "Bearer super-secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let cleared = v["cleared"].as_array().expect("cleared is array");
    assert!(
        cleared.iter().any(|s| s == "hard_kill"),
        "cleared must include hard_kill (DB authoritative); got {cleared:?}"
    );
    let probation_until = v["probation_until"]
        .as_i64()
        .expect("probation_until must be set for default reset_probation=false");
    assert!(
        probation_until >= before_ms + 600_000 - 2_000,
        "probation window must be ~10min; got {probation_until} vs before {before_ms}"
    );

    // In-memory state must reflect the probation window — otherwise
    // fire_pipeline's is_blocked won't see ProbationLogOnly and won't
    // apply the [PROBATION] prefix.
    let rec = q_for_post
        .get_state("sovereign", "file-inbox-watch")
        .await
        .expect(
            "admin clear from DB-only state must materialize an in-memory \
             probation record so fire_pipeline's gate observes it",
        );
    assert!(
        rec.probation_until.is_some(),
        "in-memory probation_until must be set after admin clear of DB-only hard-kill"
    );
}

// ---------------------------------------------------------------------------
// p0a-four-eyes (the dual-custody invariant) — HTTP-surface tests for the arming
// ceremony: dedicated 401s (closes the "no dedicated 401 test" engine-fact
// gap), boot-time fail-closed with <2 principals, the append-only
// hash-chained arm_audit table, and the 410 on the legacy single-shot route.
// ---------------------------------------------------------------------------

async fn arm_audit_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Arc<WatchDb>,
    Arc<QuarantineState>,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let q = Arc::new(QuarantineState::new_with_db(
        gateway_sidecar::watch::quarantine::QuarantineConfig::default(),
        db.clone(),
    ));
    (tmp, db_path, db, q)
}

fn two_principals() -> Arc<gateway_sidecar::watch::api::ArmPrincipals> {
    Arc::new(gateway_sidecar::watch::api::ArmPrincipals::parse(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
    ))
}

/// Stage with no bearer -> 401, counted in `arm_rejected_unauth_total`.
/// P1 : an UNAUTHENTICATED rejection must NOT
/// append a permanent row to the engine-unprunable arm_audit chain.
#[tokio::test]
async fn test_arm_stage_401_no_token() {
    use gateway_sidecar::watch::api::admin_arm_stage_json;

    let (_tmp, _path, db, q) = arm_audit_fixture().await;
    let principals = two_principals();

    let resp = admin_arm_stage_json(
        q.clone(),
        principals,
        Duration::from_millis(120_000),
        None,
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(q.producer_kill_state.lock().is_none());

    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        !rows.iter().any(|r| r.action == "stage_rejected"),
        "401 unauthenticated rejection must NOT write a permanent arm_audit row (DoS guard); got {rows:?}"
    );
    assert_eq!(
        q.arm_rejected_unauth_total(),
        1,
        "the unauthenticated 401 must be counted, not audited"
    );
}

/// Stage with a bearer matching no principal -> 401. Closes the
/// "no dedicated 401 test" gap noted in the engine facts.
#[tokio::test]
async fn test_arm_stage_401_bad_token() {
    use gateway_sidecar::watch::api::admin_arm_stage_json;

    let (_tmp, _path, db, q) = arm_audit_fixture().await;
    let principals = two_principals();

    // Unknown principal name.
    let resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("mallory:tok_nope".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Known name, wrong token.
    let resp2 = admin_arm_stage_json(
        q.clone(),
        principals,
        Duration::from_millis(120_000),
        Some("alice:tok_wrong_token".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);

    assert!(q.producer_kill_state.lock().is_none());
    // P1 : neither an unknown name nor a known
    // name with a wrong token AUTHENTICATES, so neither may write a permanent
    // arm_audit row — both land in the prunable counter instead.
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        !rows.iter().any(|r| r.action == "stage_rejected"),
        "401 unauthenticated rejections must NOT write permanent arm_audit rows (DoS guard); got {rows:?}"
    );
    assert_eq!(
        q.arm_rejected_unauth_total(),
        2,
        "both unauthenticated 401s must be counted, not audited"
    );
}

/// dual-custody-local-attest (spec §2): ONE principal is arm-capable — the
/// second custody domain is the enclave key at confirm time, not a second
/// token. A single principal stages fine; an empty/unset registry still
/// fails closed at authentication; a stale '@otc' entry rejects the ENTIRE
/// registry (B6 retirement guard).
#[tokio::test]
async fn test_single_principal_is_arm_capable_empty_is_not() {
    use gateway_sidecar::watch::api::{admin_arm_stage_json, ArmPrincipals};

    let (_tmp, _path, _db, q) = arm_audit_fixture().await;
    let lone = Arc::new(ArmPrincipals::parse("alice:tok_alpha_0001"));
    assert!(
        lone.is_arm_capable(),
        "single principal must be arm-capable (spec §2: domain 2 is the enclave key)"
    );

    let resp = admin_arm_stage_json(
        q.clone(),
        lone.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "single principal must be able to stage"
    );

    // Empty/unset registry: even authentication fails (401, fail-closed).
    let empty = Arc::new(ArmPrincipals::parse(""));
    let resp_empty = admin_arm_stage_json(
        q.clone(),
        empty,
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(resp_empty.status(), StatusCode::UNAUTHORIZED);

    // B6 retirement guard: a leftover '@otc' entry rejects the ENTIRE
    // registry (fail-closed) — stale OTC config cannot half-work.
    let stale = ArmPrincipals::parse("sovereign-op:tok_x_0001,sovereign-2fa:@otc");
    assert!(
        !stale.is_arm_capable(),
        "an '@otc' entry must reject the entire registry (OTC retired)"
    );
}

/// stage -> confirm -> disarm leaves >= 3 audit rows whose hash chain
/// verifies from the distinct genesis, and the table rejects UPDATE/DELETE
/// at the engine level (append-only triggers).
#[tokio::test]
async fn test_arm_audit_append_only_and_chained() {
    use gateway_sidecar::watch::api::{
        admin_arm_confirm_json, admin_arm_stage_json, admin_disarm_producer_json,
    };
    use gateway_sidecar::watch::db::{arm_audit_distinct_genesis, compute_arm_audit_preimage};
    use sha2::{Digest, Sha256};

    let (_tmp, db_path, db, q) = arm_audit_fixture().await;
    let principals = two_principals();

    // Full ceremony: stage (alice) -> confirm (bob) -> disarm (alice).
    let stage_resp = admin_arm_stage_json(
        q.clone(),
        principals.clone(),
        Duration::from_millis(120_000),
        Some("alice:tok_alpha_0001".to_string()),
        None,
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        true,
    )
    .await;
    assert_eq!(stage_resp.status(), StatusCode::OK);
    let bytes = to_bytes(stage_resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let stage_id = v["stage_id"].as_str().unwrap().to_string();
    let challenge = arm_attest_common::b64d(v["challenge"].as_str().unwrap());

    let confirm_resp = admin_arm_confirm_json(
        q.clone(),
        principals.clone(),
        Some("bob:tok_bravo_0002".to_string()),
        Some(arm_attest_common::se_confirm_body(&stage_id, &challenge)),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
        Arc::new(gateway_sidecar::watch::api::ArmDeviationTags::default()),
        arm_attest_common::loaded_attest_keys(),
        true,
    )
    .await;
    assert_eq!(confirm_resp.status(), StatusCode::OK);

    let disarm_resp = admin_disarm_producer_json(
        q.clone(),
        "unused_admin_token".to_string(),
        principals.clone(),
        Some("alice:tok_alpha_0001".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;
    assert_eq!(disarm_resp.status(), StatusCode::OK);

    // >= 3 rows: stage, confirm, disarm.
    let rows = db.list_arm_audit().await.unwrap();
    assert!(rows.len() >= 3, "expected >=3 audit rows; got {rows:?}");
    let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
    for needed in ["stage", "confirm", "disarm"] {
        assert!(
            actions.contains(&needed),
            "missing action {needed}: {actions:?}"
        );
    }

    // Chain verification: row 0 links to the distinct genesis; every row's
    // hash recomputes from its preimage; every prev_hash links backwards.
    let mut prev = arm_audit_distinct_genesis();
    for row in &rows {
        assert_eq!(
            row.prev_hash, prev,
            "row {} prev_hash must equal prior row hash/genesis",
            row.id
        );
        let preimage = compute_arm_audit_preimage(
            row.at_ms,
            &row.action,
            &row.principal,
            row.detail.as_deref().unwrap_or(""),
            &row.prev_hash,
        );
        let expect = hex::encode(Sha256::digest(preimage.as_bytes()));
        assert_eq!(row.hash, expect, "row {} hash mismatch", row.id);
        prev = row.hash.clone();
    }

    // Append-only at the engine level: UPDATE and DELETE must be rejected.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let upd = conn.execute("UPDATE arm_audit SET principal='evil' WHERE id=1", []);
    assert!(
        upd.is_err(),
        "UPDATE on arm_audit must be rejected by trigger"
    );
    let del = conn.execute("DELETE FROM arm_audit WHERE id=1", []);
    assert!(
        del.is_err(),
        "DELETE on arm_audit must be rejected by trigger"
    );
}

/// The legacy single-shot /watch/admin/producer/arm path is gone — 410
/// pointing at stage/confirm, so there is no four-eyes bypass.
#[tokio::test]
async fn test_legacy_single_shot_arm_route_gone() {
    use gateway_sidecar::watch::api::admin_arm_producer_json;
    let resp = admin_arm_producer_json().await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

// Outbox reads are admin-only (Invariant, Option 3): the D1/T1
// unauthed projection was REMOVED (§6 cadence/tenant leak). Unauthed GET -> 401
// before any store lookup, and the body must carry NONE of the old projection/
// metadata fields. Mutation wrappers (main.rs) still 401 on no-bearer.
#[tokio::test]
async fn t1_outbox_unauthed_is_401_and_mutation_auth() {
    let (_tmp, db) = outbox_fixture().await;
    let state = OutboxState::new(db.clone(), "dummy");
    let app = outbox_router(state);

    // unauthed (false) -> 401. Path tenant is the canary tenant ("sovereign"),
    // but auth is checked BEFORE the tripwire so we get 401, not 403, and the
    // store is never read (no 401-vs-403/404 oracle).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/sovereign/proj")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    // No leaked surface: none of the old projection / routing / cadence / count
    // fields may appear anywhere in the 401 body.
    for needle in [
        "projection",
        "envelope_sha256_jcs",
        "envelope_json_canonical",
        "created_at_ms",
        "directives",
        "sovereign",
    ] {
        assert!(
            !body.contains(needle),
            "401 body must not leak `{needle}`, got: {body}"
        );
    }

    // 4x distinct real main.rs 401 wrappers (N): oneshots to claim/heartbeat/worker_ack/nack routes in harness (which replicate the bearer extract + if bearer.is_none() { 401 "auth_required_for_mutation" } from main's watch_claim/heartbeat/worker_ack/nack_outbox exactly; not ack representative, not direct api calls). D1 unauthed proj asserts above remain.
    let resp_c = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/claim")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_c.status(), StatusCode::UNAUTHORIZED);
    let resp_h = app.clone()
        .oneshot(Request::builder().method("POST").uri("/watch/outbox/someid/heartbeat").header("content-type", "application/json").body(Body::from(r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true}}"#)).unwrap())
        .await.unwrap();
    assert_eq!(resp_h.status(), StatusCode::UNAUTHORIZED);
    let resp_wa = app.clone()
        .oneshot(Request::builder().method("POST").uri("/watch/outbox/someid/worker_ack").header("content-type", "application/json").body(Body::from(r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true}}"#)).unwrap())
        .await.unwrap();
    assert_eq!(resp_wa.status(), StatusCode::UNAUTHORIZED);
    let resp_n = app.clone()
        .oneshot(Request::builder().method("POST").uri("/watch/outbox/someid/nack").header("content-type", "application/json").body(Body::from(r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true},"error_reason":"sim-nack"}"#)).unwrap())
        .await.unwrap();
    assert_eq!(resp_n.status(), StatusCode::UNAUTHORIZED);
}

// Direct-call proof for the single GET handler: unauthed -> 401 BEFORE any store
// lookup (auth gate is first in `get_outbox_json`, so a missing id can never reach
// the 404 branch and 401-vs-404 cannot be used as an existence oracle). Body leaks
// none of the old projection/metadata fields.
#[tokio::test]
async fn t1_outbox_unauthed_single_get_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let resp = get_outbox_json(
        db,
        "sovereign".to_string(),
        "any-id".to_string(),
        false,
        "sovereign",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    for needle in [
        "projection",
        "envelope_sha256_jcs",
        "envelope_json_canonical",
        "envelope",
        "created_at_ms",
    ] {
        assert!(
            !body.contains(needle),
            "single-GET 401 body must not leak `{needle}`, got: {body}"
        );
    }
}

// H2 (single-tenant tripwire): direct-call proof for the LIST handler,
// symmetric to the single-GET test above — guards against router-wiring drift.
// Unauthed -> 401 BEFORE any store lookup; body leaks no projection/metadata
// fields (incl. the `directives` array).
#[tokio::test]
async fn t1_outbox_unauthed_list_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let resp = list_outbox_json(
        db,
        "sovereign".to_string(),
        None,
        None,
        50,
        false,
        "sovereign",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    for needle in [
        "projection",
        "envelope_sha256_jcs",
        "envelope_json_canonical",
        "envelope",
        "created_at_ms",
        "directives",
    ] {
        assert!(
            !body.contains(needle),
            "list 401 body must not leak `{needle}`, got: {body}"
        );
    }
}

// ---------------------------------------------------------------------------
// T1 fix proof (security: presence-only auth → real constant-time admin token).
//
// The bug was "any non-empty Bearer is accepted". The existing t1 test only
// covered the NO-bearer case (which always 401'd). These tests cover the
// gap the bug lived in: a *junk, non-empty* bearer must now 401 on every
// mutation, the *valid* token must clear the gate, and a junk bearer must
// NOT unlock the authed-read plaintext (it gets a 401 — outbox reads are
// admin-only, the invariant Option 3; no public projection fallback).
// ---------------------------------------------------------------------------

const T1_ADMIN: &str = "the-real-admin-token";

/// claim: junk non-empty bearer → 401 (was 2xx/4xx under presence-only auth).
#[tokio::test]
async fn t1_fix_claim_junk_bearer_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/claim")
                .header("authorization", "Bearer not-the-admin-token")
                .header("x-tenant-scope", "sovereign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// heartbeat: junk non-empty bearer → 401.
#[tokio::test]
async fn t1_fix_heartbeat_junk_bearer_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-new/heartbeat")
                .header("authorization", "Bearer not-the-admin-token")
                .header("x-tenant-scope", "sovereign")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// worker_ack: junk non-empty bearer → 401.
#[tokio::test]
async fn t1_fix_worker_ack_junk_bearer_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-new/worker_ack")
                .header("authorization", "Bearer not-the-admin-token")
                .header("x-tenant-scope", "sovereign")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// nack: junk non-empty bearer → 401.
#[tokio::test]
async fn t1_fix_nack_junk_bearer_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-new/nack")
                .header("authorization", "Bearer not-the-admin-token")
                .header("x-tenant-scope", "sovereign")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true},"error_reason":"sim-nack"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Valid admin token clears the gate. claim returns 200 with the claimed set;
/// heartbeat/worker_ack/nack get past auth and reach business logic (so they
/// return a NON-401 status — never the unauthorized gate).
#[tokio::test]
async fn t1_fix_valid_admin_token_clears_gate() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));

    // claim with the real token → 200 (clean success path).
    let resp_claim = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/claim")
                .header("authorization", format!("Bearer {T1_ADMIN}"))
                .header("x-tenant-scope", "sovereign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"limit":5,"lease_duration_ms":30000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_claim.status(), StatusCode::OK);

    // heartbeat/worker_ack/nack: with a valid token the gate is OPEN, so they
    // proceed to business logic (a bad/missing handle yields 400/404 — anything
    // but 401). The point is the auth gate no longer fires.
    for (uri, body) in [
        (
            "/watch/outbox/dir-new/heartbeat",
            r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true}}"#,
        ),
        (
            "/watch/outbox/dir-new/worker_ack",
            r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true}}"#,
        ),
        (
            "/watch/outbox/dir-new/nack",
            r#"{"worker_provenance":{"status":"opaque_handle_only","fabrication_guard":true},"error_reason":"x"}"#,
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("authorization", format!("Bearer {T1_ADMIN}"))
                    .header("x-tenant-scope", "sovereign")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "valid token must clear the gate for {uri}"
        );
    }
}

/// Read surface: a junk non-empty bearer must NOT unlock plaintext. Outbox reads
/// are admin-only (the invariant, Option 3): a junk bearer fails the
/// constant-time token compare and gets a 401 — there is no public projection to
/// fall back to. The body must leak none of the old projection/metadata fields.
#[tokio::test]
async fn t1_fix_read_junk_bearer_is_401() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/sovereign/authed-list")
                .header("authorization", "Bearer not-the-admin-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    for needle in [
        "projection",
        "envelope_sha256_jcs",
        "envelope_json_canonical",
        "envelope",
        "directives",
    ] {
        assert!(
            !body.contains(needle),
            "junk-bearer 401 body must not leak `{needle}`, got: {body}"
        );
    }
}

/// Read surface: the valid admin token DOES unlock the full plaintext envelope.
#[tokio::test]
async fn t1_fix_read_valid_token_gets_plaintext() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/watch/outbox/sovereign/authed-list")
                .header("authorization", format!("Bearer {T1_ADMIN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let dirs = v
        .get("directives")
        .and_then(|d| d.as_array())
        .expect("directives array");
    let first = dirs.first().expect("at least one directive");
    assert!(
        first.get("envelope").is_some(),
        "valid token must unlock full plaintext envelope, got: {first}"
    );
}

/// tenant-policy POST (previously had NO auth at all): junk/no bearer → 401,
/// valid admin token → success.
#[tokio::test]
async fn t1_fix_tenant_policy_requires_admin_token() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let policy_body = r#"{"tenant":"sovereign"}"#;

    // no bearer → 401
    let resp_none = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/policy/sovereign")
                .header("content-type", "application/json")
                .body(Body::from(policy_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_none.status(), StatusCode::UNAUTHORIZED);

    // junk bearer → 401
    let resp_junk = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/policy/sovereign")
                .header("authorization", "Bearer not-the-admin-token")
                .header("content-type", "application/json")
                .body(Body::from(policy_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_junk.status(), StatusCode::UNAUTHORIZED);

    // valid admin token → success (200)
    let resp_ok = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/policy/sovereign")
                .header("authorization", format!("Bearer {T1_ADMIN}"))
                .header("content-type", "application/json")
                .body(Body::from(policy_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ok.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// W1 BLOCKING: single-tenant tripwire (Council Wave-1 condition).
//
// The W1 fix gates on a GLOBAL admin token, so a token holder could otherwise
// target ANY tenant via X-Tenant-Scope. The tripwire (`assert_canary_tenant`)
// makes that fail LOUD (403 single_tenant_violation) instead of silent the day
// a second tenant onboards. Removed in Wave 2 when capability tokens land.
// ---------------------------------------------------------------------------

/// Foreign X-Tenant-Scope with a VALID admin token → 403 single_tenant_violation
/// (the gap the tripwire closes: a good token must NOT reach a non-canary tenant).
#[tokio::test]
async fn t1_tripwire_foreign_scope_is_403() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/claim")
                .header("authorization", format!("Bearer {T1_ADMIN}"))
                .header("x-tenant-scope", "not-sovereign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"limit":5,"lease_duration_ms":30000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["error"],
        serde_json::Value::from("single_tenant_violation")
    );
}

/// Canary scope ("sovereign") + valid token → success path unaffected by the
/// tripwire (claim returns 200 with the claimed set).
#[tokio::test]
async fn t1_tripwire_canary_scope_unaffected() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/claim")
                .header("authorization", format!("Bearer {T1_ADMIN}"))
                .header("x-tenant-scope", "sovereign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"limit":5,"lease_duration_ms":30000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v.get("claimed").and_then(|c| c.as_array()).is_some(),
        "canary scope must reach the claim success path, got: {v}"
    );
}

/// ack is now ALSO under the single-tenant tripwire. A valid
/// admin token + a non-canary `X-Tenant-Scope` → 403 single_tenant_violation,
/// fired BEFORE the DB ack — so a matched non-canary (row==scope) pair can't
/// silently operate the day a second tenant onboards.
#[tokio::test]
async fn t1_tripwire_ack_foreign_scope_is_403() {
    let (_tmp, db) = outbox_fixture().await;
    let app = outbox_router(OutboxState::new(db, T1_ADMIN));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/watch/outbox/dir-new/ack")
                .header("authorization", format!("Bearer {T1_ADMIN}"))
                .header("x-tenant-scope", "not-sovereign")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["error"],
        serde_json::Value::from("single_tenant_violation")
    );
}

/// W1 env-config refinement (fix(watch): WATCH_CANARY_TENANT). The single-tenant
/// guard is now a deployment config (default "sovereign"); the configured value,
/// not a hardcoded const, decides which tenant clears. This proves the guard
/// TRACKS the config and stays fail-closed:
///   * the configured tenant ("phase3-smoke") clears the tripwire → claim 200;
///   * an arbitrary other tenant → 403 single_tenant_violation;
///   * the historical default "sovereign" ALSO → 403 once the config is
///     non-sovereign (so the guard is not pinned to a const — it generalizes to
///     exactly the one configured tenant, still loud, still fail-closed).
/// Drives the same in-process router used by the other tripwire tests; the
/// only difference is `OutboxState.canary_tenant`, mirroring what
/// `AppState.watch_canary_tenant` holds in production (resolved once at boot
/// from `WATCH_CANARY_TENANT` via `api::resolve_canary_tenant`).
#[tokio::test]
async fn t1_tripwire_tracks_configured_tenant() {
    let (_tmp, db) = outbox_fixture().await;
    // Pin the guard to a NON-default tenant, exactly as the CI/phase-3-smoke
    // sidecar does via WATCH_CANARY_TENANT=phase3-smoke.
    let state = OutboxState {
        db,
        admin_token: T1_ADMIN.into(),
        canary_tenant: "phase3-smoke".into(),
    };
    let app = outbox_router(state);

    let claim = |scope: &str| {
        let app = app.clone();
        let scope = scope.to_string();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/watch/outbox/claim")
                    .header("authorization", format!("Bearer {T1_ADMIN}"))
                    .header("x-tenant-scope", scope)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"limit":5,"lease_duration_ms":30000}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // 1) The CONFIGURED tenant clears the guard and reaches the claim success path.
    let resp_ok = claim("phase3-smoke").await;
    assert_eq!(
        resp_ok.status(),
        StatusCode::OK,
        "configured canary tenant must clear the tripwire"
    );
    let bytes = to_bytes(resp_ok.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v.get("claimed").and_then(|c| c.as_array()).is_some(),
        "configured tenant must reach the claim success path, got: {v}"
    );

    // 2) An arbitrary OTHER tenant is rejected — guard still fail-closed.
    let resp_other = claim("some-other-tenant").await;
    assert_eq!(resp_other.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(resp_other.into_body(), 64 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["error"],
        serde_json::Value::from("single_tenant_violation")
    );

    // 3) The historical default "sovereign" ALSO gets 403 now that the config is
    //    non-sovereign — proving the guard tracks the configured value, not a
    //    hardcoded const, and that the override is a true single-tenant pin.
    let resp_sovereign = claim("sovereign").await;
    assert_eq!(
        resp_sovereign.status(),
        StatusCode::FORBIDDEN,
        "with a non-default config, even 'sovereign' must be rejected — \
         the guard tracks config, not a const"
    );
    let bytes = to_bytes(resp_sovereign.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["error"],
        serde_json::Value::from("single_tenant_violation")
    );
}

/// H7a: auto_disarm_producer drains an armed producer, writes a `disarm`
/// audit row attributed to the recon trigger, and is idempotent on a second
/// call (already-disarmed → no-op, no new kill).
#[tokio::test]
async fn test_auto_disarm_producer_drains_and_is_idempotent() {
    use gateway_sidecar::watch::api::{auto_disarm_producer, ArmNotifier};

    let (_tmp, _path, db, q) = arm_audit_fixture().await;

    // Install a fake armed producer: a watch kill channel + a responder task
    // that acks the drain (mirrors the real cdc_sweep_loop kill handling).
    let (kill_tx, mut kill_rx) = tokio::sync::watch::channel(false);
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<()>();
    *q.producer_kill_state.lock() = Some((kill_tx, ack_rx));
    tokio::spawn(async move {
        // Wait for the kill signal, then ack the drain.
        while kill_rx.changed().await.is_ok() {
            if *kill_rx.borrow() {
                let _ = ack_tx.send(());
                break;
            }
        }
    });

    let notifier = ArmNotifier::for_tests(None);
    auto_disarm_producer(&q, &notifier, "recon-divergence(auto)", "test divergence").await;

    // Producer disarmed (kill state consumed).
    assert!(
        q.producer_kill_state.lock().is_none(),
        "auto-disarm must consume the producer kill state"
    );

    // Audit chain carries the disarm attributed to the recon trigger.
    let rows = db.list_arm_audit().await.unwrap();
    let disarm = rows
        .iter()
        .find(|r| r.action == "disarm")
        .expect("a disarm audit row must exist");
    assert_eq!(disarm.principal, "recon-divergence(auto)");

    // Idempotent: a second call with no kill state is a clean no-op.
    auto_disarm_producer(&q, &notifier, "recon-divergence(auto)", "second call").await;
    assert!(q.producer_kill_state.lock().is_none());
}
