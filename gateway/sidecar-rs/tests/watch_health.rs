//! Phase 2 meta-sentinel tests — watch-health-watch (Grok G5).

use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::sentinels::watch_health::WatchHealthSentinel;
use gateway_sidecar::watch::Sentinel;

/// T_NEW1: meta-sentinel FIRES when another sentinel has a hard-kill marker.
/// Verifies the cross-sentinel signal works AND that self-exclusion does
/// not accidentally suppress fires for OTHER hard-killed sentinels.
#[tokio::test]
async fn t_new1_meta_sentinel_fires_on_quarantine_or_hard_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db_path = tmp.path().join("watch.db");

    // Bootstrap the watch.db with the v1 schema.
    let db = WatchDb::open(&watch_db_path).await.unwrap();
    db.run_migrations().await.unwrap();

    // Plant a hard-kill row for a different sentinel.
    db.upsert_hard_kill(
        "sovereign",
        "broken-sentinel",
        1_700_000_000_000,
        "panic in observe",
    )
    .await
    .unwrap();

    let sentinel = WatchHealthSentinel::new("watch-health-watch", "sovereign", &watch_db_path);

    let state = sentinel.observe().await.unwrap();
    assert_eq!(
        state.payload["hard_killed_count"].as_i64().unwrap(),
        1,
        "expected to count the planted hard-killed sentinel"
    );
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_some(),
        "expected fire on hard-killed peer sentinel, got: {:?}",
        reason
    );
    assert!(
        reason.unwrap().contains("hard-killed"),
        "reason should mention hard-killed signal"
    );
}

/// T_NEW1b: meta-sentinel STAYS SILENT on a clean (just-migrated) watch.db
/// with no quarantines, no hard-kills, and an intact (empty) chain.
#[tokio::test]
async fn t_new1b_meta_sentinel_silent_on_healthy_plane() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db_path = tmp.path().join("watch.db");

    let db = WatchDb::open(&watch_db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    drop(db);

    let sentinel = WatchHealthSentinel::new("watch-health-watch", "sovereign", &watch_db_path);

    let state = sentinel.observe().await.unwrap();
    assert_eq!(state.payload["quarantined_count"].as_i64().unwrap(), 0);
    assert_eq!(state.payload["hard_killed_count"].as_i64().unwrap(), 0);
    assert!(state.payload["chain_ok"].as_bool().unwrap());
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_none(),
        "expected no fire on healthy watch plane, got: {:?}",
        reason
    );
}

/// T_NEW1c: meta-sentinel EXCLUDES SELF — a hard-kill row for
/// "watch-health-watch" itself must NOT cause it to fire (would be an
/// infinite-loop bug).
#[tokio::test]
async fn t_new1c_meta_sentinel_excludes_self() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db_path = tmp.path().join("watch.db");

    let db = WatchDb::open(&watch_db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    db.upsert_hard_kill(
        "sovereign",
        "watch-health-watch",
        1_700_000_000_000,
        "should be excluded",
    )
    .await
    .unwrap();
    drop(db);

    let sentinel = WatchHealthSentinel::new("watch-health-watch", "sovereign", &watch_db_path);

    let state = sentinel.observe().await.unwrap();
    assert_eq!(
        state.payload["hard_killed_count"].as_i64().unwrap(),
        0,
        "self-hard-kill must be filtered out"
    );
    let reason = sentinel.interesting(&state);
    assert!(
        reason.is_none(),
        "self-exclusion must prevent self-fire, got: {:?}",
        reason
    );
}
