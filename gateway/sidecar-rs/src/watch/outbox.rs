//! Phase 3 shared durable outbox helpers.
//!
//! This module is intentionally storage-only: Act, Dismiss, and restart
//! recovery paths will all call the same insert helper once the full
//! dispatcher lands.

use rusqlite::{params, OptionalExtension, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveAuthority {
    Recommend,
    Prepare,
    Execute,
}

impl DirectiveAuthority {
    const ALL: [Self; 3] = [Self::Recommend, Self::Prepare, Self::Execute];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recommend => "recommend",
            Self::Prepare => "prepare",
            Self::Execute => "execute",
        }
    }

    pub fn contains(value: &str) -> bool {
        Self::ALL
            .iter()
            .any(|authority| authority.as_str() == value)
    }

    pub fn sql_check_literals() -> String {
        Self::ALL
            .iter()
            .map(|authority| format!("'{}'", authority.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutboxAuditEvent {
    DirectiveClockSkewNormalized {
        directive_id: String,
        tenant: String,
        original_ms: i64,
        normalized_ms: i64,
    },
    DirectiveStaged {
        tenant: String,
        directive_id: String,
        in_response_to: String,
    },
    OutboxRecoveredFromRestart {
        tenant: String,
        directive_id: String,
        in_response_to: String,
    },
}

pub trait OutboxAuditSink {
    fn emit(&mut self, event: OutboxAuditEvent) -> Result<(), String>;
}

impl OutboxAuditSink for Vec<OutboxAuditEvent> {
    fn emit(&mut self, event: OutboxAuditEvent) -> Result<(), String> {
        self.push(event);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    #[error("outbox db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("outbox audit emit failed: {0}")]
    Audit(String),
    /// P2 clock-skew circuit-breaker. The created-time normalization forces
    /// per-tenant monotonicity by pushing `created_at_ms` forward past the prior
    /// max; with the #45 fix `expires_at_ms` shifts by the SAME delta, so a poisoned
    /// `prior_max` (an NTP forward-glitch row stamped far in the future) would float
    /// every later directive's absolute authorization window forward by that delta —
    /// defeating the auth-window policy on the money path. When the normalization
    /// delta exceeds `MAX_ALLOWED_SKEW_MS` the breaker REFUSES to stage the row
    /// (fail-safe: blocks dispatch, never spends) rather than silently over-extend it.
    #[error(
        "clock skew exceeded for directive {directive_id} (tenant {tenant}): \
         normalization delta {skew_delta_ms}ms > MAX_ALLOWED_SKEW_MS {max_skew_ms}ms"
    )]
    ClockSkewExceeded {
        directive_id: String,
        tenant: String,
        skew_delta_ms: i64,
        max_skew_ms: i64,
    },
}

#[derive(Clone)]
pub struct DirectiveOutboxRow {
    pub id: String,
    pub in_response_to: String,
    pub tenant: String,
    pub status: String,
    pub verdict: String,
    pub authority: String,
    pub envelope_json: String,
    /// envelope_json_canonical: strictly the parsed bytes of the fenced
    /// irin.directive.proposal.v1 JSON only (no raw multi-round chair/seat
    /// chatter or full transcript from Council sessions/*.json).
    /// Per output-fidelity invariant (the invariant, Tier 1):
    /// "strictly limit envelope_json_canonical to the parsed, fenced JSON directive proposal."
    /// Rollback: if sig test fails due to leak, revert persistence change.
    /// This struct is storage-only (insert helper); production/scope guard is
    /// in dispatcher (forbidden per contract) + asserted in watch_dispatch_keymgmt tests.
    pub envelope_json_canonical: String,
    pub signature_b64: String,
    pub signing_kid: String,
    pub council_session_id: Option<String>,
    pub council_cost_usd: Option<f64>,
    /// Stage time, stamped by the caller from the SAME clock read as
    /// `expires_at_ms` (`created_at_ms = now_ms`, `expires_at_ms = now_ms + TTL`).
    /// The insert helper normalizes this forward past any prior per-tenant max and
    /// shifts `expires_at_ms` by the same delta — see the single-clock-sample
    /// contract on `outbox_insert_with_skew_normalize`.
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

// Manual redacting Debug: the signed directive envelope + its canonical form +
// the signature are content that must never reach a future `{:?}` log line (T24).
impl std::fmt::Debug for DirectiveOutboxRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectiveOutboxRow")
            .field("id", &self.id)
            .field("in_response_to", &self.in_response_to)
            .field("tenant", &self.tenant)
            .field("status", &self.status)
            .field("verdict", &self.verdict)
            .field("authority", &self.authority)
            .field("envelope_json", &"<redacted>")
            .field("envelope_json_canonical", &"<redacted>")
            .field("signature_b64", &"<redacted>")
            .field("signing_kid", &self.signing_kid)
            .field("council_session_id", &self.council_session_id)
            .field("council_cost_usd", &self.council_cost_usd)
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Shared helper for inserting a signed directive row.
///
/// The direct SQLite trigger rejects created_at regressions as defense in
/// depth; this helper is the normal path and normalizes backward host-clock
/// skew before INSERT. It also owns idempotent restart recovery for
/// `UNIQUE (tenant, in_response_to)` collisions.
///
/// **Single-clock-sample contract (T21c P1 fix).** The caller MUST stamp
/// `row.created_at_ms` and `row.expires_at_ms` from the SAME clock read (the
/// dispatcher's `now_ms`): `created_at_ms = now_ms`, `expires_at_ms = now_ms + TTL`.
/// This helper normalizes `created_at_ms` forward past any prior per-tenant max and
/// shifts `expires_at_ms` by the identical (>= 0) delta, so the authorization-window
/// length is preserved exactly and the row can never be born already-expired by skew
/// normalization. The helper reads no clock of its own — created/expiry are
/// row-derived, which eliminates the prior two clock-sample drift (stage time vs a
/// separate insert-time clock read) that left the absolute expiry behind on backward skew.
pub fn outbox_insert_with_skew_normalize<A: OutboxAuditSink>(
    tx: &Transaction<'_>,
    row: DirectiveOutboxRow,
    audit: &mut A,
) -> Result<String, OutboxError> {
    let prior_max_ms: Option<i64> = tx
        .query_row(
            "SELECT MAX(created_at_ms) FROM directive_outbox WHERE tenant = ?1",
            params![row.tenant],
            |r| r.get(0),
        )
        .optional()?
        .flatten();

    // Normalize the row's OWN stage time forward past any prior per-tenant max so
    // created_at_ms stays strictly monotonic (defense-in-depth for the SQLite
    // created_at-regression trigger).
    let normalized_at = match prior_max_ms {
        Some(prior) if row.created_at_ms <= prior => prior + 1,
        _ => row.created_at_ms,
    };

    let skew_event = (normalized_at != row.created_at_ms).then(|| {
        OutboxAuditEvent::DirectiveClockSkewNormalized {
            directive_id: row.id.clone(),
            tenant: row.tenant.clone(),
            original_ms: row.created_at_ms,
            normalized_ms: normalized_at,
        }
    });

    // T21c skew-coupling: expires_at_ms shares the caller's single clock sample with
    // created_at_ms. Shift the absolute expiry by the SAME normalization delta the created
    // time moved, or the authorization window silently shrinks (and can land already-expired,
    // making the TTL fence sweep a legitimately fresh directive). Derived from the row's own
    // base, not an external clock, so window preservation holds even if a future caller drifts.
    // The delta is >= 0 by construction (normalization only ever moves time forward), so this
    // can only restore the window to its intended length, never shorten it — fail-safe on the
    // money path.
    let skew_delta_ms = normalized_at.saturating_sub(row.created_at_ms);

    // P2 clock-skew circuit-breaker. A legitimate monotonic bump is sub-second (clock
    // jitter, or a same-millisecond same-tenant burst nudging created_at forward by a
    // few ms). A delta past MAX_ALLOWED_SKEW_MS means `prior_max` is poisoned by an NTP
    // forward-glitch row (or this row's clock jumped backward) — staging it would float the
    // absolute authorization window forward by that delta. Refuse instead: fail-safe (blocks
    // dispatch, never spends), counted so the operator sees the breaker trip. The check is on
    // the row-derived delta, so the helper still reads no clock of its own (#45 invariant).
    let max_skew_ms = crate::watch::dispatcher::max_allowed_skew_ms();
    if skew_delta_ms > max_skew_ms {
        crate::watch::dispatcher::bump_directive_clock_skew_rejected(1);
        return Err(OutboxError::ClockSkewExceeded {
            directive_id: row.id,
            tenant: row.tenant,
            skew_delta_ms,
            max_skew_ms,
        });
    }

    let normalized_expires_at_ms = row.expires_at_ms.saturating_add(skew_delta_ms);
    debug_assert!(
        normalized_expires_at_ms >= normalized_at,
        "directive must not be born expired after skew normalization"
    );

    let insert = tx.execute(
        "INSERT INTO directive_outbox
            (id, in_response_to, tenant, status, verdict, authority,
             envelope_json, envelope_json_canonical, signature_b64, signing_kid,
             council_session_id, council_cost_usd, created_at_ms, expires_at_ms)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            &row.id,
            &row.in_response_to,
            &row.tenant,
            &row.status,
            &row.verdict,
            &row.authority,
            &row.envelope_json,
            &row.envelope_json_canonical,
            &row.signature_b64,
            &row.signing_kid,
            row.council_session_id.as_deref(),
            row.council_cost_usd,
            normalized_at,
            normalized_expires_at_ms,
        ],
    );

    match insert {
        Ok(_) => {
            if let Some(event) = skew_event {
                audit.emit(event).map_err(OutboxError::Audit)?;
            }
            audit
                .emit(OutboxAuditEvent::DirectiveStaged {
                    tenant: row.tenant.clone(),
                    directive_id: row.id.clone(),
                    in_response_to: row.in_response_to.clone(),
                })
                .map_err(OutboxError::Audit)?;
            Ok(row.id)
        }
        Err(rusqlite::Error::SqliteFailure(code, _))
            if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            let existing_id: String = tx.query_row(
                "SELECT id FROM directive_outbox
                 WHERE tenant = ?1 AND in_response_to = ?2",
                params![row.tenant, row.in_response_to],
                |r| r.get(0),
            )?;
            audit
                .emit(OutboxAuditEvent::OutboxRecoveredFromRestart {
                    tenant: row.tenant,
                    directive_id: existing_id.clone(),
                    in_response_to: row.in_response_to,
                })
                .map_err(OutboxError::Audit)?;
            Ok(existing_id)
        }
        Err(e) => Err(OutboxError::Db(e)),
    }
}

/// P1 outbox surface read shape (separate from the insert-only DirectiveOutboxRow
/// so that adding created_at_ms / acked_at_ms does not affect the dispatcher
/// insert path or any forbidden files).
#[derive(Clone, PartialEq)]
pub struct DirectiveOutboxRecord {
    pub id: String,
    pub in_response_to: String,
    pub tenant: String,
    pub status: String,
    pub verdict: String,
    pub authority: String,
    pub envelope_json: String,
    /// envelope_json_canonical: strictly the parsed bytes of the fenced
    /// irin.directive.proposal.v1 JSON only (no raw multi-round chair/seat
    /// chatter or full transcript from Council sessions/*.json).
    /// Per output-fidelity invariant (the invariant, Tier 1):
    /// "strictly limit envelope_json_canonical to the parsed, fenced JSON directive proposal."
    /// Rollback: if sig test fails due to leak, revert persistence change.
    /// This struct is read shape (P1 surface); canonical populated upstream.
    pub envelope_json_canonical: String,
    pub signature_b64: String,
    pub signing_kid: String,
    pub council_session_id: Option<String>,
    pub council_cost_usd: Option<f64>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub acked_at_ms: Option<i64>,
    pub claimed_until_ms: Option<i64>,
    pub claim_count: i64,
    pub last_error: Option<String>,
    pub worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
}

// Manual redacting Debug (same content-leak class as DirectiveOutboxRow): the
// P1 read shape carries the same signed envelope + canonical form + signature,
// so they are redacted here too before any future `{:?}` sink (T24).
impl std::fmt::Debug for DirectiveOutboxRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectiveOutboxRecord")
            .field("id", &self.id)
            .field("in_response_to", &self.in_response_to)
            .field("tenant", &self.tenant)
            .field("status", &self.status)
            .field("verdict", &self.verdict)
            .field("authority", &self.authority)
            .field("envelope_json", &"<redacted>")
            .field("envelope_json_canonical", &"<redacted>")
            .field("signature_b64", &"<redacted>")
            .field("signing_kid", &self.signing_kid)
            .field("council_session_id", &self.council_session_id)
            .field("council_cost_usd", &self.council_cost_usd)
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("acked_at_ms", &self.acked_at_ms)
            .field("claimed_until_ms", &self.claimed_until_ms)
            .field("claim_count", &self.claim_count)
            .field("last_error", &self.last_error)
            .field("worker_provenance", &self.worker_provenance)
            .finish()
    }
}

/// Admin ack outcome for the P1 POST /watch/outbox/{id}/ack surface.
/// Distinguishes idempotent success vs "already terminal non-ackable" (409).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckOutcome {
    /// Successfully marked acked (or was already acked — idempotent).
    Acked {
        id: String,
        tenant: String,
        was_already: bool,
    },
    /// Terminal but not actionable (dismissed / expired). Caller should 409.
    NotActionable { id: String, status: String },
    /// Row exists, but the caller's tenant scope header does not match. Caller should 403.
    TenantMismatch { id: String },
    /// No row for (tenant, id) — 404.
    NotFound { id: String },
    /// Worker handle does not match the current claim_handle.
    InvalidHandle { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    // T24 redaction: the signed envelope + canonical form + signature are
    // content that must never reach a future `{:?}` log line.
    #[test]
    fn test_directive_outbox_row_debug_redacts_content() {
        let row = DirectiveOutboxRow {
            id: "id-1".to_string(),
            in_response_to: "esc-1".to_string(),
            tenant: "tenant-a".to_string(),
            status: "council_response_staged".to_string(),
            verdict: "Act".to_string(),
            authority: "council".to_string(),
            envelope_json: "SENTINEL_ENVELOPE".to_string(),
            envelope_json_canonical: "SENTINEL_CANONICAL".to_string(),
            signature_b64: "SENTINEL_SIGNATURE".to_string(),
            signing_kid: "sidecar-v1-abc".to_string(),
            council_session_id: Some("sess-1".to_string()),
            council_cost_usd: Some(0.01),
            created_at_ms: 1,
            expires_at_ms: 2,
        };
        let dbg = format!("{:?}", row);
        for sentinel in [
            "SENTINEL_ENVELOPE",
            "SENTINEL_CANONICAL",
            "SENTINEL_SIGNATURE",
        ] {
            assert!(!dbg.contains(sentinel), "content leaked into Debug: {dbg}");
        }
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        // Non-sensitive routing fields stay visible.
        assert!(dbg.contains("tenant-a") && dbg.contains("sidecar-v1-abc"));
    }

    #[test]
    fn test_directive_outbox_record_debug_redacts_content() {
        let rec = DirectiveOutboxRecord {
            id: "id-1".to_string(),
            in_response_to: "esc-1".to_string(),
            tenant: "tenant-a".to_string(),
            status: "council_response_staged".to_string(),
            verdict: "Act".to_string(),
            authority: "council".to_string(),
            envelope_json: "SENTINEL_ENVELOPE".to_string(),
            envelope_json_canonical: "SENTINEL_CANONICAL".to_string(),
            signature_b64: "SENTINEL_SIGNATURE".to_string(),
            signing_kid: "sidecar-v1-abc".to_string(),
            council_session_id: Some("sess-1".to_string()),
            council_cost_usd: Some(0.01),
            created_at_ms: 1,
            expires_at_ms: 2,
            acked_at_ms: None,
            claimed_until_ms: None,
            claim_count: 0,
            last_error: None,
            worker_provenance: None,
        };
        let dbg = format!("{:?}", rec);
        for sentinel in [
            "SENTINEL_ENVELOPE",
            "SENTINEL_CANONICAL",
            "SENTINEL_SIGNATURE",
        ] {
            assert!(!dbg.contains(sentinel), "content leaked into Debug: {dbg}");
        }
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        assert!(dbg.contains("tenant-a") && dbg.contains("sidecar-v1-abc"));
    }
}
