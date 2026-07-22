//! Reserve re-anchors spend to the ES256 signature.
//!
//! The marquee property: the reserve does NOT trust the attacker-writable
//! `active_arm` columns — it RE-VERIFIES the persisted hardware signature at
//! spend time, asserts the signed content == columns == running binary, and
//! enforces the SIGNED expiry. A watch.db-write attacker who forges a higher cap
//! (or any column) but cannot produce a valid signature is refused.
//!
//! Ambient `daily_spend_cap()` is the boot default ($50) here (never mutated →
//! parallel-safe within this crate). Per-claim estimate is the $5 default.

#[path = "arm_attest_common/mod.rs"]
mod arm_attest_common;

use arm_attest_common::{
    attest_registry_json, attest_signing_key, b64d, now_ms, publish_test_boot_registry,
    sign_and_write_active_arm, sign_and_write_active_arm_windowed,
};
use gateway_sidecar::watch::attest::{
    build_challenge_bytes, build_id, ArmContent, AttestKeyRegistry, CANARY_TENANT,
    ENABLED_SURFACE_WATCH_PRODUCER,
};
use gateway_sidecar::watch::db::{ArmConfirmTxOutcome, AttestVerification, WatchDb};
use std::sync::Arc;

const TENANT: &str = "canary";
const SENTINEL: &str = "s1";
const FAR_FUTURE_MS: i64 = i64::MAX;

/// Default boot-locked spend window (24h) — no test sets GW_ARM_WINDOW_MS, so
/// the reserve deadline is `signed.iat_ms + WINDOW_MS` and the persisted column
/// must equal that. Helpers stamp it; manual upsert sites compute it inline.
const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

async fn fresh_db() -> (tempfile::TempDir, WatchDb) {
    let tmp = tempfile::tempdir().unwrap();
    let db = WatchDb::open(&tmp.path().join("reserve.db")).await.unwrap();
    db.run_migrations().await.unwrap();
    (tmp, db)
}

async fn enqueue(db: &WatchDb, n: usize, replay_epoch: i64) {
    let id = format!("esc-{n}");
    let causal = format!("causal-{n}");
    let env = format!(r#"{{"id":"{id}"}}"#);
    let inserted = db
        .insert_pending_escalation_with_causal_dedup(
            &id,
            TENANT,
            SENTINEL,
            &env,
            &causal,
            1_000,
            replay_epoch,
        )
        .await
        .unwrap();
    assert!(inserted, "row {n} must enqueue");
}

/// The boot-registry path (production-shaped): claim with no overrides.
async fn claim(db: &WatchDb) -> Option<gateway_sidecar::watch::db::PendingClaim> {
    db.claim_next_queued_or_failed().await.unwrap()
}

// ---------------------------------------------------------------------------
// MARQUEE — forged active_arm with no valid signature → reserve REFUSES
// ---------------------------------------------------------------------------

/// The CRITICAL closure: an attacker forges a sky-high cap column AND mutates
/// the signature bytes. With no valid signature over a matching challenge, the
/// reserve refuses. Proves the column is no longer the anchor.
#[tokio::test]
async fn forged_active_arm_garbage_signature_refused() {
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: 5000,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: WINDOW_MS,
    };
    let iat = now_ms();
    let challenge = build_challenge_bytes(
        "aa00aa00aa00aa00aa00aa00aa00aa00",
        "alice",
        iat,
        iat + 120_000,
        &content,
    )
    .unwrap();
    db.upsert_active_arm_for_test(
        &build_id(),
        ENABLED_SURFACE_WATCH_PRODUCER,
        9_007_199_254_740_991, // forged sky-high cap COLUMN
        CANARY_TENANT,
        0,
        iat + WINDOW_MS,
        challenge,
        vec![0xDE, 0xAD, 0xBE, 0xEF], // garbage signature_der
        "se-cred-0001",
        "se-p256",
    )
    .await
    .unwrap();
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "a forged arm with no valid signature must be refused by the reserve"
    );
}

/// Forged COLUMN desync: a VALID signature over a $5 challenge, but the attacker
/// raises only the cap COLUMN to $5000. The reserve re-derives the cap from the
/// SIGNED bytes ($5) AND the signed-content assertion (column != signed) refuses.
#[tokio::test]
async fn forged_cap_column_vs_signed_bytes_refused() {
    let (_tmp, db) = fresh_db().await;
    sign_and_write_active_arm(&db, 500, 0, now_ms(), Some(500_000), None, None).await;
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "a cap column that disagrees with the signed bytes must be refused"
    );
}

/// Wrong credential_id (not in the registry) → unknown_credential → refuse.
#[tokio::test]
async fn unknown_credential_refused() {
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: 5000,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: WINDOW_MS,
    };
    let iat = now_ms();
    let challenge = build_challenge_bytes(
        "aa00aa00aa00aa00aa00aa00aa00aa00",
        "alice",
        iat,
        iat + 120_000,
        &content,
    )
    .unwrap();
    use p256::ecdsa::signature::Signer;
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&challenge);
    db.upsert_active_arm_for_test(
        &build_id(),
        ENABLED_SURFACE_WATCH_PRODUCER,
        5000,
        CANARY_TENANT,
        0,
        iat + WINDOW_MS,
        challenge,
        sig.to_der().as_bytes().to_vec(),
        "not-enrolled-cred", // not in the registry
        "se-p256",
    )
    .await
    .unwrap();
    enqueue(&db, 1, 0).await;
    assert!(claim(&db).await.is_none(), "unknown credential must refuse");
}

/// Registry UNLOADED → fail-closed (no signature can be re-verified).
#[tokio::test]
async fn registry_unloaded_refused() {
    let (_tmp, db) = fresh_db().await;
    sign_and_write_active_arm(&db, 5000, 0, now_ms(), None, None, None).await;
    enqueue(&db, 1, 0).await;
    let unloaded = Arc::new(AttestKeyRegistry::unloaded());
    assert!(
        db.claim_next_queued_or_failed_with_lease_epoch_registry(150_000, None, Some(unloaded))
            .await
            .unwrap()
            .is_none(),
        "an unloaded registry must fail-closed"
    );
}

// ---------------------------------------------------------------------------
// valid signed arm allows spend up to the SIGNED cap; ambient narrows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn valid_signed_arm_allows_up_to_signed_cap_then_blocks() {
    let (_tmp, db) = fresh_db().await;
    sign_and_write_active_arm(&db, 1000, 0, now_ms(), None, None, None).await;
    enqueue(&db, 1, 0).await;
    enqueue(&db, 2, 0).await;
    enqueue(&db, 3, 0).await;
    assert!(claim(&db).await.is_some(), "$5 <= $10 allowed");
    assert!(claim(&db).await.is_some(), "$10 <= $10 allowed");
    assert!(
        claim(&db).await.is_none(),
        "$15 > the signed $10 ceiling → blocked even though ambient is $50"
    );
}

#[tokio::test]
async fn reserve_refuses_when_no_active_arm() {
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "a DARK / never-armed producer must not reserve real funds (unconditional)"
    );
}

/// HIGH (spend-window split-brain): the spend deadline is the SIGNED tap time
/// (`iat_ms`) + the boot-locked 24h window — NOT the short ceremony exp and NOT
/// the raw column. A signed iat older than one window means the deadline has
/// passed; the reserve refuses even though the column exp is stamped honestly.
#[tokio::test]
async fn reserve_refuses_after_spend_window() {
    let (_tmp, db) = fresh_db().await;
    // Signed tap > 24h ago → iat + WINDOW_MS is already in the past. The helper
    // stamps the column to that same (past) deadline, so the gate fires on the
    // computed deadline, not the column.
    let stale_iat = now_ms() - WINDOW_MS - 1;
    sign_and_write_active_arm(&db, 5000, 0, stale_iat, None, None, None).await;
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "now >= signed iat + boot-locked window must refuse (spend window, not ceremony exp)"
    );
}

/// HIGH (split-brain, real flow): stage → confirm (valid sig, iat=now) → reserve
/// WITHIN the 24h window is ALLOWED; the same arm with a stale signed iat is
/// REFUSED. Drives the real signed-iat/window path (no direct upsert), proving
/// the arm lives the full window, not the ~2-minute confirm TTL.
#[tokio::test]
async fn real_flow_allows_within_window_refuses_past_it() {
    // Allowed leg: fresh confirm, reserve inside the window.
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    let out = stage_and_confirm(&db, "ff55ff55ff55ff55ff55ff55ff55ff55", 1000, true, false).await;
    assert!(matches!(
        out,
        ArmConfirmTxOutcome::Verified {
            rehearsal: false,
            ..
        }
    ));
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_some(),
        "a fresh real confirm must allow the reserve inside the 24h window"
    );

    // Refused leg: a stale signed iat (> window ago) via the signed-path helper.
    let (_tmp2, db2) = fresh_db().await;
    sign_and_write_active_arm(&db2, 1000, 0, now_ms() - WINDOW_MS - 1, None, None, None).await;
    enqueue(&db2, 1, 0).await;
    assert!(
        claim(&db2).await.is_none(),
        "the same arm past its 24h spend window must refuse"
    );
}

/// HIGH (env extend): GW_ARM_WINDOW_MS set HUGE *after* boot must NOT extend the
/// window — the reserve uses the boot-locked value. The window was already boot-
/// resolved (default 24h) before this test, so a stale-iat arm stays refused even
/// with a giant live env value present.
#[tokio::test]
async fn live_env_window_extend_is_ignored() {
    let (_tmp, db) = fresh_db().await;
    // A post-boot attacker sets a 100-year window via env.
    std::env::set_var("GW_ARM_WINDOW_MS", "3153600000000");
    // Arm signed > the boot-locked 24h ago: live env would say "still valid",
    // but the boot-locked window says expired.
    sign_and_write_active_arm(&db, 5000, 0, now_ms() - WINDOW_MS - 1, None, None, None).await;
    enqueue(&db, 1, 0).await;
    let refused = claim(&db).await.is_none();
    std::env::remove_var("GW_ARM_WINDOW_MS");
    assert!(
        refused,
        "a live GW_ARM_WINDOW_MS must not extend the boot-locked spend window"
    );
}

/// HIGH (column/computed desync): a column exp far in the FUTURE but a signed iat
/// whose computed deadline is in the PAST → the reserve gates on the COMPUTED
/// deadline (refuse), not the forged column. Proves the column is a tripwire,
/// never the gate.
#[tokio::test]
async fn column_exp_future_but_computed_past_refuses() {
    let (_tmp, db) = fresh_db().await;
    // Signed iat is > window ago (computed deadline past), but the attacker
    // stamps a far-future column exp to try to keep the arm alive.
    sign_and_write_active_arm(
        &db,
        5000,
        0,
        now_ms() - WINDOW_MS - 1,
        None,
        None,
        Some(FAR_FUTURE_MS), // forged far-future column exp
    )
    .await;
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "a forged far-future column exp cannot revive an arm past its computed deadline"
    );
}

/// Attested-arm item 2 (invariant, the named negative smoke) — the
/// spend window is now SIGNED (`spend_window_ms`, bound into the ES256
/// challenge), so it can no longer be extended without a fresh hardware tap.
///
/// Construct the attack: a genuine tap signed a SHORT window (1s) some seconds
/// ago, so the SIGNED deadline (`iat + 1s`) is already in the past. The attacker
/// then (a) stamps a far-future column exp AND (b) — irrelevant here, the env
/// knob is boot-locked — would set GW_ARM_WINDOW_MS huge. With the signed window
/// honored, the reserve refuses. Were the reserve to read the boot-locked 24h
/// window (the legacy behavior) or the column, this arm would still be "live"
/// and a $5 claim would go through — so a SUCCESSFUL claim here is the precise
/// regression signature for the window-extension hole.
#[tokio::test]
async fn test_reserve_rejects_window_extension() {
    let (_tmp, db) = fresh_db().await;
    // Signed window = 1s; tap = 10s ago → signed deadline (iat + 1s) is 9s past.
    let short_window_ms = 1_000;
    let tap_iat = now_ms() - 10_000;
    sign_and_write_active_arm_windowed(
        &db,
        5000,                  // signed cap $50 (ambient $50 → not the limiting factor)
        0,                     // armed_epoch
        tap_iat,               // SIGNED tap time, 10s ago
        None,                  // column cap == signed
        None,                  // column build == running
        Some(FAR_FUTURE_MS),   // attacker stamps a far-future column exp to "extend"
        Some(short_window_ms), // but the SIGNED window is only 1s
    )
    .await;
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "a 1s signed window 10s in the past must refuse — the signed window, not the \
         column or the boot-locked env knob, is the spend horizon"
    );
}

/// HIGH (grok-4.3): a pathological signed `iat_ms` near i64::MAX makes
/// `iat_ms + window` overflow. Both the reserve gate and the confirm exp
/// computation use checked_add → fail-closed. Built manually (the shared helper
/// adds to iat internally, which would itself overflow).
#[tokio::test]
async fn spend_deadline_overflow_refuses() {
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: 5000,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: WINDOW_MS,
    };
    // iat at i64::MAX so iat + 24h window overflows i64.
    let challenge = build_challenge_bytes(
        "aa00aa00aa00aa00aa00aa00aa00aa00",
        "alice",
        i64::MAX,
        i64::MAX,
        &content,
    )
    .unwrap();
    use p256::ecdsa::signature::Signer;
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&challenge);
    db.upsert_active_arm_for_test(
        &build_id(),
        ENABLED_SURFACE_WATCH_PRODUCER,
        5000,
        CANARY_TENANT,
        0,
        i64::MAX, // column exp — irrelevant; the gate overflows before the tripwire
        challenge,
        sig.to_der().as_bytes().to_vec(),
        "se-cred-0001",
        "se-p256",
    )
    .await
    .unwrap();
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "a signed iat that overflows the spend deadline must fail closed (reserve)"
    );
}

/// HIGH (grok-4.3, confirm leg): a confirm whose signed `iat_ms` overflows the
/// spend-deadline add is REJECTED (active_arm is not written with a garbage exp).
#[tokio::test]
async fn confirm_rejects_spend_deadline_overflow() {
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    let stage_id = "f00ff00ff00ff00ff00ff00ff00ff00f";
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: 1000,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: WINDOW_MS,
    };
    // Ceremony exp must be > now_ms (the tap-by TTL passes) but iat overflows the
    // 24h spend-window add. iat = i64::MAX - window keeps the challenge well-formed
    // while iat + window wraps.
    let iat = i64::MAX - 1000;
    let challenge = build_challenge_bytes(stage_id, "alice", iat, i64::MAX, &content).unwrap();
    db.stage_arm_pending(
        "alice",
        &format!("stage_id={stage_id} ttl_ms=120000"),
        stage_id,
        challenge.clone(),
        i64::MAX, // ceremony exp_at_ms (tap-by) far future
        false,
        content.clone(),
        2,
    )
    .await
    .unwrap();
    use p256::ecdsa::signature::Signer;
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&challenge);
    let material = gateway_sidecar::watch::db::PersistedArmSignature {
        credential_id: "se-cred-0001".to_string(),
        credential_type: "se-p256".to_string(),
        signature_der: sig.to_der().as_bytes().to_vec(),
        authenticator_data: None,
        client_data_json: None,
    };
    let out = db
        .confirm_arm_attest(
            stage_id,
            "alice",
            "",
            0, // now_ms (< ceremony exp)
            0, // armed_epoch
            true,
            content,
            material,
            move |_c: &[u8]| {
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
        matches!(out, ArmConfirmTxOutcome::Rejected { ref reason } if reason == "spend_deadline_overflow"),
        "confirm with an overflowing signed iat must be rejected, got {out:?}"
    );
    assert!(
        db.get_active_arm().await.unwrap().is_none(),
        "no active_arm written on a rejected overflow confirm"
    );
}

// ---------------------------------------------------------------------------
// confirm writes / does not write active_arm (real vs rehearsal/DARK)
// ---------------------------------------------------------------------------

async fn stage_and_confirm(
    db: &WatchDb,
    stage_id: &str,
    cap_cents: i64,
    allow_real_arm: bool,
    rehearse: bool,
) -> ArmConfirmTxOutcome {
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: cap_cents,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: WINDOW_MS,
    };
    // Sign at iat=now so the confirm-derived spend deadline (signed.iat +
    // boot-locked window) lands ~24h in the future and the reserve allows.
    let iat = now_ms();
    let ceremony_exp = iat + 120_000;
    let challenge = build_challenge_bytes(stage_id, "alice", iat, ceremony_exp, &content).unwrap();
    db.stage_arm_pending(
        "alice",
        &format!("stage_id={stage_id} ttl_ms=120000"),
        stage_id,
        challenge.clone(),
        ceremony_exp,
        rehearse,
        content.clone(),
        2,
    )
    .await
    .unwrap();
    use p256::ecdsa::signature::Signer;
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(&challenge);
    let material = gateway_sidecar::watch::db::PersistedArmSignature {
        credential_id: "se-cred-0001".to_string(),
        credential_type: "se-p256".to_string(),
        signature_der: sig.to_der().as_bytes().to_vec(),
        authenticator_data: None,
        client_data_json: None,
    };
    db.confirm_arm_attest(
        stage_id,
        "alice",
        "",
        iat,            // now_ms (< ceremony_exp, so the tap-by TTL passes)
        0,              // armed_epoch
        allow_real_arm, // active_arm_exp_at_ms param removed (confirm derives it)
        content,
        material,
        move |_c: &[u8]| {
            Ok(AttestVerification {
                credential_id: "se-cred-0001".to_string(),
                credential_type: "se-p256".to_string(),
                sig_counter: 0,
            })
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn real_confirm_writes_verifiable_active_arm_and_reserve_allows() {
    let (_tmp, db) = fresh_db().await;
    publish_test_boot_registry();
    let out = stage_and_confirm(&db, "bb11bb11bb11bb11bb11bb11bb11bb11", 1000, true, false).await;
    assert!(matches!(
        out,
        ArmConfirmTxOutcome::Verified {
            rehearsal: false,
            ..
        }
    ));
    let arm = db
        .get_active_arm()
        .await
        .unwrap()
        .expect("real confirm writes active_arm");
    assert_eq!(arm.effective_daily_cap_cents, 1000);
    enqueue(&db, 1, 0).await;
    assert!(
        claim(&db).await.is_some(),
        "a real confirmed arm must allow the reserve (signature re-verifies)"
    );
}

#[tokio::test]
async fn rehearsal_confirm_writes_no_active_arm() {
    let (_tmp, db) = fresh_db().await;
    let out = stage_and_confirm(&db, "cc22cc22cc22cc22cc22cc22cc22cc22", 1000, true, true).await;
    assert!(matches!(
        out,
        ArmConfirmTxOutcome::Verified {
            rehearsal: true,
            ..
        }
    ));
    assert!(
        db.get_active_arm().await.unwrap().is_none(),
        "rehearsal writes no ceiling"
    );
}

#[tokio::test]
async fn dark_confirm_writes_no_active_arm() {
    let (_tmp, db) = fresh_db().await;
    let out = stage_and_confirm(&db, "dd33dd33dd33dd33dd33dd33dd33dd33", 1000, false, false).await;
    assert!(matches!(
        out,
        ArmConfirmTxOutcome::Verified {
            rehearsal: true,
            ..
        }
    ));
    assert!(
        db.get_active_arm().await.unwrap().is_none(),
        "DARK writes no ceiling"
    );
}

// ---------------------------------------------------------------------------
// lifecycle: disarm deletes; fenced clear keeps; monotonic upsert
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disarm_deletes_active_arm_and_reserve_refuses() {
    let (_tmp, db) = fresh_db().await;
    sign_and_write_active_arm(&db, 5000, 0, now_ms(), None, None, None).await;
    enqueue(&db, 1, 0).await;
    assert!(claim(&db).await.is_some());
    db.clear_arm_pending(None).await.unwrap();
    assert!(
        db.get_active_arm().await.unwrap().is_none(),
        "disarm deletes active_arm"
    );
    enqueue(&db, 2, 0).await;
    assert!(
        claim(&db).await.is_none(),
        "after disarm the reserve fails closed"
    );
}

#[tokio::test]
async fn fenced_clear_some_leaves_active_arm_intact() {
    let (_tmp, db) = fresh_db().await;
    sign_and_write_active_arm(&db, 5000, 0, now_ms(), None, None, None).await;
    db.clear_arm_pending(Some("no-such-stage")).await.unwrap();
    assert!(
        db.get_active_arm().await.unwrap().is_some(),
        "a fenced Some(stage_id) clear must leave active_arm intact"
    );
}

#[tokio::test]
async fn monotonic_upsert_rejects_equal_epoch() {
    let (_tmp, db) = fresh_db().await;
    sign_and_write_active_arm(&db, 5000, 10, now_ms(), None, None, None).await;
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: 1,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: WINDOW_MS,
    };
    let ch = build_challenge_bytes(
        "ee44ee44ee44ee44ee44ee44ee44ee44",
        "alice",
        0,
        FAR_FUTURE_MS,
        &content,
    )
    .unwrap();
    let sig = b64d(&arm_attest_common::sign_se_p256(&ch));
    let eq = db
        .upsert_active_arm_for_test(
            &build_id(),
            ENABLED_SURFACE_WATCH_PRODUCER,
            1,
            CANARY_TENANT,
            10, // equal epoch
            FAR_FUTURE_MS,
            ch,
            sig,
            "se-cred-0001",
            "se-p256",
        )
        .await
        .unwrap();
    assert_eq!(eq, 0, "equal epoch must not overwrite a live arm");
    let live = db.get_active_arm().await.unwrap().unwrap();
    assert_eq!(live.effective_daily_cap_cents, 5000);
    assert_eq!(live.armed_epoch, 10);
}

/// Sanity: the fixed test registry json is loadable + carries the credential.
#[tokio::test]
async fn registry_json_is_loadable() {
    let reg = AttestKeyRegistry::parse(&attest_registry_json());
    assert!(reg.is_loaded());
    assert!(reg.get("se-cred-0001").is_some());
}

#[ignore = "v2-multiprocess: reserve must assert instance_id==writer_claim.holder before spending under a confirmed ceiling. Needs a shared-writer-identity seam OR per-tenant ledger partition first (collides with test_falsification_multiprocess_spend_cap until then). See v2 ticket. DO NOT un-ignore by adding an ownership gate to reserve in v1 — it kills the multiprocess ledger-cap proof."]
#[tokio::test]
async fn test_stale_claim_cannot_spend_confirmed_ceiling() {
    // v2 acceptance criterion (intentionally unimplemented in v1). See residual in
    // The signed spend window is part of the attested ceiling contract.
}
