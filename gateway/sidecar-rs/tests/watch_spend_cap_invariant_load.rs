//! T2 spend-cap invariant load + shadow detection tests.
//! Concurrency under the atomic reserve + recon blind spot for reserved.
//! Uses harness patterns from watch_falsification_multiprocess, watch_falsification_occ, watch_threading.

use std::sync::Arc;

use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
use gateway_sidecar::watch::recon::{
    cap_breach_page_edge, run_recon_for_day, FileImportRecon, ReconOutcome,
};

#[tokio::test]
async fn cap_holds_under_concurrent_claim_and_settle() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_t2_cap_hold.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let base_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let today = gateway_sidecar::watch::db::utc_day_bucket(base_now);

    // Seed enough pending escalations for real contention (15 workers, 5.0 ceiling, 50 cap => ~10 can fit).
    for i in 0..30 {
        db.insert_pending_escalation_with_causal_dedup(
            &format!("t2load{}", i),
            "tenant_t2",
            "sentinel_t2",
            "{}",
            &format!("digt2{}", i),
            base_now,
            0,
        )
        .await
        .unwrap();
    }

    // Benign recon source: external matches a low settled so divergence path stays quiet.
    let import_path = tmp.path().join("t2_costs.json");
    std::fs::write(&import_path, format!(r#"{{"{today}": 0.0}}"#)).unwrap();
    let source = FileImportRecon::new(import_path);

    // 15 concurrent workers: claim (reserves ceiling) -> renew -> settle (realized < ceiling).
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(15);
    for _ in 0..15 {
        let db = db.clone();
        let now = base_now;
        handles.push(tokio::spawn(async move {
            if let Ok(Some(claim)) = db.claim_next_queued_or_failed().await {
                // Renew to exercise full happy path.
                let _ = db
                    .renew_deliberation_lease(
                        &claim.tenant,
                        &claim.id,
                        &claim.claim_token,
                        now,
                        10_000,
                    )
                    .await;
                // Settle with realized safely under per-directive ceiling.
                let council_json =
                    r#"{"body":"{}","headers":{"x-total-cost-usd":"2.0"}}"#.to_string();
                let _ = db
                    .store_council_response_and_stage(
                        &claim.tenant,
                        &claim.id,
                        &council_json,
                        &claim.claim_token,
                    )
                    .await;
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    // Direct read: reserved must be <= cap (atomic held during contention).
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let reserved: f64 = conn
        .query_row(
            "SELECT COALESCE(reserved_usd, 0.0) FROM spend_ledger WHERE day_bucket = ?1",
            rusqlite::params![today],
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    assert!(
        reserved <= 50.0,
        "reserved must not exceed DAILY_SPEND_CAP after concurrent claim/settle, got {}",
        reserved
    );

    // Recon must see no breach (happy path, all settled cleanly).
    let q = Arc::new(QuarantineState::new_in_memory(QuarantineConfig::default()));
    let outcome = run_recon_for_day(&db, &q, &source, 1.0, &today)
        .await
        .unwrap();
    assert!(
        !outcome.cap_breached,
        "cap_breached must be false on happy concurrent path, outcome={:?}",
        outcome
    );
    assert!(outcome.reserved_usd <= 50.0);
}

#[tokio::test]
async fn invariant_detects_orphaned_reservation() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch_t2_orphan.db");
    let db = Arc::new(WatchDb::open(&db_path).await.unwrap());
    db.run_migrations().await.unwrap();

    let base_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let today = gateway_sidecar::watch::db::utc_day_bucket(base_now);

    // Seed a clean bucket with some settled, no initial reserved.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO spend_ledger (day_bucket, reserved_usd, settled_usd) VALUES (?1, 0.0, 5.0)",
        rusqlite::params![today],
    )
    .unwrap();

    // Benign external total == settled so divergence alarm stays quiet; only cap_breach fires.
    let import_path = tmp.path().join("t2_orphan_costs.json");
    std::fs::write(&import_path, format!(r#"{{"{today}": 5.0}}"#)).unwrap();
    let source = FileImportRecon::new(import_path);

    // Synthetic orphan: direct UPDATE simulates stale-reclaim leaving reserved > cap.
    // (read-only invariant, no change to reserve/settle paths).
    conn.execute(
        "UPDATE spend_ledger SET reserved_usd = 60.0 WHERE day_bucket = ?1",
        rusqlite::params![today],
    )
    .unwrap();

    let q = Arc::new(QuarantineState::new_in_memory(QuarantineConfig::default()));
    let before = q.recon_cap_breach_total();

    let outcome = run_recon_for_day(&db, &q, &source, 1.0, &today)
        .await
        .unwrap();

    assert!(
        outcome.cap_breached,
        "cap_breached must be true for orphaned reservation, outcome={:?}",
        outcome
    );
    assert!(
        (outcome.reserved_usd - 60.0).abs() < 1e-9,
        "reserved in outcome must reflect the orphan"
    );
    assert!((outcome.cap_usd - gateway_sidecar::watch::db::daily_spend_cap()).abs() < 1e-9);
    // Divergence stayed quiet (external == settled).
    assert!(
        !outcome.alarmed,
        "divergence alarm must not fire for this benign source"
    );
    assert_eq!(
        q.recon_cap_breach_total(),
        before + 1,
        "cap breach counter must increment"
    );
}

/// Review blocker: the cap-breach PAGE must edge-trigger per
/// day_bucket so a sticky breach does not page every tick (self-DoS on on-call).
#[test]
fn cap_breach_page_edge_dedups_per_bucket() {
    let mk = |bucket: &str, breached: bool| ReconOutcome {
        day_bucket: bucket.to_string(),
        local_usd: 0.0,
        external_usd: 0.0,
        divergence_usd: 0.0,
        alarmed: false,
        reserved_usd: if breached { 60.0 } else { 0.0 },
        cap_usd: 50.0,
        cap_breached: breached,
        billed_minus_reserved_usd: 0.0,
    };
    let mut breached = std::collections::HashSet::new();

    // First breach on a bucket pages; subsequent ticks while still breached do not.
    assert!(
        cap_breach_page_edge(&mut breached, &mk("2026-06-22", true)),
        "first breach pages"
    );
    assert!(
        !cap_breach_page_edge(&mut breached, &mk("2026-06-22", true)),
        "sticky breach must not re-page"
    );
    assert!(
        !cap_breach_page_edge(&mut breached, &mk("2026-06-22", true)),
        "still sticky, still silent"
    );

    // A different bucket in breach pages independently.
    assert!(
        cap_breach_page_edge(&mut breached, &mk("2026-06-21", true)),
        "distinct bucket pages once"
    );

    // Clearing a bucket re-arms it: a later re-breach pages again (edge, not latch).
    assert!(
        !cap_breach_page_edge(&mut breached, &mk("2026-06-22", false)),
        "clear does not page"
    );
    assert!(
        cap_breach_page_edge(&mut breached, &mk("2026-06-22", true)),
        "re-breach after clear pages again"
    );
}
