//! Recovery of one durable `council_response_staged` row into a signed outbox row.
//!
//! Phases stay separate: arm gate, envelope parse, session/cost validation,
//! proposal enrich, sign, then persist. Claim/stage is a different crash
//! boundary and is not folded into this function. A clock-skew refusal parks
//! the row (`SkewHeld`); it is not a lost lease. OCC no-rows handling stays
//! on the dispatcher claim path.

use crate::watch::capability::is_capability_token_valid;
use crate::watch::header_segment::safe_tenant_token;
use crate::watch::outbox::{
    outbox_insert_with_skew_normalize, DirectiveOutboxRow, OutboxAuditEvent,
};
use crate::watch::stage_ttl::directive_stage_ttl_ms;
use base64::Engine;
use rusqlite::OptionalExtension;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const COST_CEILING_USD: f64 = 1_000_000.0;

/// Real recovery body for a single `council_response_staged` row (narrow seam).
///
/// - Parses durable `{body, headers}` from `council_response_json`.
/// - Validates `x-council-session-id` (non-empty) and `x-total-cost-usd` (finite, >= 0).
/// - Builds `PersistedDirectivePayloadV1` (enriched with session/cost from headers).
/// - Computes canonical JSON and **real Ed25519 signature** using the passed key (P0-epsilon).
/// - Calls `outbox_insert_with_skew_normalize` inside BEGIN IMMEDIATE + defer pragma.
/// - Updates `pending_escalations` with composite `WHERE tenant = ? AND id = ?`.
/// - Idempotent restart (same (tenant, in_response_to)) is handled by the helper returning Ok(existing_id).
pub(crate) fn recover_council_response_staged(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    council_response_json: &str,
    audit_sink: &mut Vec<WatchPhase3AuditEvent>,
    signing_key: crate::keymgmt::DirectiveSigningKey, // owned for crossing conn.call (P0-epsilon)
) -> anyhow::Result<(RecoveryOutcome, Vec<WatchPhase3AuditEvent>)> {
    // Phase pipeline (0..6). Bodies live in the phase fns below — bit-identical to the
    // prior monolithic path: same guards, dead-letter reasons, signature seam, tx shape,
    // and event order.
    match check_recover_arm_gate(conn, escalation_id, tenant, audit_sink)? {
        RecoverStep::Done(out) => return Ok(out),
        RecoverStep::Next(()) => {}
    }

    let (body, headers) =
        match parse_staged_envelope(conn, escalation_id, tenant, council_response_json)? {
            RecoverStep::Done(out) => return Ok(out),
            RecoverStep::Next(v) => v,
        };

    let (session_id, cost_usd) =
        match validate_staged_session_cost(conn, escalation_id, tenant, &body, &headers)? {
            RecoverStep::Done(out) => return Ok(out),
            RecoverStep::Next(v) => v,
        };

    let enriched = match parse_and_enrich_proposal(
        conn,
        escalation_id,
        tenant,
        &body,
        session_id,
        cost_usd,
    )? {
        RecoverStep::Done(out) => return Ok(out),
        RecoverStep::Next(v) => v,
    };

    let (canonical, sig_b64) = sign_recovered_directive(&signing_key, &enriched.persisted);
    let (row, now_ms) = build_signed_outbox_row(
        escalation_id,
        tenant,
        &enriched,
        canonical,
        sig_b64,
        signing_key.kid(),
    );

    persist_signed_directive_tx(
        conn,
        escalation_id,
        tenant,
        audit_sink,
        row,
        enriched.verdict,
        now_ms,
    )
}

/// Control flow for recovery phase extraction.
/// `Done` finishes the recovery (ArmHeld / DeadLettered / SkewHeld / success).
/// `Next` carries intermediate state into the following phase.
enum RecoverStep<T> {
    Next(T),
    Done((RecoveryOutcome, Vec<WatchPhase3AuditEvent>)),
}

/// Owned intermediate after phase 3 (parse + enrich). Fields mirror what the
/// monolithic path held in locals between enrichment and outbox row build.
struct EnrichedRecoveredProposal {
    persisted: Value,
    envelope_json: String,
    authority_str: String,
    verdict: &'static str,
    session_id: String,
    cost_usd: f64,
}

/// 0. ARM GATE (disarm re-check at the sign seam).
fn check_recover_arm_gate(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    audit_sink: &mut Vec<WatchPhase3AuditEvent>,
) -> anyhow::Result<RecoverStep<()>> {
    // 0. ARM GATE (disarm re-check at the sign seam). Every path through this
    // function ends in a SIGNED outbox row (Act *and* Dismiss), so signing is
    // gated on a currently-valid hardware-attested arm — the SAME decision the
    // spend reserve makes (attest::verify_arm_row, shared, never a mirror).
    // Rows staged under a prior arm are therefore NOT signable after disarm:
    // boot hydration on a disarmed box parks everything, and a disarm that
    // races the live tick between claim and recovery parks that row too.
    //
    // Refusal is `ArmHeld` — the SkewHeld shape (Review Option
    // A precedent): the row stays `council_response_staged` (NOT terminal, NOT
    // dead-lettered — the council work product is intact), no directive row is
    // written, nothing is mutated. It self-heals on the first sweep under a
    // valid arm. Fail-closed: no arm row, no boot registry, bad signature,
    // expired window — all park identically.
    {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let arm_row = crate::watch::db::read_active_arm_row(conn)?;
        let refusal: Option<&'static str> = match arm_row {
            None => Some("no_active_arm"),
            Some(row) => crate::watch::attest::verify_arm_row(
                &row,
                crate::watch::attest::boot_registry().as_deref(),
                now_ms,
            )
            .err(),
        };
        if let Some(reason) = refusal {
            let event = WatchPhase3AuditEvent::RecoveryArmHeld {
                escalation_id: escalation_id.to_string(),
                tenant: tenant.to_string(),
                reason: reason.to_string(),
            };
            audit_sink.push(event.clone());
            return Ok(RecoverStep::Done((RecoveryOutcome::ArmHeld, vec![event])));
        }
    }
    Ok(RecoverStep::Next(()))
}

/// 1. Parse durable envelope.
fn parse_staged_envelope(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    council_response_json: &str,
) -> anyhow::Result<RecoverStep<(String, serde_json::Map<String, Value>)>> {
    // 1. Parse durable envelope
    let envelope: Value = match serde_json::from_str(council_response_json) {
        Ok(v) => v,
        Err(_) => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "malformed durable envelope (not valid JSON)",
            )
            .map(RecoverStep::Done);
        }
    };
    let body = match envelope.get("body").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "missing body in durable envelope",
            )
            .map(RecoverStep::Done);
        }
    };
    let headers = match envelope.get("headers").and_then(|v| v.as_object()) {
        Some(v) => v.clone(),
        None => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "missing headers in durable envelope",
            )
            .map(RecoverStep::Done);
        }
    };
    Ok(RecoverStep::Next((body, headers)))
}

/// 2. Validate session + cost (strict), including exact-one-fence rule.
fn validate_staged_session_cost(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    body: &str,
    headers: &serde_json::Map<String, Value>,
) -> anyhow::Result<RecoverStep<(String, f64)>> {
    // 2. Validate session + cost (strict)
    let session_id = match headers
        .get("x-council-session-id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(v) => v.to_string(),
        None => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "missing or empty x-council-session-id in durable envelope",
            )
            .map(RecoverStep::Done);
        }
    };

    let cost_usd = match headers
        .get("x-total-cost-usd")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|c: &f64| c.is_finite() && *c >= 0.0 && *c < COST_CEILING_USD)
    {
        Some(v) => v,
        None => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "invalid, non-finite, negative or excessive council cost in durable envelope",
            )
            .map(RecoverStep::Done);
        }
    };

    // Enforce exact-one-fence for fenced council outputs (live machine-output contract).
    // Replaces previous first-fence-wins. Multiple fences → dead-letter (clear audit signal).
    // Legacy raw JSON (0 fences) remains compatible for old staged rows/tests.
    if body.matches("```").count() > 2 {
        return dead_letter_staged_row(
            conn,
            escalation_id,
            tenant,
            "multiple JSON fences in council-triage response (exactly one fence required by machine-output contract)",
        )
        .map(RecoverStep::Done);
    }

    Ok(RecoverStep::Next((session_id, cost_usd)))
}

/// 3. Parse proposal body + build enriched persisted payload.
fn parse_and_enrich_proposal(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    body: &str,
    session_id: String,
    cost_usd: f64,
) -> anyhow::Result<RecoverStep<EnrichedRecoveredProposal>> {
    // 3. Parse proposal body + build enriched persisted payload.
    // council-triage's machine-output contract returns a ```json fence; raw JSON
    // remains accepted for older staged rows and unit fixtures.
    let proposal: Value = match parse_proposal_body(body) {
        Ok(v) => v,
        Err(e) => {
            // Distinguishable reason (malformed JSON vs strict dup-key intake reject)
            // so the dead-letter audit row records WHICH gate fired (canary observability).
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                &format!("proposal intake rejected: {e}"),
            )
            .map(RecoverStep::Done);
        }
    };
    let proposal_obj = match proposal.as_object() {
        Some(v) => v,
        None => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "proposal in durable envelope is not a JSON object",
            )
            .map(RecoverStep::Done);
        }
    };
    if proposal_obj.get("schema").and_then(Value::as_str) != Some("irin.directive.proposal.v1") {
        return dead_letter_staged_row(
            conn,
            escalation_id,
            tenant,
            "wrong schema in proposal fence (expected irin.directive.proposal.v1)",
        )
        .map(RecoverStep::Done);
    }
    if proposal_obj.get("in_response_to").and_then(Value::as_str) != Some(escalation_id) {
        return dead_letter_staged_row(
            conn,
            escalation_id,
            tenant,
            "proposal in_response_to does not match escalation id",
        )
        .map(RecoverStep::Done);
    }
    let authority_str = match proposal_obj.get("authority").and_then(Value::as_str) {
        Some(a) => a,
        None => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "proposal missing authority",
            )
            .map(RecoverStep::Done);
        }
    };

    if authority_str != "recommend" {
        if authority_str == "prepare" || authority_str == "execute" {
            let token = proposal_obj
                .get("capability_token")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Planned outbox id is deterministic: same formula as insert path.
            // Execute requires directive bind; prepare ignores claimed id.
            let planned_directive_id =
                format!("{}-rec-{}", safe_tenant_token(tenant), escalation_id);
            let claimed = if authority_str == "execute" {
                Some(planned_directive_id.as_str())
            } else {
                None
            };
            if !is_capability_token_valid(&*conn, tenant, token, authority_str, claimed) {
                return dead_letter_staged_row(
                    conn,
                    escalation_id,
                    tenant,
                    &format!(
                        "capability token verification failed for authority '{}'",
                        authority_str
                    ),
                )
                .map(RecoverStep::Done);
            }
        } else {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "proposal authority must be recommend, prepare, or execute",
            )
            .map(RecoverStep::Done);
        }
    }
    let verdict = match proposal_obj.get("verdict").and_then(Value::as_str) {
        Some("Act") => "Act",
        Some("Dismiss") => "Dismiss",
        _ => {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "invalid verdict in proposal (must be Act or Dismiss)",
            )
            .map(RecoverStep::Done);
        }
    };

    // rationale is always required (Act and Dismiss) per spec §3.2.1
    if proposal_obj
        .get("rationale")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
    {
        return dead_letter_staged_row(
            conn,
            escalation_id,
            tenant,
            "missing or empty rationale in proposal (required for both Act and Dismiss)",
        )
        .map(RecoverStep::Done);
    }

    if verdict == "Act" {
        let scope_tenant = proposal_obj
            .get("scope")
            .and_then(|v| v.get("tenant"))
            .and_then(Value::as_str);
        if scope_tenant != Some(tenant) {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "Act proposal scope.tenant does not match escalation tenant",
            )
            .map(RecoverStep::Done);
        }

        // Full Act required fields per spec (job, stop_condition, return_expectation, scope.subject + non-empty allowed_actions)
        for field in ["job", "stop_condition", "return_expectation"] {
            if proposal_obj
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
            {
                return dead_letter_staged_row(
                    conn,
                    escalation_id,
                    tenant,
                    &format!("Act proposal missing or empty {} (required for Act)", field),
                )
                .map(RecoverStep::Done);
            }
        }
        if let Some(scope) = proposal_obj.get("scope").and_then(Value::as_object) {
            if scope
                .get("subject")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
            {
                return dead_letter_staged_row(
                    conn,
                    escalation_id,
                    tenant,
                    "Act proposal scope missing or empty subject",
                )
                .map(RecoverStep::Done);
            }
            match scope.get("allowed_actions").and_then(Value::as_array) {
                Some(arr)
                    if !arr.is_empty()
                        && arr
                            .iter()
                            .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())) => {}
                _ => {
                    return dead_letter_staged_row(
                        conn,
                        escalation_id,
                        tenant,
                        "Act proposal scope.allowed_actions must be non-empty array of non-empty strings",
                    )
                    .map(RecoverStep::Done);
                }
            }
        } else {
            return dead_letter_staged_row(
                conn,
                escalation_id,
                tenant,
                "Act proposal missing scope object",
            )
            .map(RecoverStep::Done);
        }
    }

    // Shared proposal.v1 validator:
    // Delegate the remaining shape checks (no dispatcher-injected fields in fence,
    // Dismiss must not carry Act-only fields) to the single source of truth in
    // startup_probe. This guarantees boot-probe and live-recovery stay in parity
    // for any future contract changes. The call is after recovery-specific correlation
    // (in_response_to, authority, tenant cross-check) but re-validates the common
    // cabinet shape rules without duplication.
    if let Err(e) = super::startup_probe::validate_proposal_v1_shape(&proposal, tenant) {
        return dead_letter_staged_row(
            conn,
            escalation_id,
            tenant,
            &format!("proposal failed shared v1 shape validator: {}", e),
        )
        .map(RecoverStep::Done);
    }

    let mut persisted = proposal.clone();
    if let Some(obj) = persisted.as_object_mut() {
        obj.insert(
            "schema".into(),
            Value::String("irin.directive.payload.v1".to_string()),
        );
        if verdict == "Dismiss" {
            obj.remove("job");
            obj.remove("scope");
            obj.remove("stop_condition");
            obj.remove("return_expectation");
        }
        obj.insert(
            "council_session_id".into(),
            Value::String(session_id.clone()),
        );
        // Fix B (boundary defense-in-depth): finite-guard at the point we sign.
        // json!(non-finite f64) silently becomes Value::Null, so by the time the
        // enriched object reaches to_jcs_bytes the non-finiteness is already erased
        // and jcs's finite-check (which runs on typed input) cannot see it. The
        // parse-site guard (x-total-cost-usd above) already dead-letters non-finite
        // today; this is a hard runtime trap at the signing boundary, NOT a live-bug
        // fix. Hard guard (no debug_assert — that compiles out in release).
        if let Err(reason) = insert_finite_f64(obj, "council_cost_usd", cost_usd) {
            return dead_letter_staged_row(conn, escalation_id, tenant, &reason)
                .map(RecoverStep::Done);
        }
    }

    // Same position as the monolithic path: serialize enriched payload before sign.
    let envelope_json = serde_json::to_string(&persisted)?;

    Ok(RecoverStep::Next(EnrichedRecoveredProposal {
        persisted,
        envelope_json,
        authority_str: authority_str.to_string(),
        verdict,
        session_id,
        cost_usd,
    }))
}

/// 4. Real Ed25519 signature via the Phase 3 JCS canonical signing seam.
fn sign_recovered_directive(
    signing_key: &crate::keymgmt::DirectiveSigningKey,
    persisted: &Value,
) -> (String, String) {
    // 4. Real Ed25519 signature via the Phase 3 JCS canonical signing seam.
    // The helper in DirectiveSigningKey is the chokepoint for both the persisted
    // canonical string and the signed bytes, so a future RFC 8785 JCS swap stays
    // localized there. Keeps v0.2 boot semantics untouched.
    let (canonical, sig) = signing_key.sign_directive_envelope(persisted);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    (canonical, sig_b64)
}

/// 5. Build the outbox row (tenant-scoped id, authority pre-seal, single clock).
fn build_signed_outbox_row(
    escalation_id: &str,
    tenant: &str,
    enriched: &EnrichedRecoveredProposal,
    canonical: String,
    sig_b64: String,
    signing_kid: &str,
) -> (DirectiveOutboxRow, i64) {
    // 5. Build the outbox row
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // P0-alpha: directive id must be tenant-scoped to avoid PK collision
    // when two tenants have the same raw escalation_id during recovery.
    // Include the safe tenant token (stable, no ':') in the generated id.
    let tenant_scoped_directive_id = format!("{}-rec-{}", safe_tenant_token(tenant), escalation_id);

    let row = DirectiveOutboxRow {
        id: tenant_scoped_directive_id,
        in_response_to: escalation_id.to_string(),
        tenant: tenant.to_string(),
        status: if enriched.verdict == "Dismiss" {
            "dismissed".to_string()
        } else {
            "staged".to_string()
        },
        verdict: enriched.verdict.to_string(),
        // Pre-seal W2 authority integrity: the stored authority column MUST be
        // the authority the proposal was validated under and signed with
        // (`authority_str`, checked + capability-gated at staging above), NOT a
        // hardcoded `recommend`. The worker keys its capability-token gate off
        // this column / the signed envelope; pinning it to recommend let an
        // execute/prepare directive bypass the worker-side captoken check.
        authority: enriched.authority_str.clone(),
        envelope_json: enriched.envelope_json.clone(),
        envelope_json_canonical: canonical,
        signature_b64: sig_b64,
        signing_kid: signing_kid.to_string(),
        council_session_id: Some(enriched.session_id.clone()),
        council_cost_usd: Some(enriched.cost_usd),
        // Single clock sample: created_at_ms and expires_at_ms are stamped from the
        // same `now_ms`. The insert helper normalizes created_at_ms forward on backward
        // skew and shifts expires_at_ms by the identical delta, preserving the window.
        created_at_ms: now_ms,
        expires_at_ms: now_ms + directive_stage_ttl_ms(),
    };
    (row, now_ms)
}

/// 6. Exact transaction shape: insert, audit-in-tx, W3 verbatim hash, event bridge.
fn persist_signed_directive_tx(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    audit_sink: &mut Vec<WatchPhase3AuditEvent>,
    row: DirectiveOutboxRow,
    verdict: &str,
    now_ms: i64,
) -> anyhow::Result<(RecoveryOutcome, Vec<WatchPhase3AuditEvent>)> {
    // 6. Exact transaction shape requested
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("PRAGMA defer_foreign_keys = ON;", [])?;

    let mut sink: Vec<OutboxAuditEvent> = Vec::new();

    // The helper returns Ok(directive_id) both for fresh insert and for
    // UNIQUE (tenant, in_response_to) idempotent restart recovery.
    let directive_id = match outbox_insert_with_skew_normalize(&tx, row, &mut sink) {
        Ok(id) => id,
        Err(crate::watch::outbox::OutboxError::ClockSkewExceeded {
            directive_id,
            skew_delta_ms,
            max_skew_ms,
            ..
        }) => {
            // P2 PARK (Review, Option A). The clock-skew breaker refused to stage
            // this directive — the per-tenant `prior_max` floor is poisoned by a future-dated
            // row (NTP forward-glitch / VM suspend-resume). The directive row was NEVER inserted
            // (helper errored pre-INSERT). Roll back the staging tx and DO NOT propagate the
            // error: the boot/live wrapper flattens any Err -> rusqlite::Error, and the boot loop
            // treats that as a FATAL whole-batch abort (one poison row would block recovery of
            // every sibling staged row, every sweep). Return SkewHeld so siblings keep flowing.
            //
            // The escalation stays in 'council_response_staged' (NOT terminal): it self-heals on
            // a later sweep once the poison row is evicted (re-staged from the stored council
            // response — NO council re-call, no re-spend). Eviction/decay is the deferred P2
            drop(tx); // rollback: no directive row, no audit writes committed this attempt

            // P1-1 (faithful T21d): encode the held distinction in `last_error` so a skew-held
            // row is SQL-distinguishable from a healthy staged row (status UNCHANGED, column
            // write only — NO money-table CHECK rebuild). Best-effort: a failed observability
            // write must not turn a held row back into a fatal error.
            let _ = conn.execute(
                "UPDATE pending_escalations
                    SET last_error = ?1
                  WHERE tenant = ?2 AND id = ?3 AND status = 'council_response_staged'",
                rusqlite::params![
                    format!(
                        "ClockSkewExceeded: held @ {}ms (delta {}ms > MAX_ALLOWED_SKEW_MS {}ms); directive {}",
                        now_ms, skew_delta_ms, max_skew_ms, directive_id
                    ),
                    tenant,
                    escalation_id,
                ],
            );
            return Ok((RecoveryOutcome::SkewHeld, Vec::new()));
        }
        Err(e) => return Err(e.into()),
    };
    let unique_collision = sink
        .iter()
        .any(|event| matches!(event, OutboxAuditEvent::OutboxRecoveredFromRestart { .. }));
    let pending_status = if verdict == "Dismiss" {
        "dismissed"
    } else {
        "outbox_written"
    };

    tx.execute(
        "UPDATE pending_escalations
         SET status = ?1, directive_id = ?2
         WHERE tenant = ?3 AND id = ?4",
        rusqlite::params![pending_status, directive_id, tenant, escalation_id],
    )?;

    // P0-zeta: All Phase 3 audit writes happen inside this tx (before commit).
    let mut written_events: Vec<WatchPhase3AuditEvent> = Vec::new();

    written_events.push(WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox {
        escalation_id: escalation_id.to_string(),
        tenant: tenant.to_string(),
    });

    for e in sink.drain(..) {
        match e {
            OutboxAuditEvent::DirectiveStaged {
                directive_id,
                tenant,
                in_response_to,
            } => {
                written_events.push(WatchPhase3AuditEvent::DirectiveStaged {
                    directive_id,
                    tenant,
                    in_response_to,
                });
            }
            OutboxAuditEvent::OutboxRecoveredFromRestart {
                directive_id,
                tenant,
                in_response_to,
            } => {
                written_events.push(WatchPhase3AuditEvent::OutboxRecoveredFromRestart {
                    directive_id,
                    tenant,
                    in_response_to,
                });
            }
            OutboxAuditEvent::DirectiveClockSkewNormalized {
                directive_id,
                tenant,
                original_ms,
                normalized_ms,
            } => {
                written_events.push(WatchPhase3AuditEvent::DirectiveClockSkewNormalized {
                    directive_id,
                    tenant,
                    original_ms,
                    normalized_ms,
                });
            }
        }
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    for event in &written_events {
        let state_json = event.to_state_json();
        let reason = event.reason();
        let sentinel = event.sentinel();

        let prev_hash: String = tx
            .query_row(
                "SELECT hash FROM watch_fires WHERE tenant=?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params![tenant],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(crate::watch::db::watch_distinct_genesis);

        // W3: hash the VERBATIM envelope bytes that get stored — bind to one
        // var so the preimage and the INSERT cannot diverge.
        let envelope_json = serde_json::to_string(&event).unwrap_or_default();
        let preimage = crate::watch::db::compute_watch_fire_preimage(
            tenant,
            sentinel,
            now_ms,
            &state_json,
            &reason,
            &prev_hash,
            Some(&envelope_json), // W3: v4 — envelope in preimage.
        );
        let hash = hex::encode(Sha256::digest(preimage.as_bytes()));

        tx.execute(
            "INSERT INTO watch_fires (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash, envelope_json, envelope_schema_version, preimage_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                tenant, sentinel, now_ms, state_json, reason, prev_hash, hash,
                envelope_json, 3i64, 4i64, // W3: preimage_version=4, explicit.
            ],
        )?;
    }

    tx.commit()?;

    // Emit the recovery-specific escalation event before the helper events so
    // the persisted audit chain orders as
    // escalation_recovered_resume_outbox -> directive_staged.
    audit_sink.push(WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox {
        escalation_id: escalation_id.to_string(),
        tenant: tenant.to_string(),
    });

    // The helper already emitted DirectiveStaged / OutboxRecoveredFromRestart /
    // DirectiveClockSkewNormalized into the internal sink. We bridge them
    // into the high-level watch audit chain events the user listed.
    for e in sink.drain(..) {
        match e {
            OutboxAuditEvent::DirectiveStaged {
                directive_id,
                tenant,
                in_response_to,
            } => {
                audit_sink.push(WatchPhase3AuditEvent::DirectiveStaged {
                    directive_id,
                    tenant,
                    in_response_to,
                });
            }
            OutboxAuditEvent::OutboxRecoveredFromRestart {
                directive_id,
                tenant,
                in_response_to,
            } => {
                audit_sink.push(WatchPhase3AuditEvent::OutboxRecoveredFromRestart {
                    directive_id,
                    tenant,
                    in_response_to,
                });
            }
            OutboxAuditEvent::DirectiveClockSkewNormalized {
                directive_id,
                tenant,
                original_ms,
                normalized_ms,
            } => {
                audit_sink.push(WatchPhase3AuditEvent::DirectiveClockSkewNormalized {
                    directive_id,
                    tenant,
                    original_ms,
                    normalized_ms,
                });
            }
        }
    }

    if unique_collision {
        Ok((RecoveryOutcome::RecoveredViaUniqueCollision, written_events))
    } else {
        Ok((RecoveryOutcome::Recovered, written_events))
    }
}

/// Fix C: distinguishable rejection reasons for proposal-body intake. The single
/// caller (recover_council_response_staged) maps any `Err(_)` to dead_letter_staged_row;
/// the variants exist so a reviewer/test can tell a malformed-JSON reject from a
/// duplicate-key (RFC 8785 §3.2.1) reject. `Display` feeds the dead-letter reason.
#[derive(Debug)]
pub(crate) enum ProposalParseError {
    /// The selected JSON slice is not valid JSON (raw body and any fence both failed).
    Json(serde_json::Error),
    /// The selected JSON slice has duplicate keys (top-level OR nested) or otherwise
    /// fails the strict RFC 8785 canonicalization gate — last-wins collapse would let
    /// the signed preimage differ from the raw intake bytes. From the strict validator.
    Strict(sovereign_protocol::jcs::JcsError),
}

impl std::fmt::Display for ProposalParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalParseError::Json(e) => write!(f, "invalid proposal JSON: {e}"),
            ProposalParseError::Strict(e) => {
                write!(f, "proposal failed strict JCS intake gate: {e}")
            }
        }
    }
}

/// Parse-and-select the EXACT slice serde_json will consume, THEN run it through the
/// strict RFC 8785 dup-key gate. `serde_json::from_str` silently collapses duplicate
/// keys (last-wins), so without this gate the signed preimage could differ from the raw
/// council/LLM intake. The strict canonical OUTPUT is discarded — it is a validator
/// only; the enriched `persisted` object (not this raw body) is what is later signed
/// via the normal to_jcs_bytes path.
///
/// Slice selection preserves the original contract: raw `body` if it parses, else the
/// first ```json fence (whose inner must itself be valid JSON). A raw body with
/// duplicate keys parses (last-wins) so it is selected as the slice and then caught by
/// the strict gate. Parsing-while-selecting (not gating-then-parsing) means a malformed
/// slice surfaces as `Json` rather than leaking out of the strict gate as `Strict`.
pub(crate) fn parse_proposal_body(body: &str) -> Result<Value, ProposalParseError> {
    // Pick AND parse the slice in one pass: raw `body` if it is valid JSON, else the first
    // ```json fence — which must itself be valid JSON. Parsing here (not merely selecting)
    // means a malformed slice surfaces as `Json` (the honest malformed-JSON reason) instead
    // of leaking out of the strict gate below as a `Strict` reason, and we keep the parsed
    // Value so the success path parses exactly once through serde.
    let (slice, value) = match serde_json::from_str::<Value>(body) {
        Ok(v) => (body, v),
        Err(raw_err) => match super::startup_probe::extract_first_json_fence(body) {
            Some(fenced) => match serde_json::from_str::<Value>(fenced) {
                Ok(v) => (fenced, v),
                // A fence was present but its inner is not valid JSON -> malformed, not a
                // strict-gate rejection.
                Err(fence_err) => return Err(ProposalParseError::Json(fence_err)),
            },
            // Neither raw nor fenced is parseable JSON — surface the raw parse error.
            None => return Err(ProposalParseError::Json(raw_err)),
        },
    };

    // Strict dup-key gate on the EXACT slice serde consumed (top-level + nested, via
    // has_duplicate_keys recursion). Runs only on known-valid JSON, so a `Strict` error
    // here is specifically a dup-key / strict-canon rejection, never malformed-JSON. The
    // canonical output is discarded — validator only; the enriched `persisted` (not this
    // raw body) is what is later signed via the normal to_jcs_bytes path.
    sovereign_protocol::jcs::to_jcs_bytes_strict(slice).map_err(ProposalParseError::Strict)?;

    Ok(value)
}

/// Fix B helper: insert an f64 into a signed JSON object only if it is finite.
/// `serde_json::json!(non_finite_f64)` silently produces `Value::Null`, which would
/// then be canonicalized + signed as `null` rather than rejected. Returning `Err`
/// here lets the caller dead-letter at the signing boundary instead. Hard runtime
/// guard (NOT a debug_assert — that would compile out in release builds).
pub(crate) fn insert_finite_f64(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    val: f64,
) -> Result<(), String> {
    if !val.is_finite() {
        return Err(format!("non-finite f64 for key '{key}'"));
    }
    obj.insert(key.into(), serde_json::json!(val));
    Ok(())
}

/// P0-gamma helper: transactionally dead-letters a council_response_staged row,
/// sets last_error, and writes the corresponding Phase 3 audit event in the same tx.
/// The audit write is required; if it fails the dead_lettered transition is rolled back.
/// Returns DeadLettered on success.
fn dead_letter_staged_row(
    conn: &mut rusqlite::Connection,
    escalation_id: &str,
    tenant: &str,
    reason: &str,
) -> anyhow::Result<(RecoveryOutcome, Vec<WatchPhase3AuditEvent>)> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    tx.execute(
        "UPDATE pending_escalations
         SET status = 'dead_lettered', last_error = ?1
         WHERE tenant = ?2 AND id = ?3",
        rusqlite::params![reason, tenant, escalation_id],
    )?;

    // Write a DirectiveParseFailed audit event into the same transaction.
    // This write is mandatory (P0-zeta). If it fails, the entire transaction
    // rolls back, so the pending_escalations row will not become dead_lettered.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let prev_hash: String = tx
        .query_row(
            "SELECT hash FROM watch_fires WHERE tenant=?1 ORDER BY id DESC LIMIT 1",
            rusqlite::params![tenant],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(crate::watch::db::watch_distinct_genesis);

    let state_json = serde_json::json!({
        "event_type": "directive_parse_failed",
        "escalation_id": escalation_id,
        "tenant": tenant,
        "reason": reason,
    })
    .to_string();

    // W3: this path stores state_json AS the envelope_json column, so the v4
    // preimage must hash that same value verbatim.
    let preimage = crate::watch::db::compute_watch_fire_preimage(
        tenant,
        "watch-dispatcher",
        now_ms,
        &state_json,
        "directive_parse_failed",
        &prev_hash,
        Some(&state_json), // W3: v4 — envelope_json column == state_json here.
    );
    let hash = hex::encode(Sha256::digest(preimage.as_bytes()));

    tx.execute(
        "INSERT INTO watch_fires (tenant, sentinel, fired_at, state_json, reason, prev_hash, hash, envelope_json, envelope_schema_version, preimage_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            tenant,
            "watch-dispatcher",
            now_ms,
            state_json,
            "directive_parse_failed",
            prev_hash,
            hash,
            state_json,
            3i64,
            4i64, // W3: preimage_version=4, explicit.
        ],
    )?;

    tx.commit()?;

    let event = WatchPhase3AuditEvent::DirectiveParseFailed {
        escalation_id: escalation_id.to_string(),
        tenant: tenant.to_string(),
        reason: reason.to_string(),
    };

    Ok((RecoveryOutcome::DeadLettered, vec![event]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    Recovered,
    RecoveredViaUniqueCollision,
    DeadLettered, // P0-gamma (all soft failures now dead-letter)
    /// P2 PARK (Review, Option A). The clock-skew breaker refused to stage the
    /// directive (poisoned per-tenant `prior_max`). The directive row was never inserted; the
    /// escalation stays in `council_response_staged` (NOT terminal) with a `last_error` sentinel,
    /// and self-heals on a later sweep once the poison row is evicted (re-staged from the stored
    /// council response — no re-spend). A migration-free outcome variant, NOT a SQL status label
    /// (dodges the money-table CHECK rebuild a `skew_held` status would need — sibling to T21d).
    SkewHeld,
    /// Disarm re-check refused the sign: no / invalid / expired attested arm
    /// at recovery time (attest::verify_arm_row, the same decision the spend
    /// reserve makes). Same parking shape as `SkewHeld`: the escalation stays
    /// `council_response_staged` (NOT terminal), no directive row is written,
    /// and the row self-heals on the first sweep under a valid arm. A disarm
    /// never destroys the council work product — it only forbids signing it.
    ///
    /// Healing path (operational): the only sweep over pre-existing staged
    /// rows is boot hydration — the live tick recovers only rows it claimed
    /// this tick. A row parked by a live-tick disarm race therefore heals at
    /// re-arm + restart (the canary cold-boot chain), not on the next tick.
    /// Both park sites log `arm_held` so the parked backlog is visible.
    ArmHeld,
}

/// High-level watch audit events for the Phase 3 closed signal loop.
/// These are the events that must appear in the watch audit chain
/// (visible via /watch/audit and in the sovereign preimage corpus).
///
/// The recovery seam is responsible for emitting:
/// - escalation_recovered_resume_outbox when a council_response_staged row
///   is successfully turned into a durable outbox row during boot hydration.
/// - directive_staged / outbox_recovered_from_restart by bridging the
///   OutboxAuditEvent produced by the shared helper.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum WatchPhase3AuditEvent {
    EscalationRecoveredResumeOutbox {
        escalation_id: String,
        tenant: String,
    },
    DirectiveStaged {
        directive_id: String,
        tenant: String,
        in_response_to: String,
    },
    OutboxRecoveredFromRestart {
        directive_id: String,
        tenant: String,
        in_response_to: String,
    },
    DirectiveClockSkewNormalized {
        directive_id: String,
        tenant: String,
        original_ms: i64,
        normalized_ms: i64,
    },
    /// The recovery arm gate parked a staged row (`RecoveryOutcome::ArmHeld`).
    /// `reason` is a stable refusal tag (no_active_arm / bad_signature /
    /// window_expired / ...), never row content.
    RecoveryArmHeld {
        escalation_id: String,
        tenant: String,
        reason: String,
    },
    /// P0-gamma: soft failure during staged recovery
    DirectiveParseFailed {
        escalation_id: String,
        tenant: String,
        reason: String,
    },
}

impl WatchPhase3AuditEvent {
    /// Returns the canonical event_type string used in state_json and reason.
    pub fn event_type(&self) -> &'static str {
        match self {
            WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox { .. } => {
                "escalation_recovered_resume_outbox"
            }
            WatchPhase3AuditEvent::DirectiveStaged { .. } => "directive_staged",
            WatchPhase3AuditEvent::OutboxRecoveredFromRestart { .. } => {
                "outbox_recovered_from_restart"
            }
            WatchPhase3AuditEvent::DirectiveClockSkewNormalized { .. } => {
                "directive_clock_skew_normalized"
            }
            WatchPhase3AuditEvent::DirectiveParseFailed { .. } => "directive_parse_failed",
            WatchPhase3AuditEvent::RecoveryArmHeld { .. } => "recovery_arm_held",
        }
    }

    /// Serializes to the state_json shape expected by the watch audit chain
    /// and the preimage corpus (matches the fixture examples).
    pub fn to_state_json(&self) -> String {
        match self {
            WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox {
                escalation_id,
                tenant,
            } => {
                #[derive(serde::Serialize)]
                struct State<'a> {
                    event_type: &'static str,
                    escalation_id: &'a str,
                    tenant: &'a str,
                }

                serde_json::to_string(&State {
                    event_type: self.event_type(),
                    escalation_id,
                    tenant,
                })
                .expect("phase3 audit state_json serialization")
            }
            WatchPhase3AuditEvent::DirectiveStaged {
                directive_id,
                tenant,
                in_response_to,
            } => {
                #[derive(serde::Serialize)]
                struct State<'a> {
                    event_type: &'static str,
                    directive_id: &'a str,
                    tenant: &'a str,
                    in_response_to: &'a str,
                }

                serde_json::to_string(&State {
                    event_type: self.event_type(),
                    directive_id,
                    tenant,
                    in_response_to,
                })
                .expect("phase3 audit state_json serialization")
            }
            WatchPhase3AuditEvent::OutboxRecoveredFromRestart {
                directive_id,
                tenant,
                in_response_to,
            } => {
                #[derive(serde::Serialize)]
                struct State<'a> {
                    event_type: &'static str,
                    directive_id: &'a str,
                    tenant: &'a str,
                    in_response_to: &'a str,
                }

                serde_json::to_string(&State {
                    event_type: self.event_type(),
                    directive_id,
                    tenant,
                    in_response_to,
                })
                .expect("phase3 audit state_json serialization")
            }
            WatchPhase3AuditEvent::DirectiveClockSkewNormalized {
                directive_id,
                tenant,
                original_ms,
                normalized_ms,
            } => {
                #[derive(serde::Serialize)]
                struct State<'a> {
                    event_type: &'static str,
                    directive_id: &'a str,
                    tenant: &'a str,
                    original_ms: i64,
                    normalized_ms: i64,
                }

                serde_json::to_string(&State {
                    event_type: self.event_type(),
                    directive_id,
                    tenant,
                    original_ms: *original_ms,
                    normalized_ms: *normalized_ms,
                })
                .expect("phase3 audit state_json serialization")
            }
            WatchPhase3AuditEvent::DirectiveParseFailed {
                escalation_id,
                tenant,
                reason,
            } => {
                #[derive(serde::Serialize)]
                struct State<'a> {
                    event_type: &'static str,
                    escalation_id: &'a str,
                    tenant: &'a str,
                    reason: &'a str,
                }

                serde_json::to_string(&State {
                    event_type: self.event_type(),
                    escalation_id,
                    tenant,
                    reason,
                })
                .expect("phase3 audit state_json serialization")
            }
            WatchPhase3AuditEvent::RecoveryArmHeld {
                escalation_id,
                tenant,
                reason,
            } => {
                #[derive(serde::Serialize)]
                struct State<'a> {
                    event_type: &'static str,
                    escalation_id: &'a str,
                    tenant: &'a str,
                    reason: &'a str,
                }

                serde_json::to_string(&State {
                    event_type: self.event_type(),
                    escalation_id,
                    tenant,
                    reason,
                })
                .expect("phase3 audit state_json serialization")
            }
        }
    }

    /// Human / audit reason string (used in the preimage and visible in /watch/audit).
    pub fn reason(&self) -> String {
        self.event_type().to_string()
    }

    /// Sentinel name to use when writing this event as a watch_fires row.
    /// For recovery events we use a stable synthetic sentinel so the
    /// originating sentinel's hard-kill state does not affect system events.
    pub fn sentinel(&self) -> &'static str {
        "watch-dispatcher"
    }

    pub fn tenant(&self) -> &str {
        match self {
            WatchPhase3AuditEvent::EscalationRecoveredResumeOutbox { tenant, .. } => tenant,
            WatchPhase3AuditEvent::DirectiveStaged { tenant, .. } => tenant,
            WatchPhase3AuditEvent::OutboxRecoveredFromRestart { tenant, .. } => tenant,
            WatchPhase3AuditEvent::DirectiveClockSkewNormalized { tenant, .. } => tenant,
            WatchPhase3AuditEvent::DirectiveParseFailed { tenant, .. } => tenant,
            WatchPhase3AuditEvent::RecoveryArmHeld { tenant, .. } => tenant,
        }
    }
}
