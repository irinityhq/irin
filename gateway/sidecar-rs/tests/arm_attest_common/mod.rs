//! dual-custody-local-attest B6 — shared ceremony helpers for integration
//! tests that arm the producer through the REAL confirm path (bearer token +
//! ES256 attest signature; the bearer-only and OTC confirm legs are retired,
//! spec §2/§9). Included per test crate via `#[path = "..."] mod`.

#![allow(dead_code)] // each including test crate uses a subset

use gateway_sidecar::watch::attest::AttestKeyRegistry;
use std::sync::Arc;

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn b64d(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

/// Deterministic test keypair (fixed scalar — NOT a real credential).
pub fn attest_signing_key() -> p256::ecdsa::SigningKey {
    p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).unwrap()
}

/// Registry JSON with one se-p256 credential bound to the test key.
pub fn attest_registry_json() -> String {
    let pk = b64(attest_signing_key()
        .verifying_key()
        .to_encoded_point(true)
        .as_bytes());
    serde_json::json!([
        {"credential_id": "se-cred-0001", "credential_type": "se-p256",
         "public_key": pk, "label": "test SE", "enrolled_at": "2026-06-12T00:00:00Z"}
    ])
    .to_string()
}

/// A LOADED registry for ceremony tests.
pub fn loaded_attest_keys() -> Arc<AttestKeyRegistry> {
    let r = AttestKeyRegistry::parse(&attest_registry_json());
    assert!(r.is_loaded(), "test registry must parse as loaded");
    Arc::new(r)
}

pub fn sign_se_p256(challenge: &[u8]) -> String {
    use p256::ecdsa::signature::Signer;
    let sig: p256::ecdsa::Signature = attest_signing_key().sign(challenge);
    b64(sig.to_der().as_bytes())
}

/// se-p256 confirm body for the stored challenge (spec §4.2 wire shape).
pub fn se_confirm_body(stage_id: &str, challenge: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "stage_id": stage_id,
        "credential_id": "se-cred-0001",
        "credential_type": "se-p256",
        "signature": sign_se_p256(challenge),
    })
}

/// Parse (stage_id, challenge bytes) out of a stage handler/route Response.
pub async fn stage_fields(resp: axum::response::Response) -> (String, Vec<u8>) {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let stage_id = v["stage_id"]
        .as_str()
        .expect("stage response must carry stage_id")
        .to_string();
    let challenge = b64d(
        v["challenge"]
            .as_str()
            .expect("stage response must carry challenge"),
    );
    (stage_id, challenge)
}

/// Attested-arm — transparently arm a freshly-migrated test DB so the reserve atomic
/// (which now reads `active_arm` as an absolute ceiling, fail-closed when
/// absent) behaves exactly like the legacy ambient-cap reserve for tests that
/// exercise the LEDGER/dispatch mechanics rather than the arming ceremony.
///
/// The stamped ceiling is INTENTIONALLY ambient-transparent: cap = $50 in cents
/// (== the boot DAILY_SPEND_CAP default, so `min(attested, ambient)` == ambient),
/// epoch 0, exp far in the future, build/surface/tenant matching the running
/// binary + v1 surface. CRITICAL re-anchor: the reserve now RE-VERIFIES the
/// ES256 signature, so this writes a REAL challenge signed by the fixed test key
/// and publishes the matching boot registry. Call right after `run_migrations()`
/// in any test that claims real spend.
pub async fn arm_db_for_reserve_test(db: &gateway_sidecar::watch::db::WatchDb) {
    arm_db_for_reserve_test_at_epoch(db, 0).await;
}

/// Attested-arm — like [`arm_db_for_reserve_test`] but at an explicit `armed_epoch`.
/// Builds a real, signature-verifiable active_arm at the running build's cap and
/// publishes the boot registry so the reserve's spend-time re-verify passes.
pub async fn arm_db_for_reserve_test_at_epoch(
    db: &gateway_sidecar::watch::db::WatchDb,
    armed_epoch: i64,
) {
    // signed iat = now → the spend deadline (iat + boot-locked 24h window) is
    // comfortably in the future for ledger/dispatch mechanics tests.
    let iat = now_ms();
    sign_and_write_active_arm(
        db,
        5000, // $50.00 cents == ambient default (transparent ceiling)
        armed_epoch,
        iat,
        None,
        None,
        None,
    )
    .await;
}

/// Wall-clock now in ms (test convenience).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Attested-arm — publish the boot registry holding the fixed test credential so the
/// reserve can re-verify arms signed by [`attest_signing_key`]. Idempotent
/// (OnceLock set-once); safe to call from every helper invocation.
pub fn publish_test_boot_registry() {
    let reg = std::sync::Arc::new(AttestKeyRegistry::parse(&attest_registry_json()));
    assert!(reg.is_loaded(), "test boot registry must load");
    gateway_sidecar::watch::attest::publish_boot_registry(reg);
}

/// Attested-arm — write a real signature-verifiable active_arm. Signs a v2 challenge
/// (running build/surface/tenant + `cap_cents`) at the SIGNED tap time
/// `signed_iat_ms` with the fixed test key, writes it via the gated test helper,
/// and publishes the boot registry.
///
/// HIGH (spend-window split-brain): the reserve gates on
/// `signed.iat_ms + arm_window_ms_bootlocked()` (NOT the short ceremony exp) and
/// tripwires the persisted column against that SAME value. So the column exp is
/// stamped to the computed deadline by default. Overrides let a test desync:
/// `column_cap_override` / `column_build_override` desync content columns (prove
/// the signed-content assertion), `column_exp_override` desyncs the column exp
/// (prove the column tripwire).
pub async fn sign_and_write_active_arm(
    db: &gateway_sidecar::watch::db::WatchDb,
    cap_cents: i64,
    armed_epoch: i64,
    signed_iat_ms: i64,
    column_cap_override: Option<i64>,
    column_build_override: Option<&str>,
    column_exp_override: Option<i64>,
) {
    sign_and_write_active_arm_windowed(
        db,
        cap_cents,
        armed_epoch,
        signed_iat_ms,
        column_cap_override,
        column_build_override,
        column_exp_override,
        None,
    )
    .await;
}

/// Attested-arm (v3) — like [`sign_and_write_active_arm`] but with an explicit SIGNED
/// `spend_window_ms` (defaults to the boot-locked window). Lets the
/// window-extension negative test sign a SHORT window while the persisted
/// column / ambient knob claim a long one — proving the reserve gates on the
/// SIGNED value, not the column or `GW_ARM_WINDOW_MS`.
#[allow(clippy::too_many_arguments)]
pub async fn sign_and_write_active_arm_windowed(
    db: &gateway_sidecar::watch::db::WatchDb,
    cap_cents: i64,
    armed_epoch: i64,
    signed_iat_ms: i64,
    column_cap_override: Option<i64>,
    column_build_override: Option<&str>,
    column_exp_override: Option<i64>,
    signed_window_override: Option<i64>,
) {
    use gateway_sidecar::watch::attest::{
        build_challenge_bytes, build_id, ArmContent, CANARY_TENANT, ENABLED_SURFACE_WATCH_PRODUCER,
    };
    use gateway_sidecar::watch::db::arm_window_ms_bootlocked;
    publish_test_boot_registry();
    let signed_window = signed_window_override.unwrap_or_else(arm_window_ms_bootlocked);
    let content = ArmContent {
        build_id: build_id(),
        enabled_surface: ENABLED_SURFACE_WATCH_PRODUCER.to_string(),
        effective_daily_cap_cents: cap_cents,
        tenant: CANARY_TENANT.to_string(),
        effective_spend_window_ms: signed_window,
    };
    // Ceremony exp (signed) — the short stage/confirm TTL; irrelevant to the
    // spend gate but must be a well-formed v3 challenge. iat is the SIGNED tap
    // time the reserve's spend-window math anchors to.
    let ceremony_exp = signed_iat_ms + 120_000;
    let challenge = build_challenge_bytes(
        "aa00aa00aa00aa00aa00aa00aa00aa00",
        "alice",
        signed_iat_ms,
        ceremony_exp,
        &content,
    )
    .unwrap();
    let sig_der = b64d(&sign_se_p256(&challenge));
    // The reserve recomputes the deadline from signed iat + SIGNED window and
    // asserts the column equals it; stamp the column to that by default.
    let column_exp = column_exp_override.unwrap_or(signed_iat_ms + signed_window);
    db.upsert_active_arm_for_test(
        column_build_override.unwrap_or(&build_id()),
        ENABLED_SURFACE_WATCH_PRODUCER,
        column_cap_override.unwrap_or(cap_cents),
        CANARY_TENANT,
        armed_epoch,
        column_exp,
        challenge,
        sig_der,
        "se-cred-0001",
        "se-p256",
    )
    .await
    .unwrap();
}
