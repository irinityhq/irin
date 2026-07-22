//! Arming ceremony persistence: arm_audit chain, arm_pending, active_arm.

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use super::WatchDb;

/// Read the `active_arm` singleton row (id = 0) column-for-column into the
/// shared [`crate::watch::attest::ActiveArmRow`]. Works on a plain connection
/// or inside a transaction (`Transaction` derefs to `Connection`). Validity
/// is decided by `attest::verify_arm_row` — this only fetches.
pub(crate) fn read_active_arm_row(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<Option<crate::watch::attest::ActiveArmRow>> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        "SELECT build_id, enabled_surface, effective_daily_cap_cents,
                tenant, armed_epoch, exp_at_ms,
                challenge_bytes, signature_der, credential_id, credential_type,
                authenticator_data, client_data_json
         FROM active_arm WHERE id = 0",
        [],
        |r| {
            Ok(crate::watch::attest::ActiveArmRow {
                build_id: r.get(0)?,
                enabled_surface: r.get(1)?,
                effective_daily_cap_cents: r.get(2)?,
                tenant: r.get(3)?,
                armed_epoch: r.get(4)?,
                exp_at_ms: r.get(5)?,
                challenge_bytes: r.get(6)?,
                signature_der: r.get(7)?,
                credential_id: r.get(8)?,
                credential_type: r.get(9)?,
                authenticator_data: r.get(10)?,
                client_data_json: r.get(11)?,
            })
        },
    )
    .optional()
}

/// p0a-four-eyes — frozen distinct arm-audit genesis hash. Storing the digest
/// directly preserves every existing chain without retaining the original
/// domain-separation value in product source.
pub const ARM_AUDIT_DISTINCT_GENESIS_HASH: &str =
    "19a4241571e9a575010d63c80a8e8b51a316a90c02c3a47dd90aed1d04a863aa";

/// Returns the distinct arm_audit genesis hash as a lowercase hex string.
pub fn arm_audit_distinct_genesis() -> String {
    ARM_AUDIT_DISTINCT_GENESIS_HASH.to_owned()
}

#[cfg(test)]
mod genesis_tests {
    use super::{arm_audit_distinct_genesis, ARM_AUDIT_DISTINCT_GENESIS_HASH};

    #[test]
    fn frozen_arm_audit_genesis_digest_is_stable() {
        assert_eq!(
            ARM_AUDIT_DISTINCT_GENESIS_HASH,
            "19a4241571e9a575010d63c80a8e8b51a316a90c02c3a47dd90aed1d04a863aa"
        );
        assert_eq!(
            arm_audit_distinct_genesis(),
            ARM_AUDIT_DISTINCT_GENESIS_HASH
        );
    }
}

/// p0a-four-eyes — length-prefixed preimage for a row in the arming-ceremony
/// audit chain (`arm_audit`). Mirrors `compute_watch_fire_preimage`'s
/// length-prefix scheme (collision-resistant under adversarial `|:`
/// injection in field content — see T_NEW3). `detail` is hashed as the
/// empty string when NULL so verification treats NULL and '' identically.
/// Pub (not pub(crate)) so integration tests can re-verify the chain.
pub fn compute_arm_audit_preimage(
    at_ms: i64,
    action: &str,
    principal: &str,
    detail: &str,
    prev_hash: &str,
) -> String {
    let at_str = at_ms.to_string();
    format!(
        "{}:{}|{}:{}|{}:{}|{}:{}|{}:{}",
        at_str.len(),
        at_str,
        action.len(),
        action,
        principal.len(),
        principal,
        detail.len(),
        detail,
        prev_hash.len(),
        prev_hash
    )
}

/// dual-custody-local-attest hardening (Hardening B4–B6 #8, defense in
/// depth): a stage_id reaching the audit-history LIKE queries must be the
/// 32-hex shape this process mints. The HTTP handler already enforces this
/// for attacker-suppliable input; this guard keeps the invariant even if a
/// future caller skips that check — `%`/`_` must never expand a LIKE.
fn assert_stage_id_shape(stage_id: &str) -> Result<(), rusqlite::Error> {
    if stage_id.len() == 32 && stage_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(format!(
            "stage_id is not 32-hex (len {}) — refusing before any LIKE query (fail-closed)",
            stage_id.len()
        )),
    ))
}

/// p0a-four-eyes / dual-custody-local-attest B1 — append one arm_audit row
/// INSIDE a caller-owned transaction. The chain invariant lives here:
/// `prev_hash` is read in the same tx that writes the new row — no caching,
/// no pre-fetch. Shared by `append_arm_audit` (single-row tx) and
/// `stage_arm_pending` (audit row + pending row in one tx, spec §4.3).
fn append_arm_audit_in_tx(
    tx: &rusqlite::Transaction<'_>,
    at_ms: i64,
    action: &str,
    principal: &str,
    detail: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    let prev_hash: String = tx
        .query_row(
            "SELECT hash FROM arm_audit ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(arm_audit_distinct_genesis);

    let preimage =
        compute_arm_audit_preimage(at_ms, action, principal, detail.unwrap_or(""), &prev_hash);
    let hash = hex::encode(Sha256::digest(preimage.as_bytes()));

    tx.execute(
        "INSERT INTO arm_audit (at_ms, action, principal, detail, prev_hash, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![at_ms, action, principal, detail, prev_hash, hash],
    )?;
    Ok(tx.last_insert_rowid())
}

/// p0a-four-eyes — one row of the arming-ceremony audit chain (`arm_audit`).
/// `detail` is free-text context (stage_id nonce, rejection reason, ...);
/// `principal` is the principal NAME (never the token).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArmAuditRow {
    pub id: i64,
    pub at_ms: i64,
    pub action: String,
    pub principal: String,
    pub detail: Option<String>,
    pub prev_hash: String,
    pub hash: String,
}

/// dual-custody-local-attest B1 (spec §4.3) — the persisted pending arm
/// stage. `challenge_bytes` are the verbatim JCS challenge bytes produced
/// once at stage time (B2); `exp_at_ms` is WALL-CLOCK Unix millis so expiry
/// survives restart. The in-memory `StagedArm` is a cache of this row.
#[derive(Debug, Clone)]
pub struct ArmPendingRow {
    pub stage_id: String,
    pub staged_by: String,
    pub challenge_bytes: Vec<u8>,
    pub exp_at_ms: i64,
    /// B7 (spec §8): this stage is a rehearsal — confirm verifies everything
    /// but the producer never starts.
    pub rehearsal: bool,
    // T1 MF-1 (B3): the PERSISTED content-binding fields — what the human is
    // shown before they tap. The display renders THESE bytes, never live
    // ambient config (no-trusted-display residual is on the persisted row).
    pub build_id: String,
    pub enabled_surface: String,
    pub effective_daily_cap_cents: i64,
    pub tenant: String,
    pub challenge_format_version: i64,
}

/// Attested-arm (B1) — the attested-arm singleton row. The ceiling the reserve
/// enforces as an ABSOLUTE bound (ambient may only narrow it). `armed_epoch` is
/// the generation the producer must match; `exp_at_ms` is the spend window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveArmRow {
    pub build_id: String,
    pub enabled_surface: String,
    pub effective_daily_cap_cents: i64,
    pub tenant: String,
    pub armed_epoch: i64,
    pub exp_at_ms: i64,
    pub challenge_sha256: String,
    pub audit_id: i64,
}

/// Signed-material invariant — the owned signature material the confirm
/// handler hands to `confirm_arm_attest` so it can persist it on active_arm for
/// reserve-time re-verification. Built from the same request fields the verify
/// closure consumed. `authenticator_data` / `client_data_json` are `None` for
/// se-p256 and populated for the fido2-es256 (native / browser) legs.
#[derive(Debug, Clone)]
pub struct PersistedArmSignature {
    pub credential_id: String,
    pub credential_type: String,
    pub signature_der: Vec<u8>,
    pub authenticator_data: Option<Vec<u8>>,
    pub client_data_json: Option<Vec<u8>>,
}

/// dual-custody-local-attest B4 (spec §6) — what a successful crypto verify
/// hands back to the one-tx confirm: the §6 audit-binding fields.
/// `sig_counter` is the FIDO2 monotonic counter; se-p256 reports 0 with the
/// type making that explicit — NOTHING branches on the value, only on
/// `credential_type`.
#[derive(Debug, Clone)]
pub struct AttestVerification {
    pub credential_id: String,
    pub credential_type: String,
    pub sig_counter: u32,
}

/// dual-custody-local-attest B4 — outcome of the one-transaction confirm
/// (`confirm_arm_attest`). Exactly one of these per attempt; the handler
/// maps them to HTTP statuses + best-effort rejection audit rows.
#[derive(Debug, Clone)]
pub enum ArmConfirmTxOutcome {
    /// Verified, §6 audit row committed, pending row deleted — the caller
    /// may arm. `detail` is the committed audit detail JSON (for ntfy).
    Verified {
        staged_by: String,
        audit_id: i64,
        detail: String,
        /// B7 (spec §8): the consumed stage was a rehearsal — the caller
        /// must NOT arm (the §6-shaped row is 'confirm_rehearsal').
        rehearsal: bool,
    },
    /// This stage_id already has a 'confirm' row in the full audit history
    /// — idempotent success, nothing written (spec §4.3 + condition 8).
    AlreadyConfirmed,
    NoPendingStage,
    /// Pending row was past `exp_at_ms` — deleted in the same tx.
    Expired,
    StageIdMismatch,
    /// Crypto/credential rejection with a stable §6 reason
    /// (`unknown_credential`, `bad_signature`, `counter_regression`, …).
    Rejected {
        reason: String,
    },
}

impl WatchDb {
    /// p0a-four-eyes — append one row to the arming-ceremony audit chain.
    ///
    /// INVARIANT (same shape as `insert_fire`): `prev_hash` is read INSIDE
    /// the same BEGIN IMMEDIATE tx that writes the new row — no in-memory
    /// caching, no pre-fetch outside the tx. The chain is global (single
    /// arming ceremony per sidecar, not tenant-scoped). Every
    /// stage/confirm/disarm AND every stage/confirm rejection writes a row,
    /// so who/when/action/principal is tamper-evident. `principal` is the
    /// principal NAME — tokens must never reach this method.
    pub async fn append_arm_audit(
        &self,
        action: &str,
        principal: &str,
        detail: Option<&str>,
    ) -> anyhow::Result<i64> {
        let action = action.to_string();
        let principal = principal.to_string();
        let detail = detail.map(|s| s.to_string());
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        self.conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let id =
                    append_arm_audit_in_tx(&tx, at_ms, &action, &principal, detail.as_deref())?;
                tx.commit()?;
                Ok::<i64, rusqlite::Error>(id)
            })
            .await
            .map_err(Into::into)
    }

    /// dual-custody-local-attest B1 (spec §4.3) — write the 'stage' audit row
    /// AND the persisted pending stage in ONE BEGIN IMMEDIATE transaction, so
    /// the durable pending-state and its audit record land or fail together
    /// (fail-closed: callers must abort the stage on Err). Any prior pending
    /// row is replaced — a new stage supersedes the old one, matching the
    /// in-memory overwrite semantics of `arm_staging`.
    #[allow(clippy::too_many_arguments)]
    pub async fn stage_arm_pending(
        &self,
        principal: &str,
        detail: &str,
        stage_id: &str,
        challenge_bytes: Vec<u8>,
        exp_at_ms: i64,
        rehearsal: bool,
        // T1 MF-1 (B5/B2): the persisted content-binding fields + the challenge
        // format version, stored next to the verbatim challenge bytes so
        // confirm re-derives and STRICT-EQUALITY-compares against the staged
        // truth (never live ambient), and GET /arm/pending renders the PERSISTED
        // intent (B3, no-trusted-display residual).
        content: crate::watch::attest::ArmContent,
        challenge_format_version: u32,
    ) -> anyhow::Result<i64> {
        let principal = principal.to_string();
        let detail = detail.to_string();
        let stage_id = stage_id.to_string();
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        self.conn
            .call(move |conn| {
                assert_stage_id_shape(&stage_id)?;
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                // B4 (stage-id uniqueness invariant): stage_id uniqueness is enforced
                // against the FULL audit history, not just the live pending
                // row — a stage_id that ever appeared in arm_audit (text
                // `stage_id=<id>` rows or §6 JSON rows) cannot be staged
                // again. stage_ids are 16-byte CSPRNG hex generated in this
                // process — a hit means replay or an RNG fault; refuse loud.
                let seen: bool = tx
                    .query_row(
                        "SELECT 1 FROM arm_audit
                         WHERE detail LIKE ?1 OR detail LIKE ?2 LIMIT 1",
                        rusqlite::params![
                            format!("%stage_id={stage_id}%"),
                            format!("%\"stage_id\":\"{stage_id}\"%")
                        ],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if seen {
                    return Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                        Some(format!(
                            "stage_id {stage_id} already appears in arm_audit history (condition 8: never re-staged)"
                        )),
                    ));
                }
                let action = if rehearsal { "stage_rehearsal" } else { "stage" };
                let id = append_arm_audit_in_tx(&tx, at_ms, action, &principal, Some(&detail))?;
                // Attested-arm (P0-3 mapping): stage-replace deliberately does NOT
                // touch active_arm. A new stage opens a fresh ceremony; it does
                // not authorize or revoke spend. A LIVE arm of an earlier epoch
                // must survive a re-stage (the operator may stage a new ceremony
                // while the prior arm is still live) — the reserve's epoch +
                // exp_at_ms predicates are the teeth, and only a successful
                // confirm (with a strictly greater epoch) overwrites the ceiling.
                tx.execute("DELETE FROM arm_pending", [])?;
                tx.execute(
                    "INSERT INTO arm_pending
                       (stage_id, staged_by, challenge_bytes, exp_at_ms, rehearsal,
                        build_id, enabled_surface, effective_daily_cap_cents, tenant,
                        challenge_format_version)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        stage_id,
                        principal,
                        challenge_bytes,
                        exp_at_ms,
                        rehearsal as i64,
                        content.build_id,
                        content.enabled_surface,
                        content.effective_daily_cap_cents,
                        content.tenant,
                        challenge_format_version as i64,
                    ],
                )?;
                tx.commit()?;
                Ok::<i64, rusqlite::Error>(id)
            })
            .await
            .map_err(Into::into)
    }

    /// dual-custody-local-attest B4 (spec §4.2) — the ENTIRE confirm
    /// decision in ONE BEGIN IMMEDIATE transaction: read the pending row,
    /// check wall-clock expiry against the DURABLE `exp_at_ms` (the truth —
    /// the in-memory StagedArm is only a cache, so suspend/clock drift after
    /// rehydrate cannot widen the window), run the caller's crypto closure
    /// over the VERBATIM stored challenge bytes, enforce the fido2 counter
    /// strictly-increasing, append the §6 confirm audit row, and delete the
    /// pending row. The single writer connection plus IMMEDIATE transaction
    /// closes the concurrent-confirm TOCTOU: exactly one of
    /// two racing confirms sees the pending row.
    ///
    /// `verify` receives the stored challenge bytes and returns the §6
    /// binding fields on success, or a stable rejection reason. It runs on
    /// the dedicated SQLite thread — pure CPU (ES256), no I/O, no await.
    ///
    /// Idempotency (spec §4.3, condition 8): a stage_id that already has a
    /// 'confirm' row in the FULL audit history returns `AlreadyConfirmed`
    /// and writes nothing.
    #[allow(clippy::too_many_arguments)]
    pub async fn confirm_arm_attest(
        &self,
        presented_stage_id: &str,
        principal: &str,
        deviation_suffix: &str,
        now_ms: i64,
        // Attested-arm (P0-2 epoch teeth) — the armed generation this confirm grants.
        // Production passes `current_replay_epoch()` (the env value the running
        // producer reads at claim time); the reserve later cross-checks the
        // producer's INDEPENDENTLY-captured epoch against the value written here.
        // The monotonic upsert (`excluded.armed_epoch > active_arm.armed_epoch`)
        // means a re-arm must carry a STRICTLY GREATER epoch to overwrite a live
        // arm. Only written on a REAL confirm (never rehearsal/DARK).
        armed_epoch: i64,
        // B6 confirm-side (grok `ab533eae` HIGH#3 + audit-fidelity follow-up):
        // may THIS build start the real producer? When false (a `-dirty` /
        // unidentifiable build), the ceremony is folded to an EFFECTIVE rehearsal
        // INSIDE this tx — so the unprunable hash-chained audit honestly records
        // `confirm_rehearsal` (+ a `dark_reason`), never a misleading `confirm`,
        // and the returned `rehearsal=true` keeps the producer from starting at
        // the single enforcement point. Runtime build identity is authoritative
        // over the persisted `rehearsal` flag (which is not bound into the sig).
        allow_real_arm: bool,
        // T1 MF-1 (B1-confirm-leg / B5): the content RE-DERIVED at confirm time
        // from CURRENT ambient (`derive_arm_content(daily_spend_cap())` +
        // embedded build_id). Compared with STRICT EQUALITY (integer cents,
        // exact string match) against the PERSISTED staged values BEFORE the
        // signature is accepted; any drift → Rejected{arm_content_drift}. This
        // closes the symmetric confirm→content TOCTOU (an agent that mutates
        // cap/surface/tenant or swaps the build between stage and tap).
        expected_content: crate::watch::attest::ArmContent,
        // Signed-material invariant — signature material persisted on a REAL
        // confirm so the reserve re-verifies the ES256 signature at spend time.
        signed_material: PersistedArmSignature,
        verify: impl FnOnce(&[u8]) -> Result<AttestVerification, String> + Send + 'static,
    ) -> anyhow::Result<ArmConfirmTxOutcome> {
        let presented = presented_stage_id.to_string();
        let principal = principal.to_string();
        let deviation_suffix = deviation_suffix.to_string();
        self.conn
            .call(move |conn| -> Result<ArmConfirmTxOutcome, rusqlite::Error> {
                assert_stage_id_shape(&presented)?;
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let confirmed_before: bool = tx
                    .query_row(
                        "SELECT 1 FROM arm_audit
                         WHERE action IN ('confirm', 'confirm_rehearsal')
                         AND detail LIKE ?1 LIMIT 1",
                        rusqlite::params![format!("%\"stage_id\":\"{presented}\"%")],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);

                #[allow(clippy::type_complexity)]
                let row: Option<(String, String, Vec<u8>, i64, bool, String, String, i64, String, i64)> = tx
                    .query_row(
                        "SELECT stage_id, staged_by, challenge_bytes, exp_at_ms, rehearsal,
                                build_id, enabled_surface, effective_daily_cap_cents, tenant,
                                challenge_format_version
                         FROM arm_pending LIMIT 1",
                        [],
                        |r| {
                            Ok((
                                r.get(0)?,
                                r.get(1)?,
                                r.get(2)?,
                                r.get(3)?,
                                r.get::<_, i64>(4)? != 0,
                                r.get(5)?,
                                r.get(6)?,
                                r.get(7)?,
                                r.get(8)?,
                                r.get(9)?,
                            ))
                        },
                    )
                    .optional()?;

                let Some((
                    stage_id,
                    staged_by,
                    challenge_bytes,
                    exp_at_ms,
                    rehearsal,
                    staged_build_id,
                    staged_enabled_surface,
                    staged_cap_cents,
                    staged_tenant,
                    staged_format_version,
                )) = row
                else {
                    return Ok(if confirmed_before {
                        ArmConfirmTxOutcome::AlreadyConfirmed
                    } else {
                        ArmConfirmTxOutcome::NoPendingStage
                    });
                };
                if stage_id != presented {
                    return Ok(if confirmed_before {
                        ArmConfirmTxOutcome::AlreadyConfirmed
                    } else {
                        ArmConfirmTxOutcome::StageIdMismatch
                    });
                }
                if confirmed_before {
                    // Belt-and-braces: a live pending row for an already-
                    // confirmed stage_id should be impossible (confirm
                    // deletes it in this same tx) — refuse to confirm twice.
                    return Ok(ArmConfirmTxOutcome::AlreadyConfirmed);
                }
                if exp_at_ms <= now_ms {
                    // Expiry clears the durable row (spec §4.3) — in the
                    // same tx, so the decision and the cleanup are atomic.
                    tx.execute(
                        "DELETE FROM arm_pending WHERE stage_id = ?1",
                        rusqlite::params![stage_id],
                    )?;
                    tx.commit()?;
                    return Ok(ArmConfirmTxOutcome::Expired);
                }

                // T1 MF-1 read-side version floor (B2, hardening
                // HIGH#2). The DB-layer `CHECK(challenge_format_version >= 2)`
                // is a WRITE constraint; it cannot stop a v1 row planted by a
                // second SQLite connection or surviving a pre-rebuild crash
                // window. Gate the floor on READ too: a sub-v2 row is refused
                // before drift/verify, fail-closed. (The signed-bytes `v` is
                // also asserted post-verify below; this is the cheap pre-crypto
                // fast-fail on the persisted column.)
                if staged_format_version < 2 {
                    return Ok(ArmConfirmTxOutcome::Rejected {
                        reason: "stale_challenge_version".to_string(),
                    });
                }

                // T1 MF-1 strict-equality content binding (review
                // Q2: STRICT EQUALITY, integer cents). Re-derived CURRENT content
                // vs the PERSISTED staged values — BEFORE the signature is
                // accepted. build_id compares the EMBEDDED constant (Q1), so a
                // stage-on-build-A / confirm-on-build-B is caught here. Any
                // benign lowering of the cap is an explicit re-arm, not a silent
                // accept (mirrors the counter_regression rejection shape). No
                // float on this path — the comparison is i64 cents and exact
                // strings.
                if expected_content.build_id != staged_build_id
                    || expected_content.enabled_surface != staged_enabled_surface
                    || expected_content.effective_daily_cap_cents != staged_cap_cents
                    || expected_content.tenant != staged_tenant
                {
                    return Ok(ArmConfirmTxOutcome::Rejected {
                        reason: "arm_content_drift".to_string(),
                    });
                }

                let verified = match verify(&challenge_bytes) {
                    Ok(v) => v,
                    Err(reason) => return Ok(ArmConfirmTxOutcome::Rejected { reason }),
                };

                // T1 MF-1 signature-anchored content check (hardening
                // `ab533eae` CRITICAL). `verify()` proves only that the SIGNATURE
                // is valid over `challenge_bytes`; the strict-equality drift check
                // above compares the PERSISTED COLUMNS, not the signed bytes. Those
                // are two distinct representations — an attacker with watch.db write
                // (the laptop-owning agent MF-1 exists to stop) can leave the signed
                // bytes intact and mutate ONLY the columns, desyncing what was tapped
                // from what gets armed: the drift check then passes (columns vs
                // ambient) AND verify passes (sig over untouched bytes), yet the tap
                // authorized different content. Close it: parse the AUTHENTICATED
                // bytes and assert their embedded content EQUALS the persisted
                // columns (and the signed format version is >= 2). The signature now
                // provably covers the exact content this confirm acts on. Parse
                // failure or any mismatch → fail-closed reject.
                let signed: crate::watch::attest::ArmConfirmChallenge =
                    match serde_json::from_slice(&challenge_bytes) {
                        Ok(c) => c,
                        Err(_) => {
                            return Ok(ArmConfirmTxOutcome::Rejected {
                                reason: "challenge_unparseable".to_string(),
                            })
                        }
                    };
                if signed.v < 3
                    || signed.build_id != staged_build_id
                    || signed.enabled_surface != staged_enabled_surface
                    || signed.effective_daily_cap_cents != staged_cap_cents
                    || signed.tenant != staged_tenant
                {
                    return Ok(ArmConfirmTxOutcome::Rejected {
                        reason: "signed_content_mismatch".to_string(),
                    });
                }

                // HIGH (spend-window split-brain): the active_arm spend deadline
                // is computed from the SIGNED tap time (`signed.iat_ms`) + the
                // SIGNED window (`signed.spend_window_ms`, Attested-arm) — the SAME
                // formula the reserve uses to gate. One source of truth: confirm
                // stamps it, reserve recomputes it and tripwires the column
                // against it. NOT a caller-passed value and NOT a live env read.
                // The short ceremony TTL (`exp_at_ms <= now_ms` check above) stays
                // the tap-by deadline; this is the separate 24h spend horizon.
                //
                // Attested-arm: the window is now SIGNED, so a post-tap
                // GW_ARM_WINDOW_MS restart cannot extend it (the signed value in
                // this already-signed challenge does not change; a larger window
                // needs a fresh tap). The reserve reads the SAME flag, so the
                // column it stamps here matches the deadline the reserve recomputes.
                // Named rollback GW_ARM_SIGNED_WINDOW=false → legacy boot-locked
                // window, no redeploy.
                //
                // checked_add (grok-4.3 HIGH): a pathological signed `iat_ms`
                // near i64::MAX must not wrap to a garbage exp. Overflow → refuse
                // the confirm (do NOT write active_arm with a corrupt deadline).
                let confirm_window_ms = if crate::watch::db::signed_spend_window_enabled() {
                    signed.spend_window_ms
                } else {
                    crate::watch::db::arm_window_ms_bootlocked()
                };
                let Some(active_arm_exp_at_ms) = signed.iat_ms.checked_add(confirm_window_ms)
                else {
                    return Ok(ArmConfirmTxOutcome::Rejected {
                        reason: "spend_deadline_overflow".to_string(),
                    });
                };

                // Counter enforcement keys on credential_type ONLY — se-p256
                // records 0 and NEVER reads this table (1aba8e1d-445 action 6).
                // FIDO2/WebAuthn §7.1 step 17 treats counter=0 as "no global
                // counter"; skip both the check and write so stateless tokens
                // do not store 0 and self-lock on their next ceremony.
                if verified.credential_type == "fido2-es256" && verified.sig_counter > 0 {
                    let last: Option<i64> = tx
                        .query_row(
                            "SELECT last_counter FROM arm_attest_counters WHERE credential_id = ?1",
                            rusqlite::params![verified.credential_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if let Some(last) = last {
                        if i64::from(verified.sig_counter) <= last {
                            return Ok(ArmConfirmTxOutcome::Rejected {
                                reason: "counter_regression".to_string(),
                            });
                        }
                    }
                    tx.execute(
                        "INSERT INTO arm_attest_counters (credential_id, last_counter)
                         VALUES (?1, ?2)
                         ON CONFLICT(credential_id) DO UPDATE SET last_counter = excluded.last_counter",
                        rusqlite::params![verified.credential_id, i64::from(verified.sig_counter)],
                    )?;
                }

                // §6 audit binding — detail is JSON; the RIDER D deviation
                // suffix rides inside it as a field so the detail stays
                // machine-parseable.
                // A confirm goes effectively-rehearsal if the row was staged as a
                // rehearsal OR this build may not arm for real (B6). Folding the
                // dirty-build veto in HERE — inside the same tx that writes the
                // chain — keeps the unprunable audit honest (a build-vetoed
                // ceremony records `confirm_rehearsal` + `dark_reason`, never a
                // `confirm` it didn't perform) and routes it to the single
                // producer-never-starts path via the returned flag.
                let effective_rehearsal = rehearsal || !allow_real_arm;
                use sha2::Digest as _;
                let mut detail_json = serde_json::json!({
                    "mechanism": "local-attest",
                    "credential_id": verified.credential_id,
                    "credential_type": verified.credential_type,
                    "sig_counter": verified.sig_counter,
                    "challenge_sha256": hex::encode(Sha256::digest(&challenge_bytes)),
                    "stage_id": stage_id,
                    "staged_by": staged_by,
                });
                if effective_rehearsal {
                    detail_json["rehearsal"] = serde_json::Value::Bool(true);
                }
                // Distinguish a build-veto (dirty/unidentifiable build forced to
                // DARK) from an operator-requested rehearsal — both are
                // `confirm_rehearsal`, but only the veto carries `dark_reason`.
                if !allow_real_arm && !rehearsal {
                    detail_json["dark_reason"] =
                        serde_json::Value::String("build_not_real_arm_capable".to_string());
                }
                if !deviation_suffix.trim().is_empty() {
                    detail_json["deviation"] =
                        serde_json::Value::String(deviation_suffix.trim().to_string());
                }
                let detail = detail_json.to_string();

                let at_ms = now_ms;
                // B7 (spec §8): a rehearsal stage commits a 'confirm_rehearsal'
                // row — same chain, same §6 binding — and the handler never
                // starts the producer for it. The ROW decided, not the request.
                let action = if effective_rehearsal {
                    "confirm_rehearsal"
                } else {
                    "confirm"
                };
                let audit_id =
                    append_arm_audit_in_tx(&tx, at_ms, action, &principal, Some(&detail))?;
                // Attested-arm (P0-3) — consume the pending row. The active_arm write
                // below is guarded so the pending-consume AND the ceiling write
                // are atomic on a REAL confirm: a partial commit (one without the
                // other) would either spend with no ceiling or strand a stale
                // ceiling, so we roll back unless the expected statement count
                // changed.
                let pending_deleted = tx.execute(
                    "DELETE FROM arm_pending WHERE stage_id = ?1",
                    rusqlite::params![stage_id],
                )?;
                if !effective_rehearsal {
                    // B1 — write the attested spend ceiling in the SAME tx that
                    // consumes the pending row. The persisted columns equal
                    // `expected_content` (verified by the strict-equality drift
                    // check above) and the signed bytes (signed_content_mismatch
                    // check), so a REAL confirm's ceiling provably matches the
                    // tapped intent. P1-4 monotonic: ON CONFLICT overwrites ONLY
                    // when the new epoch is strictly greater — an equal/lower
                    // epoch confirm cannot silently lower or replace a live or
                    // newer arm (its `changes()` is 0, caught below).
                    let challenge_sha256 = hex::encode(Sha256::digest(&challenge_bytes));
                    // Signed-material invariant: persist the signed material (verbatim
                    // challenge bytes + DER signature + credential identity) so
                    // the reserve re-verifies the signature at spend time. The
                    // challenge_bytes are NOT lost with the arm_pending DELETE —
                    // they live here for the lifetime of the active arm.
                    let arm_written = tx.execute(
                        "INSERT INTO active_arm
                           (id, build_id, enabled_surface, effective_daily_cap_cents,
                            tenant, armed_epoch, exp_at_ms, challenge_sha256, audit_id,
                            challenge_bytes, signature_der, credential_id, credential_type,
                            authenticator_data, client_data_json)
                         VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                         ON CONFLICT(id) DO UPDATE SET
                           build_id=excluded.build_id,
                           enabled_surface=excluded.enabled_surface,
                           effective_daily_cap_cents=excluded.effective_daily_cap_cents,
                           tenant=excluded.tenant,
                           armed_epoch=excluded.armed_epoch,
                           exp_at_ms=excluded.exp_at_ms,
                           challenge_sha256=excluded.challenge_sha256,
                           audit_id=excluded.audit_id,
                           challenge_bytes=excluded.challenge_bytes,
                           signature_der=excluded.signature_der,
                           credential_id=excluded.credential_id,
                           credential_type=excluded.credential_type,
                           authenticator_data=excluded.authenticator_data,
                           client_data_json=excluded.client_data_json
                         WHERE excluded.armed_epoch > active_arm.armed_epoch",
                        rusqlite::params![
                            staged_build_id,
                            staged_enabled_surface,
                            staged_cap_cents,
                            staged_tenant,
                            armed_epoch,
                            active_arm_exp_at_ms,
                            challenge_sha256,
                            audit_id,
                            challenge_bytes,
                            signed_material.signature_der,
                            signed_material.credential_id,
                            signed_material.credential_type,
                            signed_material.authenticator_data,
                            signed_material.client_data_json,
                        ],
                    )?;
                    // P0-3 atomicity intent: BOTH the pending-consume and the
                    // active_arm write must have landed. If the active_arm write
                    // silently no-op'd (the monotonic guard rejected an
                    // equal/lower epoch overwrite of a live arm), this confirm
                    // would have armed the producer with NO fresh ceiling — a
                    // re-arm that lowers/repeats an epoch is a caller error
                    // (epoch must strictly increase). Fail closed: roll back the
                    // whole confirm rather than leave the producer authorized
                    // against a stale/foreign ceiling.
                    if pending_deleted != 1 || arm_written != 1 {
                        tx.rollback()?;
                        return Ok(ArmConfirmTxOutcome::Rejected {
                            reason: "active_arm_not_written".to_string(),
                        });
                    }
                }
                tx.commit()?;
                Ok(ArmConfirmTxOutcome::Verified {
                    staged_by,
                    audit_id,
                    detail,
                    rehearsal: effective_rehearsal,
                })
            })
            .await
            .map_err(Into::into)
    }

    /// Attested-arm (B1) — read the attested-arm singleton, if any. The ceiling the
    /// reserve enforces; ops/debug surface for "is real spend authorized, and up
    /// to how much / under which generation". Returns None when never armed or
    /// after disarm.
    pub async fn get_active_arm(&self) -> anyhow::Result<Option<ActiveArmRow>> {
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT build_id, enabled_surface, effective_daily_cap_cents,
                            tenant, armed_epoch, exp_at_ms, challenge_sha256, audit_id
                     FROM active_arm WHERE id = 0",
                    [],
                    |r| {
                        Ok(ActiveArmRow {
                            build_id: r.get(0)?,
                            enabled_surface: r.get(1)?,
                            effective_daily_cap_cents: r.get(2)?,
                            tenant: r.get(3)?,
                            armed_epoch: r.get(4)?,
                            exp_at_ms: r.get(5)?,
                            challenge_sha256: r.get(6)?,
                            audit_id: r.get(7)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(Into::into)
    }

    /// Attested-arm — TEST-ONLY direct active_arm upsert with REAL signed material.
    /// Lets reserve-focused tests stamp a ceiling whose signature the reserve
    /// can actually re-verify (against a registry the test publishes/injects),
    /// without re-running the full HTTP confirm handler. Gated behind
    /// `#[cfg(any(test, feature = "test-helpers"))]` (MED finding) so it is NOT
    /// compiled into the production binary; the integration-test crate enables
    /// the feature via the self dev-dependency. Uses the SAME monotonic ON
    /// CONFLICT guard as the real confirm write. `column_*` overrides exist ONLY
    /// so a test can DESYNC the columns from the signed bytes (to prove the
    /// reserve's signed-content assertion catches a forged column); pass `None`
    /// to mirror the signed values.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-helpers"))]
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_active_arm_for_test(
        &self,
        column_build_id: &str,
        column_enabled_surface: &str,
        column_cap_cents: i64,
        column_tenant: &str,
        armed_epoch: i64,
        exp_at_ms: i64,
        challenge_bytes: Vec<u8>,
        signature_der: Vec<u8>,
        credential_id: &str,
        credential_type: &str,
    ) -> anyhow::Result<u64> {
        use sha2::Digest as _;
        let column_build_id = column_build_id.to_string();
        let column_enabled_surface = column_enabled_surface.to_string();
        let column_tenant = column_tenant.to_string();
        let credential_id = credential_id.to_string();
        let credential_type = credential_type.to_string();
        let challenge_sha256 = hex::encode(sha2::Sha256::digest(&challenge_bytes));
        self.conn
            .call(move |conn| {
                let changed = conn.execute(
                    "INSERT INTO active_arm
                       (id, build_id, enabled_surface, effective_daily_cap_cents,
                        tenant, armed_epoch, exp_at_ms, challenge_sha256, audit_id,
                        challenge_bytes, signature_der, credential_id, credential_type,
                        authenticator_data, client_data_json)
                     VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, NULL, NULL)
                     ON CONFLICT(id) DO UPDATE SET
                       build_id=excluded.build_id,
                       enabled_surface=excluded.enabled_surface,
                       effective_daily_cap_cents=excluded.effective_daily_cap_cents,
                       tenant=excluded.tenant,
                       armed_epoch=excluded.armed_epoch,
                       exp_at_ms=excluded.exp_at_ms,
                       challenge_sha256=excluded.challenge_sha256,
                       challenge_bytes=excluded.challenge_bytes,
                       signature_der=excluded.signature_der,
                       credential_id=excluded.credential_id,
                       credential_type=excluded.credential_type
                     WHERE excluded.armed_epoch > active_arm.armed_epoch",
                    rusqlite::params![
                        column_build_id,
                        column_enabled_surface,
                        column_cap_cents,
                        column_tenant,
                        armed_epoch,
                        exp_at_ms,
                        challenge_sha256,
                        challenge_bytes,
                        signature_der,
                        credential_id,
                        credential_type,
                    ],
                )?;
                Ok::<u64, rusqlite::Error>(changed as u64)
            })
            .await
            .map_err(Into::into)
    }

    /// dual-custody-local-attest B1 — read the open pending stage, if any.
    /// Returns None when no row exists OR the row is past its wall-clock
    /// expiry (`exp_at_ms <= now_ms`) — an expired row is dead, never served
    /// and never rehydrated. `now_ms` is a parameter (not read inside) so
    /// tests can simulate clock advance, same injection pattern as
    /// `try_acquire_writer_claim`.
    pub async fn get_arm_pending(&self, now_ms: i64) -> anyhow::Result<Option<ArmPendingRow>> {
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT stage_id, staged_by, challenge_bytes, exp_at_ms, rehearsal,
                            build_id, enabled_surface, effective_daily_cap_cents, tenant,
                            challenge_format_version
                     FROM arm_pending WHERE exp_at_ms > ?1 LIMIT 1",
                    rusqlite::params![now_ms],
                    |r| {
                        Ok(ArmPendingRow {
                            stage_id: r.get(0)?,
                            staged_by: r.get(1)?,
                            challenge_bytes: r.get(2)?,
                            exp_at_ms: r.get(3)?,
                            rehearsal: r.get::<_, i64>(4)? != 0,
                            build_id: r.get(5)?,
                            enabled_surface: r.get(6)?,
                            effective_daily_cap_cents: r.get(7)?,
                            tenant: r.get(8)?,
                            challenge_format_version: r.get(9)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(Into::into)
    }

    /// dual-custody-local-attest B1 — clear the pending stage (confirm
    /// consumed it, it expired, or the operator disarmed). `stage_id: Some`
    /// is a fenced delete (only that exact stage's row dies — a concurrently
    /// re-staged ceremony's row survives, mirroring the conditional clear of
    /// the in-memory slot); `None` clears unconditionally (disarm). Returns
    /// true when a row was deleted.
    pub async fn clear_arm_pending(&self, stage_id: Option<&str>) -> anyhow::Result<bool> {
        let stage_id = stage_id.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                let affected = match stage_id {
                    Some(sid) => conn.execute(
                        "DELETE FROM arm_pending WHERE stage_id = ?1",
                        rusqlite::params![sid],
                    )?,
                    None => {
                        // Attested-arm (P0-3) — disarm is the SINGLE active_arm DELETE
                        // site. "May spend now" has exactly one source of truth,
                        // so the unconditional pending clear (the kill switch /
                        // disarm path) must drop the attested ceiling in the SAME
                        // tx — after this returns, the reserve fails closed (no
                        // active_arm row) until a fresh confirm. A fenced
                        // `Some(stage_id)` clear is a per-ceremony cleanup (a
                        // superseded stage), NOT a disarm, so it leaves active_arm
                        // untouched: a live arm of a DIFFERENT epoch must survive
                        // a stale stage being swept.
                        let tx = conn
                            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                        let pending = tx.execute("DELETE FROM arm_pending", [])?;
                        tx.execute("DELETE FROM active_arm", [])?;
                        tx.commit()?;
                        pending
                    }
                };
                Ok::<bool, rusqlite::Error>(affected > 0)
            })
            .await
            .map_err(Into::into)
    }

    /// p0a-four-eyes — full ascending read of the arming-ceremony audit
    /// chain. The arming ceremony is rare (human-gated), so an unbounded
    /// read is fine; revisit with pagination only if this ever grows hot.
    pub async fn list_arm_audit(&self) -> anyhow::Result<Vec<ArmAuditRow>> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, at_ms, action, principal, detail, prev_hash, hash
                     FROM arm_audit ORDER BY id ASC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(ArmAuditRow {
                        id: r.get(0)?,
                        at_ms: r.get(1)?,
                        action: r.get(2)?,
                        principal: r.get(3)?,
                        detail: r.get(4)?,
                        prev_hash: r.get(5)?,
                        hash: r.get(6)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(Into::into)
    }
}
