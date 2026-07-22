//! Four-eyes arming ceremony, producer start/disarm, and arm admin router.

use crate::watch::quarantine::QuarantineState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use super::helpers::{admin_token_matches, json_response, problem};
use super::writer_claim::writer_claim_heartbeat_loop;

// ---------------------------------------------------------------------------
// p0a-four-eyes (the dual-custody invariant / Blind spot 3) — distinct-principal
// stage->confirm arming flow.
//
// /arm is the highest-blast-radius admin action on this surface (it starts
// the CDC producer, the first leg of real council spend). It is therefore
// gated behind a two-person ceremony: principal A POSTs /arm/stage, a
// DIFFERENT principal B POSTs /arm/confirm with the stage_id nonce echoed at
// stage time. Same-principal confirm is rejected (the core four-eyes
// invariant); a mismatched stage_id is rejected (amendment: a confirm can
// never ratify a different/older stage than intended). The legacy
// single-shot /arm returns 410 Gone so there is no bypass.
//
// /disarm deliberately stays single-principal: a kill-switch that requires
// two humans is a safety regression — under an active dup-charge/cap-bust
// incident the operator must /disarm in one call.
// Fast kill must never block on a second signature. /disarm still writes an
// arm_audit row.
//
// Every stage/confirm/disarm AND every stage/confirm rejection appends to
// the hash-chained, trigger-enforced append-only `arm_audit` table
// (who/when/action/principal, tamper-evident). The audit row stores the
// principal NAME, never the token.
//
// MANAGEMENT SURFACE: the sidecar binds UDS only, with no production TCP
// fallback. Local bearer authentication remains required for arm operations.
//
// ATTACKER MODEL: the UDS at
// SIDECAR_SOCKET_PATH (default /tmp/gateway-sidecar.sock) is now bound with a
// tightened default mode 0o660 (owner+group rw, WORLD NONE) — configurable via
// SIDECAR_SOCKET_MODE/SIDECAR_SOCKET_GID, fail-closed on bad values (socket.rs).
// The prior 0o777 (world-rwx) is gone. The arm routes are NOT proxied by nginx
// (nginx.conf has no /watch/admin/ location), so the file mode is the FIRST
// transport boundary: at 0o660 only the socket OWNER and the configured GROUP
// can open the socket and reach this management surface — blast radius reduced
// from "any local process/uid" to "owner + configured group" (compose:
// root:triad_mgmt gid 9999, so the nginx worker and root only). The arm ceremony's real
// control is STILL the GW_ARM_PRINCIPALS bearer + the four-eyes stage/confirm
// split — the mode is defense-in-depth, NOT a replacement for the bearer. mTLS
// WOULD still add attacker-model value here (a same-owner/same-group process
// the 0o660 mode does NOT exclude gets no mutual transport auth), so the
// deviation remains a documented gap, not a
// no-op. See the main.rs UDS bind + docs/runbooks/arming-authorization.md §9.
//
// RESIDUAL (documented, accepted): principal authentication compares the
// token constant-time AFTER a literal name lookup; an unknown name burns a
// dummy compare, but a timing oracle on principal-name existence is not
// fully closed. The token-length oracle is closed: both
// sides are SHA-256-hashed to fixed length before the constant-time compare,
// so no length-dependent early return remains. Principal tokens live in env
// (GW_ARM_PRINCIPALS) — env-only, never logged, never stored.
// ---------------------------------------------------------------------------

/// Stage TTL: how long a staged arm stays confirmable.
/// Env `ARM_STAGE_TTL_MS`, default 120_000 (2 min). Non-positive /
/// unparseable values fall back to the default.
pub const ARM_STAGE_TTL_MS_DEFAULT: u64 = 120_000;

pub fn arm_stage_ttl() -> Duration {
    let ms = std::env::var("ARM_STAGE_TTL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(ARM_STAGE_TTL_MS_DEFAULT);
    Duration::from_millis(ms)
}

/// p0a-four-eyes — the arming principal registry, parsed once at boot from
/// `GW_ARM_PRINCIPALS='alice:tok_aaaa,bob:tok_bbbb'` (comma-separated
/// `name:token` entries). Fail-closed posture:
///   * empty/unset            → no principal authenticates (401 everywhere);
///   * malformed entry        → ENTIRE registry rejected (401 everywhere);
///   * duplicate name OR token → ENTIRE registry rejected — two "principals"
///     sharing a name or a token is four-eyes theater (one human could hold
///     both halves of the ceremony);
///   * exactly one principal  → that principal authenticates, but
///     `is_arm_capable()` is false, so stage/confirm refuse with 403
///     ("a four-eyes gate with one principal is theater").
pub struct ArmPrincipals {
    /// (name, token). Tokens are secrets — never logged, never audited.
    entries: Vec<(String, String)>,
}

impl ArmPrincipals {
    /// Registry with no principals — everything fails closed. Used by call
    /// sites (tests, legacy fixtures) that need a placeholder.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Parse a `GW_ARM_PRINCIPALS`-shaped string. See type-level doc for the
    /// fail-closed rules. Names and tokens are matched literally (no
    /// trimming inside an entry; surrounding whitespace per entry is
    /// tolerated so `'a:1, b:2'` works).
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::empty();
        }
        let mut entries: Vec<(String, String)> = Vec::new();
        for part in trimmed.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue; // tolerate trailing comma
            }
            let Some((name, token)) = part.split_once(':') else {
                tracing::warn!(
                    "GW_ARM_PRINCIPALS entry without ':' separator — rejecting ENTIRE registry (fail-closed)"
                );
                return Self::empty();
            };
            if name.is_empty() || token.is_empty() {
                tracing::warn!(
                    "GW_ARM_PRINCIPALS entry with empty name or token — rejecting ENTIRE registry (fail-closed)"
                );
                return Self::empty();
            }
            // dual-custody-local-attest B6 (spec §9): the OTC mechanism is
            // RETIRED. A leftover '@otc' sentinel entry is a stale config —
            // reject the ENTIRE registry so the operator must update it
            // (fail-closed; a silently-ignored entry would leave the
            // ceremony running on a config the operator believes says
            // something else).
            if token == "@otc" {
                tracing::error!(
                    principal = name,
                    "GW_ARM_PRINCIPALS carries an '@otc' entry but OTC is RETIRED (dual-custody-local-attest §9) — rejecting ENTIRE registry (fail-closed); remove the entry"
                );
                return Self::empty();
            }
            if entries.iter().any(|(n, t)| n == name || t == token) {
                tracing::warn!(
                    "GW_ARM_PRINCIPALS duplicate principal name or token — rejecting ENTIRE registry (four-eyes theater guard, fail-closed)"
                );
                return Self::empty();
            }
            entries.push((name.to_string(), token.to_string()));
        }
        Self { entries }
    }

    /// Parse from the `GW_ARM_PRINCIPALS` env var (unset → empty registry).
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("GW_ARM_PRINCIPALS").unwrap_or_default())
    }

    /// True with >= 1 principal. dual-custody-local-attest (spec §2): the
    /// second custody domain is the ENCLAVE KEY verified at confirm time,
    /// not a second token string — the old >= 2 four-eyes-by-tokens minimum
    /// is retired with the second bearer. An empty registry (parse failure
    /// or unset env) still refuses both stage and confirm.
    pub fn is_arm_capable(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Authenticate a bearer of the form `name:token`. Literal name lookup,
    /// then constant-time compare over FIXED-LENGTH SHA-256 digests of both
    /// tokens  != token.len()`
    /// early-return leaked the principal token's length via timing; hashing
    /// both sides first makes the compare length-independent). Returns the
    /// principal NAME on success — callers must never see or propagate the
    /// token. Unknown name burns a dummy digest compare so its timing stays
    /// close to the bad-token path (residual name-existence oracle
    /// documented at the module-level comment).
    pub fn authenticate(&self, bearer: Option<&str>) -> Option<String> {
        use sha2::{Digest, Sha256};
        use subtle::ConstantTimeEq;
        let given = bearer?;
        let (name, token) = given.split_once(':')?;
        if name.is_empty() || token.is_empty() {
            return None;
        }
        let token_digest = Sha256::digest(token.as_bytes());
        let Some((_, expected)) = self.entries.iter().find(|(n, _)| n == name) else {
            // Dummy compare: unknown-name ~ bad-token timing.
            let _: bool = token_digest.ct_eq(&token_digest).into();
            return None;
        };
        let expected_digest = Sha256::digest(expected.as_bytes());
        if bool::from(expected_digest.ct_eq(&token_digest)) {
            Some(name.to_string())
        } else {
            None
        }
    }

    /// cap on attacker-supplied claimed names recorded
    /// into the append-only (unprunable) arm_audit table. 64 bytes is ample
    /// for any legitimate principal name and bounds the per-rejection bloat.
    pub const CLAIMED_NAME_MAX_BYTES: usize = 64;

    /// Best-effort claimed principal name for rejection audit rows: the
    /// `name` half of an unauthenticated bearer (an attacker-supplied
    /// claim, recorded as such), or "unknown". Never returns token bytes.
    /// Truncated to [`Self::CLAIMED_NAME_MAX_BYTES`] (char-boundary safe) so
    /// unauthenticated requests cannot write unbounded-length names into the
    /// permanently-unprunable audit chain .
    pub fn claimed_name(bearer: Option<&str>) -> String {
        let name = bearer
            .and_then(|b| b.split_once(':'))
            .map(|(n, _)| n)
            .filter(|n| !n.is_empty())
            .unwrap_or("unknown");
        if name.len() <= Self::CLAIMED_NAME_MAX_BYTES {
            return name.to_string();
        }
        let mut cut = Self::CLAIMED_NAME_MAX_BYTES;
        while cut > 0 && !name.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…(truncated)", &name[..cut])
    }
}

/// RIDER C (the invariant) — out-of-band arm-ceremony alerting: the
/// async substitute for the missing second pair of eyes at n=1. Fires a
/// best-effort POST to `ARM_NOTIFY_URL` on every stage, confirm, rejection,
/// and disarm. Plain-text body (ntfy.sh-compatible; any webhook that accepts
/// a raw POST works). Failures are logged loudly but NEVER block the
/// ceremony — and never, ever the disarm path. Unset URL = disabled (boot
/// logs which, so a canary with alerting accidentally off is visible).
#[derive(Clone, Default)]
pub struct ArmNotifier {
    url: Option<String>,
}

impl ArmNotifier {
    pub fn from_env() -> Self {
        let url = std::env::var("ARM_NOTIFY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        match &url {
            Some(u) => {
                tracing::info!(url = %u, "arm-ceremony out-of-band alerting ENABLED (RIDER C)")
            }
            None => tracing::warn!(
                "ARM_NOTIFY_URL unset — arm-ceremony out-of-band alerting DISABLED. \
                 Dual-custody single-operator canary requires it (the invariant RIDER C)."
            ),
        }
        Self { url }
    }

    /// Test-only: a notifier pointed at an explicit URL (or disabled).
    pub fn for_tests(url: Option<String>) -> Self {
        Self { url }
    }

    /// Quiet constructor (no boot log) for secondary callers — e.g. the H7a
    /// recon auto-disarm path, which logs its own spawn. Same `ARM_NOTIFY_URL`
    /// source as `from_env`.
    pub fn from_env_quiet() -> Self {
        let url = std::env::var("ARM_NOTIFY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self { url }
    }

    /// Fire-and-forget. Spawned so the ceremony response never waits on the
    /// alert channel; 5s timeout bounds the spawned task.
    pub fn notify(&self, event: &str, principal: &str, detail: &str) {
        let Some(url) = self.url.clone() else { return };
        let body = format!("ARM-CEREMONY {event} principal={principal} {detail}");
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let sent = client
                .post(&url)
                .header("Title", "gateway arm ceremony")
                .body(body)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
            match sent {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => tracing::error!(
                    status = %resp.status(),
                    "arm notify POST rejected by alert endpoint (best-effort — ceremony unaffected)"
                ),
                Err(e) => tracing::error!(
                    error = %e,
                    "arm notify POST failed (best-effort — ceremony unaffected)"
                ),
            }
        });
    }
}

/// RIDER D (the invariant) — machine-readable deviation tagging for
/// the audit chain. When `GW_ARM_DEVIATION_FLAG` is set (e.g.
/// "dual-custody-single-operator"), every stage/confirm detail string gains
/// ` deviation=<flag> domain=<principal-domain>` so a future n>1 audit can
/// separate single-operator arms from true four-eyes arms without parsing
/// prose. Domains come from `GW_ARM_PRINCIPAL_DOMAINS`
/// ('name:domain,name:domain' — e.g. 'sovereign-op:host').
/// Tags ride INSIDE the hash-chained detail field on purpose: no schema
/// migration on the engine-append-only arm_audit table, and the tags are
/// integrity-protected by the existing chain.
#[derive(Clone, Default)]
pub struct ArmDeviationTags {
    flag: Option<String>,
    domains: Vec<(String, String)>,
}

impl ArmDeviationTags {
    pub fn from_env() -> Self {
        let flag = std::env::var("GW_ARM_DEVIATION_FLAG")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let domains = std::env::var("GW_ARM_PRINCIPAL_DOMAINS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|e| {
                let (n, d) = e.trim().split_once(':')?;
                if n.is_empty() || d.is_empty() {
                    return None;
                }
                Some((n.to_string(), d.to_string()))
            })
            .collect();
        Self { flag, domains }
    }

    /// Test-only constructor.
    pub fn for_tests(flag: Option<String>, domains: Vec<(String, String)>) -> Self {
        Self { flag, domains }
    }

    /// Suffix for an arm_audit detail string ('' when no flag configured).
    pub fn detail_suffix(&self, principal: &str) -> String {
        let Some(flag) = &self.flag else {
            return String::new();
        };
        let domain = self
            .domains
            .iter()
            .find(|(n, _)| n == principal)
            .map(|(_, d)| d.as_str())
            .unwrap_or("undeclared");
        format!(" deviation={flag} domain={domain}")
    }
}

/// Append an arm_audit row, fail-closed for ceremony-success paths: callers
/// that are about to CHANGE state (stage recorded, arm spawned) must abort
/// on Err. Rejection paths call this best-effort (the request is already
/// being refused; a failed rejection-audit is logged, not escalated).
/// In-memory QuarantineState (db = None — test-only) skips the write.
async fn append_arm_audit_row(
    quarantine: &QuarantineState,
    action: &str,
    principal: &str,
    detail: &str,
) -> anyhow::Result<()> {
    if let Some(db) = quarantine.db_for_arm_audit() {
        db.append_arm_audit(action, principal, Some(detail)).await?;
    }
    Ok(())
}

/// Same as `append_arm_audit_row` but swallows (and logs) failures — for
/// rejection paths and the disarm path, where blocking on the audit write
/// would either be pointless (already refusing) or dangerous (fast kill).
pub(super) async fn append_arm_audit_best_effort(
    quarantine: &QuarantineState,
    action: &str,
    principal: &str,
    detail: &str,
) {
    if let Err(e) = append_arm_audit_row(quarantine, action, principal, detail).await {
        tracing::error!(
            action = action,
            principal = principal,
            error = %e,
            "arm_audit append failed (best-effort path) — ceremony decision already taken"
        );
    }
}

/// dual-custody-local-attest B1 (spec §4.3) — best-effort clear of the
/// durable pending-stage row (confirm consumed / expiry / disarm).
/// `Some(stage_id)` = fenced delete (a re-staged ceremony's row survives);
/// `None` = unconditional (disarm). Never blocks the ceremony decision: an
/// orphaned row is inert — expired rows are never served or rehydrated, and
/// a consumed stage_id cannot confirm twice (B4 history-uniqueness).
async fn clear_arm_pending_best_effort(quarantine: &QuarantineState, stage_id: Option<&str>) {
    if let Some(db) = quarantine.db_for_arm_audit() {
        if let Err(e) = db.clear_arm_pending(stage_id).await {
            tracing::error!(
                stage_id = stage_id.unwrap_or("<all>"),
                error = %e,
                "arm_pending clear failed (best-effort) — orphan row is inert; next stage replaces it"
            );
        }
    }
}

/// H7a (recon-divergence → auto-disarm) — programmatic disarm triggered by a
/// safety alarm rather than an operator. Same drain mechanics as the operator
/// `/disarm` and `self_disarm_on_lost_writer_claim` paths: audit the kill
/// intent, fire the out-of-band page, clear any open ceremony, take the kill
/// state, signal the drain, await the ack with the same 5s bound, record
/// kill-switch latency on the SAME telemetry series. Idempotent — an
/// already-disarmed producer is a logged no-op. `principal_label` names the
/// trigger (e.g. "recon-divergence(auto)") in the hash-chained arm_audit.
/// Single-principal by design (H-2 ruling): a safety kill never waits on a
/// second signature.
pub async fn auto_disarm_producer(
    quarantine: &QuarantineState,
    notifier: &ArmNotifier,
    principal_label: &str,
    reason: &str,
) {
    // Council P1: check kill_state FIRST. If already disarmed, short-circuit
    // to avoid per-cadence audit-row spam and ntfy page-storm.
    let state = quarantine.producer_kill_state.lock().take();
    let Some((tx, ack_rx)) = state else {
        tracing::debug!(
            principal = principal_label,
            reason,
            "auto-disarm requested but producer already disarmed (no-op, no page)"
        );
        return;
    };

    append_arm_audit_best_effort(quarantine, "disarm", principal_label, reason).await;
    notifier.notify("disarm", principal_label, reason);

    // Parity with operator /disarm: kill any open ceremony too.
    *quarantine.arm_staging.lock() = None;
    clear_arm_pending_best_effort(quarantine, None).await;
    let kill_sent_at = std::time::Instant::now();
    if tx.send(true).is_err() {
        tracing::error!(
            principal = principal_label,
            "auto-disarm: kill channel dropped — CDC producer already gone"
        );
        return;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), ack_rx).await {
        Ok(Ok(_)) => {
            let drain_ms = (kill_sent_at.elapsed().as_millis() as u64).max(1);
            quarantine.record_kill_switch_latency_ms(drain_ms);
            tracing::error!(
                drain_ms,
                principal = principal_label,
                reason,
                "H7a auto-disarm complete: CDC producer drained on safety alarm"
            );
        }
        Ok(Err(_)) => {
            let crash_ms = (kill_sent_at.elapsed().as_millis() as u64).max(1);
            quarantine.record_kill_switch_latency_ms(crash_ms);
            tracing::error!(
                crash_ms,
                principal = principal_label,
                "auto-disarm: producer dropped ack channel without completing drain"
            );
        }
        Err(_) => {
            quarantine.record_kill_switch_drain_timeout(5_000);
            tracing::error!(
                principal = principal_label,
                "auto-disarm: producer drain timed out after 5 seconds (kill_switch_drain_timeout_total bumped; 5000ms floor recorded)"
            );
        }
    }
}

/// The actual arm action — spawns `cdc_sweep_loop` with the kill-switch
/// wiring. Reachable ONLY through the four-eyes confirm path (p0a); the
/// legacy single-shot route returns 410. Body unchanged from the original
/// `admin_arm_producer_json` (Phase 1 CDC weld) except the p07
/// single-writer gate in front of it (single-writer invariant: refuse-to-arm on a second
/// writer) and the heartbeat watchdog spawned alongside the producer.
async fn arm_producer_start(quarantine: &Arc<QuarantineState>) -> Response {
    // single-writer (single-writer invariant): acquire the singleton writer claim
    // BEFORE any spawn decision. Refused -> 409 (another LIVE writer holds
    // it). DB error -> 500, NOT armed (#13 DB-unavailable = fail-closed).
    // The await happens before producer_kill_state.lock() is taken, so no
    // lock is held across it. db None falls through — the body below
    // already refuses with 500 when no durable DB is wired.
    if let Some(db) = quarantine.db_for_cdc_sweep() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match db
            .try_acquire_writer_claim(
                crate::watch::db::process_instance_uuid(),
                now_ms,
                crate::watch::db::writer_claim_stale_ms(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return problem(
                    StatusCode::CONFLICT,
                    "writer-claim-held",
                    "another writer holds the single-writer claim; refusing to arm (single shared watch.db topology, single-writer invariant)",
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "arm refused: writer-claim acquisition failed (fail-closed)");
                return problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal-error",
                    "writer-claim check failed; refusing to arm (DB-unavailable = fail-closed)",
                );
            }
        }
    }

    let mut lock = quarantine.producer_kill_state.lock();
    if lock.is_some() {
        return json_response(
            StatusCode::CONFLICT,
            serde_json::json!({"error": "Producer already armed"}),
        )
        .into_response();
    }

    if let Some(db_for_sweep) = quarantine.db_for_cdc_sweep() {
        let (kill_tx, mut kill_rx) = tokio::sync::watch::channel(false);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        *lock = Some((kill_tx, ack_rx));

        // single-writer: heartbeat watchdog keeps the claim fresh while
        // armed; self-disarms (fail-closed) if the claim is ever lost.
        tokio::spawn(writer_claim_heartbeat_loop(
            Arc::clone(quarantine),
            db_for_sweep.clone(),
            crate::watch::db::process_instance_uuid().to_string(),
            std::time::Duration::from_millis(crate::watch::db::writer_claim_heartbeat_ms()),
            None,
        ));

        tokio::spawn(async move {
            let (unified_tx, unified_rx) = tokio::sync::watch::channel(false);
            let mut unified_tx = Some(unified_tx);

            let loop_fut = crate::watch::runner::cdc_sweep_loop(db_for_sweep, unified_rx);
            let mut loop_fut = std::pin::pin!(loop_fut);

            loop {
                tokio::select! {
                    _ = &mut loop_fut => { break; }
                    _ = kill_rx.changed() => {
                        if *kill_rx.borrow() {
                            tracing::warn!("Runtime kill-switch activated for CDC producer. Draining in-flight tick...");
                            if let Some(tx) = unified_tx.take() { let _ = tx.send(true); }
                        }
                    }
                }
            }
            let _ = ack_tx.send(());
        });
        json_response(StatusCode::OK, serde_json::json!({"status": "armed"})).into_response()
    } else {
        problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            "Watch DB not available for sweep",
        )
    }
}

/// p0a-four-eyes — `POST /watch/admin/producer/arm/stage`.
///
/// Auth: bearer `name:token` must match a principal in the registry.
/// Requires at least one configured principal (403 otherwise — fail-closed).
/// The second custody domain is the local hardware attestation key.
/// Overwrites any prior unexpired stage (re-stage allowed by the same or a
/// different principal — the stage_id nonce changes, so a confirm prepared
/// against the old stage can no longer land). Returns
/// `{staged_by, stage_id, expires_in_ms}`.
#[allow(clippy::too_many_arguments)]
pub async fn admin_arm_stage_json(
    quarantine: Arc<QuarantineState>,
    principals: Arc<ArmPrincipals>,
    stage_ttl: Duration,
    bearer: Option<String>,
    body: Option<Value>,
    notifier: Arc<ArmNotifier>,
    deviation: Arc<ArmDeviationTags>,
    // B6: false ⇒ this build may NOT arm the real producer (dirty/unidentifiable
    // SHA) — every stage is forced to a rehearsal/DARK ceremony.
    allow_real_arm: bool,
) -> Response {
    // B7 (spec §8): {"rehearse": true} stages a REHEARSAL ceremony — same
    // paths, same crypto, *_rehearsal audit actions, and the producer never
    // starts at confirm. The flag is persisted on the pending row at stage
    // time; the confirm request cannot change it.
    let mut rehearsal = body
        .as_ref()
        .and_then(|v| v.get("rehearse"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // A build that may not arm for real (a
    // `-dirty` / unidentifiable SHA — `allow_real_arm == false`, derived once
    // in main.rs from the EMBEDDED build identity) is forced to a rehearsal/
    // DARK ceremony regardless of the request. Only a clean SHA may arm for
    // spend. Reusing the rehearsal flag keeps the producer-never-starts
    // guarantee on the persisted ROW, not the wire.
    //
    // Regression guard: the B6 dirty-build rehearsal-forcing —
    // and its `tracing::warn!` + `build_id()` formatting — is deferred until
    // AFTER bearer auth + arm-capable authz below. An UNAUTHENTICATED caller
    // must not trigger warn-level logs / formatting work (same posture as the
    // arm-audit-DoS fix on the 401 path). The `rehearsal` decision is
    // unaffected: it is applied before any stage state is derived.
    let Some(principal) = principals.authenticate(bearer.as_deref()) else {
        // P1 : do NOT append a permanent row to the
        // engine-unprunable `arm_audit` hash chain for an UNAUTHENTICATED 401.
        // An attacker who can reach the UDS could otherwise grow that
        // integrity-critical, trigger-unprunable table one row per request with
        // no in-governance remediation (DELETE is trigger-blocked). Count it in
        // a prunable metric and log the claimed (attacker-supplied) name for
        // forensics instead. Permanent rows are reserved for
        // authenticated-but-unauthorized events where the identity is real.
        quarantine.bump_arm_rejected_unauth();
        tracing::warn!(
            claimed_principal = %ArmPrincipals::claimed_name(bearer.as_deref()),
            "arm stage rejected (401: invalid or missing principal bearer) — counted in arm_rejected_unauth_total, not audited"
        );
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or missing arm-principal bearer",
        );
    };

    if !principals.is_arm_capable() {
        append_arm_audit_best_effort(
            &quarantine,
            "stage_rejected",
            &principal,
            "403: no arm principals configured (fail-closed)",
        )
        .await;
        notifier.notify(
            "stage_rejected",
            &principal,
            "403: registry not arm-capable",
        );
        return problem(
            StatusCode::FORBIDDEN,
            "arm-disabled",
            "arm-capable mode requires at least one principal in GW_ARM_PRINCIPALS",
        );
    }

    // Regression guard: B6 dirty-build rehearsal-forcing, now AFTER auth.
    // A build that may not arm for real (a `-dirty` / unidentifiable SHA —
    // `allow_real_arm == false`, derived once in main.rs from the EMBEDDED build
    // identity) is forced to a rehearsal/DARK ceremony regardless of the request.
    if !allow_real_arm && !rehearsal {
        tracing::warn!(
            build_id = %crate::watch::attest::build_id(),
            "arm stage on a build that may not arm for real — forcing REHEARSAL (B6: DARK-only, never arms real producer)"
        );
        rehearsal = true;
    }

    // Nonce binding the eventual confirm to THIS stage (amendment).
    let stage_id = {
        use rand_core::{OsRng, RngCore};
        let mut rng = OsRng;
        let mut bytes = [0u8; 16];
        rng.fill_bytes(&mut bytes);
        hex::encode(bytes)
    };

    // Fail-closed: the stage only takes effect if the audit row landed.
    // B1 (spec §4.3): the durable pending row is written in the SAME tx as
    // the 'stage' audit row — the persisted stage and its audit record land
    // or fail together. Expiry is wall-clock so it survives restart.
    // B2 (spec §5): the confirm challenge is canonicalized exactly ONCE,
    // here, and stored verbatim — every later read (GET /arm/pending, B4
    // verify) serves/compares these stored bytes, never a re-derivation.
    let stage_detail = format!(
        "stage_id={stage_id} ttl_ms={}{}{}",
        stage_ttl.as_millis(),
        if rehearsal { " rehearsal=true" } else { "" },
        deviation.detail_suffix(&principal)
    );
    let iat_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let exp_at_ms = iat_ms + stage_ttl.as_millis() as i64;
    // B5/MF-1: derive the 4 content-binding fields ONCE (the same fn confirm
    // re-derives) and bind them into the signed challenge. Cap is captured in
    // integer cents here, OUTSIDE the JCS path. Fail closed if the ambient cap
    // is not a bindable value (float-saturation guard, ).
    // Attested-arm: the spend window is now SIGNED. Bind the boot-locked window
    // policy (`arm_window_ms_bootlocked()`) — the cap-floor pattern applied to
    // time: the signed value equals the boot policy and can never exceed it, and
    // a post-tap GW_ARM_WINDOW_MS restart cannot change what was already signed.
    let content = match crate::watch::attest::derive_arm_content(
        crate::watch::db::daily_spend_cap(),
        crate::watch::db::arm_window_ms_bootlocked(),
    ) {
        Ok(c) => c,
        Err(reason) => {
            tracing::error!(%reason, "arm stage refused: ambient spend cap is not bindable (fail-closed)");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "spend cap is not a bindable value; stage refused (fail-closed)",
            );
        }
    };
    let challenge_bytes = match crate::watch::attest::build_challenge_bytes(
        &stage_id, &principal, iat_ms, exp_at_ms, &content,
    ) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, "arm stage refused: challenge build failed (fail-closed)");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "challenge build failed; stage refused (fail-closed)",
            );
        }
    };
    if let Some(db) = quarantine.db_for_arm_audit() {
        if let Err(e) = db
            .stage_arm_pending(
                &principal,
                &stage_detail,
                &stage_id,
                challenge_bytes.clone(),
                exp_at_ms,
                rehearsal,
                content.clone(),
                crate::watch::attest::CHALLENGE_FORMAT_VERSION,
            )
            .await
        {
            tracing::error!(error = %e, "arm stage refused: arm_audit+arm_pending tx failed (fail-closed)");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "arm_audit write failed; stage refused (fail-closed)",
            );
        }
    }

    *quarantine.arm_staging.lock() = Some(crate::watch::quarantine::StagedArm {
        stage_id: stage_id.clone(),
        staged_by: principal.clone(),
        staged_at: std::time::Instant::now(),
        ttl: stage_ttl,
    });

    // RIDER C: out-of-band alert on every stage — the absent second eye
    // hears about the ceremony starting, not just ending.
    let stage_action = if rehearsal {
        "stage_rehearsal"
    } else {
        "stage"
    };
    notifier.notify(stage_action, &principal, &stage_detail);

    // B2 (spec §4.1): the stage response carries the challenge (base64 of
    // the canonical bytes) so bin/arm can hand it straight to the signing
    // helper without a second round-trip.
    let challenge_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&challenge_bytes)
    };
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "staged_by": principal,
            "stage_id": stage_id,
            "expires_in_ms": stage_ttl.as_millis() as u64,
            "challenge": challenge_b64,
            "rehearsal": rehearsal,
        }),
    )
}

/// dual-custody-local-attest B1 (spec §4.3) — `GET /watch/admin/producer/arm/pending`.
///
/// Auth: bearer `name:token` must match a principal in the registry (same
/// 401 posture as stage: unauthenticated rejections are counted, never
/// audited). Returns `{stage_id, challenge, expires_in_ms}` for the open
/// unexpired stage, or 404. The DURABLE `arm_pending` row is the truth —
/// this endpoint works across a sidecar restart, which is exactly its job
/// (crash-resume: `bin/arm` calls it first and resumes instead of
/// re-firing). The challenge is created once at stage time (B2, spec §5)
/// and is stable for the stage's life — re-fetching returns the same
/// verbatim stored bytes, never a new nonce.
pub async fn admin_arm_pending_json(
    quarantine: Arc<QuarantineState>,
    principals: Arc<ArmPrincipals>,
    bearer: Option<String>,
) -> Response {
    let Some(_principal) = principals.authenticate(bearer.as_deref()) else {
        quarantine.bump_arm_rejected_unauth();
        tracing::warn!(
            claimed_principal = %ArmPrincipals::claimed_name(bearer.as_deref()),
            "arm pending read rejected (401: invalid or missing principal bearer) — counted in arm_rejected_unauth_total, not audited"
        );
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or missing arm-principal bearer",
        );
    };

    let Some(db) = quarantine.db_for_arm_audit() else {
        // In-memory test path (no durable DB) — nothing persisted to serve.
        return problem(StatusCode::NOT_FOUND, "no-stage", "no open arm stage");
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    match db.get_arm_pending(now_ms).await {
        Ok(Some(row)) => {
            use base64::Engine as _;
            // B3 (review): render the 4 content-binding fields
            // from the PERSISTED arm_pending row — what the human is shown
            // before they tap, the verbatim staged intent, NEVER live ambient
            // config. RESIDUAL (no-trusted-display): the host that renders this
            // is the same host that signs; an out-of-band confirm channel
            // (phone push of the decoded challenge) is the only full mitigation
            // and is a Sovereign-decision v1.x ticket, not in this PR.
            json_response(
                StatusCode::OK,
                serde_json::json!({
                    "stage_id": row.stage_id,
                    "challenge": base64::engine::general_purpose::STANDARD.encode(&row.challenge_bytes),
                    "expires_in_ms": (row.exp_at_ms - now_ms).max(0) as u64,
                    "rehearsal": row.rehearsal,
                    "build_id": row.build_id,
                    "enabled_surface": row.enabled_surface,
                    "effective_daily_cap_cents": row.effective_daily_cap_cents,
                    "tenant": row.tenant,
                    "challenge_format_version": row.challenge_format_version,
                }),
            )
        }
        Ok(None) => problem(StatusCode::NOT_FOUND, "no-stage", "no open arm stage"),
        Err(e) => {
            tracing::error!(error = %e, "arm_pending read failed");
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "arm_pending read failed",
            )
        }
    }
}

/// `POST /watch/admin/producer/arm/confirm` — dual-custody-local-attest
/// (spec §4.2). The ONLY confirm mechanism: bearer token A (custody
/// domain 1) plus an enclave/security-key ES256 signature over the stored
/// stage challenge (custody domain 2). The OTC and bearer-only legacy
/// paths are RETIRED (spec §9 / §2) — a body without credential fields is
/// a 400, and nothing arms on tokens alone.
///
/// Rejections, in check order: 401 (bad bearer), 403 (registry not
/// arm-capable), 400 (missing stage_id / malformed or missing credential
/// fields), then the one-tx attest outcomes (`admin_arm_confirm_attest`):
/// 410 no stage / expired, 409 stage_id mismatch, 403 attest-rejected
/// with the audited §6 reason, 200 armed (or idempotent re-confirm).
#[allow(clippy::too_many_arguments)] // stable arm-confirm handler API; mirrors the router state fields.
pub async fn admin_arm_confirm_json(
    quarantine: Arc<QuarantineState>,
    principals: Arc<ArmPrincipals>,
    bearer: Option<String>,
    body: Option<Value>,
    notifier: Arc<ArmNotifier>,
    deviation: Arc<ArmDeviationTags>,
    attest_keys: Arc<crate::watch::attest::AttestKeyRegistry>,
    allow_real_arm: bool,
) -> Response {
    let Some(confirmer) = principals.authenticate(bearer.as_deref()) else {
        // P1 : same as the stage 401 path — an
        // UNAUTHENTICATED rejection must NOT append to the engine-unprunable
        // `arm_audit` chain. Count + log instead; reserve permanent rows for
        // authenticated-but-unauthorized events with a real principal identity.
        quarantine.bump_arm_rejected_unauth();
        tracing::warn!(
            claimed_principal = %ArmPrincipals::claimed_name(bearer.as_deref()),
            "arm confirm rejected (401: invalid or missing principal bearer) — counted in arm_rejected_unauth_total, not audited"
        );
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or missing arm-principal bearer",
        );
    };

    if !principals.is_arm_capable() {
        append_arm_audit_best_effort(
            &quarantine,
            "confirm_rejected",
            &confirmer,
            "403: no arm principals configured (fail-closed)",
        )
        .await;
        notifier.notify(
            "confirm_rejected",
            &confirmer,
            "403: registry not arm-capable",
        );
        return problem(
            StatusCode::FORBIDDEN,
            "arm-disabled",
            "arm-capable mode requires at least one principal in GW_ARM_PRINCIPALS",
        );
    }

    let Some(presented_stage_id) = body
        .as_ref()
        .and_then(|v| v.get("stage_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
    else {
        append_arm_audit_best_effort(
            &quarantine,
            "confirm_rejected",
            &confirmer,
            "400: missing stage_id (confirm must ratify a specific stage)",
        )
        .await;
        return problem(
            StatusCode::BAD_REQUEST,
            "stage-id-required",
            "confirm body must carry the stage_id echoed at stage time",
        );
    };

    // B6 (spec §4.2/§9): the local-attest leg IS the confirm path — the
    // ENTIRE decision (durable expiry check → crypto verify → §6 audit row
    // → pending delete) runs in one SQLite tx; the in-memory staging slot is
    // only a cache and is never consulted. A body without credential fields
    // gets the attest leg's 400 — nothing arms on bearer tokens alone.
    admin_arm_confirm_attest(
        quarantine,
        presented_stage_id,
        confirmer,
        body,
        notifier,
        deviation,
        attest_keys,
        allow_real_arm,
    )
    .await
}

/// dual-custody-local-attest B4 (spec §4.2) — the local-attest confirm leg.
/// Reached from `admin_arm_confirm_json` AFTER bearer auth + arm-capable +
/// stage_id extraction. Verification order, each fail-closed with a distinct
/// audited rejection reason: stage exists and unexpired (durable wall-clock
/// row — the truth) → challenge bytes taken from the STORED pending stage →
/// registry loaded → credential_id enrolled → ES256 valid (DER only) →
/// (fido2 only) counter strictly increasing → ARM. Steps 1–6 run inside ONE
/// SQLite transaction (`confirm_arm_attest`, atomic confirmation invariant). The
/// same-principal rule is retired for this path (spec §2): the second
/// custody domain is the enclave key, not a second token.
#[allow(clippy::too_many_arguments)] // stable arm-confirm-attest leg; mirrors the router state fields.
async fn admin_arm_confirm_attest(
    quarantine: Arc<QuarantineState>,
    presented_stage_id: String,
    confirmer: String,
    body: Option<Value>,
    notifier: Arc<ArmNotifier>,
    deviation: Arc<ArmDeviationTags>,
    attest_keys: Arc<crate::watch::attest::AttestKeyRegistry>,
    allow_real_arm: bool,
) -> Response {
    use base64::Engine as _;

    // stage_ids are 16-byte CSPRNG hex minted by this process. Reject any
    // other shape BEFORE it reaches the audit-history LIKE queries — an
    // attacker-supplied wildcard must never pattern-match the chain.
    if presented_stage_id.len() != 32 || !presented_stage_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        append_arm_audit_best_effort(
            &quarantine,
            "confirm_rejected",
            &confirmer,
            "400: malformed stage_id (not 32-hex)",
        )
        .await;
        return problem(
            StatusCode::BAD_REQUEST,
            "stage-id-malformed",
            "stage_id must be the 32-hex nonce echoed at stage time",
        );
    }

    let field = |k: &str| -> Option<String> {
        body.as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let Some(credential_id) = field("credential_id") else {
        return problem(
            StatusCode::BAD_REQUEST,
            "credential-id-required",
            "confirm body must carry credential_id",
        );
    };
    let Some(credential_type) = field("credential_type") else {
        return problem(
            StatusCode::BAD_REQUEST,
            "credential-type-required",
            "confirm body must carry credential_type",
        );
    };
    let signature_der =
        match field("signature").map(|s| base64::engine::general_purpose::STANDARD.decode(s)) {
            Some(Ok(bytes)) => bytes,
            _ => {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "signature-required",
                    "confirm body must carry a base64 DER ES256 signature",
                );
            }
        };
    // authenticator_data: required for fido2-es256, must be null/absent for
    // se-p256 (the §5 composition is type-keyed, no mixing).
    let authenticator_data = match body.as_ref().and_then(|v| v.get("authenticator_data")) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match base64::engine::general_purpose::STANDARD.decode(s) {
            Ok(b) => Some(b),
            Err(_) => {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "authenticator-data-malformed",
                    "authenticator_data must be base64",
                );
            }
        },
        Some(_) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "authenticator-data-malformed",
                "authenticator_data must be a base64 string or null",
            );
        }
    };

    // For FIDO2 via browser WebAuthn get() (macOS HID workaround, parallel to enroll).
    // The browser produces a real clientDataJSON; we hash *those bytes* so the
    // authenticatorData || clientDataHash composition the key actually signed will verify.
    // Native CTAP path continues to pass raw stage challenge (fn will sha it).
    let client_data_for_hash: Option<Vec<u8>> =
        match body.as_ref().and_then(|v| v.get("client_data_json")) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => match base64::engine::general_purpose::STANDARD.decode(s) {
                Ok(b) => Some(b),
                Err(_) => {
                    return problem(
                        StatusCode::BAD_REQUEST,
                        "client-data-malformed",
                        "client_data_json must be base64",
                    );
                }
            },
            Some(_) => {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "client-data-malformed",
                    "client_data_json must be a base64 string or null",
                );
            }
        };

    let Some(db) = quarantine.db_for_arm_audit() else {
        // No durable DB (in-memory test path) — the attest ceremony cannot
        // run at all: there is no stored challenge to verify against.
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            "local-attest confirm requires the durable watch.db (fail-closed)",
        );
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // Signed-material invariant: capture the signature material to PERSIST on
    // a real confirm so the reserve can re-verify the ES256 signature at spend
    // time. Cloned BEFORE the verify closure moves the originals.
    let signed_material = crate::watch::db::PersistedArmSignature {
        credential_id: credential_id.clone(),
        credential_type: credential_type.clone(),
        signature_der: signature_der.clone(),
        authenticator_data: authenticator_data.clone(),
        client_data_json: client_data_for_hash.clone(),
    };

    // Crypto closure — runs INSIDE the confirm tx, over the verbatim stored
    // challenge bytes, preserving the spec's §4.2 rejection order (registry
    // and credential checks come AFTER the stage-exists/unexpired checks).
    let verify = {
        let registry = Arc::clone(&attest_keys);
        let credential_id = credential_id.clone();
        let credential_type = credential_type.clone();
        let client_data_for_hash = client_data_for_hash.clone();
        move |challenge_bytes: &[u8]| -> Result<crate::watch::db::AttestVerification, String> {
            use crate::watch::attest as att;
            if !registry.is_loaded() {
                return Err("registry_unloaded".to_string());
            }
            let Some(cred) = registry.get(&credential_id) else {
                return Err("unknown_credential".to_string());
            };
            if cred.credential_type != credential_type {
                return Err("unknown_credential".to_string());
            }
            let pk = base64::engine::general_purpose::STANDARD
                .decode(&cred.public_key)
                .map_err(|_| "bad_signature".to_string())?;
            let sig_counter = match cred.credential_type.as_str() {
                att::CREDENTIAL_TYPE_SE_P256 => {
                    if authenticator_data.is_some() {
                        return Err("bad_signature".to_string());
                    }
                    att::verify_se_p256(&pk, &signature_der, challenge_bytes).map_err(|e| {
                        tracing::warn!(reason = e, "se-p256 confirm verify failed");
                        "bad_signature".to_string()
                    })?;
                    0
                }
                att::CREDENTIAL_TYPE_FIDO2_ES256 => {
                    let Some(ad) = authenticator_data.as_deref() else {
                        return Err("bad_signature".to_string());
                    };
                    // Browser path: verify the challenge embedded in clientDataJSON
                    // matches the stored stage challenge (council BLOCK: without this
                    // check, a captured (clientDataJSON, sig) from stage A could confirm
                    // stage B). Native CTAP path uses the raw challenge bytes directly.
                    let chal_for_fido2: &[u8] = if let Some(cdj) = client_data_for_hash.as_deref() {
                        // Fail-closed: parse clientDataJSON and assert challenge
                        // binding + type. Any parse failure rejects.
                        use base64::Engine as _;
                        let cdj_val: serde_json::Value = serde_json::from_slice(cdj)
                            .map_err(|_| {
                                tracing::warn!("fido2 browser path: clientDataJSON is not valid JSON (fail-closed)");
                                "bad_signature".to_string()
                            })?;
                        let cdj_challenge_b64url = cdj_val.get("challenge")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                tracing::warn!("fido2 browser path: clientDataJSON missing 'challenge' field (fail-closed)");
                                "bad_signature".to_string()
                            })?;
                        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(cdj_challenge_b64url)
                            .map_err(|_| {
                                tracing::warn!("fido2 browser path: clientDataJSON challenge is not valid base64url");
                                "bad_signature".to_string()
                            })?;
                        if decoded != challenge_bytes {
                            tracing::warn!("fido2 browser path: clientDataJSON challenge does not match stored stage challenge (replay blocked)");
                            return Err("bad_signature".to_string());
                        }
                        if cdj_val.get("type").and_then(|v| v.as_str()) != Some("webauthn.get") {
                            tracing::warn!(
                                "fido2 browser path: clientDataJSON type is not 'webauthn.get'"
                            );
                            return Err("bad_signature".to_string());
                        }
                        cdj
                    } else {
                        challenge_bytes
                    };
                    let v = att::verify_fido2_es256(&pk, &signature_der, chal_for_fido2, ad)
                        .map_err(|e| {
                            tracing::warn!(reason = e, "fido2-es256 confirm verify failed");
                            "bad_signature".to_string()
                        })?;
                    v.counter
                }
                _ => return Err("unknown_credential".to_string()),
            };
            Ok(crate::watch::db::AttestVerification {
                credential_id: cred.credential_id.clone(),
                credential_type: cred.credential_type.clone(),
                sig_counter,
            })
        }
    };

    // B5/MF-1: re-derive the content from CURRENT ambient (same fn stage used).
    // confirm_arm_attest strict-equality-compares this against the PERSISTED
    // staged values inside the tx, before accepting the signature. Fail closed
    // if the ambient cap is not bindable (float-saturation guard, b5429114).
    let expected_content = match crate::watch::attest::derive_arm_content(
        crate::watch::db::daily_spend_cap(),
        // Attested-arm: re-derive the SAME boot-locked window stage signed (same
        // boot ⇒ same value); the signed `spend_window_ms` carries it.
        crate::watch::db::arm_window_ms_bootlocked(),
    ) {
        Ok(c) => c,
        Err(reason) => {
            append_arm_audit_best_effort(
                &quarantine,
                "confirm_rejected",
                &confirmer,
                &format!(
                    "500: ambient spend cap not bindable ({reason}); confirm refused (fail-closed)"
                ),
            )
            .await;
            tracing::error!(%reason, "arm confirm refused: ambient spend cap is not bindable (fail-closed)");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "spend cap is not a bindable value; confirm refused (fail-closed)",
            );
        }
    };
    let outcome = match db
        .confirm_arm_attest(
            &presented_stage_id,
            &confirmer,
            &deviation.detail_suffix(&confirmer),
            now_ms,
            // Attested-arm (P0-2) — grant the producer's armed generation. The running
            // producer reads `WATCH_REPLAY_EPOCH` (`current_replay_epoch()`) at
            // claim time; writing the SAME value into active_arm makes the
            // reserve's epoch cross-check a real two-term match.
            crate::watch::dispatcher::current_replay_epoch(),
            // Attested-arm (B1 / HIGH split-brain) — the spend-window expiry is no
            // longer passed in: confirm derives it from the SIGNED tap time
            // (signed.iat_ms) + the BOOT-LOCKED window, the same formula the
            // reserve gate recomputes, so the column and the gate cannot drift
            // and no runtime env read governs the spend horizon.
            allow_real_arm,
            expected_content,
            signed_material,
            verify,
        )
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "local-attest confirm tx failed (fail-closed; stage left intact)");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "confirm transaction failed (fail-closed)",
            );
        }
    };

    use crate::watch::db::ArmConfirmTxOutcome as Tx;
    match outcome {
        Tx::Verified {
            staged_by,
            detail,
            rehearsal: true,
            ..
        } => {
            // B7 (spec §8): rehearsal confirm — every code path ran except
            // this one: arm_producer_start is never called. The
            // confirm_rehearsal row is already committed.
            {
                let mut staging = quarantine.arm_staging.lock();
                if staging
                    .as_ref()
                    .is_some_and(|s| s.stage_id == presented_stage_id)
                {
                    *staging = None;
                }
            }
            notifier.notify(
                "confirm_rehearsal",
                &confirmer,
                &format!("rehearsal stage_id={presented_stage_id} staged_by={staged_by} (producer NOT started)"),
            );
            tracing::info!(detail = %detail, "local-attest REHEARSAL confirm verified — producer not started");
            json_response(
                StatusCode::OK,
                serde_json::json!({"status": "rehearsal-ok", "stage_id": presented_stage_id}),
            )
        }
        Tx::Verified {
            staged_by, detail, ..
        } => {
            // The §6 confirm row is committed and the pending row consumed —
            // mirror that into the in-memory cache (fenced on our stage_id).
            {
                let mut staging = quarantine.arm_staging.lock();
                if staging
                    .as_ref()
                    .is_some_and(|s| s.stage_id == presented_stage_id)
                {
                    *staging = None;
                }
            }
            // B6 NOTE (grok `ab533eae` HIGH#3): the dirty-build veto is enforced
            // upstream INSIDE `confirm_arm_attest` — a build that may not arm for
            // real folds to an effective rehearsal there (honest
            // `confirm_rehearsal` + `dark_reason` in the unprunable chain) and
            // returns `rehearsal=true`, so it lands in the rehearsal branch above
            // and never reaches here. Reaching this non-rehearsal branch
            // therefore implies `allow_real_arm` held at confirm time.
            let resp = arm_producer_start(&quarantine).await;
            notifier.notify(
                "confirm",
                &confirmer,
                &format!(
                    "local-attest stage_id={presented_stage_id} staged_by={staged_by} arm_http={}",
                    resp.status().as_u16()
                ),
            );
            tracing::info!(detail = %detail, "local-attest confirm verified");
            // Same posture as the legacy path: a refused arm after a
            // committed confirm row gets a best-effort rejection row so the
            // chain tells the whole story.
            if resp.status() != StatusCode::OK {
                append_arm_audit_best_effort(
                    &quarantine,
                    "confirm_rejected",
                    &confirmer,
                    &format!(
                        "arm_producer_start refused after confirmed stage {presented_stage_id}: HTTP {}",
                        resp.status().as_u16()
                    ),
                )
                .await;
            }
            resp
        }
        Tx::AlreadyConfirmed => json_response(
            StatusCode::OK,
            serde_json::json!({"status": "armed", "idempotent": true}),
        ),
        Tx::NoPendingStage => {
            append_arm_audit_best_effort(
                &quarantine,
                "confirm_rejected",
                &confirmer,
                "410: no_pending_stage",
            )
            .await;
            notifier.notify("confirm_rejected", &confirmer, "410: no_pending_stage");
            problem(
                StatusCode::GONE,
                "no-stage",
                "no staged arm to confirm; stage first",
            )
        }
        Tx::Expired => {
            // The tx already deleted the durable row; drop the cache too.
            {
                let mut staging = quarantine.arm_staging.lock();
                if staging
                    .as_ref()
                    .is_some_and(|s| s.stage_id == presented_stage_id)
                {
                    *staging = None;
                }
            }
            append_arm_audit_best_effort(
                &quarantine,
                "confirm_rejected",
                &confirmer,
                "410: challenge_expired",
            )
            .await;
            notifier.notify("confirm_rejected", &confirmer, "410: challenge_expired");
            problem(
                StatusCode::GONE,
                "stage-expired",
                "staged arm expired; re-stage and confirm within the TTL",
            )
        }
        Tx::StageIdMismatch => {
            append_arm_audit_best_effort(
                &quarantine,
                "confirm_rejected",
                &confirmer,
                "409: stage_id mismatch (confirm bound to a different stage)",
            )
            .await;
            notifier.notify("confirm_rejected", &confirmer, "409: stage_id mismatch");
            problem(
                StatusCode::CONFLICT,
                "stage-id-mismatch",
                "presented stage_id does not match the current stage",
            )
        }
        Tx::Rejected { reason } => {
            append_arm_audit_best_effort(
                &quarantine,
                "confirm_rejected",
                &confirmer,
                &format!("403: {reason}"),
            )
            .await;
            notifier.notify("confirm_rejected", &confirmer, &format!("403: {reason}"));
            problem(
                StatusCode::FORBIDDEN,
                "attest-rejected",
                "local-attest verification failed; see audit chain",
            )
        }
    }
}

/// LEGACY — `POST /watch/admin/producer/arm` (single-shot, single-bearer).
/// Removed by p0a-four-eyes (the dual-custody invariant): always 410 Gone pointing
/// at the stage/confirm ceremony so there is no four-eyes bypass.
pub async fn admin_arm_producer_json() -> Response {
    problem(
        StatusCode::GONE,
        "gone",
        "single-shot arm removed (four-eyes): POST /watch/admin/producer/arm/stage then /watch/admin/producer/arm/confirm with a second principal",
    )
}

/// `POST /watch/admin/producer/disarm` — the kill switch. Deliberately
/// single-principal (see module comment): accepts EITHER the shared watch
/// admin bearer (ops continuity, unchanged) OR any single arm-principal
/// bearer. Writes an arm_audit 'disarm' row (best-effort, never blocks the
/// kill) recording who pulled it, before the drain begins.
/// the arm/disarm admin routes as a self-contained,
/// lib-level axum Router so the REAL route wiring (Bearer-prefix extraction,
/// /arm -> 410, stage/confirm mapping, state plumbing) is testable with
/// `tower::ServiceExt::oneshot` AND is the single source main.rs merges —
/// the binary and the tests can no longer drift apart.
#[derive(Clone)]
pub struct ArmAdminRouterState {
    pub quarantine: Arc<QuarantineState>,
    pub principals: Arc<ArmPrincipals>,
    pub stage_ttl: Duration,
    pub admin_token: String,
    /// RIDER C — out-of-band ceremony alerting (disabled when URL unset).
    pub notifier: Arc<ArmNotifier>,
    /// RIDER D — deviation/domain tags appended to audit detail strings.
    pub deviation: Arc<ArmDeviationTags>,
    /// B4 (spec §7.2) — boot-loaded enrolled-credential registry; unloaded
    /// = every local-attest confirm rejects (`registry_unloaded`).
    pub attest_keys: Arc<crate::watch::attest::AttestKeyRegistry>,
    /// May this build arm the real
    /// producer? main.rs derives it ONCE from the EMBEDDED build identity
    /// (`!attest::build_is_dirty()`) — a `-dirty` build is `false` and every
    /// stage is forced to a rehearsal/DARK ceremony (the producer never
    /// starts). Carried as construction-time config (like `admin_token`),
    /// NOT a runtime env knob, so no agent can flip a dirty build to
    /// real-arm; production can only ever set `true` from a clean SHA.
    pub allow_real_arm: bool,
}

/// Canonical `Authorization: Bearer <x>` extraction shared by every arm
/// admin route (previously duplicated per-handler in main.rs).
fn bearer_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

async fn arm_legacy_route() -> Response {
    admin_arm_producer_json().await
}

async fn arm_stage_route(
    axum::extract::State(st): axum::extract::State<ArmAdminRouterState>,
    headers: axum::http::HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    admin_arm_stage_json(
        st.quarantine,
        st.principals,
        st.stage_ttl,
        bearer_from_headers(&headers),
        body.map(|axum::Json(v)| v),
        st.notifier,
        st.deviation,
        st.allow_real_arm,
    )
    .await
}

async fn arm_pending_route(
    axum::extract::State(st): axum::extract::State<ArmAdminRouterState>,
    headers: axum::http::HeaderMap,
) -> Response {
    admin_arm_pending_json(st.quarantine, st.principals, bearer_from_headers(&headers)).await
}

async fn arm_confirm_route(
    axum::extract::State(st): axum::extract::State<ArmAdminRouterState>,
    headers: axum::http::HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    admin_arm_confirm_json(
        st.quarantine,
        st.principals,
        bearer_from_headers(&headers),
        body.map(|axum::Json(v)| v),
        st.notifier,
        st.deviation,
        st.attest_keys,
        st.allow_real_arm,
    )
    .await
}

async fn disarm_route(
    axum::extract::State(st): axum::extract::State<ArmAdminRouterState>,
    headers: axum::http::HeaderMap,
) -> Response {
    admin_disarm_producer_json(
        st.quarantine,
        st.admin_token,
        st.principals,
        bearer_from_headers(&headers),
        st.notifier,
    )
    .await
}

/// Build the arm/disarm admin sub-router. Generic over the caller's missing
/// state `S` so main.rs can `.merge(...)` it into the AppState router while
/// tests drive it standalone (`Router<()>`).
pub fn arm_admin_router<S>(state: ArmAdminRouterState) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    use axum::routing::{get, post};
    axum::Router::new()
        // p0a-four-eyes (the dual-custody invariant): legacy single-shot arm is 410
        // Gone; arming requires stage (principal A) + confirm (principal B).
        .route("/watch/admin/producer/arm", post(arm_legacy_route))
        .route("/watch/admin/producer/arm/stage", post(arm_stage_route))
        // B1 (spec §4.3): crash-resume read of the open stage (bin/arm
        // resumes instead of re-firing within the TTL).
        .route("/watch/admin/producer/arm/pending", get(arm_pending_route))
        .route("/watch/admin/producer/arm/confirm", post(arm_confirm_route))
        .route("/watch/admin/producer/disarm", post(disarm_route))
        .with_state(state)
}

pub async fn admin_disarm_producer_json(
    quarantine: Arc<QuarantineState>,
    admin_token: String,
    principals: Arc<ArmPrincipals>,
    bearer: Option<String>,
    notifier: Arc<ArmNotifier>,
) -> Response {
    let principal = if admin_token_matches(&admin_token, bearer.as_deref()) {
        "admin(shared-token)".to_string()
    } else if let Some(name) = principals.authenticate(bearer.as_deref()) {
        name
    } else {
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or missing admin token",
        );
    };

    // Audit the kill intent BEFORE the drain so the row exists even if the
    // drain times out / crashes. Best-effort: fast kill must never block on
    // the audit write.
    append_arm_audit_best_effort(&quarantine, "disarm", &principal, "disarm requested").await;
    // RIDER C: disarm alert is spawned best-effort — it can never slow the
    // kill path.
    notifier.notify("disarm", &principal, "disarm requested");

    // B1 (spec §4.3): disarm kills any open ceremony too — pending stage is
    // cleared in memory and durably (unconditional: the operator pulling the
    // kill switch ends every in-flight ceremony). Best-effort, same posture
    // as the audit row above — the drain below is never blocked on it.
    *quarantine.arm_staging.lock() = None;
    clear_arm_pending_best_effort(&quarantine, None).await;

    let state = quarantine.producer_kill_state.lock().take();
    if let Some((tx, ack_rx)) = state {
        // watch telemetry (telemetry invariant): kill-switch latency = wall time from the
        // disarm signal to the drain ack. Recorded on every successful disarm.
        let kill_sent_at = std::time::Instant::now();
        if tx.send(true).is_err() {
            tracing::error!("Failed to send kill signal; CDC producer may have already crashed.");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "Producer channel dropped unexpectedly",
            )
            .into_response();
        }
        // Wait for graceful drain with timeout
        match tokio::time::timeout(std::time::Duration::from_secs(5), ack_rx).await {
            Ok(Ok(_)) => {
                // Sub-ms drains round up to 1 so a recorded disarm is never
                // confused with "no disarm yet" (0) on the scrape surface.
                let drain_ms = (kill_sent_at.elapsed().as_millis() as u64).max(1);
                quarantine.record_kill_switch_latency_ms(drain_ms);
                json_response(
                    StatusCode::OK,
                    serde_json::json!({"status": "disarmed", "drain_ms": drain_ms}),
                )
                .into_response()
            }
            Ok(Err(_)) => {
                // the crash path must still record a
                // latency observation — the wall time until the ack channel
                // dropped is the real "how long until we knew" number. The
                // scraped distribution may no longer exclude the bad cases.
                let crash_ms = (kill_sent_at.elapsed().as_millis() as u64).max(1);
                quarantine.record_kill_switch_latency_ms(crash_ms);
                tracing::error!(
                    crash_ms,
                    "CDC producer loop dropped ack channel without completing drain (panic/crash)."
                );
                problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal-error",
                    "Producer crashed during drain",
                )
                .into_response()
            }
            Err(_) => {
                // a drain TIMEOUT is the worst case the
                // alarm_latency_p99 in single-writer invariant's max_loss derivation must
                // see. Record a 5000ms floor observation + bump the timeout
                // counter instead of recording nothing.
                quarantine.record_kill_switch_drain_timeout(5_000);
                tracing::error!("CDC producer drain timed out after 5 seconds (kill_switch_drain_timeout_total bumped; 5000ms floor recorded).");
                problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "timeout",
                    "Producer drain timed out",
                )
                .into_response()
            }
        }
    } else {
        json_response(
            StatusCode::OK,
            serde_json::json!({"status": "already_disarmed"}),
        )
        .into_response()
    }
}
