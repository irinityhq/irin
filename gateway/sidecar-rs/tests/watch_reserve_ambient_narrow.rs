//! Attested-arm — ambient daily_spend_cap may only NARROW the attested ceiling.
//!
//! Its own test binary because it lowers the boot `daily_spend_cap` OnceLock,
//! which is process-global. Boot cap = $4 (400 cents); the SIGNED attested cap is
//! far higher ($50), so the effective ceiling is the LOWER ambient $4. With a $5
//! per-claim estimate, the first claim ($5 > $4) is refused — ambient narrowed
//! the attested ceiling. The arm is a REAL signed arm (the reserve re-verifies).

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;

use arm_attest_common::{now_ms, sign_and_write_active_arm};
use gateway_sidecar::watch::db::{self, WatchDb};

const TENANT: &str = "canary";
const SENTINEL: &str = "s1";

#[tokio::test]
async fn ambient_lower_than_attested_narrows_the_ceiling() {
    // Lower the boot cap to $4 (a whole, >= 1-cent value so cap-safety accepts).
    db::init_daily_spend_cap_at_boot(Some("4")).unwrap();
    assert_eq!(db::daily_spend_cap(), 4.0);

    let tmp = tempfile::tempdir().unwrap();
    let dbh = WatchDb::open(&tmp.path().join("narrow.db")).await.unwrap();
    dbh.run_migrations().await.unwrap();

    // Real signed arm at $50 (5000 cents), far above ambient $4.
    sign_and_write_active_arm(&dbh, 5000, 0, now_ms(), None, None, None).await;

    let env = r#"{"id":"esc-1"}"#;
    assert!(dbh
        .insert_pending_escalation_with_causal_dedup(
            "esc-1", TENANT, SENTINEL, env, "causal-1", 1_000, 0,
        )
        .await
        .unwrap());

    // min($50 signed, $4 ambient) = $4; estimate $5 > $4 → refused.
    assert!(
        dbh.claim_next_queued_or_failed().await.unwrap().is_none(),
        "ambient $4 must narrow the signed $50 ceiling and block a $5 reserve"
    );
}
