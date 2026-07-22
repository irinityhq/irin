//! Directive outbox storage, capability tokens, and terminal-row maintenance.

use rusqlite::OptionalExtension;

use crate::watch::outbox::{AckOutcome, DirectiveOutboxRecord};

use super::{pending_escalations_max_nonterminal, WatchDb};

fn outbox_record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DirectiveOutboxRecord> {
    // NOTE: SELECTs must include claim_handle (18) + worker_provenance (19).
    // Legacy rows fall back so tests + old DBs continue to surface the claim
    // handle as an OpaqueHandleOnly guard. claim_handle column is the lease key.
    Ok(DirectiveOutboxRecord {
        id: r.get(0)?,
        in_response_to: r.get(1)?,
        tenant: r.get(2)?,
        status: r.get(3)?,
        verdict: r.get(4)?,
        authority: r.get(5)?,
        envelope_json: r.get(6)?,
        envelope_json_canonical: r.get(7)?,
        signature_b64: r.get(8)?,
        signing_kid: r.get(9)?,
        council_session_id: r.get(10)?,
        council_cost_usd: r.get(11)?,
        created_at_ms: r.get(12)?,
        expires_at_ms: r.get(13)?,
        acked_at_ms: r.get(14)?,
        claimed_until_ms: r.get(15)?,
        claim_count: r.get(16)?,
        last_error: r.get(17)?,
        // claim_handle lives at column 18 (lease checks only); worker_provenance at 19.
        // Legacy rows (pre-add) fall back to reading the (opaque) claim_handle value from col 18.
        worker_provenance: match r.get::<_, Option<String>>(19)? {
            Some(h) => {
                // Prefer JCS roundtrip where possible; serde_json accepts JCS output.
                if let Ok(guard) =
                    serde_json::from_str::<sovereign_protocol::types::WorkerProvenanceGuard>(&h)
                {
                    Some(guard)
                } else {
                    Some(sovereign_protocol::types::WorkerProvenanceGuard::new_opaque(Some(h)))
                }
            }
            None => match r.get::<_, Option<String>>(18)? {
                // Legacy fallback (col 19 NULL): attempt parse (tests store JSON guard in
                // the old claim_handle slot); on failure treat the value as opaque handle.
                Some(h) => {
                    if let Ok(guard) =
                        serde_json::from_str::<sovereign_protocol::types::WorkerProvenanceGuard>(&h)
                    {
                        Some(guard)
                    } else {
                        Some(sovereign_protocol::types::WorkerProvenanceGuard::new_opaque(Some(h)))
                    }
                }
                None => None,
            },
        },
    })
}

impl WatchDb {
    /// List directive_outbox rows for a tenant (newest first).
    /// Optional status filter (e.g. "staged"). Limit is capped at 200.
    pub async fn list_outbox(
        &self,
        tenant: &str,
        status: Option<&str>,
        limit: i64,
        cursor: Option<(i64, String)>,
    ) -> anyhow::Result<Vec<DirectiveOutboxRecord>> {
        let limit = limit.clamp(1, 201);
        let t = tenant.to_string();
        let s = status.map(|x| x.to_string());
        let c = cursor;
        let rows = self
            .conn
            .call(move |conn| {
                match (s.as_ref(), c.as_ref()) {
                    (Some(st), Some((cursor_ms, cursor_id))) => {
                        let mut stmt = conn.prepare(
                            "SELECT id, in_response_to, tenant, status, verdict, authority,
                                    envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                                    council_session_id, council_cost_usd, created_at_ms, expires_at_ms, acked_at_ms,
                                    claimed_until_ms, claim_count, last_error, claim_handle, worker_provenance
                             FROM directive_outbox
                             WHERE tenant = ?1 AND status = ?2
                               AND (created_at_ms < ?3 OR (created_at_ms = ?3 AND id < ?4))
                             ORDER BY created_at_ms DESC, id DESC
                             LIMIT ?5",
                        )?;
                        let rows = stmt
                            .query_map(
                                [
                                    &t as &dyn rusqlite::ToSql,
                                    st as &dyn rusqlite::ToSql,
                                    cursor_ms,
                                    cursor_id,
                                    &limit,
                                ],
                                outbox_record_from_row,
                            )?
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok::<Vec<DirectiveOutboxRecord>, rusqlite::Error>(rows)
                    }
                    (Some(st), None) => {
                        let mut stmt = conn.prepare(
                            "SELECT id, in_response_to, tenant, status, verdict, authority,
                                    envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                                    council_session_id, council_cost_usd, created_at_ms, expires_at_ms, acked_at_ms,
                                    claimed_until_ms, claim_count, last_error, claim_handle, worker_provenance
                             FROM directive_outbox
                             WHERE tenant = ?1 AND status = ?2
                             ORDER BY created_at_ms DESC, id DESC
                             LIMIT ?3",
                        )?;
                        let rows = stmt
                            .query_map(
                                [&t as &dyn rusqlite::ToSql, st as &dyn rusqlite::ToSql, &limit],
                                outbox_record_from_row,
                            )?
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok::<Vec<DirectiveOutboxRecord>, rusqlite::Error>(rows)
                    }
                    (None, Some((cursor_ms, cursor_id))) => {
                        let mut stmt = conn.prepare(
                            "SELECT id, in_response_to, tenant, status, verdict, authority,
                                    envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                                    council_session_id, council_cost_usd, created_at_ms, expires_at_ms, acked_at_ms,
                                    claimed_until_ms, claim_count, last_error, claim_handle, worker_provenance
                             FROM directive_outbox
                             WHERE tenant = ?1
                               AND (created_at_ms < ?2 OR (created_at_ms = ?2 AND id < ?3))
                             ORDER BY created_at_ms DESC, id DESC
                             LIMIT ?4",
                        )?;
                        let rows = stmt
                            .query_map(
                                [
                                    &t as &dyn rusqlite::ToSql,
                                    cursor_ms,
                                    cursor_id,
                                    &limit,
                                ],
                                outbox_record_from_row,
                            )?
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok::<Vec<DirectiveOutboxRecord>, rusqlite::Error>(rows)
                    }
                    (None, None) => {
                        let mut stmt = conn.prepare(
                            "SELECT id, in_response_to, tenant, status, verdict, authority,
                                    envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                                    council_session_id, council_cost_usd, created_at_ms, expires_at_ms, acked_at_ms,
                                    claimed_until_ms, claim_count, last_error, claim_handle, worker_provenance
                             FROM directive_outbox
                             WHERE tenant = ?1
                             ORDER BY created_at_ms DESC, id DESC
                             LIMIT ?2",
                        )?;
                        let rows = stmt
                            .query_map(
                                [&t as &dyn rusqlite::ToSql, &limit],
                                outbox_record_from_row,
                            )?
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok::<Vec<DirectiveOutboxRecord>, rusqlite::Error>(rows)
                    }
                }
            })
            .await?;
        Ok(rows)
    }

    /// Fetch one directive_outbox row by (tenant, id). Cross-tenant returns None
    /// (enforces tenant scope at the DB seam).
    pub async fn get_outbox(
        &self,
        tenant: &str,
        id: &str,
    ) -> anyhow::Result<Option<DirectiveOutboxRecord>> {
        let t = tenant.to_string();
        let i = id.to_string();
        let row = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, in_response_to, tenant, status, verdict, authority,
                            envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                            council_session_id, council_cost_usd, created_at_ms, expires_at_ms, acked_at_ms,
                            claimed_until_ms, claim_count, last_error, claim_handle, worker_provenance
                     FROM directive_outbox
                     WHERE tenant = ?1 AND id = ?2
                     LIMIT 1",
                )?;
                let mut rows = stmt.query_map([&t, &i], outbox_record_from_row)?;
                rows.next().transpose()
            })
            .await?;
        Ok(row)
    }

    /// Admin ack: set status='acked', acked_at_ms = now for a staged row.
    /// Idempotent on already-acked. Returns NotActionable for dismissed/expired
    /// (caller maps to 409). Tenant-scope mismatches are explicit 403s.
    pub async fn ack_outbox(&self, tenant: &str, id: &str) -> anyhow::Result<AckOutcome> {
        let t = tenant.to_string();
        let i = id.to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let outcome = self
            .conn
            .call(move |conn| -> Result<AckOutcome, rusqlite::Error> {
                // First, check by globally unique id so a wrong X-Tenant-Scope
                // becomes a 403 instead of being disguised as a missing row.
                let current: Option<(String, String)> = conn
                    .query_row(
                        "SELECT status, tenant FROM directive_outbox WHERE id = ?1",
                        [&i],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                match current {
                    None => return Ok(AckOutcome::NotFound { id: i.clone() }),
                    Some((_, row_tenant)) if row_tenant != t => {
                        return Ok(AckOutcome::TenantMismatch { id: i.clone() });
                    }
                    Some((status, _)) if status == "acked" => {
                        return Ok(AckOutcome::Acked {
                            id: i.clone(),
                            tenant: t.clone(),
                            was_already: true,
                        });
                    }
                    Some((status, _)) if status == "dismissed" || status == "expired" => {
                        return Ok(AckOutcome::NotActionable {
                            id: i.clone(),
                            status,
                        });
                    }
                    _ => {}
                }

                // Perform the ack (only if still actionable).
                let n = conn.execute(
                    "UPDATE directive_outbox
                     SET status = 'acked', acked_at_ms = ?1
                     WHERE tenant = ?2 AND id = ?3 AND status = 'staged'",
                    rusqlite::params![now, &t, &i],
                )?;
                if n == 0 {
                    // Race or non-staged — re-check for precise outcome.
                    let final_status: Option<String> = conn
                        .query_row(
                            "SELECT status FROM directive_outbox WHERE tenant = ?1 AND id = ?2",
                            [&t, &i],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match final_status.as_deref() {
                        Some("acked") => Ok(AckOutcome::Acked {
                            id: i.clone(),
                            tenant: t.clone(),
                            was_already: true,
                        }),
                        Some(s @ ("dismissed" | "expired")) => Ok(AckOutcome::NotActionable {
                            id: i.clone(),
                            status: s.to_string(),
                        }),
                        _ => Ok(AckOutcome::NotFound { id: i.clone() }),
                    }
                } else {
                    Ok(AckOutcome::Acked {
                        id: i.clone(),
                        tenant: t.clone(),
                        was_already: false,
                    })
                }
            })
            .await?;
        Ok(outcome)
    }

    /// Test-only:
    /// wedge the single-connection tokio-rusqlite worker thread for `sleep`
    /// real-time duration by dispatching a `std::thread::sleep` closure via
    /// `conn.call`. All subsequent `conn.call`s queue behind it FIFO on the
    /// dedicated worker. Lets tests drive `retry_pending_hard_kill_once`
    /// into the `RetryOutcome::TimedOut` arm deterministically when paired
    /// with `tokio::test(start_paused = true)` + `tokio::time::advance` past
    /// `RETRY_DB_BUDGET`, without forging a `tokio::time::error::Elapsed`
    /// directly. `#[doc(hidden)] pub` so the integration test crate (which
    /// can't see `#[cfg(test)]` items) can reach it.
    #[doc(hidden)]
    pub async fn test_block_worker(&self, sleep: std::time::Duration) {
        let _ = self
            .conn
            .call(move |_conn| {
                std::thread::sleep(sleep);
                Ok::<(), rusqlite::Error>(())
            })
            .await;
    }

    // -------------------------------------------------------------------------
    // Phase 1 CDC / transactional-outbox weld (P0-2/4, plan §5 Step 2, producer-seam §2/3)
    // All outside hot audit path (insert_fire / fire_pipeline). Consumer dedup is source
    // of truth for exactly-once. Boot re-scan uses same helpers (idempotent).
    // C11 derivation happens in caller (sweep) using safe_tenant_token + causal (read-only refs).
    // -------------------------------------------------------------------------

    /// Insert (or dedup) a pending escalation from a committed watch_fires row.
    /// Uses ON CONFLICT (tenant, sentinel_name, causal_fire_id) DO NOTHING.
    /// Returns true iff a new row was created (not a duplicate causal fire).
    /// Matches simulate_causal_sweep_enqueue harness exactly (test continues to prove collapse).
    /// Does not mutate watch_fires or touch 200 ms budget path.
    /// replay_epoch: shadow/test tag for replay bomb guard (0 for legacy/test inserts; future armed producer uses >0 so executor can refuse pre-arm rows).
    ///
    /// §7 item 9 backpressure: refuses (Err, classified TRANSIENT by the CDC
    /// sweep) when the tenant's NON-TERMINAL row count has reached
    /// [`pending_escalations_max_nonterminal`]. A duplicate causal at the cap
    /// still reports benign dedup (Ok(false)) — dedup is not new depth.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_pending_escalation_with_causal_dedup(
        &self,
        id: &str,
        tenant: &str,
        sentinel_name: &str,
        envelope_json: &str,
        causal_fire_id: &str,
        created_at_ms: i64,
        replay_epoch: i64,
    ) -> anyhow::Result<bool> {
        self.insert_pending_escalation_with_causal_dedup_capped(
            id,
            tenant,
            sentinel_name,
            envelope_json,
            causal_fire_id,
            created_at_ms,
            replay_epoch,
            pending_escalations_max_nonterminal(),
        )
        .await
    }

    /// Cap-parameterized body of the above (public so the cap gate is
    /// directly testable without racing env vars across parallel tests).
    ///
    /// A `max_nonterminal <= 0` refuses every NEW causal (duplicates still
    /// report dedup) — an explicit argument is honored, not clamped; the
    /// production path can never pass one (`pending_escalations_max_nonterminal`
    /// filters non-positive env values back to the default).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_pending_escalation_with_causal_dedup_capped(
        &self,
        id: &str,
        tenant: &str,
        sentinel_name: &str,
        envelope_json: &str,
        causal_fire_id: &str,
        created_at_ms: i64,
        replay_epoch: i64,
        max_nonterminal: i64,
    ) -> anyhow::Result<bool> {
        enum EnqueueDecision {
            Inserted,
            Deduped,
            CapRefused { depth: i64 },
        }
        let decision = self
            .conn
            .call({
                let id_s = id.to_string();
                let t = tenant.to_string();
                let s = sentinel_name.to_string();
                let e = envelope_json.to_string();
                let c = causal_fire_id.to_string();
                move |conn| -> Result<EnqueueDecision, rusqlite::Error> {
                    // Backpressure gate: count NON-TERMINAL rows (the set the
                    // hourly prune never touches — see prune_terminal_rows).
                    // Same connection, so the count and the insert cannot
                    // interleave with another producer write.
                    //
                    // 'failed' counts DELIBERATELY: failed rows are retryable
                    // live work (claim SELECT takes queued|failed) with no
                    // attempts ceiling on this leg — dead-lettering after N
                    // attempts would silently drop an escalation, which this
                    // subsystem never does. Excluding them would let new
                    // enqueues stack on top of an unbounded failed backlog,
                    // reopening the growth hole this cap closes. A tenant
                    // pinned at the cap by permanently-failing rows is the
                    // designed outcome: the sweep stalls loudly (transient
                    // refusal below) until the operator fixes the council
                    // path, raises the cap, or drains the queue.
                    let depth: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM pending_escalations
                         WHERE tenant = ?1
                           AND status IN ('queued', 'claimed', 'council_response_staged', 'failed')",
                        rusqlite::params![t],
                        |r| r.get(0),
                    )?;
                    if depth >= max_nonterminal {
                        // A causal that is already enqueued is benign dedup even
                        // at the cap — it adds no depth and must not stall the
                        // sweep cursor behind a full queue.
                        let dup: bool = conn.query_row(
                            "SELECT EXISTS(
                                 SELECT 1 FROM pending_escalations
                                 WHERE tenant = ?1 AND sentinel_name = ?2 AND causal_fire_id = ?3
                             )",
                            rusqlite::params![t, s, c],
                            |r| r.get(0),
                        )?;
                        return Ok(if dup {
                            EnqueueDecision::Deduped
                        } else {
                            EnqueueDecision::CapRefused { depth }
                        });
                    }
                    let rows_affected = conn.execute(
                        "INSERT INTO pending_escalations
                         (id, tenant, sentinel_name, envelope_json, status, created_at_ms, causal_fire_id, attempts, replay_epoch)
                         VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, 0, ?7)
                         ON CONFLICT (tenant, sentinel_name, causal_fire_id) DO NOTHING",
                        rusqlite::params![id_s, t, s, e, created_at_ms, c, replay_epoch],
                    )?;
                    Ok(if rows_affected > 0 {
                        EnqueueDecision::Inserted
                    } else {
                        EnqueueDecision::Deduped
                    })
                }
            })
            .await?;
        match decision {
            EnqueueDecision::Inserted => Ok(true),
            EnqueueDecision::Deduped => Ok(false),
            EnqueueDecision::CapRefused { depth } => Err(anyhow::anyhow!(
                "pending_escalations backpressure: {depth} non-terminal rows >= cap {max_nonterminal} \
                 for this tenant — enqueue refused (transient: sweep stalls and retries; \
                 raise WATCH_PENDING_ESCALATIONS_MAX_NONTERMINAL or drain the queue)"
            )),
        }
    }

    /// Claim up to `limit` staged directives for the tenant.
    /// Returns the records and their new claim_handles.
    pub async fn claim_outbox(
        &self,
        tenant: &str,
        limit: u32,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> anyhow::Result<Vec<DirectiveOutboxRecord>> {
        let t = tenant.to_string();
        self.conn
            .call(move |conn| {
                // BEGIN IMMEDIATE: this tx always writes (both sweeps + the claim UPDATEs),
                // so take the write lock up front. Avoids a DEFERRED read->write upgrade
                // race where a second worker could claim a count==MAX row between this
                // worker's sweep-UPDATE and its claim-SELECT (Council T21d H3). Both sweeps
                // and the SELECT below run inside this single transaction.
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                // T21d worker-leg delivery-attempt fence: a staged directive that has been
                // claimed (and nack'd back to staged, or had its lease expire under a crashing
                // worker) DIRECTIVE_MAX_DELIVERY_ATTEMPTS times is a poison directive (fails
                // verify/parse every tick) or a flapping worker. The lease half of the v0.2
                // outbox hardening (claimed_until_ms) was wired; this enforces the
                // schema-reserved-but-unused claim_count half. Sweep it to the terminal
                // 'expired' status -- reusing the T21c terminal state + its ack/nack
                // NotActionable guards; a distinct 'dead_lettered' status would need a
                // money-table CHECK rebuild, deferred (see T21d spec). The last_error STAMP
                // (Council T21d H1) records trigger + the ceiling-at-death + the preserved root
                // cause: 'max_delivery_attempts(N); root=<prior or timeout_or_crash>'. This
                // disambiguates a dead-letter from a TTL-expiry at the row level (the audit hole
                // that was B's only real argument) and stays interpretable after the env-lifted
                // ceiling drifts -- the MAX that killed THIS row is recorded, not the current one.
                // SQLite NULL-safe: the root is COALESCE'd inside the concat (|| with any NULL
                // operand yields NULL). Distinct directive_max_delivery_exceeded_total counter
                // marks attempt-exhaustion vs clock-TTL. Same chokepoint, same unleased-only
                // predicate as the TTL sweep (an in-flight leased row is left to the lease/ack
                // machinery, not yanked mid-exec). Fail-safe -- only REFUSES further dispatch,
                // never adds spend. Bounds the loop by ATTEMPTS, tighter than the <=TTL window.
                //
                // Ordering (Council T21d H2): the attempt-sweep runs BEFORE the TTL sweep so a
                // row crossing BOTH the ceiling and its TTL on the same tick is attributed to
                // delivery-exceeded (the poison signal this fence exists to surface), not
                // silently absorbed by the TTL sweep flipping it to 'expired' first.
                let max_delivery_attempts =
                    crate::watch::dispatcher::directive_max_delivery_attempts();
                let max_delivery_exceeded = tx.execute(
                    "UPDATE directive_outbox
                        SET status = 'expired',
                            last_error = 'max_delivery_attempts(' || ?2 || '); root='
                                         || COALESCE(last_error, 'timeout_or_crash')
                      WHERE tenant = ?1 AND status = 'staged' AND claim_count >= ?2
                        AND (claimed_until_ms IS NULL OR claimed_until_ms < ?3)",
                    rusqlite::params![t, max_delivery_attempts, now_ms],
                )?;

                // A4a/T21 worker-leg dispatch fence: a staged directive whose absolute
                // TTL (expires_at_ms, set at stage time to now+90s) has elapsed must never
                // be claimed/dispatched -- otherwise real spend can occur after the
                // authorization window closed. The escalation/council leg already has a
                // monotonic in-flight fence (dispatcher.rs); this closes the directive_outbox
                // leg, which previously had no expiry guard. Flip expired rows to the
                // schema-reserved 'expired' status in the SAME tx so the SELECT below cannot
                // see them and there is an audit trail (idx_do_expires backs this sweep).
                // Wall-clock compare is correct here: expires_at_ms is an absolute producer
                // deadline, not an elapsed-time measure. Fail-safe -- a mis-set past TTL only
                // REFUSES dispatch (no spend), never adds one. Only UNLEASED expired rows are
                // swept (mirrors the claim predicate), so an in-flight leased row is left to
                // the lease/ack machinery + the council-leg fence rather than yanked mid-exec.
                // Clock note: wall-clock compare assumes a monotone-ish host clock. An NTP
                // backward step is the canary-kill mode this fence's counter
                // (directive_ttl_expired_total) exists to surface.
                let ttl_expired = tx.execute(
                    "UPDATE directive_outbox
                        SET status = 'expired',
                            last_error = COALESCE(last_error, 'ttl_expired_before_dispatch')
                      WHERE tenant = ?1 AND status = 'staged' AND expires_at_ms <= ?2
                        AND (claimed_until_ms IS NULL OR claimed_until_ms < ?2)",
                    rusqlite::params![t, now_ms],
                )?;

                // Select IDs that are free to be claimed. Backoff logic handles failed rows
                // by leaving them in staged but claimed_until_ms=NULL.
                // We'll prioritize oldest created_at_ms.
                let mut stmt = tx.prepare(
                    "SELECT id FROM directive_outbox
                     WHERE tenant = ?1
                       AND status = 'staged'
                       AND (claimed_until_ms IS NULL OR claimed_until_ms < ?2)
                     ORDER BY created_at_ms ASC, id ASC
                     LIMIT ?3"
                )?;
                let ids: Vec<String> = stmt.query_map(rusqlite::params![t, now_ms, limit], |r| r.get(0))?
                    .filter_map(Result::ok).collect();
                drop(stmt);

                let mut records = Vec::new();
                let new_claimed_until = now_ms + lease_duration_ms;

                for id in ids {
                    use sha2::Digest;
                    let mut hasher = sha2::Sha256::new();
                    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
                    hasher.update(nanos.to_le_bytes());
                    hasher.update(t.as_bytes());
                    hasher.update(id.as_bytes());
                    let claim_handle = hex::encode(hasher.finalize());

                    // T21d H6 (Council pinned task — fix when the real Worker execute leg ships):
                    // claim_count counts CLAIMS, not FAILURES. Today every claim of a
                    // non-recommend directive is followed by a verify gate that either acks
                    // (terminal) or nacks (failure) in the same tick, so claim_count ~= failures
                    // and the T21d ceiling is correct. Once execute can run real long work, a
                    // LEGITIMATE directive whose execution exceeds its lease is reclaimed on
                    // lease-expiry -> claim_count++ with ZERO failures -> after MAX lease churns
                    // it is FALSE-dead-lettered (a real money directive killed mid-flight). Fix
                    // before execute ships: key the ceiling on a nack_count (incremented only on
                    // real nack), or guarantee lease >> max_execute_time AND that
                    // lease-expiry-without-nack does not feed the ceiling. See T21d spec H6 +
                    // the H5 post-execute idempotency fence (H7b) — both are execute-leg gates.
                    tx.execute(
                        "UPDATE directive_outbox
                         SET claimed_until_ms = ?1, claim_count = claim_count + 1, claim_handle = ?2
                         WHERE tenant = ?3 AND id = ?4",
                        rusqlite::params![new_claimed_until, claim_handle, t, id],
                    )?;

                    let rec = tx.query_row(
                        "SELECT id, in_response_to, tenant, status, verdict, authority,
                                envelope_json, envelope_json_canonical, signature_b64, signing_kid,
                                council_session_id, council_cost_usd, created_at_ms, expires_at_ms, acked_at_ms,
                                claimed_until_ms, claim_count, last_error, claim_handle, worker_provenance
                         FROM directive_outbox WHERE tenant = ?1 AND id = ?2",
                        rusqlite::params![t, id],
                        outbox_record_from_row,
                    )?;
                    records.push(rec);
                }
                tx.commit()?;
                // Surface the fence's effect (observability is the safety instrument
                // for a fail-safe sweep). Bump only after a successful commit. NOTE: the
                // bump is outside the tx commit edge, so a crash between commit() and here
                // undercounts -- the metric is approximate, never authoritative.
                if ttl_expired > 0 {
                    crate::watch::dispatcher::bump_directive_ttl_expired(ttl_expired as u64);
                }
                if max_delivery_exceeded > 0 {
                    crate::watch::dispatcher::bump_directive_max_delivery_exceeded(
                        max_delivery_exceeded as u64,
                    );
                }
                Ok::<_, rusqlite::Error>(records)
            })
            .await
            .map_err(Into::into)
    }

    pub async fn heartbeat_outbox(
        &self,
        tenant: &str,
        id: &str,
        claim_handle: &str,
        now_ms: i64,
        extension_ms: i64,
    ) -> anyhow::Result<AckOutcome> {
        let t = tenant.to_string();
        let i = id.to_string();
        let handle = claim_handle.to_string();

        self.conn
            .call(move |conn| -> Result<AckOutcome, rusqlite::Error> {
                let current: Option<(String, String, Option<String>)> = conn
                    .query_row(
                        "SELECT status, tenant, claim_handle FROM directive_outbox WHERE id = ?1",
                        [&i],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;

                match current {
                    None => return Ok(AckOutcome::NotFound { id: i.clone() }),
                    Some((_, row_tenant, _)) if row_tenant != t => {
                        return Ok(AckOutcome::TenantMismatch { id: i.clone() });
                    }
                    Some((status, _, row_handle)) => {
                        if status != "staged" {
                            return Ok(AckOutcome::NotActionable { id: i.clone(), status });
                        }
                        if row_handle.as_deref() != Some(&handle) {
                            return Ok(AckOutcome::InvalidHandle { id: i.clone() });
                        }
                    }
                }

                let new_claimed_until = now_ms + extension_ms;
                conn.execute(
                    "UPDATE directive_outbox SET claimed_until_ms = ?1 WHERE tenant = ?2 AND id = ?3",
                    rusqlite::params![new_claimed_until, t, i],
                )?;

                Ok(AckOutcome::Acked { id: i.clone(), tenant: t.clone(), was_already: false })
            })
            .await
            .map_err(Into::into)
    }

    pub async fn worker_ack_outbox(
        &self,
        tenant: &str,
        id: &str,
        claim_handle: &str,
        worker_provenance: sovereign_protocol::types::WorkerProvenanceGuard,
    ) -> anyhow::Result<AckOutcome> {
        let t = tenant.to_string();
        let i = id.to_string();
        let handle = claim_handle.to_string();
        // Serialize with JCS for canonical storage (matches test harness + provenance contract).
        let prov_json = sovereign_protocol::jcs::to_jcs_string(&worker_provenance)
            .unwrap_or_else(|_| serde_json::to_string(&worker_provenance).unwrap_or_default());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        self.conn
            .call(move |conn| -> Result<AckOutcome, rusqlite::Error> {
                let current: Option<(String, String, Option<String>)> = conn
                    .query_row(
                        "SELECT status, tenant, claim_handle FROM directive_outbox WHERE id = ?1",
                        [&i],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;

                match current {
                    None => return Ok(AckOutcome::NotFound { id: i.clone() }),
                    Some((_, row_tenant, _)) if row_tenant != t => {
                        return Ok(AckOutcome::TenantMismatch { id: i.clone() });
                    }
                    Some((status, _, _)) if status == "acked" => {
                        return Ok(AckOutcome::Acked { id: i.clone(), tenant: t.clone(), was_already: true });
                    }
                    Some((status, _, row_handle)) => {
                        if status != "staged" {
                            return Ok(AckOutcome::NotActionable { id: i.clone(), status });
                        }
                        if row_handle.as_deref() != Some(&handle) {
                            return Ok(AckOutcome::InvalidHandle { id: i.clone() });
                        }
                    }
                }

                // Set worker_provenance on success (VerifiedExact for internal worker path; or passed guard).
                // claim_handle column is retained unchanged for lease/inflight checks.
                conn.execute(
                    "UPDATE directive_outbox SET status = 'acked', acked_at_ms = ?1, worker_provenance = ?2 WHERE tenant = ?3 AND id = ?4",
                    rusqlite::params![now_ms, prov_json, t, i],
                )?;
                Ok(AckOutcome::Acked { id: i.clone(), tenant: t.clone(), was_already: false })
            })
            .await
            .map_err(Into::into)
    }

    pub async fn nack_outbox(
        &self,
        tenant: &str,
        id: &str,
        claim_handle: &str,
        error_reason: &str,
    ) -> anyhow::Result<AckOutcome> {
        let t = tenant.to_string();
        let i = id.to_string();
        let handle = claim_handle.to_string();
        let err = error_reason.to_string();

        self.conn
            .call(move |conn| -> Result<AckOutcome, rusqlite::Error> {
                let current: Option<(String, String, Option<String>)> = conn
                    .query_row(
                        "SELECT status, tenant, claim_handle FROM directive_outbox WHERE id = ?1",
                        [&i],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;

                match current {
                    None => return Ok(AckOutcome::NotFound { id: i.clone() }),
                    Some((_, row_tenant, _)) if row_tenant != t => {
                        return Ok(AckOutcome::TenantMismatch { id: i.clone() });
                    }
                    Some((status, _, row_handle)) => {
                        if status != "staged" {
                            return Ok(AckOutcome::NotActionable { id: i.clone(), status });
                        }
                        if row_handle.as_deref() != Some(&handle) {
                            return Ok(AckOutcome::InvalidHandle { id: i.clone() });
                        }
                    }
                }

                conn.execute(
                    "UPDATE directive_outbox SET claimed_until_ms = NULL, last_error = ?1 WHERE tenant = ?2 AND id = ?3",
                    rusqlite::params![err, t, i],
                )?;

                // Return Acked internally to signal success of the Nack operation.
                Ok(AckOutcome::Acked { id: i.clone(), tenant: t.clone(), was_already: false })
            })
            .await
            .map_err(Into::into)
    }

    pub async fn get_tenant_tokens(&self, tenant: String) -> anyhow::Result<Vec<(String, String)>> {
        let rows = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT token, authority FROM tenant_policy_tokens WHERE tenant = ?1",
                )?;
                let rows: Vec<(String, String)> = stmt
                    .query_map(rusqlite::params![tenant], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<Vec<(String, String)>, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }

    pub async fn add_capability_token(
        &self,
        tenant: String,
        token: String,
        authority: String,
    ) -> anyhow::Result<()> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO tenant_policy_tokens (tenant, token, authority) VALUES (?1, ?2, ?3)",
                    rusqlite::params![tenant, token, authority],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn is_capability_token_valid(
        &self,
        tenant: &str,
        token: &str,
        authority: &str,
    ) -> anyhow::Result<bool> {
        let t = tenant.to_string();
        let tok = token.to_string();
        let auth = authority.to_string();
        let valid = self
            .conn
            .call(move |conn| {
                Ok::<_, rusqlite::Error>(crate::watch::dispatcher::is_capability_token_valid(
                    conn, &t, &tok, &auth,
                ))
            })
            .await?;
        Ok(valid)
    }

    pub async fn remove_capability_token(
        &self,
        tenant: String,
        token: String,
    ) -> anyhow::Result<()> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM tenant_policy_tokens WHERE tenant = ?1 AND token = ?2",
                    rusqlite::params![tenant, token],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Prunes terminal rows from `pending_escalations` and `directive_outbox`
    /// older than the retention window (`created_at_ms`). Returns
    /// `(pe_pruned, do_pruned, aged_staged_blocked)`.
    ///
    /// W3 item 1 (Review): the old version deleted the PARENT
    /// (pending_escalations) before the CHILD with a child predicate that
    /// excluded `staged` — so an aged `outbox_written` parent whose Act
    /// directive was still `staged` (the steady state with no acking worker)
    /// orphaned the child, the DEFERRABLE FK fired at COMMIT, and the WHOLE prune
    /// rolled back. Retention was dead from day 8.
    ///
    /// The fix (Council improved my orphan-delete to NOT EXISTS — never delete a
    /// live staged child):
    /// 1. delete independently-terminal children, age-gated. The old predicate
    ///    `('acked','nacked','expired')` was doubly wrong — `nacked` is
    ///    impossible (not in the directive_outbox CHECK, never SET) and
    ///    `dismissed` (a real terminal child state) was missing. Now
    ///    `('acked','expired','dismissed')`.
    /// 2. delete terminal, aged, NOW-CHILDLESS parents (NOT EXISTS over the
    ///    composite FK). A parent whose child is still `staged` past retention is
    ///    deliberately NOT pruned — that is the correct, safe outcome, and it is
    ///    surfaced as the `aged_staged_blocked` alarm count (P1: loud retention
    ///    signal, NOT a silent-loss counter; the dead-letter transition for that
    ///    stuck-staged case is fast-follow #28).
    ///
    /// BEGIN IMMEDIATE (P2): take the write lock up front, matching arm_audit's
    /// tx behaviour, to avoid a mid-tx SQLITE_BUSY upgrade. No orphan is ever
    /// created, so the deferred FK never fires → the deadlock is gone.
    pub async fn prune_terminal_rows(
        &self,
        older_than_ms: i64,
    ) -> anyhow::Result<(usize, usize, usize)> {
        self.conn
            .call(
                move |conn| -> Result<(usize, usize, usize), rusqlite::Error> {
                    let tx =
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                    // step 1 — independently-terminal children, age-gated.
                    let do_pruned = tx.execute(
                        "DELETE FROM directive_outbox
                     WHERE created_at_ms < ?1
                       AND status IN ('acked', 'expired', 'dismissed')",
                        rusqlite::params![older_than_ms],
                    )?;

                    // step 2 — terminal, aged, now-childless parents (composite FK).
                    let pe_pruned = tx.execute(
                        "DELETE FROM pending_escalations
                     WHERE status IN ('outbox_written', 'dismissed', 'expired', 'dead_lettered')
                       AND created_at_ms < ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM directive_outbox c
                           WHERE c.tenant = pending_escalations.tenant
                             AND c.in_response_to = pending_escalations.id
                       )",
                        rusqlite::params![older_than_ms],
                    )?;

                    // P1 alarm signal — aged terminal parents still pinned by a live
                    // (non-terminal, i.e. staged) child past retention. These did NOT
                    // prune (correct), but their unbounded accumulation is the
                    // retention-health signal the caller warns on.
                    let aged_staged_blocked: usize = tx.query_row(
                        "SELECT COUNT(*) FROM pending_escalations p
                     WHERE p.status IN ('outbox_written', 'dismissed', 'expired', 'dead_lettered')
                       AND p.created_at_ms < ?1
                       AND EXISTS (
                           SELECT 1 FROM directive_outbox c
                           WHERE c.tenant = p.tenant
                             AND c.in_response_to = p.id
                       )",
                        rusqlite::params![older_than_ms],
                        |r| r.get::<_, i64>(0),
                    )? as usize;

                    tx.commit()?;
                    Ok((pe_pruned, do_pruned, aged_staged_blocked))
                },
            )
            .await
            .map_err(Into::into)
    }
}
