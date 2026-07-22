//! the four-eyes ceremony is
//! proven end-to-end through the ACTUAL axum router built by
//! `watch::api::arm_admin_router` (the same sub-router main.rs merges),
//! not by calling the handler fns directly. This pins route paths, the
//! Bearer-header extraction, the JSON body plumbing, and the legacy-410.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use gateway_sidecar::watch::api::{
    arm_admin_router, ArmAdminRouterState, ArmDeviationTags, ArmNotifier, ArmPrincipals,
};
use gateway_sidecar::watch::attest::AttestKeyRegistry;
use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// Attested-arm — minimal signature material for the direct `confirm_arm_attest`
/// unit tests below. These tests assert the confirm OUTCOME (drift/desync
/// rejection or Verified), not the reserve's spend-time re-verify, so the
/// persisted material only needs to satisfy the NOT NULL columns.
fn dummy_signed_material() -> gateway_sidecar::watch::db::PersistedArmSignature {
    gateway_sidecar::watch::db::PersistedArmSignature {
        credential_id: "se-cred-0001".to_string(),
        credential_type: "se-p256".to_string(),
        signature_der: vec![0u8; 8],
        authenticator_data: None,
        client_data_json: None,
    }
}

async fn fixture_with_principals(
    principals: &str,
) -> (
    tempfile::TempDir,
    Arc<WatchDb>,
    Arc<QuarantineState>,
    axum::Router,
) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_arm_router.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let router: axum::Router = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse(principals)),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        // RIDER C disabled in tests (no URL): notify is a no-op.
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(AttestKeyRegistry::unloaded()),
        allow_real_arm: true,
    });
    (tmp, db, quarantine, router)
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<WatchDb>,
    Arc<QuarantineState>,
    axum::Router,
) {
    fixture_with_principals("alice:tok_alpha_0001,bob:tok_bravo_0002").await
}

fn post(path: &str, bearer: Option<&str>, body: Option<serde_json::Value>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri(path);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Legacy single-shot arm is permanently 410 Gone over HTTP.
#[tokio::test]
async fn test_router_legacy_arm_is_410() {
    let (_tmp, _db, _q, router) = fixture().await;
    let resp = router
        .oneshot(post("/watch/admin/producer/arm", Some("anything"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
}

/// Full ceremony over HTTP: alice stages (Bearer), bob confirms (Bearer +
/// attest body) -> armed; admin-token disarm drains and unarms. B6: a
/// bearer-only confirm body is 400 — nothing arms on tokens alone (spec §2).
#[tokio::test]
async fn test_router_stage_confirm_disarm_roundtrip() {
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;

    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    // Bearer-only body (no credential fields): 400, not armed.
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/confirm",
            Some("bob:tok_bravo_0002"),
            Some(serde_json::json!({ "stage_id": stage_id })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "bearer-only confirm must 400 (credential fields required, spec §4.2)"
    );
    assert!(quarantine.producer_kill_state.lock().is_none());

    // Confirm as bob with the attest body.
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/confirm",
            Some("bob:tok_bravo_0002"),
            Some(se_confirm_body(&stage_id, &challenge)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "confirm by bob must 200");
    assert!(
        quarantine.producer_kill_state.lock().is_some(),
        "producer must be armed after the full HTTP ceremony"
    );

    // Disarm with the shared admin token (cleanup + proves the route).
    let resp = router
        .oneshot(post(
            "/watch/admin/producer/disarm",
            Some("shared-admin-token"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "disarm must 200");
    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "producer must be unarmed after disarm"
    );

    // Audit chain carries the whole ceremony.
    let rows = db.list_arm_audit().await.unwrap();
    let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
    for needed in ["stage", "confirm", "disarm"] {
        assert!(
            actions.contains(&needed),
            "audit chain must carry '{needed}'; got {actions:?}"
        );
    }
}

/// No/garbage Bearer over HTTP: stage 401s. P1 :
/// UNAUTHENTICATED rejections are counted in the prunable
/// `arm_rejected_unauth_total` metric and must NOT append permanent rows to
/// the engine-unprunable arm_audit chain (an attacker who can reach the UDS
/// could otherwise grow it without bound, one row per request).
#[tokio::test]
async fn test_router_stage_rejects_bad_bearer() {
    let (_tmp, db, q, router) = fixture().await;
    let resp = router
        .clone()
        .oneshot(post("/watch/admin/producer/arm/stage", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = router
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("mallory:tok_wrong_9999"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let rows = db.list_arm_audit().await.unwrap();
    assert!(
        !rows.iter().any(|r| r.action == "stage_rejected"),
        "401 unauthenticated rejections must NOT write permanent arm_audit rows (DoS guard); got {rows:?}"
    );
    assert_eq!(
        q.arm_rejected_unauth_total(),
        2,
        "both unauthenticated 401s must be counted in arm_rejected_unauth_total"
    );
}

/// RIDER D — deviation/domain tags land inside the hash-chained detail
/// strings for both stage and confirm (B6: single operator + local-attest;
/// the OTC second principal is retired, spec §9).
#[tokio::test]
async fn test_deviation_tags_in_audit_detail() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(WatchDb::open(&tmp.path().join("w.db")).await.unwrap());
    db.run_migrations().await.unwrap();
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let registry = AttestKeyRegistry::parse(&attest_registry_json());
    assert!(registry.is_loaded());
    let router: axum::Router = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse("sovereign-op:tok_static_0001")),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::for_tests(
            Some("dual-custody-local-attest".to_string()),
            vec![("sovereign-op".to_string(), "host".to_string())],
        )),
        attest_keys: Arc::new(registry),
        allow_real_arm: true,
    });

    let (stage_id, challenge) = stage_as(&router, "sovereign-op:tok_static_0001").await;
    let resp = confirm_with(
        &router,
        "sovereign-op:tok_static_0001",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let rows = db.list_arm_audit().await.unwrap();
    let stage = rows.iter().find(|r| r.action == "stage").unwrap();
    assert!(stage
        .detail
        .as_deref()
        .unwrap()
        .contains("deviation=dual-custody-local-attest domain=host"));
    let confirm = rows.iter().find(|r| r.action == "confirm").unwrap();
    assert!(confirm
        .detail
        .as_deref()
        .unwrap()
        .contains("deviation=dual-custody-local-attest domain=host"));
}

// ---------------------------------------------------------------------------
// dual-custody-local-attest B1 (spec §4.3, restart-recovery invariant):
// persisted pending stage (arm_pending) + GET /arm/pending + crash-resume.
// ---------------------------------------------------------------------------

fn get(path: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

/// Same fixture as `fixture_with_principals` but with a caller-chosen stage
/// TTL, for expiry tests. Returns the db so a "restarted" QuarantineState
/// can be built over the same file.
async fn fixture_with_ttl(
    principals: &str,
    ttl: Duration,
) -> (
    tempfile::TempDir,
    Arc<WatchDb>,
    Arc<QuarantineState>,
    axum::Router,
) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_arm_router.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let router: axum::Router = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse(principals)),
        stage_ttl: ttl,
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(AttestKeyRegistry::unloaded()),
        allow_real_arm: true,
    });
    (tmp, db, quarantine, router)
}

/// GET /arm/pending: 401 unauthenticated (counted, never audited — same DoS
/// posture as stage/confirm), 404 with no open stage.
#[tokio::test]
async fn test_router_pending_unauth_and_empty() {
    let (_tmp, db, q, router) = fixture().await;
    let resp = router
        .clone()
        .oneshot(get("/watch/admin/producer/arm/pending", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        q.arm_rejected_unauth_total(),
        1,
        "unauthenticated pending read must be counted"
    );
    assert!(
        db.list_arm_audit().await.unwrap().is_empty(),
        "401 pending read must not write arm_audit rows"
    );

    let resp = router
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("alice:tok_alpha_0001"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no stage open → 404");
}

/// GET /arm/pending returns the open stage, and a re-fetch returns the SAME
/// stage_id and the SAME challenge bytes — the challenge is created once at
/// stage time and is stable for the stage's life (spec §4.3: never a new
/// nonce on re-fetch). B2 (spec §5): the stage response itself carries the
/// challenge, identical to the stored bytes served by /arm/pending, and the
/// canonical JSON embeds the staged↔approved binding fields.
#[tokio::test]
async fn test_router_pending_returns_stable_open_stage() {
    use base64::Engine as _;
    let (_tmp, _db, _q, router) = fixture().await;
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let staged = body_json(resp).await;
    let stage_id = staged["stage_id"].as_str().unwrap().to_string();
    let stage_challenge = staged["challenge"]
        .as_str()
        .expect("B2: stage response must carry the challenge")
        .to_string();

    // The challenge is canonical JCS embedding the binding fields (spec §5).
    let challenge_bytes = base64::engine::general_purpose::STANDARD
        .decode(&stage_challenge)
        .expect("challenge must be base64");
    let parsed: serde_json::Value = serde_json::from_slice(&challenge_bytes).unwrap();
    assert_eq!(
        parsed["v"], 3,
        "Attested-arm bumped the challenge format to v3 (signed spend_window_ms)"
    );
    assert_eq!(parsed["kind"], "arm-confirm-challenge");
    assert_eq!(parsed["stage_id"].as_str().unwrap(), stage_id);
    assert_eq!(parsed["staged_by"], "alice");
    assert_eq!(parsed["deviation_tag"], "dual-custody-local-attest");
    // T1 MF-1: the 4 content-binding fields are embedded in the signed bytes.
    assert_eq!(parsed["enabled_surface"], "watch-producer");
    assert_eq!(parsed["tenant"], "canary");
    assert!(
        parsed["effective_daily_cap_cents"].is_i64(),
        "cap must be integer cents on the signed path"
    );
    assert!(
        parsed["build_id"].as_str().is_some(),
        "build_id must be embedded"
    );
    // Attested-arm: the spend window is now SIGNED (integer ms) so it cannot be
    // extended post-tap without a fresh signature.
    assert!(
        parsed["spend_window_ms"].is_i64(),
        "spend_window_ms must be embedded as integer ms on the signed path"
    );

    let resp = router
        .clone()
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("bob:tok_bravo_0002"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let first = body_json(resp).await;
    assert_eq!(first["stage_id"].as_str().unwrap(), stage_id);
    assert_eq!(
        first["challenge"].as_str().unwrap(),
        stage_challenge,
        "pending must serve the exact bytes issued at stage time"
    );
    let expires = first["expires_in_ms"].as_u64().unwrap();
    assert!(
        expires > 0 && expires <= 120_000,
        "expires_in_ms must be the remaining wall-clock ttl; got {expires}"
    );

    let resp = router
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("alice:tok_alpha_0001"),
        ))
        .await
        .unwrap();
    let second = body_json(resp).await;
    assert_eq!(second["stage_id"], first["stage_id"]);
    assert_eq!(
        second["challenge"], first["challenge"],
        "re-fetch must return the same challenge bytes, never a new nonce"
    );
}

/// B1 acceptance — restart-rehydrate: a stage written before a sidecar
/// restart survives it. A FRESH QuarantineState over the same watch.db
/// rehydrates the pending row (same stage_id), GET /arm/pending still serves
/// it, and the ceremony COMPLETES against the rehydrated stage. The durable
/// row is cleared by the confirm.
#[tokio::test]
async fn test_pending_rehydrates_after_restart_and_ceremony_completes() {
    let (_tmp, db, _q1, router1) = fixture().await;
    let resp = router1
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let stage_body = body_json(resp).await;
    let stage_id = stage_body["stage_id"].as_str().unwrap().to_string();
    let challenge = b64d(stage_body["challenge"].as_str().unwrap());

    // "Restart": a fresh in-memory state over the SAME db file, then boot
    // rehydration (what main.rs runs).
    let quarantine2 = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let rehydrated = quarantine2.rehydrate_arm_pending().await;
    assert_eq!(
        rehydrated.as_deref(),
        Some(stage_id.as_str()),
        "unexpired pending stage must rehydrate with the same stage_id"
    );
    assert!(
        quarantine2
            .arm_staging
            .lock()
            .as_ref()
            .is_some_and(|s| s.stage_id == stage_id),
        "in-memory staging slot must mirror the rehydrated row"
    );

    let router2: axum::Router = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine2.clone(),
        principals: Arc::new(ArmPrincipals::parse(
            "alice:tok_alpha_0001,bob:tok_bravo_0002",
        )),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(AttestKeyRegistry::parse(&attest_registry_json())),
        allow_real_arm: true,
    });

    // The resumed stage is visible over HTTP...
    let resp = router2
        .clone()
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("alice:tok_alpha_0001"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await["stage_id"].as_str().unwrap(),
        stage_id
    );

    // ...and the ceremony completes against it after the "restart" — the
    // signature verifies against the REHYDRATED stored challenge bytes.
    let resp = router2
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/confirm",
            Some("bob:tok_bravo_0002"),
            Some(se_confirm_body(&stage_id, &challenge)),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "confirm must arm the rehydrated stage"
    );
    assert!(quarantine2.producer_kill_state.lock().is_some());

    // Confirm consumed the durable row (queried at now=0 so even an
    // unexpired leftover would show — None means DELETED).
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "confirm must clear the arm_pending row"
    );

    // Cleanup.
    let resp = router2
        .oneshot(post(
            "/watch/admin/producer/disarm",
            Some("shared-admin-token"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// B1 acceptance — an EXPIRED pending row is dead: not rehydrated at boot,
/// not served by GET /arm/pending.
#[tokio::test]
async fn test_pending_expired_row_not_rehydrated() {
    let (_tmp, db, _q1, router) = fixture_with_ttl(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
        Duration::from_millis(1),
    )
    .await;
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(25)).await;

    let quarantine2 = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    assert!(
        quarantine2.rehydrate_arm_pending().await.is_none(),
        "expired pending row must NOT rehydrate"
    );
    assert!(
        quarantine2.arm_staging.lock().is_none(),
        "staging slot must stay empty after refusing an expired row"
    );

    let resp = router
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("bob:tok_bravo_0002"),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "expired pending row must not be served"
    );
}

/// B1 — an expired stage detected at confirm time clears the durable row
/// (spec §4.3: cleared on confirm, expiry, or disarm).
#[tokio::test]
async fn test_confirm_on_expired_stage_clears_pending_row() {
    let (_tmp, db, _q, router) = fixture_with_ttl(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
        Duration::from_millis(1),
    )
    .await;
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            None,
        ))
        .await
        .unwrap();
    let stage_body = body_json(resp).await;
    let stage_id = stage_body["stage_id"].as_str().unwrap().to_string();
    let challenge = b64d(stage_body["challenge"].as_str().unwrap());
    tokio::time::sleep(Duration::from_millis(25)).await;

    let resp = router
        .oneshot(post(
            "/watch/admin/producer/arm/confirm",
            Some("bob:tok_bravo_0002"),
            Some(se_confirm_body(&stage_id, &challenge)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "expiry detection must delete the arm_pending row"
    );
}

/// B1 — disarm kills any open ceremony: pending stage cleared in memory and
/// durably (spec §4.3).
#[tokio::test]
async fn test_disarm_clears_pending_stage() {
    let (_tmp, db, quarantine, router) = fixture().await;
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(db.get_arm_pending(0).await.unwrap().is_some());

    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/disarm",
            Some("shared-admin-token"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        quarantine.arm_staging.lock().is_none(),
        "disarm must clear the in-memory staging slot"
    );
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "disarm must clear the durable arm_pending row"
    );

    let resp = router
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("alice:tok_alpha_0001"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// B4 — local-attest confirm leg (spec §4.2/§4.3/§6, review).
// Every §6 rejection reason has a test; the concurrent-confirm test proves
// the TOCTOU is closed by the one-tx design; the counter-keying tests pin
// credential-type invariant (behavior keys on credential_type ONLY —
// nothing branches on counter == 0).
// ---------------------------------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64d(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

/// Same fixed scalar as the attest.rs unit tests — NOT a real credential.
fn attest_signing_key() -> p256::ecdsa::SigningKey {
    p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).unwrap()
}

/// Registry JSON holding one credential of each type, both bound to the
/// test key (a registry credential is (id, type, pubkey) — sharing the
/// pubkey is fine and lets one signer drive both ceremony shapes).
fn attest_registry_json() -> String {
    let pk = b64(attest_signing_key()
        .verifying_key()
        .to_encoded_point(true)
        .as_bytes());
    serde_json::json!([
        {"credential_id": "se-cred-0001", "credential_type": "se-p256",
         "public_key": pk, "label": "test SE", "enrolled_at": "2026-06-12T00:00:00Z"},
        {"credential_id": "fido2-cred-0001", "credential_type": "fido2-es256",
         "public_key": pk, "label": "test FIDO2", "enrolled_at": "2026-06-12T00:00:00Z"}
    ])
    .to_string()
}

/// Fixture with a LOADED attest registry (the unloaded-registry cases use
/// the plain `fixture()`).
async fn fixture_attest(
    principals: &str,
) -> (
    tempfile::TempDir,
    Arc<WatchDb>,
    Arc<QuarantineState>,
    axum::Router,
) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_arm_router.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let registry = AttestKeyRegistry::parse(&attest_registry_json());
    assert!(registry.is_loaded(), "test registry must parse as loaded");
    let router: axum::Router = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse(principals)),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(registry),
        allow_real_arm: true,
    });
    (tmp, db, quarantine, router)
}

/// Stage and return (stage_id, verbatim challenge bytes from the response).
async fn stage_as(router: &axum::Router, bearer: &str) -> (String, Vec<u8>) {
    let resp = router
        .clone()
        .oneshot(post("/watch/admin/producer/arm/stage", Some(bearer), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "stage must 200");
    let v = body_json(resp).await;
    let stage_id = v["stage_id"].as_str().expect("stage_id").to_string();
    let challenge = b64d(v["challenge"].as_str().expect("challenge"));
    (stage_id, challenge)
}

fn sign_se_p256(challenge: &[u8]) -> String {
    use p256::ecdsa::signature::Signer;
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(challenge);
    b64(sig.to_der().as_bytes())
}

/// Minimal CTAP authenticatorData: rpIdHash(32, unchecked) || flags || counter.
fn fido2_authenticator_data(counter: u32, up_flag: bool) -> Vec<u8> {
    let mut ad = vec![0u8; 32];
    ad.push(if up_flag { 0x01 } else { 0x00 });
    ad.extend_from_slice(&counter.to_be_bytes());
    ad
}

/// FIDO2 assertion per spec §5: sign authenticatorData || SHA-256(challenge).
fn sign_fido2(challenge: &[u8], counter: u32) -> (String, String) {
    use p256::ecdsa::signature::Signer;
    use sha2::{Digest, Sha256};
    let ad = fido2_authenticator_data(counter, true);
    let mut msg = ad.clone();
    msg.extend_from_slice(&Sha256::digest(challenge));
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&msg);
    (b64(sig.to_der().as_bytes()), b64(&ad))
}

fn se_confirm_body(stage_id: &str, challenge: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "stage_id": stage_id,
        "credential_id": "se-cred-0001",
        "credential_type": "se-p256",
        "signature": sign_se_p256(challenge),
    })
}

fn fido2_confirm_body(stage_id: &str, challenge: &[u8], counter: u32) -> serde_json::Value {
    let (sig, ad) = sign_fido2(challenge, counter);
    serde_json::json!({
        "stage_id": stage_id,
        "credential_id": "fido2-cred-0001",
        "credential_type": "fido2-es256",
        "signature": sig,
        "authenticator_data": ad,
    })
}

async fn confirm_with(
    router: &axum::Router,
    bearer: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/confirm",
            Some(bearer),
            Some(body),
        ))
        .await
        .unwrap()
}

async fn disarm_ok(router: &axum::Router) {
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/disarm",
            Some("shared-admin-token"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "disarm must 200");
}

/// Latest confirm_rejected detail in the audit chain (rejection-reason tests).
async fn last_reject_detail(db: &WatchDb) -> String {
    db.list_arm_audit()
        .await
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.action == "confirm_rejected")
        .and_then(|r| r.detail.clone())
        .expect("a confirm_rejected row must exist")
}

/// Happy path, se-p256: alice stages AND alice confirms — the same-principal
/// 403 is RETIRED for the attest path (spec §2: the second custody domain is
/// the SE key, not a second bearer). The §6 confirm row binds the ceremony.
#[tokio::test]
async fn test_attest_se_p256_ceremony_same_principal_arms() {
    use sha2::{Digest, Sha256};
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    let resp = confirm_with(
        &router,
        "alice:tok_alpha_0001",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "se-p256 attest confirm by the STAGING principal must arm (same-principal rule retired for attest)"
    );
    assert!(
        quarantine.producer_kill_state.lock().is_some(),
        "producer must be armed"
    );
    assert!(
        quarantine.arm_staging.lock().is_none(),
        "confirm must clear the in-memory staging cache"
    );
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "confirm must consume the durable arm_pending row"
    );

    // §6 audit binding: the confirm row's detail is machine-parseable JSON
    // carrying mechanism, credential, counter, and the challenge digest.
    let rows = db.list_arm_audit().await.unwrap();
    let confirm_row = rows
        .iter()
        .find(|r| r.action == "confirm")
        .expect("confirm row");
    assert_eq!(confirm_row.principal, "alice");
    let detail: serde_json::Value =
        serde_json::from_str(confirm_row.detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["mechanism"], "local-attest");
    assert_eq!(detail["credential_id"], "se-cred-0001");
    assert_eq!(detail["credential_type"], "se-p256");
    assert_eq!(detail["sig_counter"], 0);
    assert_eq!(detail["stage_id"], stage_id.as_str());
    assert_eq!(detail["staged_by"], "alice");
    assert_eq!(
        detail["challenge_sha256"],
        hex::encode(Sha256::digest(&challenge)),
        "challenge_sha256 must digest the VERBATIM stored challenge bytes"
    );
}

/// Happy path, fido2-es256: full ceremony with a counter, §6 row carries it.
#[tokio::test]
async fn test_attest_fido2_ceremony_arms_and_records_counter() {
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        fido2_confirm_body(&stage_id, &challenge, 5),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fido2 attest confirm must arm"
    );
    assert!(quarantine.producer_kill_state.lock().is_some());

    let rows = db.list_arm_audit().await.unwrap();
    let detail: serde_json::Value = serde_json::from_str(
        rows.iter()
            .find(|r| r.action == "confirm")
            .unwrap()
            .detail
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(detail["credential_type"], "fido2-es256");
    assert_eq!(detail["sig_counter"], 5);
}

/// §6 rejection: registry_unloaded — fail-closed registry rejects EVERY
/// attest confirm, even a cryptographically valid one. (The plain fixture
/// carries the unloaded registry.)
#[tokio::test]
async fn test_attest_registry_unloaded_rejects_all() {
    let (_tmp, db, quarantine, router) = fixture().await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "nothing may arm against an unloaded registry"
    );
    assert!(
        last_reject_detail(&db).await.contains("registry_unloaded"),
        "audit must carry the §6 reason registry_unloaded"
    );
    assert!(
        db.get_arm_pending(0).await.unwrap().is_some(),
        "a rejected confirm must leave the pending stage intact (re-confirm is allowed)"
    );
}

/// §6 rejection: unknown_credential — unknown id, AND a known id presented
/// with the wrong credential_type (no cross-type confusion).
#[tokio::test]
async fn test_attest_unknown_credential_rejects() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    let mut body = se_confirm_body(&stage_id, &challenge);
    body["credential_id"] = serde_json::json!("nope-cred-9999");
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("unknown_credential"));

    // Known id, mismatched type — still unknown_credential.
    let mut body = se_confirm_body(&stage_id, &challenge);
    body["credential_type"] = serde_json::json!("fido2-es256");
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("unknown_credential"));

    // The stage survives rejections: a correct confirm still arms.
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// §6 rejection: bad_signature — garbage DER, signature over a DIFFERENT
/// challenge, raw r||s, and se-p256 presented WITH authenticator_data (the
/// §5 composition is type-keyed; mixing is rejected).
#[tokio::test]
async fn test_attest_bad_signature_rejects() {
    use p256::ecdsa::signature::Signer;
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    // Garbage bytes where DER should be.
    let mut body = se_confirm_body(&stage_id, &challenge);
    body["signature"] = serde_json::json!(b64(&[1u8, 2, 3, 4]));
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("bad_signature"));

    // Valid DER signature — over the WRONG bytes.
    let mut body = se_confirm_body(&stage_id, &challenge);
    body["signature"] = serde_json::json!(sign_se_p256(b"not the stored challenge"));
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("bad_signature"));

    // Raw r||s (64 bytes) — DER only per spec §5.
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&challenge);
    let mut body = se_confirm_body(&stage_id, &challenge);
    body["signature"] = serde_json::json!(b64(&sig.to_bytes()));
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("bad_signature"));

    // se-p256 with authenticator_data attached — type-keyed composition.
    let mut body = se_confirm_body(&stage_id, &challenge);
    body["authenticator_data"] = serde_json::json!(b64(&fido2_authenticator_data(1, true)));
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("bad_signature"));

    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "no bad-signature variant may arm"
    );
}

/// §6 rejection: counter_regression — a fido2 counter must STRICTLY increase
/// across ceremonies (persisted in arm_attest_counters, in-tx).
#[tokio::test]
async fn test_attest_fido2_counter_regression_rejects() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;

    // Ceremony 1 at counter 5 arms.
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        fido2_confirm_body(&stage_id, &challenge, 5),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    disarm_ok(&router).await;

    // Ceremony 2: replayed counter 5 → counter_regression.
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        fido2_confirm_body(&stage_id, &challenge, 5),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("counter_regression"));

    // Lower still → counter_regression.
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        fido2_confirm_body(&stage_id, &challenge, 4),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("counter_regression"));

    // Strictly higher → arms.
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        fido2_confirm_body(&stage_id, &challenge, 6),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// FIDO2/WebAuthn §7.1 step 17: sig_counter 0 means the authenticator does not
/// maintain a global counter. Do not persist 0 or the next stateless assertion
/// would self-lock as `0 <= 0`.
#[tokio::test]
async fn test_attest_fido2_counter_zero_repeats_without_lockout() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;

    for round in 0..2 {
        let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
        let resp = confirm_with(
            &router,
            "bob:tok_bravo_0002",
            fido2_confirm_body(&stage_id, &challenge, 0),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "fido2 counter=0 ceremony {round} must not poison the counter table"
        );
        disarm_ok(&router).await;
    }

    let confirms = db
        .list_arm_audit()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.action == "confirm")
        .count();
    assert_eq!(confirms, 2, "both stateless fido2 ceremonies must confirm");
}

/// Counter-keying invariant (credential-type invariant): behavior keys
/// on credential_type ONLY. se-p256 arms repeatedly at sig_counter 0 — even
/// AFTER a fido2 ceremony has driven the counter table past 0 — because
/// se-p256 NEVER consults arm_attest_counters.
#[tokio::test]
async fn test_attest_counter_keying_se_p256_repeats_at_zero() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;

    // fido2 ceremony first, at a high counter.
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        fido2_confirm_body(&stage_id, &challenge, 100),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    disarm_ok(&router).await;

    // se-p256 at counter 0, twice in a row — both must arm.
    for round in 0..2 {
        let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
        let resp = confirm_with(
            &router,
            "bob:tok_bravo_0002",
            se_confirm_body(&stage_id, &challenge),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "se-p256 round {round} must arm at sig_counter 0 (keying is on credential_type, not counter value)"
        );
        disarm_ok(&router).await;
    }

    let confirms = db
        .list_arm_audit()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.action == "confirm")
        .count();
    assert_eq!(confirms, 3, "all three ceremonies must have confirmed");
}

/// §6 rejection: challenge_expired — the durable wall-clock exp_at_ms is the
/// truth; the expired pending row is deleted in the SAME tx as the decision.
#[tokio::test]
async fn test_attest_challenge_expired_rejects_and_clears() {
    let (_tmp, db, _q, router) = fixture_with_ttl(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
        Duration::from_millis(1),
    )
    .await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::GONE);
    assert!(last_reject_detail(&db).await.contains("challenge_expired"));
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "the expired pending row must be deleted in the confirm tx"
    );
}

/// §6 rejection: no_pending_stage — attest confirm with nothing staged.
#[tokio::test]
async fn test_attest_no_pending_stage_rejects() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let fake_stage_id = "00".repeat(16);
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&fake_stage_id, b"irrelevant"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::GONE);
    assert!(last_reject_detail(&db).await.contains("no_pending_stage"));
}

/// Malformed stage_id is 400-rejected BEFORE it can reach the audit-history
/// LIKE queries (wildcard injection guard) — and the open stage survives.
#[tokio::test]
async fn test_attest_malformed_stage_id_guard() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    for bad in ["%", "abc", &format!("{}%", &stage_id[..31])] {
        let resp = confirm_with(
            &router,
            "bob:tok_bravo_0002",
            se_confirm_body(bad, &challenge),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "non-32-hex stage_id {bad:?} must 400 before any LIKE query"
        );
    }
    assert!(db.get_arm_pending(0).await.unwrap().is_some());

    // Wrong-but-well-formed stage_id → 409 mismatch (fenced to THE stage).
    let other = "ab".repeat(16);
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&other, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

/// Idempotency (spec §4.3, condition 8): re-confirming an already-confirmed
/// stage_id returns 200 {"status":"armed","idempotent":true} and writes NO
/// second confirm row.
#[tokio::test]
async fn test_attest_reconfirm_is_idempotent() {
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    let body = se_confirm_body(&stage_id, &challenge);

    let resp = confirm_with(&router, "bob:tok_bravo_0002", body.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["status"], "armed");
    assert_eq!(v["idempotent"], true);

    let confirms = db
        .list_arm_audit()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.action == "confirm")
        .count();
    assert_eq!(
        confirms, 1,
        "idempotent re-confirm must not write a second confirm row"
    );
}

/// TOCTOU closure (atomic confirmation invariant): two concurrent confirms of the same
/// stage race through the one-tx path — exactly ONE §6 confirm row lands,
/// both callers get 200 (the loser sees the idempotent shape), and the
/// producer is armed once.
#[tokio::test]
async fn test_attest_concurrent_confirm_single_arm() {
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    let body = se_confirm_body(&stage_id, &challenge);

    let (r1, r2) = tokio::join!(
        confirm_with(&router, "bob:tok_bravo_0002", body.clone()),
        confirm_with(&router, "alice:tok_alpha_0001", body.clone()),
    );
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(r2.status(), StatusCode::OK);

    assert!(quarantine.producer_kill_state.lock().is_some());
    let confirms = db
        .list_arm_audit()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.action == "confirm")
        .count();
    assert_eq!(
        confirms, 1,
        "concurrent confirms must serialize to exactly one §6 confirm row (TOCTOU closed)"
    );
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "the pending row must be consumed exactly once"
    );
}

/// fido2 UP flag (spec §5): an assertion without user-presence is rejected —
/// hardware-attested presence is the entire point of the mechanism.
#[tokio::test]
async fn test_attest_fido2_missing_up_flag_rejects() {
    use p256::ecdsa::signature::Signer;
    use sha2::{Digest, Sha256};
    let (_tmp, db, _q, router) = fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;

    // Correctly signed assertion — but UP flag clear.
    let ad = fido2_authenticator_data(1, false);
    let mut msg = ad.clone();
    msg.extend_from_slice(&Sha256::digest(&challenge));
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&msg);
    let body = serde_json::json!({
        "stage_id": stage_id,
        "credential_id": "fido2-cred-0001",
        "credential_type": "fido2-es256",
        "signature": b64(sig.to_der().as_bytes()),
        "authenticator_data": b64(&ad),
    });
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("bad_signature"));

    // fido2 WITHOUT authenticator_data at all — also rejected.
    let mut body = fido2_confirm_body(&stage_id, &challenge, 1);
    body.as_object_mut().unwrap().remove("authenticator_data");
    let resp = confirm_with(&router, "bob:tok_bravo_0002", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(last_reject_detail(&db).await.contains("bad_signature"));
}

/// B7 (spec §8) — rehearsal ceremony: same paths, same crypto, *_rehearsal
/// audit actions, and the producer NEVER starts. The rehearsal flag lives on
/// the pending ROW (set at stage time) — the confirm request cannot upgrade
/// a rehearsal into a real arm. Re-confirm is idempotent.
#[tokio::test]
async fn test_rehearsal_ceremony_never_arms() {
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;

    // Stage with {"rehearse": true}.
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            Some(serde_json::json!({ "rehearse": true })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["rehearsal"], true, "stage response must flag rehearsal");
    let stage_id = v["stage_id"].as_str().unwrap().to_string();
    let challenge = b64d(v["challenge"].as_str().unwrap());

    // GET /arm/pending exposes the flag (bin/arm resume must know).
    let resp = router
        .clone()
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("alice:tok_alpha_0001"),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["rehearsal"], true);

    // Confirm with a VALID attest body — verifies, but never arms.
    let resp = confirm_with(
        &router,
        "alice:tok_alpha_0001",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["status"], "rehearsal-ok");
    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "a rehearsal confirm must NEVER start the producer"
    );
    assert!(
        db.get_arm_pending(0).await.unwrap().is_none(),
        "rehearsal confirm must consume the pending row"
    );

    // Audit chain: stage_rehearsal + confirm_rehearsal, never stage/confirm.
    let rows = db.list_arm_audit().await.unwrap();
    let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
    assert!(actions.contains(&"stage_rehearsal"));
    assert!(actions.contains(&"confirm_rehearsal"));
    assert!(!actions.contains(&"stage"));
    assert!(!actions.contains(&"confirm"));
    let detail: serde_json::Value = serde_json::from_str(
        rows.iter()
            .find(|r| r.action == "confirm_rehearsal")
            .unwrap()
            .detail
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(detail["rehearsal"], true);
    assert_eq!(detail["mechanism"], "local-attest");

    // Idempotent re-confirm of the rehearsal stage.
    let resp = confirm_with(
        &router,
        "alice:tok_alpha_0001",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["idempotent"], true);
    assert!(quarantine.producer_kill_state.lock().is_none());
}

/// B7 — a rehearsal stage_id is burned like a real one (condition 8): it can
/// never be re-staged, and a real ceremony after a rehearsal works cleanly.
#[tokio::test]
async fn test_rehearsal_then_real_ceremony() {
    let (_tmp, db, quarantine, router) =
        fixture_attest("alice:tok_alpha_0001,bob:tok_bravo_0002").await;

    // Rehearsal round.
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            Some(serde_json::json!({ "rehearse": true })),
        ))
        .await
        .unwrap();
    let v = body_json(resp).await;
    let rehearsal_stage_id = v["stage_id"].as_str().unwrap().to_string();
    let challenge = b64d(v["challenge"].as_str().unwrap());
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&rehearsal_stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(quarantine.producer_kill_state.lock().is_none());

    // Real round on the same router — arms normally.
    let (stage_id, challenge) = stage_as(&router, "alice:tok_alpha_0001").await;
    assert_ne!(stage_id, rehearsal_stage_id);
    let resp = confirm_with(
        &router,
        "bob:tok_bravo_0002",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        quarantine.producer_kill_state.lock().is_some(),
        "the real ceremony after a rehearsal must arm"
    );

    // The real confirm row is action 'confirm'; the rehearsal one stays
    // 'confirm_rehearsal'.
    let rows = db.list_arm_audit().await.unwrap();
    assert!(rows.iter().any(|r| r.action == "confirm"));
    assert!(rows.iter().any(|r| r.action == "confirm_rehearsal"));
}

// Content-binding coverage.

/// B5/Q2 strict-equality content binding: stage persists one content tuple,
/// confirm re-derives a DIFFERENT one (cap drifted between stage and tap), and
/// the confirm is BLOCKED with `arm_content_drift` BEFORE the signature is even
/// checked. Driven at the db layer so the cap drift is deterministic (the HTTP
/// path reads the boot-resolved `daily_spend_cap()` OnceLock, which cannot move
/// mid-test). The `verify` closure would succeed if reached — proving the
/// content gate trips first.
#[tokio::test]
async fn test_attest_stage_confirm_content_drift_blocked() {
    use gateway_sidecar::watch::attest::ArmContent;
    use gateway_sidecar::watch::db::{ArmConfirmTxOutcome, AttestVerification};

    let tmp = tempfile::tempdir().unwrap();
    let db = WatchDb::open(&tmp.path().join("drift.db")).await.unwrap();
    db.run_migrations().await.unwrap();

    let stage_id = "1234567890abcdef1234567890abcdef".to_string();
    // Staged at $50.00 = 5000 cents on a clean build id.
    let staged = ArmContent {
        build_id: "clean-sha-aaaa".to_string(),
        enabled_surface: "watch-producer".to_string(),
        effective_daily_cap_cents: 5000,
        tenant: "canary".to_string(),
        effective_spend_window_ms: 86_400_000,
    };
    // Stage REAL JCS-canonical challenge bytes whose embedded content matches
    // the persisted columns (the production flow derives both from one
    // `derive_arm_content`). The post-verify signature-anchored content check
    // (grok `ab533eae` CRITICAL) parses these bytes, so an opaque literal would
    // (correctly) trip `challenge_unparseable`.
    let challenge_bytes = gateway_sidecar::watch::attest::build_challenge_bytes(
        &stage_id,
        "alice",
        0,
        i64::MAX,
        &staged,
    )
    .unwrap();
    db.stage_arm_pending(
        "alice",
        &format!("stage_id={stage_id} ttl_ms=120000"),
        &stage_id,
        challenge_bytes,
        i64::MAX, // never expires within the test
        false,
        staged.clone(),
        2,
    )
    .await
    .unwrap();

    // Confirm re-derives a DRIFTED cap ($500.00 = 50000 cents) — ambient moved.
    let drifted = ArmContent {
        effective_daily_cap_cents: 50000,
        ..staged.clone()
    };
    let outcome = db
        .confirm_arm_attest(
            &stage_id,
            "alice",
            "",
            0,
            0, // armed_epoch (Attested-arm)
            true,
            drifted,
            dummy_signed_material(),
            |_challenge: &[u8]| {
                // Would succeed — but the content gate must trip before this.
                Ok(AttestVerification {
                    credential_id: "se-cred-0001".to_string(),
                    credential_type: "se-p256".to_string(),
                    sig_counter: 0,
                })
            },
        )
        .await
        .unwrap();
    match outcome {
        ArmConfirmTxOutcome::Rejected { reason } => {
            assert_eq!(reason, "arm_content_drift", "cap drift must be the reason");
        }
        other => panic!("expected Rejected{{arm_content_drift}}, got {other:?}"),
    }

    // The pending row survives a content-drift rejection (only confirm/expiry/
    // disarm consume it) — a corrected re-arm can still proceed.
    assert!(
        db.get_arm_pending(0).await.unwrap().is_some(),
        "drift rejection must not consume the pending stage"
    );

    // Sanity: confirm with the EXACT staged content reaches (and passes) verify.
    let ok = db
        .confirm_arm_attest(
            &stage_id,
            "alice",
            "",
            0,
            0, // armed_epoch (Attested-arm)
            true,
            staged,
            dummy_signed_material(),
            |_challenge: &[u8]| {
                Ok(AttestVerification {
                    credential_id: "se-cred-0001".to_string(),
                    credential_type: "se-p256".to_string(),
                    sig_counter: 0,
                })
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(ok, ArmConfirmTxOutcome::Verified { .. }),
        "matching content must reach and pass verify"
    );
}

/// CRITICAL (hardening): the column-drift gate compares the
/// PERSISTED COLUMNS against re-derived ambient, and `verify()` only checks the
/// signature over the stored bytes — two separate representations. An attacker
/// with `watch.db` write (the laptop-owning agent MF-1 exists to stop) can leave
/// the SIGNED bytes intact (cannot forge the signature) and mutate ONLY the
/// columns: the column gate then passes (columns == ambient) AND verify passes
/// (sig over untouched bytes), yet the tap authorized DIFFERENT content. The
/// signature-anchored content check must catch this. Here we simulate the
/// desync directly — `stage_arm_pending` takes the signed bytes and the column
/// tuple as separate args, so we stage bytes for content A under columns for
/// content B (cap $50 signed, cap $500 in the columns).
#[tokio::test]
async fn test_attest_signed_bytes_vs_columns_desync_blocked() {
    use gateway_sidecar::watch::attest::ArmContent;
    use gateway_sidecar::watch::db::{ArmConfirmTxOutcome, AttestVerification};

    let tmp = tempfile::tempdir().unwrap();
    let db = WatchDb::open(&tmp.path().join("desync.db")).await.unwrap();
    db.run_migrations().await.unwrap();

    let stage_id = "abcdef1234567890abcdef1234567890".to_string();
    // What the human actually tapped / what the signature covers: $50.00.
    let signed_content = ArmContent {
        build_id: "clean-sha-aaaa".to_string(),
        enabled_surface: "watch-producer".to_string(),
        effective_daily_cap_cents: 5000,
        tenant: "canary".to_string(),
        effective_spend_window_ms: 86_400_000,
    };
    // What an attacker wrote into the persisted COLUMNS after the tap: $500.00.
    let tampered_columns = ArmContent {
        effective_daily_cap_cents: 50000,
        ..signed_content.clone()
    };
    let signed_bytes = gateway_sidecar::watch::attest::build_challenge_bytes(
        &stage_id,
        "alice",
        0,
        i64::MAX,
        &signed_content,
    )
    .unwrap();
    db.stage_arm_pending(
        "alice",
        &format!("stage_id={stage_id} ttl_ms=120000"),
        &stage_id,
        signed_bytes,
        i64::MAX,
        false,
        tampered_columns.clone(),
        2,
    )
    .await
    .unwrap();

    // Ambient matches the TAMPERED columns ($500), so the column-drift gate
    // passes; the verify closure would succeed. Only the signature-anchored
    // bytes check (signed $50 != columns $500) can block this.
    let outcome = db
        .confirm_arm_attest(
            &stage_id,
            "alice",
            "",
            0,
            0, // armed_epoch (Attested-arm)
            true,
            tampered_columns,
            dummy_signed_material(),
            |_challenge: &[u8]| {
                Ok(AttestVerification {
                    credential_id: "se-cred-0001".to_string(),
                    credential_type: "se-p256".to_string(),
                    sig_counter: 0,
                })
            },
        )
        .await
        .unwrap();
    match outcome {
        ArmConfirmTxOutcome::Rejected { reason } => assert_eq!(
            reason, "signed_content_mismatch",
            "signed bytes ($50) must not be honored against tampered columns ($500)"
        ),
        other => panic!("expected Rejected{{signed_content_mismatch}}, got {other:?}"),
    }
    // Fail-closed: the producer must not be armed.
    assert!(
        db.get_arm_pending(0).await.unwrap().is_some(),
        "a signed/column mismatch rejection must not consume the pending stage"
    );
}

/// B6: a build that may not arm for real (`allow_real_arm: false`, derived in
/// prod from a `-dirty` embedded SHA) FORCES every stage to a rehearsal/DARK
/// ceremony — the request asked for a real arm, but the PERSISTED row is a
/// rehearsal and the producer never starts.
#[tokio::test]
async fn test_dirty_build_forces_dark_rehearsal() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(WatchDb::open(&tmp.path().join("dirty.db")).await.unwrap());
    db.run_migrations().await.unwrap();
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let registry = AttestKeyRegistry::parse(&attest_registry_json());
    let router: axum::Router = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse(
            "alice:tok_alpha_0001,bob:tok_bravo_0002",
        )),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(registry),
        allow_real_arm: false, // <-- B6: dirty/unidentifiable build
    });

    // Ask for a REAL arm (no rehearse flag in the body).
    let resp = router
        .clone()
        .oneshot(post(
            "/watch/admin/producer/arm/stage",
            Some("alice:tok_alpha_0001"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let staged = body_json(resp).await;
    assert_eq!(
        staged["rehearsal"], true,
        "B6: a dirty build must force the stage to a rehearsal"
    );

    // The PERSISTED pending row is a rehearsal (the ROW decides, not the wire).
    let pending = db.get_arm_pending(0).await.unwrap().expect("pending row");
    assert!(
        pending.rehearsal,
        "B6: persisted pending row must be rehearsal=true on a dirty build"
    );

    // And GET /arm/pending reports the persisted rehearsal flag.
    let resp = router
        .oneshot(get(
            "/watch/admin/producer/arm/pending",
            Some("bob:tok_bravo_0002"),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["rehearsal"], true);
}

/// B6 confirm-side (hardening HIGH#3): forcing rehearsal at
/// STAGE time is not enough — the persisted `rehearsal` flag is NOT bound into
/// the signed challenge, so a tampered/planted `rehearsal=false` row could reach
/// the real-arm path. The RUNTIME build identity must veto it at confirm: a
/// dirty build (`allow_real_arm:false`) never starts the real producer even on a
/// Verified non-rehearsal outcome. Stage a REAL (non-rehearsal) arm on a
/// clean-build router, then confirm it on a dirty-build router sharing the same
/// quarantine/db — the producer must NOT start.
#[tokio::test]
async fn test_dirty_build_blocks_real_producer_at_confirm() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(
        WatchDb::open(&tmp.path().join("b6_confirm.db"))
            .await
            .unwrap(),
    );
    db.run_migrations().await.unwrap();
    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        db.clone(),
    ));
    let principals = "alice:tok_alpha_0001,bob:tok_bravo_0002";

    // CLEAN-build router stages a REAL (non-rehearsal) arm.
    let router_clean = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse(principals)),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(AttestKeyRegistry::parse(&attest_registry_json())),
        allow_real_arm: true,
    });
    let (stage_id, challenge) = stage_as(&router_clean, "alice:tok_alpha_0001").await;
    assert!(
        !db.get_arm_pending(0)
            .await
            .unwrap()
            .expect("pending row")
            .rehearsal,
        "stage on a clean build must persist a non-rehearsal row"
    );

    // DIRTY-build router (SAME quarantine/db) confirms — B6 confirm-side veto.
    // Same in-process build_id, so the content-drift gate passes; only the
    // runtime allow_real_arm:false stops the real producer.
    let router_dirty = arm_admin_router(ArmAdminRouterState {
        quarantine: quarantine.clone(),
        principals: Arc::new(ArmPrincipals::parse(principals)),
        stage_ttl: Duration::from_millis(120_000),
        admin_token: "shared-admin-token".to_string(),
        notifier: Arc::new(ArmNotifier::for_tests(None)),
        deviation: Arc::new(ArmDeviationTags::default()),
        attest_keys: Arc::new(AttestKeyRegistry::parse(&attest_registry_json())),
        allow_real_arm: false,
    });
    let resp = confirm_with(
        &router_dirty,
        "bob:tok_bravo_0002",
        se_confirm_body(&stage_id, &challenge),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["status"], "rehearsal-ok",
        "B6: a dirty build folds the confirm to an effective rehearsal (producer never starts)"
    );
    assert!(
        quarantine.producer_kill_state.lock().is_none(),
        "B6 confirm-side: the real producer must NOT be armed on a dirty build"
    );
    // Audit-fidelity (grok `ab533eae` re-verify note): the UNPRUNABLE chain must
    // honestly record the DARK outcome — a `confirm_rehearsal` row carrying
    // `dark_reason` — never a misleading bare `confirm` for a ceremony that did
    // not start the producer.
    let rows = db.list_arm_audit().await.unwrap();
    let last = rows.last().expect("a confirm audit row");
    assert_eq!(
        last.action, "confirm_rehearsal",
        "dirty-build veto must record confirm_rehearsal, not confirm"
    );
    assert!(
        last.detail
            .as_deref()
            .unwrap()
            .contains("\"dark_reason\":\"build_not_real_arm_capable\""),
        "the unprunable chain must record WHY the ceremony went dark"
    );
    assert!(
        !rows.iter().any(|r| r.action == "confirm"),
        "no misleading bare 'confirm' row may exist for a dark ceremony"
    );
}

/// B2: the persistence layer rejects a downgrade — a raw INSERT of a
/// `challenge_format_version = 1` row trips the DB `CHECK(... >= 2)`. The floor
/// lives in the data layer, not just the binary, so a rolled-back writer cannot
/// stage a v1 row even if it tried.
#[tokio::test]
async fn test_arm_pending_downgrade_rejected_by_db_check() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("downgrade.db");
    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();

    // Second raw connection to the same file (SQLite allows it).
    let conn = rusqlite::Connection::open(&path).unwrap();

    // v2 row inserts fine.
    conn.execute(
        "INSERT INTO arm_pending
           (stage_id, staged_by, challenge_bytes, exp_at_ms, rehearsal,
            build_id, enabled_surface, effective_daily_cap_cents, tenant,
            challenge_format_version)
         VALUES ('aaaa1111aaaa1111aaaa1111aaaa1111', 'alice', X'00', 9999999999999, 0,
                 'sha', 'watch-producer', 5000, 'canary', 2)",
        [],
    )
    .expect("v2 row must insert");

    // v1 row is rejected by the CHECK constraint.
    let err = conn
        .execute(
            "INSERT INTO arm_pending
               (stage_id, staged_by, challenge_bytes, exp_at_ms, rehearsal,
                build_id, enabled_surface, effective_daily_cap_cents, tenant,
                challenge_format_version)
             VALUES ('bbbb2222bbbb2222bbbb2222bbbb2222', 'alice', X'00', 9999999999999, 0,
                     'sha', 'watch-producer', 5000, 'canary', 1)",
            [],
        )
        .expect_err("v1 row must be rejected by CHECK(challenge_format_version >= 2)");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "expected a CHECK-constraint failure, got: {msg}"
    );
}
