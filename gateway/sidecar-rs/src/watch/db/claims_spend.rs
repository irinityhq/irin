//! Deliberation claims, council spend ledger, and writer-claim liveness.

use rand_core::{OsRng, RngCore};
use rusqlite::OptionalExtension;

use super::{
    daily_spend_cap, lease_duration_ms, max_fanout_cost_usd, read_active_arm_row, utc_day_bucket,
    PhantomSweepReport, ReconAlarmRow, RenewOutcome, SettleReport, WatchDb,
};

/// A pending escalation row claimed by the live dispatcher for council-triage.
/// Only 'queued' or 'failed' rows are eligible for live claim (Phase 3b.1).
#[derive(Debug, Clone)]
pub struct PendingClaim {
    pub id: String,
    pub tenant: String,
    pub envelope_json: String,
    pub attempts: i64,
    pub sentinel_name: String,
    pub replay_epoch: i64,
    pub claim_token: String, // true fencing token per invariant
    /// true when this claim RECLAIMED a lease-expired
    /// 'claimed' row that was a REAL in-flight claim (non-empty prior
    /// claim_token, attempts > 0): the previous holder's council call may
    /// have already incurred provider spend. The dispatcher treats this as
    /// the same orphan-charge recon hint as a sweep reclaim (counter +
    /// RECON HINT warn) — claim_next is the dominant reclaim path in
    /// production (1s tick vs the 75s phantom sweep), so without this flag
    /// the lease_expired_during_deliberation telemetry undercounts.
    pub reclaimed_in_flight: bool,
}

impl WatchDb {
    /// Returns (directive_outbox row count, pending_escalations row count).
    /// Used by DirectiveSigningKey for DB-witness post-init detection (D15 / AC-19d / AC-19f).
    pub async fn phase3_row_counts(&self) -> anyhow::Result<(u64, u64)> {
        self.conn
            .call(|conn| -> Result<_, rusqlite::Error> {
                let outbox: u64 =
                    conn.query_row("SELECT COUNT(*) FROM directive_outbox", [], |r| r.get(0))?;
                let pending: u64 =
                    conn.query_row("SELECT COUNT(*) FROM pending_escalations", [], |r| r.get(0))?;
                Ok((outbox, pending))
            })
            .await
            .map_err(Into::into)
    }

    /// Count of rows waiting for boot hydration recovery (`council_response_staged`).
    /// Used by the Phase 3a.5 boot hydration sweep.
    pub async fn count_council_response_staged(&self) -> anyhow::Result<u64> {
        self.conn
            .call(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM pending_escalations WHERE status = 'council_response_staged'",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .map_err(Into::into)
    }

    /// Bounded list of staged rows for boot hydration recovery.
    /// Returns (id, tenant, council_response_json) tuples, limited by `limit`.
    /// Used by the Phase 3a.5 narrow recovery seam.
    pub async fn list_council_response_staged(
        &self,
        limit: u32,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, tenant, council_response_json
                     FROM pending_escalations
                     WHERE status = 'council_response_staged'
                     ORDER BY created_at_ms ASC
                     LIMIT ?1",
                )?;
                let rows = stmt.query_map([limit], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(Into::into)
    }

    /// P2 keyset-paginated variant of `list_council_response_staged` for the boot hydration
    /// sweep. A pure `LIMIT` page over `status='council_response_staged'` re-returns the same
    /// head rows when a row never leaves that status — which is exactly what a `SkewHeld` parked
    /// row does (it stays staged) — so the sweep would spin on the poison row until the deadline.
    /// Paginating on the composite cursor `(created_at_ms, id)` makes each sweep advance strictly
    /// past every row it has already visited (held or not), guaranteeing forward progress and
    /// termination. The composite (not bare `created_at_ms`) avoids skipping rows that share a
    /// millisecond. Returns `(created_at_ms, id, tenant, council_response_json)` so the caller can
    /// advance the watermark. Pass `(i64::MIN, "")` for the first page.
    pub async fn list_council_response_staged_after(
        &self,
        after_created_at_ms: i64,
        after_id: String,
        limit: u32,
    ) -> anyhow::Result<Vec<(i64, String, String, String)>> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT created_at_ms, id, tenant, council_response_json
                     FROM pending_escalations
                     WHERE status = 'council_response_staged'
                       AND (created_at_ms > ?1 OR (created_at_ms = ?1 AND id > ?2))
                     ORDER BY created_at_ms ASC, id ASC
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![after_created_at_ms, after_id, limit],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    },
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(Into::into)
    }

    /// Atomically claims the next eligible pending escalation for crash-safe recovery.
    /// Eligible = (queued | failed with retry) OR (stale 'claimed' per claimed_at_ms + attempts*30s window using existing fields).
    ///
    /// Uses BEGIN IMMEDIATE + SELECT ... LIMIT 1 + UPDATE (tenant, id) to
    /// safely claim under concurrency. Returns None if no eligible row.
    /// The stale-claim path reuses existing columns.
    /// Backoff base reused from mark_claim_failed (30s) * attempts for the stale-claimed window (design coherent model around existing fields).
    /// Per the reviewed fencing invariant:
    /// Lease is fixed long enough for max inference (~2.5 min), backoff is short for retries.
    /// Decouples the overloaded CLAIMED_STALE_BACKOFF_MS.
    /// (lease liveness: the 150_000 default now lives in `LEASE_DURATION_MS_DEFAULT`,
    /// env-overridable via WATCH_LEASE_DURATION_MS; renewal extends it mid-flight.)
    const RETRY_BACKOFF_MS: i64 = 30_000;

    pub async fn claim_next_queued_or_failed(&self) -> anyhow::Result<Option<PendingClaim>> {
        self.claim_next_queued_or_failed_with_lease(lease_duration_ms())
            .await
    }

    /// lease liveness — lease-parameterized claim. Production goes through
    /// `claim_next_queued_or_failed` (env-default lease); tests inject a
    /// compressed lease here so renewal behavior can be exercised without
    /// mutating process-global env (parallel-test safety).
    pub async fn claim_next_queued_or_failed_with_lease(
        &self,
        lease_duration_ms: i64,
    ) -> anyhow::Result<Option<PendingClaim>> {
        self.claim_next_queued_or_failed_with_lease_and_epoch(lease_duration_ms, None)
            .await
    }

    /// Signed-material invariant test seam — inject the attest registry the
    /// reserve re-verifies against, so a test process can prove the spend-time
    /// signature re-verification without publishing the process-global boot
    /// registry (parallel-test safety, same precedent as `armed_epoch_override`).
    /// Production goes through the `_and_epoch` arm which falls back to
    /// `attest::boot_registry()`.
    pub async fn claim_next_queued_or_failed_with_lease_epoch_registry(
        &self,
        lease_duration_ms: i64,
        armed_epoch_override: Option<i64>,
        registry_override: Option<std::sync::Arc<crate::watch::attest::AttestKeyRegistry>>,
    ) -> anyhow::Result<Option<PendingClaim>> {
        self.claim_reserve_impl(lease_duration_ms, armed_epoch_override, registry_override)
            .await
    }

    /// riders (A) — armed-epoch-parameterized claim (test seam, same pattern
    /// as `_with_lease`). Production passes `None` → the armed epoch is read
    /// from env (`current_replay_epoch`) INSIDE the claim tx, exactly as
    /// before. Tests inject `Some(epoch)` so the `replay_epoch = ?3 OR ?3 = 0`
    /// fence can be proven with epoch > 0 WITHOUT mutating process-global
    /// env (parallel-test safety — WATCH_REPLAY_EPOCH would poison sibling
    /// tests in the same binary).
    pub async fn claim_next_queued_or_failed_with_lease_and_epoch(
        &self,
        lease_duration_ms: i64,
        armed_epoch_override: Option<i64>,
    ) -> anyhow::Result<Option<PendingClaim>> {
        // Production: re-verify against the boot-published registry.
        self.claim_reserve_impl(lease_duration_ms, armed_epoch_override, None)
            .await
    }

    /// Signed-material invariant — the reserve atomic. Re-verifies the
    /// persisted arm's ES256 signature against the registry (override → boot)
    /// BEFORE the cap math, so a watch.db-write attacker who forges active_arm
    /// columns but cannot produce a valid hardware signature is refused.
    async fn claim_reserve_impl(
        &self,
        lease_duration_ms: i64,
        armed_epoch_override: Option<i64>,
        registry_override: Option<std::sync::Arc<crate::watch::attest::AttestKeyRegistry>>,
    ) -> anyhow::Result<Option<PendingClaim>> {
        // Use RETRY for stale eligibility / next_retry calc; LEASE for the actual claim duration (Council mandate).
        let retry_backoff_ms = Self::RETRY_BACKOFF_MS;
        // Resolve the registry OUTSIDE the SQLite closure (boot static / override).
        let registry = registry_override.or_else(crate::watch::attest::boot_registry);
        self.conn
            .call(move |conn| -> Result<Option<PendingClaim>, rusqlite::Error> {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or_else(|_| 0); // saturating for recovery paths (pre-epoch unrealistic; design crash hardening)

                // atomic spend ledger (Invariant + :5763): the BEFORE-INSERT
                // SUM check that used to live here has been REMOVED. The cap is now
                // enforced by a serialized reserve against `spend_ledger` (UPDATE
                // ...RETURNING) performed below, AFTER an eligible row is selected,
                // but inside this same BEGIN IMMEDIATE tx so writers still serialize
                // on the same lock. ?estimate is the per-directive max-fanout ceiling
                // (worst-case), so a single non-deterministic fan-out cannot bust the
                // day cap; the realized truth is settled later. Cap window is the
                // calendar UTC day (no rolling now-24h window -> Q5 hazard retired).
                // p0d/P0-1: boot-resolved cap — gauge and reserve share one value.
                let estimate = max_fanout_cost_usd();
                let day_cap = daily_spend_cap();
                let day_bucket = utc_day_bucket(now_ms);

                // Select one eligible row
                // Extended for Phase 6 outbox hardening: stale 'claimed' rows
                // use explicit claimed_until_ms for lease tracking. Fallback
                // to old claimed_at_ms + backoff for legacy rows.
                // Phase 1 Weld safety: replay fence (executor re-verify per Council P0).
                // Only drain rows matching the current armed epoch (or when armed=0 to allow legacy/test during transition).
                // This completes the end-to-end fence started at INSERT (replay_epoch tag).
                // riders (A): tests inject the armed epoch via the override; production
                // (None) reads env here, at claim time, exactly as before.
                let armed_epoch = armed_epoch_override
                    .unwrap_or_else(crate::watch::dispatcher::current_replay_epoch);

                // Signed-material invariant — the attested ceiling, anchored to
                // the ES256 SIGNATURE, not to attacker-writable columns. Read the
                // active_arm singleton (with its persisted signed material) INSIDE
                // this same BEGIN IMMEDIATE tx, RE-VERIFY the hardware signature
                // against the boot registry, assert the SIGNED content equals the
                // columns AND the running binary, and enforce the SIGNED expiry.
                // Only then is the cap a ceiling. ALL predicates fail CLOSED
                // (rollback + Ok(None)) so a DARK / forged / expired / wrong-build
                // arm never spends real funds.
                //
                // The threat actor (laptop-owning agent with watch.db write) can
                // forge any active_arm COLUMN, but cannot forge a P-256 signature
                // over a higher-cap challenge without a hardware tap. The columns
                // are cross-checked against the AUTHENTICATED challenge bytes, so a
                // forged column is caught by the signed-content assertion. The
                // armed_epoch is NO LONGER a security term (both sides are
                // attacker-writable) — it is kept only as an operational
                // disarm/rotation signal; the SIGNATURE + signed exp_ms are the
                // anchor.
                let active_arm = read_active_arm_row(&tx)?;

                // P1-1 (i64 cents) — the ceiling decision is made in INTEGER
                // CENTS, never f64 USD, so float drift can never widen the bound.
                let ambient_cents = {
                    let c = (day_cap * 100.0).round();
                    if (0.0..=crate::watch::attest::MAX_ARM_CAP_CENTS_F64).contains(&c) {
                        c as i64
                    } else {
                        0
                    }
                };

                let cap_cents: i64 = match active_arm {
                    Some(row) => {
                        // The ONE arm-validity decision — extracted to
                        // attest::verify_arm_row (share-the-struct doctrine)
                        // so claim-reserve (spend) and staged-row recovery
                        // (sign) can never drift. Inside: (1) ES256 re-verify
                        // against the boot registry, (2) signed-content
                        // assertion (columns == AUTHENTICATED challenge ==
                        // running binary — a forged column cannot carry a
                        // hardware signature), (3) SIGNED spend-window
                        // freshness (iat_ms + spend_window_ms, checked_add;
                        // GW_ARM_SIGNED_WINDOW=false named rollback to the
                        // boot-locked window) + the exp_at_ms column
                        // consistency tripwire. Full threat-model notes live
                        // on verify_arm_row. ALL predicates fail CLOSED here:
                        // rollback + Ok(None), never a partial spend.
                        match crate::watch::attest::verify_arm_row(
                            &row,
                            registry.as_deref(),
                            now_ms,
                        ) {
                            // Narrow-only ceiling, in i64 cents, off the SIGNED cap.
                            Ok(signed_cap_cents) => signed_cap_cents.min(ambient_cents),
                            Err(_refusal) => {
                                tx.rollback()?;
                                return Ok(None);
                            }
                        }
                    }
                    None => {
                        // Fail-closed: no attested arm → a DARK / never-armed
                        // producer must NOT spend real funds. Enforcement is now
                        // UNCONDITIONAL (the GW_REQUIRE_ATTESTED_ARM runtime
                        // bypass was removed — HIGH finding); rollback = redeploy
                        // the prior binary is the only revert.
                        tx.rollback()?;
                        return Ok(None);
                    }
                };
                // Bind the cents ceiling back to USD for the existing RETURNING
                // predicate. The decision was already made in i64 cents above;
                // this is exact (cents/100 has no representation error for any
                // value in the bindable range).
                let day_cap = cap_cents as f64 / 100.0;
                // also select the OLD
                // status, claim_token and reservation stamp so a reclaim of a
                // lease-expired 'claimed' row can (a) RELEASE the prior holder's
                // reservation before reserving its own (the stamp overwrite
                // below would otherwise orphan one ceiling in spend_ledger for
                // the rest of the UTC day), and (b) flag the reclaim as a real
                // in-flight orphan-charge recon hint (PendingClaim.reclaimed_in_flight).
                let mut select = tx.prepare(
                    "SELECT id, tenant, envelope_json, attempts, sentinel_name, replay_epoch,
                            status, claim_token, reserved_estimate_usd, reserved_day_bucket
                     FROM pending_escalations
                     WHERE (status IN ('queued', 'failed')
                            OR (status = 'claimed' AND
                                (claimed_until_ms < ?1 OR
                                 (claimed_until_ms IS NULL AND claimed_at_ms IS NOT NULL AND claimed_at_ms + (?2 * (attempts + 1)) < ?1))))
                       AND (next_retry_at_ms IS NULL OR next_retry_at_ms <= ?1 OR status = 'claimed')
                       AND (replay_epoch = ?3 OR ?3 = 0)
                     ORDER BY next_retry_at_ms IS NULL DESC, next_retry_at_ms ASC, created_at_ms ASC
                     LIMIT 1",
                )?;

                let mut rows = select.query_map(rusqlite::params![now_ms, retry_backoff_ms, armed_epoch], |r| {
                    Ok((
                        PendingClaim {
                            id: r.get(0)?,
                            tenant: r.get(1)?,
                            envelope_json: r.get(2)?,
                            attempts: r.get(3)?,
                            sentinel_name: r.get(4)?,
                            replay_epoch: r.get(5)?,
                            claim_token: String::new(), // overwritten with fresh token below on successful claim
                            reclaimed_in_flight: false, // computed below from the old row state
                        },
                        r.get::<_, String>(6)?,            // old status
                        r.get::<_, Option<String>>(7)?,    // old claim_token
                        r.get::<_, Option<f64>>(8)?,       // old reserved_estimate_usd
                        r.get::<_, Option<String>>(9)?,    // old reserved_day_bucket
                    ))
                })?;

                let claim_opt = rows.next().transpose()?;

                // Explicitly drop the statement and iterator before we decide to rollback or continue.
                // This releases the borrow on `tx`.
                drop(rows);
                drop(select);

                let (mut claim, old_status, old_token, old_est, old_bucket) = match claim_opt {
                    Some(c) => c,
                    None => {
                        tx.rollback()?;
                        return Ok(None);
                    }
                };

                // a stale-'claimed' reclaim must back out
                // the prior reservation stamp BEFORE reserving its own — same
                // release sweep_phantom_claims/mark_claim_failed perform, done
                // here atomically in the claim tx (the stamp is overwritten
                // below, so no later release could ever find it again).
                let stale_reclaim = old_status == "claimed";
                if stale_reclaim {
                    if let (Some(prior_est), Some(prior_bucket)) = (old_est, old_bucket) {
                        tx.execute(
                            "UPDATE spend_ledger
                             SET reserved_usd = MAX(0.0, reserved_usd - ?2)
                             WHERE day_bucket = ?1",
                            rusqlite::params![prior_bucket, prior_est],
                        )?;
                    }
                    // Orphan-charge recon hint: a real in-flight prior claim
                    // (a dispatcher actually held it) mirrors the
                    // sweep_phantom_claims in_flight_expired definition.
                    claim.reclaimed_in_flight =
                        old_token.as_deref().is_some_and(|t| !t.is_empty()) && claim.attempts > 0;
                }

                // atomic spend ledger: serialized reserve INSIDE this BEGIN IMMEDIATE tx.
                // Two statements, but the second is the cap test+reserve fused into one
                // atomic UPDATE...RETURNING (vs the prior 2-statement SUM-then-decide
                // TOCTOU). Ensure the bucket row exists, then try to add `estimate` only
                // if it keeps reserved+settled+estimate under the day cap. RETURNING a row
                // means the reservation succeeded; no row means over cap -> refuse exactly
                // as the old SUM block did (rollback + Ok(None)).
                tx.execute(
                    "INSERT INTO spend_ledger(day_bucket, reserved_usd, settled_usd)
                     VALUES (?1, 0.0, 0.0) ON CONFLICT(day_bucket) DO NOTHING",
                    rusqlite::params![day_bucket],
                )?;
                let reserved_now: Option<f64> = {
                    let mut reserve_stmt = tx.prepare(
                        "UPDATE spend_ledger
                         SET reserved_usd = reserved_usd + ?2
                         WHERE day_bucket = ?1
                           AND (reserved_usd + settled_usd + ?2) <= ?3
                         RETURNING reserved_usd",
                    )?;
                    let r = reserve_stmt
                        .query_row(
                            rusqlite::params![day_bucket, estimate, day_cap],
                            |row| row.get::<_, f64>(0),
                        )
                        .optional()?;
                    drop(reserve_stmt);
                    r
                };
                if reserved_now.is_none() {
                    // Over the UTC-day cap. Refuse the claim; the reservation was not made.
                    tx.rollback()?;
                    return Ok(None);
                }

                // Claim it — tenant-qualified composite PK
                // True fencing token (Council mandate): fresh random per claim to prevent zombie commit races
                // even if until-ms timing collides (same-ms edge). Passed back in PendingClaim to caller.
                // Uses OsRng (already a dep for ledger/keys) for good uniqueness; non-crypto is fine for fencing id.
                let mut rng = OsRng;
                let mut token_bytes = [0u8; 16];
                rng.fill_bytes(&mut token_bytes);
                let claim_token: String = token_bytes.iter().map(|b| format!("{:02x}", b)).collect();
                // Fixed lease (150s) per Council; not overloaded with backoff/attempts scaling.
                let new_claimed_until = now_ms + lease_duration_ms;
                // Stamp the reservation (estimate + day bucket) on the claim row so settle
                // (store_council_response_and_stage) and release (mark_claim_failed /
                // sweep_phantom_claims) know exactly what to back out of spend_ledger.
                tx.execute(
                    "UPDATE pending_escalations
                     SET status = 'claimed',
                         claimed_at_ms = ?1,
                         claimed_until_ms = ?4,
                         claim_token = ?5,
                         reserved_estimate_usd = ?6,
                         reserved_day_bucket = ?7,
                         attempts = attempts + 1,
                         last_error = NULL
                     WHERE tenant = ?2 AND id = ?3 AND status IN ('queued', 'failed', 'claimed')",
                    rusqlite::params![now_ms, &claim.tenant, &claim.id, new_claimed_until, &claim_token, estimate, day_bucket],
                )?;

                claim.claim_token = claim_token;
                tx.commit()?;
                Ok(Some(claim))
            })
            .await
            .map_err(Into::into)
    }
    /// atomic spend ledger: single source of today's UTC-day spend for the p0d gauge.
    /// Re-pointed from the old directive_outbox SUM (rolling now-24h) to the
    /// serialized `spend_ledger` (reserved + settled for the current UTC day bucket).
    /// Returns 0.0 when there is no bucket row yet. p0d wires this into the gauge.
    pub async fn get_daily_council_spend(&self) -> anyhow::Result<f64> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let day_bucket = utc_day_bucket(now_ms);
        self.conn
            .call(move |conn| -> Result<f64, rusqlite::Error> {
                let mut stmt = conn.prepare(
                    "SELECT COALESCE(reserved_usd, 0.0) + COALESCE(settled_usd, 0.0)
                     FROM spend_ledger WHERE day_bucket = ?1",
                )?;
                let sum: f64 = stmt
                    .query_row([day_bucket], |r| r.get(0))
                    .optional()?
                    .unwrap_or(0.0);
                Ok(sum)
            })
            .await
            .map_err(Into::into)
    }

    /// Persists the durable council response envelope and transitions the row
    /// from 'claimed' to 'council_response_staged'.
    ///
    /// Uses composite (tenant, id) for the update (C11).
    /// The council_response_json must be the canonical durable form:
    ///   {"body": "<raw council content or fence body>", "headers": {"x-council-session-id": "...", "x-total-cost-usd": "..."}}
    ///
    /// watch telemetry: returns a [`SettleReport`] whose `dup_realized_cost` is
    /// the idempotency-dedup MISS detector (telemetry invariant) — true when this settle
    /// landed on a row that ALREADY carried a realized_cost_usd from a prior
    /// settle. Callers route the report through
    /// `dispatcher::note_settle_report` to bump the dup-charge alarm.
    pub async fn store_council_response_and_stage(
        &self,
        tenant: &str,
        id: &str,
        council_response_json: &str,
        claim_token: &str, // required true fencing token (invariant)
    ) -> anyhow::Result<SettleReport> {
        let t = tenant.to_string();
        let i = id.to_string();
        let j = council_response_json.to_string();
        let tok = claim_token.to_string();

        // the SAME strict filter the outbox recovery
        // applies (dispatcher.rs, finite / >= 0 / < COST_CEILING_USD) now runs
        // HERE, at settle time, BEFORE the ledger write commits. Without it:
        //   * a missing/unparseable x-total-cost-usd settled 0.0 — the day cap
        //     failed OPEN (reserve 5.0, settle 0.0, release, repeat unbounded);
        //   * a negative value CREDITED settled_usd (cap-headroom injection);
        //   * "NaN" poisoned the bucket (every later reserve comparison false).
        // Invalid/missing realized cost now settles FAIL-CLOSED at the stamped
        // reservation estimate (see below); the row's realized_cost_usd column
        // stays NULL so recovery's own strict filter still dead-letters it.
        let realized_cost_usd: Option<f64> = serde_json::from_str::<serde_json::Value>(&j)
            .ok()
            .and_then(|v| {
                v.get("headers")
                    .and_then(|h| h.get("x-total-cost-usd"))
                    .and_then(|val| val.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
            })
            .filter(|c| {
                c.is_finite() && *c >= 0.0 && *c < crate::watch::dispatcher::COST_CEILING_USD
            });

        self.conn
            .call(move |conn| -> Result<SettleReport, rusqlite::Error> {
                // atomic spend ledger SETTLE: move the OCC transition into a BEGIN IMMEDIATE
                // tx so the status flip AND the ledger settle commit (or roll back) together.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // Read the stamped reservation BEFORE the status flip (guarded by the same
                // OCC predicate) so we know exactly what conservative reservation to back
                // out. SQLite RETURNING yields post-update values, so a SELECT-then-UPDATE
                // pair inside the one Immediate tx is the atomic way to capture the OLD stamp.
                // p0d: also capture the OLD realized_cost_usd — a non-NULL prior value means
                // this row was already settled once, and we are about to write a realized
                // cost for it a second time (dup-charge alarm input).
                let reservation: Option<(Option<f64>, Option<String>, Option<f64>)> = tx
                    .query_row(
                        "SELECT reserved_estimate_usd, reserved_day_bucket, realized_cost_usd
                         FROM pending_escalations
                         WHERE tenant = ?1 AND id = ?2 AND status = 'claimed' AND claim_token = ?3",
                        rusqlite::params![t, i, tok],
                        |row| {
                            Ok((
                                row.get::<_, Option<f64>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<f64>>(2)?,
                            ))
                        },
                    )
                    .optional()?;

                let (est, bucket, prior_realized) = match reservation {
                    Some(r) => r,
                    None => {
                        // OCC reject (wrong token / not 'claimed') — identical refusal as before.
                        tx.rollback()?;
                        return Err(rusqlite::Error::QueryReturnedNoRows);
                    }
                };

                // The OCC status flip. We NULL the reservation stamp so a re-presentation
                // of the (now-staged) row cannot double-settle the ledger.
                tx.execute(
                    "UPDATE pending_escalations
                     SET status = 'council_response_staged',
                         council_response_json = ?1,
                         realized_cost_usd = ?2,
                         reserved_estimate_usd = NULL,
                         reserved_day_bucket = NULL,
                         last_error = NULL
                     WHERE tenant = ?3 AND id = ?4 AND status = 'claimed' AND claim_token = ?5",
                    rusqlite::params![j, realized_cost_usd, t, i, tok],
                )?;

                // Settle the ledger in the SAME tx: back out the conservative reservation
                // and add the realized truth. reserve-at-ceiling was the safety;
                // settle-at-realized is the truth. MAX(0, ...) tolerates double-release races.
                //
                // when the realized cost is absent/invalid
                // (already strict-filtered above), settle FAIL-CLOSED at the
                // stamped reservation estimate — never 0.0. A header drift then
                // *over*-counts spend until recon corrects it, instead of
                // silently unbounding the UTC-day cap.
                let mut settled_at_estimate_usd = None;
                let mut ceiling_overshoot_usd = None;
                if let (Some(est), Some(bucket)) = (est, bucket) {
                    let realized_for_ledger = match realized_cost_usd {
                        Some(realized) => {
                            if realized > est {
                                // p0c P2: per-directive ceiling overshoot —
                                // accepted as the truth, but FLAGGED so the
                                // p0d alarm path can page (input to the
                                // settle_ceiling_overshoot_total counter).
                                ceiling_overshoot_usd = Some(realized - est);
                            }
                            realized
                        }
                        None => {
                            settled_at_estimate_usd = Some(est);
                            est
                        }
                    };
                    tx.execute(
                        "UPDATE spend_ledger
                         SET reserved_usd = MAX(0.0, reserved_usd - ?2),
                             settled_usd  = settled_usd + ?3
                         WHERE day_bucket = ?1",
                        rusqlite::params![bucket, est, realized_for_ledger],
                    )?;
                }

                tx.commit()?;
                Ok(SettleReport {
                    dup_realized_cost: prior_realized.is_some(),
                    settled_at_estimate_usd,
                    ceiling_overshoot_usd,
                })
            })
            .await
            .map_err(Into::into)
    }

    /// watch telemetry recon — settled-only spend for today's UTC bucket. The
    /// recon job compares THIS (the ledger's realized truth) against the
    /// out-of-band billing source; reservations are excluded because the
    /// external source only ever sees realized charges. Returns 0.0 when no
    /// bucket row exists yet.
    pub async fn get_daily_settled_council_spend(&self) -> anyhow::Result<f64> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.get_settled_council_spend_for_bucket(&utc_day_bucket(now_ms))
            .await
    }

    /// settled spend for an ARBITRARY `YYYY-MM-DD` UTC
    /// bucket. The recon loop uses this for the yesterday-lookback leg so
    /// charges near the UTC midnight boundary cannot permanently escape
    /// reconciliation (closed buckets used to never be re-checked).
    pub async fn get_settled_council_spend_for_bucket(
        &self,
        day_bucket: &str,
    ) -> anyhow::Result<f64> {
        let day_bucket = day_bucket.to_string();
        self.conn
            .call(move |conn| -> Result<f64, rusqlite::Error> {
                conn.query_row(
                    "SELECT COALESCE(settled_usd, 0.0) FROM spend_ledger WHERE day_bucket = ?1",
                    [day_bucket],
                    |r| r.get(0),
                )
                .optional()
                .map(|v| v.unwrap_or(0.0))
            })
            .await
            .map_err(Into::into)
    }

    /// T2 shadow — reserved (not-yet-settled) council spend for an ARBITRARY
    /// `YYYY-MM-DD` UTC bucket. Mirrors get_settled_council_spend_for_bucket
    /// exactly in style/signature/error handling. Returns 0.0 when no row.
    pub async fn get_reserved_council_spend_for_bucket(
        &self,
        day_bucket: &str,
    ) -> anyhow::Result<f64> {
        let day_bucket = day_bucket.to_string();
        self.conn
            .call(move |conn| -> Result<f64, rusqlite::Error> {
                conn.query_row(
                    "SELECT COALESCE(reserved_usd, 0.0) FROM spend_ledger WHERE day_bucket = ?1",
                    [day_bucket],
                    |r| r.get(0),
                )
                .optional()
                .map(|v| v.unwrap_or(0.0))
            })
            .await
            .map_err(Into::into)
    }

    /// watch telemetry (telemetry invariant) — append one out-of-band recon
    /// divergence alarm row. Called by `recon::run_recon_once` when
    /// |local - external| exceeds the configured threshold.
    pub async fn insert_recon_alarm(
        &self,
        at_ms: i64,
        day_bucket: &str,
        local_usd: f64,
        external_usd: f64,
        divergence_usd: f64,
        source: &str,
    ) -> anyhow::Result<()> {
        let d = day_bucket.to_string();
        let s = source.to_string();
        self.conn
            .call(move |conn| -> Result<(), rusqlite::Error> {
                conn.execute(
                    "INSERT INTO recon_alarm (at_ms, day_bucket, local_usd, external_usd, divergence_usd, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![at_ms, d, local_usd, external_usd, divergence_usd, s],
                )?;
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    /// watch telemetry — all recon alarm rows, oldest first (ops/test surface).
    pub async fn list_recon_alarms(&self) -> anyhow::Result<Vec<ReconAlarmRow>> {
        self.conn
            .call(|conn| -> Result<Vec<ReconAlarmRow>, rusqlite::Error> {
                let mut stmt = conn.prepare(
                    "SELECT id, at_ms, day_bucket, local_usd, external_usd, divergence_usd, source
                     FROM recon_alarm ORDER BY id ASC",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok(ReconAlarmRow {
                            id: r.get(0)?,
                            at_ms: r.get(1)?,
                            day_bucket: r.get(2)?,
                            local_usd: r.get(3)?,
                            external_usd: r.get(4)?,
                            divergence_usd: r.get(5)?,
                            source: r.get(6)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// lease liveness — K8s-Lease-style renewal of the deliberation claim,
    /// mirroring `heartbeat_outbox` (the directive_outbox worker leg) for the
    /// pending_escalations dispatcher leg. Single atomic UPDATE...RETURNING:
    /// extends claimed_until_ms to now+extension ONLY while the caller still
    /// holds the claim (status='claimed' AND claim_token matches). 0 rows ->
    /// `RenewOutcome::Lost`: the token was superseded by a competing reclaim
    /// or the status moved on — the holder must abort cleanly (the in-flight
    /// council response, if it lands, is fenced out by the OCC claim_token
    /// check in `store_council_response_and_stage`, so no double-stage).
    pub async fn renew_deliberation_lease(
        &self,
        tenant: &str,
        id: &str,
        claim_token: &str,
        now_ms: i64,
        extension_ms: i64,
    ) -> anyhow::Result<RenewOutcome> {
        let t = tenant.to_string();
        let i = id.to_string();
        let tok = claim_token.to_string();
        self.conn
            .call(move |conn| -> Result<RenewOutcome, rusqlite::Error> {
                let new_until: Option<i64> = conn
                    .query_row(
                        "UPDATE pending_escalations
                         SET claimed_until_ms = ?1
                         WHERE tenant = ?2 AND id = ?3
                           AND status = 'claimed' AND claim_token = ?4
                         RETURNING claimed_until_ms",
                        rusqlite::params![now_ms + extension_ms, t, i, tok],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(match new_until {
                    Some(claimed_until_ms) => RenewOutcome::Renewed { claimed_until_ms },
                    None => RenewOutcome::Lost,
                })
            })
            .await
            .map_err(Into::into)
    }

    /// Marks a claimed row as 'failed' with last_error. The row becomes eligible
    /// for future reclaim (by live dispatcher or boot hydration after backoff).
    /// Uses composite (tenant, id).
    pub async fn mark_claim_failed(
        &self,
        tenant: &str,
        id: &str,
        last_error: &str,
        claim_token: &str, // required true fencing token (invariant)
    ) -> anyhow::Result<()> {
        let t = tenant.to_string();
        let i = id.to_string();
        let e = last_error.to_string();
        let tok = claim_token.to_string();
        // saturate instead of panicking under a pre-epoch host
        // clock — matches the convention in every sibling path here (claim_next,
        // get_daily_council_spend, sweep_phantom_claims_report, append_arm_audit).
        // A panic on the council-call-FAILURE path would kill the dispatcher
        // tick task at the worst possible time.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // RETRY backoff (decoupled from lease per Council)
        let next_retry = now + Self::RETRY_BACKOFF_MS;

        self.conn
            .call(move |conn| -> Result<(), rusqlite::Error> {
                // atomic spend ledger RELEASE: back out the reservation when work is abandoned
                // so a crashed/expired claim does NOT permanently consume budget (the
                // correctness fix over the old COUNT(*)-forever reservation). Done in one
                // Immediate tx with the OCC status flip. The stamp is read first (SQLite
                // RETURNING is post-update), then NULLed so a double-release is a no-op.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let reservation: Option<(Option<f64>, Option<String>)> = tx
                    .query_row(
                        "SELECT reserved_estimate_usd, reserved_day_bucket
                         FROM pending_escalations
                         WHERE tenant = ?1 AND id = ?2 AND status = 'claimed' AND claim_token = ?3",
                        rusqlite::params![t, i, tok],
                        |row| {
                            Ok((
                                row.get::<_, Option<f64>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                            ))
                        },
                    )
                    .optional()?;

                let (est, bucket) = match reservation {
                    Some(r) => r,
                    None => {
                        tx.rollback()?;
                        return Err(rusqlite::Error::QueryReturnedNoRows);
                    }
                };

                tx.execute(
                    "UPDATE pending_escalations
                     SET status = 'failed',
                         last_error = ?1,
                         claimed_at_ms = NULL,
                         next_retry_at_ms = ?2,
                         reserved_estimate_usd = NULL,
                         reserved_day_bucket = NULL
                     WHERE tenant = ?3 AND id = ?4 AND status = 'claimed' AND claim_token = ?5",
                    rusqlite::params![e, next_retry, t, i, tok],
                )?;

                if let (Some(est), Some(bucket)) = (est, bucket) {
                    tx.execute(
                        "UPDATE spend_ledger
                         SET reserved_usd = MAX(0.0, reserved_usd - ?2)
                         WHERE day_bucket = ?1",
                        rusqlite::params![bucket, est],
                    )?;
                }

                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    /// Perform recovery for one `council_response_staged` row.
    /// The actual parsing + signing + outbox helper + composite-key update
    /// lives in `dispatcher::recover_council_response_staged`.
    ///
    /// Returns the outcome plus the list of bridged high-level Phase 3
    /// watch audit events (escalation_recovered_resume_outbox, directive_staged, ...).
    /// Returns the stored council_response_json for a given (tenant, id) row.
    /// Used by the live recovery continuation (3b.2) to feed the shared recovery path.
    pub async fn get_council_response_json(
        &self,
        tenant: &str,
        id: &str,
    ) -> anyhow::Result<Option<String>> {
        let t = tenant.to_string();
        let i = id.to_string();
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT council_response_json FROM pending_escalations WHERE tenant = ?1 AND id = ?2",
                    rusqlite::params![t, i],
                    |r| r.get(0),
                )
                .optional()
            })
            .await
            .map_err(Into::into)
    }

    /// Returns the current status of a pending_escalations row (tenant-qualified).
    /// Used by the live dispatcher tick report to determine outbox_written vs dismissed
    /// after successful recovery.
    pub async fn get_pending_status(
        &self,
        tenant: &str,
        id: &str,
    ) -> anyhow::Result<Option<String>> {
        let t = tenant.to_string();
        let i = id.to_string();
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT status FROM pending_escalations WHERE tenant = ?1 AND id = ?2",
                    rusqlite::params![t, i],
                    |r| r.get(0),
                )
                .optional()
            })
            .await
            .map_err(Into::into)
    }

    pub async fn recover_one_council_response_staged(
        &self,
        escalation_id: &str,
        tenant: &str,
        council_response_json: &str,
        signing_key: &crate::keymgmt::DirectiveSigningKey,
    ) -> anyhow::Result<(
        crate::watch::dispatcher::RecoveryOutcome,
        Vec<crate::watch::dispatcher::WatchPhase3AuditEvent>,
    )> {
        let id = escalation_id.to_string();
        let t = tenant.to_string();
        let j = council_response_json.to_string();

        // P0-epsilon: clone the key (cheap) and move the owned clone into the
        // conn.call closure. This removes any call to the global
        // directive_signing_key() from the recovery path.
        let owned_key = signing_key.clone();

        self.conn
            .call(move |conn| -> Result<_, rusqlite::Error> {
                let mut sink = Vec::new();
                let (outcome, events) = crate::watch::dispatcher::recover_council_response_staged(
                    conn, &id, &t, &j, &mut sink, owned_key,
                )
                .map_err(|_e| rusqlite::Error::ExecuteReturnedResults)?;
                Ok((outcome, events))
            })
            .await
            .map_err(|e| anyhow::anyhow!("recovery call failed: {:?}", e))
    }

    /// single-writer (single-writer invariant) — atomically acquire (or re-acquire /
    /// take over) the singleton writer claim.
    ///
    /// Inside one BEGIN IMMEDIATE tx: seed the singleton row if absent
    /// (INSERT OR IGNORE — first boot on a fresh db), then an atomic
    /// UPDATE...RETURNING that succeeds ONLY when
    ///   * we already hold the claim (`instance_uuid = ?uuid` — re-acquire /
    ///     heartbeat-equivalent refresh), OR
    ///   * the current holder is stale (`heartbeat_at_ms < now - stale_ms`
    ///     — crash-recovery takeover).
    ///
    /// A row coming back means the claim is OURS; no row means another LIVE
    /// writer holds it — the caller MUST refuse to arm / refuse to spawn.
    ///
    /// `now_ms` and `stale_ms` are parameters (not read inside) so tests can
    /// simulate clock advance and second instances without env mutation —
    /// same injection pattern as `claim_next_queued_or_failed_with_lease`.
    pub async fn try_acquire_writer_claim(
        &self,
        uuid: &str,
        now_ms: i64,
        stale_ms: i64,
    ) -> anyhow::Result<bool> {
        let uuid = uuid.to_string();
        self.conn
            .call(move |conn| -> Result<bool, rusqlite::Error> {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "INSERT OR IGNORE INTO writer_claim
                         (singleton, instance_uuid, boot_at_ms, heartbeat_at_ms)
                     VALUES (1, ?1, ?2, ?2)",
                    rusqlite::params![uuid, now_ms],
                )?;
                let won: Option<String> = {
                    // boot_at_ms only changes on a real
                    // takeover (uuid change). A re-acquire by the SAME instance
                    // keeps the original acquisition time, so the column reads
                    // as "when this holder first took the claim" during
                    // incident forensics (not "last acquired at"). SQLite SET
                    // expressions evaluate against the OLD row, so the CASE
                    // sees the pre-update instance_uuid.
                    let mut stmt = tx.prepare(
                        "UPDATE writer_claim
                         SET boot_at_ms = CASE WHEN instance_uuid = ?1 THEN boot_at_ms ELSE ?2 END,
                             instance_uuid = ?1,
                             heartbeat_at_ms = ?2
                         WHERE singleton = 1
                           AND (instance_uuid = ?1 OR heartbeat_at_ms < ?2 - ?3)
                         RETURNING instance_uuid",
                    )?;
                    let r = stmt
                        .query_row(rusqlite::params![uuid, now_ms, stale_ms], |row| row.get(0))
                        .optional()?;
                    drop(stmt);
                    r
                };
                tx.commit()?;
                // The UPDATE only ever writes OUR uuid, so any returned row
                // is ours; compare anyway for explicitness.
                Ok(won.as_deref() == Some(uuid.as_str()))
            })
            .await
            .map_err(Into::into)
    }

    /// single-writer — refresh the holder's liveness. Returns `true`
    /// when the UPDATE affected exactly 1 row (we still hold the claim);
    /// `false` when it affected 0 rows — the claim was TAKEN OVER by
    /// another instance, and the caller MUST fail closed (self-disarm,
    /// stop producing). Plain UPDATE, no tx needed: a single statement is
    /// already atomic, and the predicate is exact (`instance_uuid = ours`).
    pub async fn heartbeat_writer_claim(&self, uuid: &str, now_ms: i64) -> anyhow::Result<bool> {
        let uuid = uuid.to_string();
        self.conn
            .call(move |conn| -> Result<bool, rusqlite::Error> {
                let n = conn.execute(
                    "UPDATE writer_claim
                     SET heartbeat_at_ms = ?2
                     WHERE singleton = 1 AND instance_uuid = ?1",
                    rusqlite::params![uuid, now_ms],
                )?;
                Ok(n == 1)
            })
            .await
            .map_err(Into::into)
    }

    /// single-writer — RELEASE the singleton claim on graceful shutdown so
    /// a restart inside the stale window does not brick the producer for the
    /// full `WRITER_CLAIM_STALE_MS` (the smoke-failure lifecycle gap: run
    /// previous CI run). Deletes the row ONLY when `instance_uuid` matches ours —
    /// fencing-safe: a DEPOSED instance (claim already taken over by a
    /// successor with a different uuid) must NEVER release the successor's
    /// claim, so the predicate makes a wrong-uuid release a no-op. Returns
    /// `true` when a row was actually deleted (we held it), `false` otherwise
    /// (already taken over / never held / empty table). Plain DELETE, no tx:
    /// a single statement is atomic and the predicate is exact, mirroring
    /// `heartbeat_writer_claim`. Crash/SIGKILL paths do NOT reach this — they
    /// still rely on the stale-takeover predicate, by design.
    pub async fn release_writer_claim(&self, uuid: &str) -> anyhow::Result<bool> {
        let uuid = uuid.to_string();
        self.conn
            .call(move |conn| -> Result<bool, rusqlite::Error> {
                let n = conn.execute(
                    "DELETE FROM writer_claim
                     WHERE singleton = 1 AND instance_uuid = ?1",
                    rusqlite::params![uuid],
                )?;
                Ok(n == 1)
            })
            .await
            .map_err(Into::into)
    }

    /// single-writer — diagnostic read of the current claim holder:
    /// `Some((instance_uuid, heartbeat_at_ms))` or `None` on a fresh db.
    pub async fn writer_claim_holder(&self) -> anyhow::Result<Option<(String, i64)>> {
        self.conn
            .call(|conn| -> Result<Option<(String, i64)>, rusqlite::Error> {
                conn.query_row(
                    "SELECT instance_uuid, heartbeat_at_ms FROM writer_claim WHERE singleton = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
            })
            .await
            .map_err(Into::into)
    }

    /// The True Phantom Sweeper (Council P0 c1d622bf-b0c).
    /// Sweeps rows that are 'claimed' but whose lease has expired, moving them back to 'failed'
    /// so that the budget is released until a worker actually picks them up again.
    pub async fn sweep_phantom_claims(&self) -> anyhow::Result<usize> {
        Ok(self.sweep_phantom_claims_report().await?.swept)
    }

    /// lease liveness — same sweep, but also counts how many of the reclaimed
    /// rows were REAL in-flight claims (non-null claim_token AND attempts > 0,
    /// i.e. a dispatcher actually claimed them and may have an orphaned council
    /// call/charge in flight) so callers can bump
    /// `lease_expired_during_deliberation` (telemetry invariant telemetry; the orphan
    /// spend is bounded by the reservation release below + p0d's out-of-band
    /// recon cross-check).
    pub async fn sweep_phantom_claims_report(&self) -> anyhow::Result<PhantomSweepReport> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        self.conn
            .call(move |conn| {
                // atomic spend ledger RELEASE-on-sweep: back out the reservation of every
                // expired-lease claim so abandoned work does NOT permanently consume budget.
                // Done in one Immediate tx: aggregate reservations per day bucket FIRST (while
                // the stamps are still present), decrement the ledger per bucket, THEN flip the
                // rows to 'failed' and NULL the stamps. Once 'failed' they cannot be re-swept,
                // so the release is idempotent. MAX(0,...) tolerates a concurrent double-release.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                let per_bucket: Vec<(String, f64)> = {
                    let mut stmt = tx.prepare(
                        "SELECT reserved_day_bucket, COALESCE(SUM(reserved_estimate_usd), 0.0)
                         FROM pending_escalations
                         WHERE status = 'claimed' AND claimed_until_ms < ?1
                           AND reserved_day_bucket IS NOT NULL
                         GROUP BY reserved_day_bucket",
                    )?;
                    let rows = stmt.query_map([now_ms], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                    })?;
                    rows.collect::<Result<Vec<_>, _>>()?
                };

                for (bucket, total) in &per_bucket {
                    tx.execute(
                        "UPDATE spend_ledger
                         SET reserved_usd = MAX(0.0, reserved_usd - ?2)
                         WHERE day_bucket = ?1",
                        rusqlite::params![bucket, total],
                    )?;
                }

                // lease liveness: count the real in-flight subset BEFORE the flip
                // (the flip NULLs nothing token-related, but status changes, so
                // count first inside the same tx for an exact snapshot). A
                // never-started row (no claim_token / attempts=0) is a phantom
                // but NOT a lost deliberation — it must not inflate the counter.
                let in_flight_expired: usize = tx.query_row(
                    "SELECT COUNT(*) FROM pending_escalations
                     WHERE status = 'claimed' AND claimed_until_ms < ?1
                       AND claim_token IS NOT NULL AND attempts > 0",
                    [now_ms],
                    |r| r.get::<_, i64>(0),
                )? as usize;

                let count = tx.execute(
                    "UPDATE pending_escalations
                     SET status = 'failed',
                         last_error = 'Phantom lease expired (worker crashed)',
                         reserved_estimate_usd = NULL,
                         reserved_day_bucket = NULL
                     WHERE status = 'claimed' AND claimed_until_ms < ?1",
                    [now_ms],
                )?;

                tx.commit()?;
                Ok::<PhantomSweepReport, rusqlite::Error>(PhantomSweepReport {
                    swept: count,
                    in_flight_expired,
                })
            })
            .await
            .map_err(Into::into)
    }
}
