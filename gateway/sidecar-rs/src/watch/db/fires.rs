//! Watch fire hash-chain: preimage, insert, verify, and fire reads.

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use super::{watch_distinct_genesis, WatchDb};

/// Length-prefixed preimage for a row in the watch audit chain (`watch_fires`).
/// Used by both Phase 2 sentinel fires and Phase 3 directive lifecycle events.
/// Must stay in sync with the reference encoder
/// `tests/preimage_vectors.py:build_watch_preimage` (v3 base) /
/// `build_watch_preimage_v4` (v4 envelope append).
///
/// Length-prefixed preimage for a `watch_fires` row, version-tagged (W3 item 3).
///
/// `preimage_version` is the SELECTOR — it is NEVER itself a hashed field (that
/// would be circular). It chooses the field set:
/// * **v3** (`envelope_json = None`) — the original 6 fields. Byte-for-byte
///   identical to the pre-W3 preimage, so the 8 legacy canary rows (backfilled
///   to `preimage_version = 3`) keep verifying with no rewrite.
/// * **v4** (`envelope_json = Some(bytes)`) — v3's 6 fields, then the VERBATIM
///   stored `envelope_json` bytes APPENDED AT END, length-prefixed. Because it
///   is appended, a v4 preimage over an empty envelope (`0:`) is NOT equal to a
///   v3 preimage — the version tag, not byte-equality, is the discriminator.
///   The envelope is hashed exactly as stored (insert writes JCS-canonical;
///   verify never re-canonicalizes), so an UPDATE to `envelope_json` flips the
///   recomputed hash and `verify_chain` reports the break.
pub(crate) fn compute_watch_fire_preimage(
    tenant: &str,
    sentinel: &str,
    fired_at_ms: i64,
    state_json: &str,
    reason: &str,
    prev_hash: &str,
    envelope_json: Option<&str>,
) -> String {
    let fired_at_str = fired_at_ms.to_string();
    let base = format!(
        "{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}",
        tenant.len(),
        tenant,
        sentinel.len(),
        sentinel,
        fired_at_str.len(),
        fired_at_str,
        state_json.len(),
        state_json,
        reason.len(),
        reason,
        prev_hash.len(),
        prev_hash
    );
    match envelope_json {
        // v3: 6-field preimage, unchanged from the original scheme.
        None => base,
        // v4: append the verbatim envelope bytes, length-prefixed. NULL/empty
        // envelope encodes as `0:` (still distinct from v3 by the version tag).
        Some(env) => format!("{}|{}:{}", base, env.len(), env),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken_at_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found_hash: Option<String>,
    pub rows_walked: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_kind: Option<VerifyBreak>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyBreak {
    /// A row's prev_hash did not equal the previous row's stored hash
    /// (or the distinct genesis, for the first row).
    PrevHashMismatch,
    /// A row's stored hash did not equal the hash recomputed from its
    /// length-prefixed v3 preimage. Tampering or corruption.
    HashMismatch,
}

#[derive(Debug, Clone)]
pub struct FireRow {
    pub id: i64,
    pub tenant: String,
    pub sentinel: String,
    pub fired_at: i64,
    pub state_json: String,
    pub reason: String,
    pub prev_hash: String,
    pub hash: String,
    pub envelope_json: String,
}

/// Phase 1 CDC sweep candidate (committed watch_fires row). Caller computes causal_fire_id
/// from state_json (contains observed_at + sentinel payload per SentinelState) + tenant/sentinel,
/// then checks dedup before enqueue. Bounded query + app filter keeps sweep cheap (v0.1).
#[derive(Debug, Clone)]
pub struct CommittedFire {
    pub id: i64,
    pub tenant: String,
    pub sentinel: String,
    pub fired_at_ms: i64,
    pub state_json: String,
    pub envelope_json: String,
}

impl WatchDb {
    /// Insert a fire-row under BEGIN IMMEDIATE with OCC.
    /// Returns:
    ///   Ok(Some(id)) — row inserted, id is the new rowid.
    ///   Ok(None)     — sentinel is hard-killed (OCC race): row dropped.
    ///                  This is not an error condition; the runtime's
    ///                  is_blocked() check normally catches hard-kill
    ///                  earlier, but a race between that check and the
    ///                  insert is possible. Distinct genesis ensures the
    ///                  chain is still inspectable post-drop.
    ///   Err(e)       — SQLite-level failure.
    #[allow(clippy::too_many_arguments)] // stable audit-write API; collapsing args is a wire-compat change.
    pub async fn insert_fire(
        &self,
        tenant: &str,
        sentinel: &str,
        fired_at_ms: i64,
        state_json: &str,
        reason: &str,
        envelope_json: &str,
        envelope_schema_version: i64,
    ) -> anyhow::Result<Option<i64>> {
        let tenant = tenant.to_string();
        let sentinel = sentinel.to_string();
        let state_json = state_json.to_string();
        let reason = reason.to_string();
        let envelope_json = envelope_json.to_string();
        let distinct_gen = watch_distinct_genesis();

        let id = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // OCC: check hard-killed inside same tx. Read the column as
                // Option<i64> so a NULL hard_killed_at (registry-inserted
                // row, not yet hard-killed) doesn't panic the row decode.
                let hard_killed: Option<i64> = tx
                    .query_row(
                        "SELECT hard_killed_at FROM watch_sentinels
                         WHERE tenant=?1 AND name=?2",
                        rusqlite::params![tenant, sentinel],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten();

                if hard_killed.is_some() {
                    return Ok::<Option<i64>, rusqlite::Error>(None);
                }

                // Read prev_hash inside the tx — never cached.
                let prev_hash: String = tx
                    .query_row(
                        "SELECT hash FROM watch_fires
                         WHERE tenant=?1 ORDER BY id DESC LIMIT 1",
                        rusqlite::params![tenant],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(distinct_gen);

                // Compute this row's hash using the length-prefixed v4 preimage
                // (envelope_json hashed VERBATIM, appended at end). T_NEW3 proves
                // this scheme is collision-resistant under adversarial
                // `|:\n\t\0` injection. New rows are ALWAYS v4 — the version is
                // bound explicitly below, never left to the column DEFAULT.
                let preimage = compute_watch_fire_preimage(
                    &tenant,
                    &sentinel,
                    fired_at_ms,
                    &state_json,
                    &reason,
                    &prev_hash,
                    Some(&envelope_json),
                );
                let hash = hex::encode(Sha256::digest(preimage.as_bytes()));

                tx.execute(
                    "INSERT INTO watch_fires (tenant, sentinel, fired_at, state_json,
                        reason, prev_hash, hash, envelope_json, envelope_schema_version,
                        preimage_version)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        tenant,
                        sentinel,
                        fired_at_ms,
                        state_json,
                        reason,
                        prev_hash,
                        hash,
                        envelope_json,
                        envelope_schema_version,
                        4i64, // W3: new rows are v4 (envelope_json in preimage). Explicit, never the DEFAULT.
                    ],
                )?;

                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok::<Option<i64>, rusqlite::Error>(Some(id))
            })
            .await?;

        Ok(id)
    }

    /// Write a Phase 3 watch audit event (escalation_recovered_resume_outbox,
    /// directive_staged, outbox_recovered_from_restart, ...) into the watch
    /// audit chain (`watch_fires`) using the exact same length-prefixed v3
    /// preimage + SHA-256 hash chaining as Phase 2 sentinel fires.
    ///
    /// This makes the events immediately visible via `GET /watch/audit/{tenant}`
    /// and subject to `POST /watch/verify-chain/{tenant}`.
    ///
    /// Uses the Phase 2 preimage path (factored via `compute_watch_fire_preimage`).
    /// No per-sentinel hard-kill OCC check (these are system/orchestrator events).
    pub async fn write_phase3_audit_event(
        &self,
        tenant: &str,
        sentinel: &str,
        fired_at_ms: i64,
        event: &crate::watch::dispatcher::WatchPhase3AuditEvent,
    ) -> anyhow::Result<i64> {
        let tenant = tenant.to_string();
        let sentinel = sentinel.to_string();
        let state_json = event.to_state_json();
        let reason = event.reason();
        let envelope_json = serde_json::to_string(&serde_json::json!({
            "phase3_event": event
        }))?;
        let distinct_gen = watch_distinct_genesis();

        let id = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // Read prev_hash for this tenant (same logic as normal fires)
                let prev_hash: String = tx
                    .query_row(
                        "SELECT hash FROM watch_fires
                         WHERE tenant=?1 ORDER BY id DESC LIMIT 1",
                        rusqlite::params![tenant],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(distinct_gen);

                let preimage = compute_watch_fire_preimage(
                    &tenant,
                    &sentinel,
                    fired_at_ms,
                    &state_json,
                    &reason,
                    &prev_hash,
                    Some(&envelope_json), // W3: v4 — envelope hashed verbatim.
                );
                let hash = hex::encode(Sha256::digest(preimage.as_bytes()));

                tx.execute(
                    "INSERT INTO watch_fires (tenant, sentinel, fired_at, state_json,
                        reason, prev_hash, hash, envelope_json, envelope_schema_version,
                        preimage_version)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        tenant,
                        sentinel,
                        fired_at_ms,
                        state_json,
                        reason,
                        prev_hash,
                        hash,
                        envelope_json,
                        3i64, // Phase 3 envelope schema (envelope_schema_version)
                        4i64, // W3 preimage_version — explicit v4, never the DEFAULT.
                    ],
                )?;

                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok::<i64, rusqlite::Error>(id)
            })
            .await?;

        Ok(id)
    }

    /// Result of `verify_chain`. Forensic fields populated on break.
    /// Pattern: `git fsck`.
    /// Walks the per-tenant hash chain and reports the first break (if any).
    /// The dispatcher trusts this chain for routing decisions; a chain
    /// without a verifier is the "checksum
    /// without checker" anti-pattern.
    ///
    /// This chain must remain contiguous. Never delete, prune, or compact
    /// watch_fires rows, as it will break this verification.
    ///
    /// Checks three invariants in walk order:
    ///   1. First row's prev_hash equals the distinct genesis.
    ///   2. Every subsequent row's prev_hash equals the previous row's hash.
    ///   3. Each row's stored hash matches the recomputed hash from its
    ///      length-prefixed v3 preimage.
    ///
    /// First break short-circuits and is reported via VerifyResult.
    pub async fn verify_chain(&self, tenant: &str) -> anyhow::Result<VerifyResult> {
        let tenant = tenant.to_string();
        let distinct_gen = watch_distinct_genesis();

        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, sentinel, fired_at, state_json, reason, prev_hash, hash,
                            envelope_json, preimage_version
                     FROM watch_fires WHERE tenant=?1 ORDER BY id ASC",
                )?;
                let mut rows = stmt.query(rusqlite::params![tenant])?;

                let mut walked: i64 = 0;
                let mut prev_row_hash: Option<String> = None;

                while let Some(row) = rows.next()? {
                    let id: i64 = row.get(0)?;
                    let sentinel: String = row.get(1)?;
                    let fired_at: i64 = row.get(2)?;
                    let state_json: String = row.get(3)?;
                    let reason: String = row.get(4)?;
                    let prev_hash: String = row.get(5)?;
                    let stored_hash: String = row.get(6)?;
                    let envelope_json: String = row.get(7)?;
                    let preimage_version: i64 = row.get(8)?;

                    // Invariant 1+2: prev_hash continuity.
                    let expected_prev = prev_row_hash.as_ref().unwrap_or(&distinct_gen);
                    if &prev_hash != expected_prev {
                        return Ok::<VerifyResult, rusqlite::Error>(VerifyResult {
                            ok: false,
                            broken_at_id: Some(id),
                            expected_hash: Some(expected_prev.clone()),
                            found_hash: Some(prev_hash),
                            rows_walked: walked,
                            break_kind: Some(VerifyBreak::PrevHashMismatch),
                        });
                    }

                    // Invariant 3: recomputed hash matches stored hash. W3:
                    // dispatch the preimage scheme on the row's version selector —
                    // v3 (legacy) omits envelope_json; v4 hashes it verbatim.
                    // Any version other than 3/4 is an unknown-scheme break (a
                    // forged/corrupt row, or a future scheme this binary predates).
                    let env_arg = match preimage_version {
                        3 => None,
                        4 => Some(envelope_json.as_str()),
                        _ => {
                            return Ok(VerifyResult {
                                ok: false,
                                broken_at_id: Some(id),
                                expected_hash: None,
                                found_hash: Some(stored_hash),
                                rows_walked: walked,
                                break_kind: Some(VerifyBreak::HashMismatch),
                            });
                        }
                    };
                    let preimage = compute_watch_fire_preimage(
                        &tenant,
                        &sentinel,
                        fired_at,
                        &state_json,
                        &reason,
                        &prev_hash,
                        env_arg,
                    );
                    let recomputed = hex::encode(Sha256::digest(preimage.as_bytes()));
                    if recomputed != stored_hash {
                        return Ok(VerifyResult {
                            ok: false,
                            broken_at_id: Some(id),
                            expected_hash: Some(recomputed),
                            found_hash: Some(stored_hash),
                            rows_walked: walked,
                            break_kind: Some(VerifyBreak::HashMismatch),
                        });
                    }

                    prev_row_hash = Some(stored_hash);
                    walked += 1;
                }

                Ok(VerifyResult {
                    ok: true,
                    broken_at_id: None,
                    expected_hash: None,
                    found_hash: None,
                    rows_walked: walked,
                    break_kind: None,
                })
            })
            .await?;
        Ok(result)
    }

    /// T28 — count fires within a sliding window (used by temperature).
    pub async fn count_fires_since(&self, tenant: &str, since_ms: i64) -> anyhow::Result<i64> {
        let tenant_owned = tenant.to_string();
        let n = self
            .conn
            .call(move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM watch_fires
                     WHERE tenant = ?1 AND fired_at > ?2",
                    rusqlite::params![tenant_owned, since_ms],
                    |r| r.get(0),
                )?;
                Ok::<i64, rusqlite::Error>(n)
            })
            .await?;
        Ok(n)
    }

    /// T29 — descending fire log for `/watch/audit/{tenant}` cursor pagination.
    /// Cursor: `before_id = None` → newest first; otherwise rows with id < before_id.
    pub async fn list_fires_descending(
        &self,
        tenant: &str,
        limit: i64,
        before_id: Option<i64>,
    ) -> anyhow::Result<Vec<FireRow>> {
        let tenant_owned = tenant.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let before = before_id.unwrap_or(i64::MAX);
                let mut stmt = conn.prepare(
                    "SELECT id, tenant, sentinel, fired_at, state_json, reason,
                        prev_hash, hash, envelope_json
                     FROM watch_fires
                     WHERE tenant=?1 AND id < ?2
                     ORDER BY id DESC
                     LIMIT ?3",
                )?;
                let rows: Vec<FireRow> = stmt
                    .query_map(rusqlite::params![tenant_owned, before, limit], |r| {
                        Ok(FireRow {
                            id: r.get(0)?,
                            tenant: r.get(1)?,
                            sentinel: r.get(2)?,
                            fired_at: r.get(3)?,
                            state_json: r.get(4)?,
                            reason: r.get(5)?,
                            prev_hash: r.get(6)?,
                            hash: r.get(7)?,
                            envelope_json: r.get(8)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<Vec<FireRow>, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }

    /// T30 — single-row lookup by `id`. Used by the force-wake handler to
    /// echo back the hash + fired_at of the row it just inserted. Returns
    /// `Ok(None)` if the row does not exist (cursor races, deletion paths).
    pub async fn fetch_fire_by_id(&self, id: i64) -> anyhow::Result<Option<FireRow>> {
        let row = self
            .conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT id, tenant, sentinel, fired_at, state_json, reason,
                        prev_hash, hash, envelope_json
                     FROM watch_fires WHERE id=?1",
                    rusqlite::params![id],
                    |r| {
                        Ok(FireRow {
                            id: r.get(0)?,
                            tenant: r.get(1)?,
                            sentinel: r.get(2)?,
                            fired_at: r.get(3)?,
                            state_json: r.get(4)?,
                            reason: r.get(5)?,
                            prev_hash: r.get(6)?,
                            hash: r.get(7)?,
                            envelope_json: r.get(8)?,
                        })
                    },
                )
                .optional()
            })
            .await?;
        Ok(row)
    }

    /// Walk a tenant's chain ascending. Cursor pagination via `after_id`
    /// (None starts from the genesis link). `limit` caps the page size.
    pub async fn list_fires_ascending(
        &self,
        tenant: &str,
        limit: i64,
        after_id: Option<i64>,
    ) -> anyhow::Result<Vec<FireRow>> {
        let tenant = tenant.to_string();
        let rows = self
            .conn
            .call(move |conn| {
                let after = after_id.unwrap_or(0);
                let mut stmt = conn.prepare(
                    "SELECT id, tenant, sentinel, fired_at, state_json, reason,
                        prev_hash, hash, envelope_json
                     FROM watch_fires
                     WHERE tenant=?1 AND id > ?2
                     ORDER BY id ASC
                     LIMIT ?3",
                )?;
                let rows: Vec<FireRow> = stmt
                    .query_map(rusqlite::params![tenant, after, limit], |r| {
                        Ok(FireRow {
                            id: r.get(0)?,
                            tenant: r.get(1)?,
                            sentinel: r.get(2)?,
                            fired_at: r.get(3)?,
                            state_json: r.get(4)?,
                            reason: r.get(5)?,
                            prev_hash: r.get(6)?,
                            hash: r.get(7)?,
                            envelope_json: r.get(8)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<Vec<FireRow>, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }

    // ---------------------------------------------------------------------
    // P1 outbox surface (read + admin ack) — tenant-scoped only.
    // These methods are the data seam for the REST handlers in api.rs.
    // They never touch dispatcher state machine or proposal validation.
    // ---------------------------------------------------------------------

    /// Bounded committed fires for CDC sweep / boot re-scan.
    /// When `after_id` is Some, returns rows with id > after_id ordered ASC (cursor advancement,
    /// prevents starvation of older fires once the head of the table is dense with processed rows).
    /// When None (first sweep on cold start / boot re-scan), uses ASC from oldest to reach backlog
    /// older than head-200 before any cursor advancement (satisfies design §2/§4 + plan §5 Step 2
    /// "no matching pending" recovery for un-enqueued committed fires). Subsequent ticks use the
    /// after_id HWM. Tenant isolation: global but cheap (limit); caller or future per-tenant HWM
    /// cursors (design §4) can further filter. No lock on watch_fires writer.
    /// Excludes `watch-dispatcher`, the synthetic sentinel used for Phase 3 lifecycle audit rows;
    /// recovery/outbox evidence must not feed back into new producer escalations.
    /// Matches phase1-producer-seam.md "no matching pending row" / HWM cursor intent (v0.1 in-memory
    /// advancement here; persisted cursor table is additive and Allowed for follow-on).
    pub async fn get_recent_committed_fires(
        &self,
        limit: i64,
        after_id: Option<i64>,
    ) -> anyhow::Result<Vec<CommittedFire>> {
        self.conn
            .call(move |conn| {
                let (sql, params): (String, Vec<i64>) = if let Some(after) = after_id {
                    (
                        "SELECT id, tenant, sentinel, fired_at, state_json, envelope_json
                         FROM watch_fires
                         WHERE id > ?1
                           AND sentinel != 'watch-dispatcher'
                         ORDER BY id ASC LIMIT ?2"
                            .to_string(),
                        vec![after, limit],
                    )
                } else {
                    (
                        "SELECT id, tenant, sentinel, fired_at, state_json, envelope_json
                         FROM watch_fires
                         WHERE sentinel != 'watch-dispatcher'
                         ORDER BY id ASC LIMIT ?1"
                            .to_string(),
                        vec![limit],
                    )
                };
                let mut stmt = conn.prepare(&sql)?;
                let it = stmt.query_map(rusqlite::params_from_iter(params), |r| {
                    Ok(CommittedFire {
                        id: r.get(0)?,
                        tenant: r.get(1)?,
                        sentinel: r.get(2)?,
                        fired_at_ms: r.get(3)?,
                        state_json: r.get(4)?,
                        envelope_json: r.get(5)?,
                    })
                })?;
                let rows: Vec<CommittedFire> = it.collect::<Result<_, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }
}
