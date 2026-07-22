#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;
use axum::http::StatusCode;
use gateway_sidecar::watch::{
    api::{
        admin_arm_confirm_json, admin_arm_stage_json, admin_disarm_producer_json, ArmPrincipals,
    },
    db::WatchDb,
    quarantine::{QuarantineConfig, QuarantineState},
};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_producer_graceful_drain_on_kill_switch() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    // 1. Setup Phase 1: Enqueue some fires
    for i in 0..10 {
        let digest = format!("test-digest-{}", i);
        let id = format!("causal-test-{}", i);
        let envelope = format!(
            r#"{{"sentinel": "test-sentinel", "tier": "test", "i": {}}}"#,
            i
        );
        db.insert_pending_escalation_with_causal_dedup(
            &id,
            "test-tenant",
            "test-sentinel",
            &envelope,
            &digest,
            100,
            0,
        )
        .await
        .unwrap();
    }

    let quarantine = Arc::new(QuarantineState::new_with_db(
        QuarantineConfig::default(),
        Arc::clone(&db),
    ));

    // Wait slightly to ensure DB is ready
    sleep(Duration::from_millis(10)).await;

    // 2. Arm producer dynamically — via the p0a four-eyes ceremony
    // (single-shot arm is 410 Gone): alice stages, bob confirms.
    let principals = Arc::new(ArmPrincipals::parse(
        "alice:tok_alpha_0001,bob:tok_bravo_0002",
    ));
    let stage_resp = admin_arm_stage_json(
        Arc::clone(&quarantine),
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
    let bytes = axum::body::to_bytes(stage_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let stage_id = v["stage_id"].as_str().unwrap().to_string();
    let challenge = arm_attest_common::b64d(v["challenge"].as_str().unwrap());

    let arm_resp = admin_arm_confirm_json(
        Arc::clone(&quarantine),
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

    // Give the producer a moment to start processing the batch
    sleep(Duration::from_millis(50)).await;

    // 3. Disarm/Kill mid-work — single principal (fast kill, no second
    // signature required).
    let disarm_resp = admin_disarm_producer_json(
        Arc::clone(&quarantine),
        "test_admin_token".to_string(),
        principals,
        Some("test_admin_token".to_string()),
        Arc::new(gateway_sidecar::watch::api::ArmNotifier::for_tests(None)),
    )
    .await;
    assert_eq!(disarm_resp.status(), StatusCode::OK);

    // 4. Verification:
    // The producer should have cleanly exited without panicking.
    // Ensure no zombie lock remains.
    let lock = quarantine.producer_kill_state.lock();
    assert!(lock.is_none());
}
