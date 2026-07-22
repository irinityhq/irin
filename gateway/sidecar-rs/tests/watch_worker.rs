use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use gateway_sidecar::keymgmt::DirectiveVerifier;
use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::outbox::AckOutcome;
use gateway_sidecar::watch::worker::{run_worker_tick, WatchWorkerConfig};
use rusqlite::{params, Connection};

async fn setup_db() -> (tempfile::TempDir, WatchDb, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("watch.db");
    let db = WatchDb::open(&db_path).await.unwrap();
    db.run_migrations().await.unwrap();
    (tmp, db, db_path)
}

/// Pre-seal W2: a deterministic test Council signing key. Every happy-path
/// directive is signed with this and the worker is given a verifier pinned to
/// it, so signature verification mirrors production exactly.
fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// The verifier the worker runs with: pinned to `test_signing_key`. Its kid is
/// derived identically to the signer (sidecar-v1-{first8 hex sha256(pubkey)}).
fn test_verifier() -> DirectiveVerifier {
    DirectiveVerifier::from_verifying_key(test_signing_key().verifying_key())
}

/// Insert a directive_outbox row whose `envelope_json_canonical` is REALLY
/// signed by the test key, with the pinned kid. `canonical` is the exact bytes
/// signed and stored. `authority` is the column value (production derives it
/// from the verified envelope; tests set it explicitly to mirror that).
fn insert_signed_outbox_row(
    db_path: &std::path::PathBuf,
    id: &str,
    tenant: &str,
    authority: &str,
    canonical: &str,
) {
    let key = test_signing_key();
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let sig = key.sign(canonical.as_bytes());
    let sig_b64 = BASE64.encode(sig.to_bytes());
    insert_outbox_row_raw(
        db_path,
        id,
        tenant,
        authority,
        canonical,
        canonical,
        &sig_b64,
        &pinned_kid,
    );
}

/// Low-level insert with full control over canonical bytes, signature, and kid
/// — used by the negative tests to seed tampered / forged / wrong-kid rows.
#[allow(clippy::too_many_arguments)]
fn insert_outbox_row_raw(
    db_path: &std::path::PathBuf,
    id: &str,
    tenant: &str,
    authority: &str,
    envelope_json: &str,
    envelope_json_canonical: &str,
    signature_b64: &str,
    signing_kid: &str,
) {
    let conn = Connection::open(db_path).unwrap();
    // Use proper foreign keys so we insert pending_escalations first
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('resp', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .ok(); // ignore if already exists

    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms)
         VALUES (?1, 'resp', ?2, 'staged', 'Act', ?3, ?4, ?5, ?6, ?7, 1000, 9999999999999)",
        params![id, tenant, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid],
    ).unwrap();
}

/// Canonical recommend-authority envelope (no capability token needed).
fn recommend_canonical(in_response_to: &str) -> String {
    format!(
        r#"{{"schema":"irin.directive.payload.v1","in_response_to":"{}","authority":"recommend","verdict":"Act","job":"foo"}}"#,
        in_response_to
    )
}

/// Canonical execute-authority envelope carrying the given capability token.
fn execute_canonical(in_response_to: &str, token: &str) -> String {
    format!(
        r#"{{"schema":"irin.directive.payload.v1","in_response_to":"{}","authority":"execute","verdict":"Act","job":"foo","capability_token":"{}"}}"#,
        in_response_to, token
    )
}

#[tokio::test]
async fn test_worker_claim_and_execute() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-a";

    db.add_capability_token(
        tenant.to_string(),
        "tok-123".to_string(),
        "execute".to_string(),
    )
    .await
    .unwrap();

    insert_signed_outbox_row(
        &db_path,
        "dir-1",
        tenant,
        "execute",
        &execute_canonical("resp", "tok-123"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(report.executed_count, 1);
    assert_eq!(report.failed_count, 0);
    assert!(!report.idle);

    let rec = db.get_outbox(tenant, "dir-1").await.unwrap().unwrap();
    assert_eq!(rec.status, "acked");
}

#[tokio::test]
async fn test_worker_blocks_invalid_token() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-b";

    // No tokens inserted — captoken is unknown -> fail closed.
    insert_signed_outbox_row(
        &db_path,
        "dir-2",
        tenant,
        "execute",
        &execute_canonical("resp", "bad-tok"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(report.executed_count, 0);
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-2").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged"); // Nacked back to staged
    assert!(rec.last_error.unwrap().contains("invalid capability_token"));
}

#[tokio::test]
async fn test_worker_blocks_missing_token_fail_closed() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-b2";

    // Execute authority with NO capability_token field at all -> fail closed.
    let canonical = r#"{"schema":"irin.directive.payload.v1","in_response_to":"resp","authority":"execute","verdict":"Act","job":"foo"}"#;
    insert_signed_outbox_row(&db_path, "dir-2b", tenant, "execute", canonical);

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.executed_count, 0);
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-2b").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged");
    assert!(rec.last_error.unwrap().contains("missing capability_token"));
}

#[tokio::test]
async fn test_worker_allows_recommend_without_token() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-c";

    insert_signed_outbox_row(
        &db_path,
        "dir-3",
        tenant,
        "recommend",
        &recommend_canonical("resp"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(report.executed_count, 1);
    assert_eq!(report.failed_count, 0);

    let rec = db.get_outbox(tenant, "dir-3").await.unwrap().unwrap();
    assert_eq!(rec.status, "acked");
}

// ── Pre-seal W2 negative tests: provenance enforcement ───────────────────────

/// A tampered envelope (canonical bytes mutated after signing) must be rejected:
/// the signature no longer verifies over the stored bytes. Worker nacks, never
/// executes.
#[tokio::test]
async fn test_worker_rejects_tampered_envelope() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-tamper";

    // Sign the legitimate recommend envelope, then store a TAMPERED canonical
    // (escalated to execute) with the original signature + pinned kid.
    let key = test_signing_key();
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let signed = recommend_canonical("resp");
    let sig_b64 = BASE64.encode(key.sign(signed.as_bytes()).to_bytes());
    let tampered = r#"{"schema":"irin.directive.payload.v1","in_response_to":"resp","authority":"execute","verdict":"Act","job":"foo","capability_token":"forged"}"#;

    insert_outbox_row_raw(
        &db_path,
        "dir-tamper",
        tenant,
        "execute",
        tampered,
        tampered,
        &sig_b64,
        &pinned_kid,
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(
        report.executed_count, 0,
        "tampered directive must NOT execute"
    );
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-tamper").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged"); // nacked, not acked
    assert!(rec
        .last_error
        .unwrap()
        .contains("directive envelope verification failed"));
}

/// A wholly forged envelope (signed by an attacker key, not the pinned Council
/// key, but carrying the pinned kid) must be rejected.
#[tokio::test]
async fn test_worker_rejects_forged_signature_wrong_key() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-forge";

    let attacker = SigningKey::from_bytes(&[99u8; 32]);
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let canonical = execute_canonical("resp", "tok-anything");
    let forged_sig = BASE64.encode(attacker.sign(canonical.as_bytes()).to_bytes());

    insert_outbox_row_raw(
        &db_path,
        "dir-forge",
        tenant,
        "execute",
        &canonical,
        &canonical,
        &forged_sig,
        &pinned_kid, // claims the pinned kid, but signed by the wrong key
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(
        report.executed_count, 0,
        "forged directive must NOT execute"
    );
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-forge").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged");
}

/// A directive carrying an unpinned / wrong kid must be rejected before any
/// crypto work — we never look up an alternate key.
#[tokio::test]
async fn test_worker_rejects_unpinned_kid() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-kid";

    // Sign with the real key (so the signature itself would verify), but store
    // a DIFFERENT kid than the pinned one.
    let key = test_signing_key();
    let canonical = recommend_canonical("resp");
    let sig_b64 = BASE64.encode(key.sign(canonical.as_bytes()).to_bytes());

    insert_outbox_row_raw(
        &db_path,
        "dir-kid",
        tenant,
        "recommend",
        &canonical,
        &canonical,
        &sig_b64,
        "sidecar-v1-deadbeef", // not the pinned kid
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(
        report.executed_count, 0,
        "unpinned-kid directive must NOT execute"
    );
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-kid").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged");
    assert!(rec.last_error.unwrap().contains("kid mismatch"));
}

/// With NO verifier supplied (boot/wiring fault), the worker must fail closed:
/// nack every claim, execute nothing.
#[tokio::test]
async fn test_worker_no_verifier_fails_closed() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-noverify";

    insert_signed_outbox_row(
        &db_path,
        "dir-nv",
        tenant,
        "recommend",
        &recommend_canonical("resp"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let report = run_worker_tick(&db, &config, None).await.unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(
        report.executed_count, 0,
        "no verifier => fail closed, execute nothing"
    );
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-nv").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged");
}

// ── Pre-seal W2 P1-B negative tests: no silent downgrade ─────────────────────

/// A directive whose VERIFIED envelope carries an authority outside
/// {recommend,prepare,execute} must be refused — NOT silently downgraded to
/// "recommend" and executed. The signature is genuine (signed over the exact
/// bytes), so the crypto gate passes and the authority whitelist is what bites.
#[tokio::test]
async fn test_worker_rejects_unknown_authority_no_silent_downgrade() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-badauth";

    // Genuine signature over a canonical whose authority is "superuser".
    // The directive_outbox.authority COLUMN has a CHECK constraint limiting it
    // to {recommend,prepare,execute}, so seed the column with a valid value
    // ('recommend') while the SIGNED canonical carries the unknown authority —
    // the worker reads authority from the verified canonical, not the column,
    // which is exactly the property under test.
    let key = test_signing_key();
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let canonical = r#"{"schema":"irin.directive.payload.v1","in_response_to":"resp","authority":"superuser","verdict":"Act","job":"foo"}"#;
    let sig_b64 = BASE64.encode(key.sign(canonical.as_bytes()).to_bytes());
    insert_outbox_row_raw(
        &db_path,
        "dir-badauth",
        tenant,
        "recommend", // valid column value (CHECK constraint); canonical says "superuser"
        canonical,
        canonical,
        &sig_b64,
        &pinned_kid,
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(
        report.executed_count, 0,
        "unknown authority must NOT execute (no silent downgrade to recommend)"
    );
    assert_eq!(report.failed_count, 1);

    let rec = db.get_outbox(tenant, "dir-badauth").await.unwrap().unwrap();
    assert_eq!(rec.status, "staged"); // fail-closed nack, not acked
    assert!(rec
        .last_error
        .unwrap()
        .contains("authority not in {recommend,prepare,execute}"));
}

/// A directive whose VERIFIED canonical bytes are not valid JSON must be
/// refused — NOT silently treated as `{}` (which would default-authority to
/// recommend and execute a stub). The signature is genuine over the exact
/// (malformed) bytes, so the crypto gate passes and the parse guard bites.
#[tokio::test]
async fn test_worker_rejects_unparseable_canonical_no_silent_empty() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-badparse";

    // Sign genuinely over a NON-JSON canonical so verify passes but parse fails.
    let key = test_signing_key();
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let malformed = "this is not json {{{";
    let sig_b64 = BASE64.encode(key.sign(malformed.as_bytes()).to_bytes());
    insert_outbox_row_raw(
        &db_path,
        "dir-badparse",
        tenant,
        "recommend",
        malformed,
        malformed,
        &sig_b64,
        &pinned_kid,
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 1);
    assert_eq!(
        report.executed_count, 0,
        "unparseable canonical must NOT execute (no silent {{}} downgrade)"
    );
    assert_eq!(report.failed_count, 1);

    let rec = db
        .get_outbox(tenant, "dir-badparse")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.status, "staged");
    assert!(rec
        .last_error
        .unwrap()
        .contains("canonical envelope failed to parse"));
}

/// Insert a signed, otherwise-valid staged directive whose `expires_at_ms` is in
/// the distant past (epoch 2000ms) so the worker-leg TTL fence sweeps it. Mirrors
/// `insert_signed_outbox_row` but with an elapsed TTL.
fn insert_expired_outbox_row(
    db_path: &std::path::PathBuf,
    id: &str,
    tenant: &str,
    authority: &str,
    canonical: &str,
) {
    let key = test_signing_key();
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let sig = key.sign(canonical.as_bytes());
    let sig_b64 = BASE64.encode(sig.to_bytes());
    let conn = Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO pending_escalations
            (id, tenant, sentinel_name, envelope_json, status, created_at_ms)
         VALUES ('resp', ?1, 'test', '{}', 'council_response_staged', 1000)",
        params![tenant],
    )
    .ok();
    conn.execute(
        "INSERT INTO directive_outbox
         (id, in_response_to, tenant, status, verdict, authority, envelope_json, envelope_json_canonical, signature_b64, signing_kid, created_at_ms, expires_at_ms)
         VALUES (?1, 'resp', ?2, 'staged', 'Act', ?3, ?4, ?4, ?5, ?6, 1000, 2000)",
        params![id, tenant, authority, canonical, &sig_b64, &pinned_kid],
    )
    .unwrap();
}

/// A4a/T21 worker-leg dispatch fence: a staged directive whose TTL has already
/// elapsed must be swept to `expired` and never claimed/dispatched. Without the
/// fence in `claim_outbox`, the row would be claimed and executed after its
/// authorization window closed (post-expiry spend). Fail-safe direction: the
/// fence only REFUSES dispatch, never causes one.
#[tokio::test]
async fn test_worker_ttl_fence_expired_directive_not_dispatched() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-ttl";

    db.add_capability_token(
        tenant.to_string(),
        "tok-ttl".to_string(),
        "execute".to_string(),
    )
    .await
    .unwrap();
    insert_expired_outbox_row(
        &db_path,
        "dir-expired",
        tenant,
        "execute",
        &execute_canonical("resp", "tok-ttl"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };

    let ttl_before = gateway_sidecar::watch::dispatcher::directive_ttl_expired_total();
    let verifier = test_verifier();
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();

    // Nothing claimed or executed: the fence swept it before the claim SELECT.
    assert_eq!(
        report.claimed_count, 0,
        "expired directive must not be claimed"
    );
    assert_eq!(
        report.executed_count, 0,
        "expired directive must not execute"
    );
    assert!(report.idle, "tick is idle when only expired rows remain");

    // Terminal `expired` with an audit reason, not a lingering `staged` row.
    let rec = db.get_outbox(tenant, "dir-expired").await.unwrap().unwrap();
    assert_eq!(rec.status, "expired", "TTL-elapsed row swept to 'expired'");

    // The sweep is observable: the global TTL-expired counter advanced (delta,
    // not absolute, to stay robust under parallel tests sharing the static).
    assert!(
        gateway_sidecar::watch::dispatcher::directive_ttl_expired_total() > ttl_before,
        "directive_ttl_expired_total must increment when a row is swept"
    );
}

/// TTL-fence review point #1: a directive swept to `expired` while a stale worker
/// (hypothetically) still holds a handle must not panic or wrongly mutate the
/// ack/nack path. `nack_outbox` guards on status, so a late nack on an expired row
/// returns NotActionable{status:"expired"} — degrades cleanly, no spend, no state
/// change.
#[tokio::test]
async fn test_nack_on_ttl_expired_row_is_not_actionable() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-ttl-nack";

    db.add_capability_token(
        tenant.to_string(),
        "tok-n".to_string(),
        "execute".to_string(),
    )
    .await
    .unwrap();
    insert_expired_outbox_row(
        &db_path,
        "dir-exp-nack",
        tenant,
        "execute",
        &execute_canonical("resp", "tok-n"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };
    let verifier = test_verifier();
    let _ = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();

    // Late nack against the now-`expired` row: clean NotActionable, not a panic.
    let outcome = db
        .nack_outbox(tenant, "dir-exp-nack", "any-handle", "late")
        .await
        .unwrap();
    assert!(
        matches!(outcome, AckOutcome::NotActionable { ref status, .. } if status == "expired"),
        "late nack on an expired row must be NotActionable(expired)"
    );
}

/// TTL-fence review point #3: the success-`ack` path has the same status guard as
/// `nack` (db.rs:4550 treats `expired` as NotActionable). A stale worker that
/// completes after the fence swept its row must not flip an `expired` row to
/// `acked`. Parity with `test_nack_on_ttl_expired_row_is_not_actionable`.
#[tokio::test]
async fn test_ack_on_ttl_expired_row_is_not_actionable() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-ttl-ack";

    db.add_capability_token(
        tenant.to_string(),
        "tok-a".to_string(),
        "execute".to_string(),
    )
    .await
    .unwrap();
    insert_expired_outbox_row(
        &db_path,
        "dir-exp-ack",
        tenant,
        "execute",
        &execute_canonical("resp", "tok-a"),
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };
    let verifier = test_verifier();
    let _ = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();

    // Late ack against the now-`expired` row: NotActionable, never flips to acked.
    let outcome = db.ack_outbox(tenant, "dir-exp-ack").await.unwrap();
    assert!(
        matches!(outcome, AckOutcome::NotActionable { ref status, .. } if status == "expired"),
        "late ack on an expired row must be NotActionable(expired), not Acked"
    );

    let rec = db.get_outbox(tenant, "dir-exp-ack").await.unwrap().unwrap();
    assert_eq!(rec.status, "expired", "row stays expired after a late ack");
}

/// A4a/T21c env-lift (Invariant): the staged-directive TTL is operator-tunable
/// at runtime so widening the window doesn't require a rebuild. Clamp is pure and
/// deterministic — tested without touching process env.
#[test]
fn test_clamp_stage_ttl_ms() {
    use gateway_sidecar::watch::dispatcher::{
        clamp_stage_ttl_ms, DIRECTIVE_STAGE_TTL_MS_DEFAULT, DIRECTIVE_STAGE_TTL_MS_MAX,
        DIRECTIVE_STAGE_TTL_MS_MIN,
    };
    assert_eq!(
        clamp_stage_ttl_ms(None),
        DIRECTIVE_STAGE_TTL_MS_DEFAULT,
        "unset -> default"
    );
    assert_eq!(
        clamp_stage_ttl_ms(Some(120_000)),
        120_000,
        "in-band passes through"
    );
    assert_eq!(
        clamp_stage_ttl_ms(Some(1_000)),
        DIRECTIVE_STAGE_TTL_MS_MIN,
        "too-low clamps up"
    );
    assert_eq!(
        clamp_stage_ttl_ms(Some(999_000)),
        DIRECTIVE_STAGE_TTL_MS_MAX,
        "too-high clamps down"
    );
    assert_eq!(
        clamp_stage_ttl_ms(Some(-5)),
        DIRECTIVE_STAGE_TTL_MS_MIN,
        "negative clamps up (fail-safe)"
    );
}

/// T21d env-lift: the delivery-attempt ceiling is operator-tunable at runtime. Clamp is
/// pure and deterministic — tested without touching process env.
#[test]
fn test_clamp_max_delivery_attempts() {
    use gateway_sidecar::watch::dispatcher::{
        clamp_max_delivery_attempts, DIRECTIVE_MAX_DELIVERY_ATTEMPTS_DEFAULT,
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MAX, DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MIN,
    };
    assert_eq!(
        clamp_max_delivery_attempts(None),
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_DEFAULT,
        "unset -> default"
    );
    assert_eq!(
        clamp_max_delivery_attempts(Some(10)),
        10,
        "in-band passes through"
    );
    assert_eq!(
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MIN, 2,
        "floor is 2 — guarantees >=1 retry (H4)"
    );
    assert_eq!(
        clamp_max_delivery_attempts(Some(1)),
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MIN,
        "1 clamps up to the floor — never dead-letter on the first attempt"
    );
    assert_eq!(
        clamp_max_delivery_attempts(Some(0)),
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MIN,
        "too-low clamps up (at least one retry)"
    );
    assert_eq!(
        clamp_max_delivery_attempts(Some(9_999)),
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MAX,
        "too-high clamps down"
    );
    assert_eq!(
        clamp_max_delivery_attempts(Some(-5)),
        DIRECTIVE_MAX_DELIVERY_ATTEMPTS_MIN,
        "negative clamps up (fail-safe)"
    );
}

/// P2 env-lift: the clock-skew breaker cap is operator-tunable at runtime. Pure clamp,
/// tested without touching process env. Floor 1s keeps a load-burst of monotonic +1ms bumps
/// from tripping it; ceiling 30s holds margin under the 90s TTL horizon.
#[test]
fn test_clamp_max_allowed_skew_ms() {
    use gateway_sidecar::watch::dispatcher::{
        clamp_max_allowed_skew_ms, MAX_ALLOWED_SKEW_MS_DEFAULT, MAX_ALLOWED_SKEW_MS_MAX,
        MAX_ALLOWED_SKEW_MS_MIN,
    };
    assert_eq!(
        clamp_max_allowed_skew_ms(None),
        MAX_ALLOWED_SKEW_MS_DEFAULT,
        "unset -> default"
    );
    assert_eq!(MAX_ALLOWED_SKEW_MS_DEFAULT, 5_000, "default 5s");
    assert_eq!(MAX_ALLOWED_SKEW_MS_MIN, 1_000, "floor 1s");
    assert_eq!(
        MAX_ALLOWED_SKEW_MS_MAX, 10_000,
        "ceiling 10s — margin under 30s min TTL horizon"
    );
    // The cap-ceiling < TTL-floor invariant is enforced at compile time in dispatcher.rs
    // (a `const _: () = assert!(...)` static assertion beside the consts).
    assert_eq!(
        clamp_max_allowed_skew_ms(Some(7_500)),
        7_500,
        "in-band passes through"
    );
    assert_eq!(
        clamp_max_allowed_skew_ms(Some(10)),
        MAX_ALLOWED_SKEW_MS_MIN,
        "too-low clamps up to the floor"
    );
    assert_eq!(
        clamp_max_allowed_skew_ms(Some(999_000)),
        MAX_ALLOWED_SKEW_MS_MAX,
        "too-high clamps down to the ceiling"
    );
    assert_eq!(
        clamp_max_allowed_skew_ms(Some(-5)),
        MAX_ALLOWED_SKEW_MS_MIN,
        "negative clamps up (fail-safe)"
    );
}

/// T21d worker-leg delivery-attempt fence: a poison directive (forged signature → the worker
/// nacks it back to `staged` every tick) must not re-dispatch forever. After
/// DIRECTIVE_MAX_DELIVERY_ATTEMPTS claims, `claim_outbox` dead-letters it to the terminal
/// `expired` status (last_error preserved = the real nack reason), bumps the distinct
/// delivery-exceeded counter, and stops re-claiming it. Fail-safe: the row never executes and
/// the loop is bounded by ATTEMPTS, not just the TTL window. The row's expires_at_ms is far in
/// the future, so this proves the attempt fence in isolation from the TTL fence.
#[tokio::test]
async fn test_worker_dead_letters_poison_directive_after_max_attempts() {
    use gateway_sidecar::watch::dispatcher::directive_max_delivery_exceeded_total;

    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-poison";

    // Forged signature (64 zero bytes) → verification fails every tick → nack back to staged.
    // recommend authority needs no capability token, so the only failure is the signature.
    let zero_sig = BASE64.encode([0u8; 64]);
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let canonical = recommend_canonical("resp");
    insert_outbox_row_raw(
        &db_path,
        "dir-poison",
        tenant,
        "recommend",
        &canonical,
        &canonical,
        &zero_sig,
        &pinned_kid,
    );

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };
    let verifier = test_verifier();
    let ceiling = gateway_sidecar::watch::dispatcher::directive_max_delivery_attempts();
    let exceeded_before = directive_max_delivery_exceeded_total();

    // Exhaust the ceiling: each tick claims (claim_count += 1), fails verify, nacks → staged.
    for _ in 0..ceiling {
        let report = run_worker_tick(&db, &config, Some(&verifier))
            .await
            .unwrap();
        assert_eq!(
            report.claimed_count, 1,
            "poison row re-claimed each attempt"
        );
        assert_eq!(report.executed_count, 0, "poison row never executes");
        assert_eq!(report.failed_count, 1, "poison row nacked back to staged");
        // Still staged and re-claimable until the ceiling is reached.
        let rec = db.get_outbox(tenant, "dir-poison").await.unwrap().unwrap();
        assert_eq!(
            rec.status, "staged",
            "below the ceiling the row stays staged"
        );
    }

    // One more tick: claim_outbox's pre-SELECT sweep sees claim_count >= ceiling and
    // dead-letters the row before it can be claimed again.
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(
        report.claimed_count, 0,
        "dead-lettered row is not re-claimed"
    );
    assert_eq!(report.executed_count, 0, "dead-lettered row never executes");
    assert!(
        report.idle,
        "tick is idle once the only row is dead-lettered"
    );

    let rec = db.get_outbox(tenant, "dir-poison").await.unwrap().unwrap();
    assert_eq!(
        rec.status, "expired",
        "attempt-exhausted row swept to terminal 'expired'"
    );
    // last_error carries the H1 audit stamp: trigger + ceiling-at-death + preserved root cause
    // ('max_delivery_attempts(N); root=<verify-fail reason>'). The stamp disambiguates a
    // dead-letter from a TTL-expiry at the row level, and the root keeps the real failure.
    let last_error = rec.last_error.as_deref().unwrap_or_default();
    assert!(
        last_error.starts_with(&format!("max_delivery_attempts({ceiling}); root=")),
        "dead-letter stamps trigger + ceiling-at-death; got {:?}",
        rec.last_error
    );
    assert!(
        last_error.contains("verification-failed"),
        "dead-letter stamp preserves the nack root cause; got {:?}",
        rec.last_error
    );

    // The sweep is observable on the distinct counter (delta, robust under parallel tests).
    assert!(
        directive_max_delivery_exceeded_total() > exceeded_before,
        "directive_max_delivery_exceeded_total must increment when a poison row is dead-lettered"
    );
}

/// T21d leased-row guard (Council nice-to-have): a row whose claim_count is already past the
/// ceiling but which is currently LEASED (in-flight worker) must NOT be dead-lettered mid-exec —
/// the attempt-sweep shares the TTL sweep's unleased-only predicate. Only once the lease expires
/// (worker died/finished without ack) does the sweep reclaim it to terminal 'expired'.
#[tokio::test]
async fn test_leased_row_past_ceiling_not_swept_until_lease_expires() {
    let (_tmp, db, db_path) = setup_db().await;
    let tenant = "tenant-leased";

    let zero_sig = BASE64.encode([0u8; 64]);
    let pinned_kid = test_verifier().pinned_kid().to_string();
    let canonical = recommend_canonical("resp");
    insert_outbox_row_raw(
        &db_path,
        "dir-leased",
        tenant,
        "recommend",
        &canonical,
        &canonical,
        &zero_sig,
        &pinned_kid,
    );

    let ceiling = gateway_sidecar::watch::dispatcher::directive_max_delivery_attempts();

    // Force the row past the ceiling while holding an active (far-future) lease — an in-flight
    // worker that has claimed it many times but not yet acked/nacked.
    let far_future_lease = i64::MAX / 2;
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE directive_outbox SET claim_count = ?1, claimed_until_ms = ?2 WHERE id = 'dir-leased'",
            params![ceiling + 3, far_future_lease],
        )
        .unwrap();
    }

    let config = WatchWorkerConfig {
        enabled: true,
        tick_interval_ms: 1000,
        max_claims_per_tick: 10,
        lease_duration_ms: 30_000,
        tenant_scope: tenant.to_string(),
    };
    let verifier = test_verifier();

    // Leased: the unleased-only predicate skips it — neither swept nor re-claimed.
    let report = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    assert_eq!(report.claimed_count, 0, "leased row is not re-claimed");
    let rec = db.get_outbox(tenant, "dir-leased").await.unwrap().unwrap();
    assert_eq!(
        rec.status, "staged",
        "an in-flight leased row past the ceiling must NOT be yanked mid-exec"
    );

    // Lease expires (worker died/finished without ack): now the attempt-sweep reclaims it.
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE directive_outbox SET claimed_until_ms = 1 WHERE id = 'dir-leased'",
            [],
        )
        .unwrap();
    }
    let _ = run_worker_tick(&db, &config, Some(&verifier))
        .await
        .unwrap();
    let rec = db.get_outbox(tenant, "dir-leased").await.unwrap().unwrap();
    assert_eq!(
        rec.status, "expired",
        "once the lease expires, the over-ceiling row is dead-lettered"
    );
}
