//! Capability-token checks for Watch prepare and execute.
//!
//! Structured tokens and the opaque prepare fallback live here, with the
//! rejection counters they bump. A database error denies; it is not treated
//! as an empty allowlist.

static CAP_TOKEN_REJECTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// T21a remaining-lifetime ceiling for structured capability tokens (24h).
/// Shared by authorize (`is_capability_token_valid`) and admin mint so the
/// two sides cannot drift. Crate-private: not a public API contract.
pub(crate) const MAX_CAPABILITY_TOKEN_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

pub fn cap_token_rejected_total() -> u64 {
    CAP_TOKEN_REJECTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Pre-seal W2 — count of capability-token checks DENIED because the backing DB
/// query errored (prepare/query/iteration Err), as distinct from a clean empty
/// result. The legacy fallback previously failed OPEN on such an error (skipped
/// the DB check and fell through to the env allowlist); it now fails CLOSED and
/// bumps this so a transient/poisoned DB that hides a tenant's tokens is
/// visible rather than silent. Mirrors the CAP_TOKEN_REJECTED pattern. Also
/// bumped by the structured-token allowed_workers DB-error path (#3b).
///
/// Exported on `/watch/stats` as `cap_token_db_error_deny_total` →
/// `gw_watch_cap_token_db_error_deny_total`.
static CAP_TOKEN_DB_ERROR_DENY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn cap_token_db_error_deny_total() -> u64 {
    CAP_TOKEN_DB_ERROR_DENY.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn allowed_worker_policy_allows(
    workers_json: &str,
    actor: &str,
) -> Result<bool, serde_json::Error> {
    let allowed_workers = serde_json::from_str::<Vec<String>>(workers_json)?;
    Ok(allowed_workers.is_empty() || allowed_workers.iter().any(|worker| worker == actor))
}

/// Atomically bind `(tenant, token_id)` to `directive_id` for durable replay.
///
/// First successful insert wins. Same-directive retries may proceed; a different
/// directive id against a consumed `token_id` is refused. Survives restart via
/// the Watch DB table `capability_token_consumptions`.
fn bind_capability_token_consumption(
    conn: &rusqlite::Connection,
    tenant: &str,
    token_id: &str,
    directive_id: &str,
) -> bool {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    // INSERT OR IGNORE is atomic with the subsequent read: first claim wins.
    if let Err(e) = conn.execute(
        "INSERT OR IGNORE INTO capability_token_consumptions
            (tenant, token_id, directive_id, consumed_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![tenant, token_id, directive_id, now_ms],
    ) {
        CAP_TOKEN_DB_ERROR_DENY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::error!(
            tenant = tenant,
            token_id = token_id,
            error = %e,
            "capability-token consumption insert errored — denying (fail closed)"
        );
        return false;
    }
    match conn.query_row(
        "SELECT directive_id FROM capability_token_consumptions
         WHERE tenant = ?1 AND token_id = ?2",
        rusqlite::params![tenant, token_id],
        |r| r.get::<_, String>(0),
    ) {
        Ok(bound) => bound == directive_id,
        Err(e) => {
            CAP_TOKEN_DB_ERROR_DENY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                tenant = tenant,
                token_id = token_id,
                error = %e,
                "capability-token consumption read errored — denying (fail closed)"
            );
            false
        }
    }
}

/// Validate a capability token for `desired_authority`.
///
/// `claimed_directive_id` is required for **execute** (structured bind + durable
/// same-directive-only replay). Prepare may pass `None` and still use opaque
/// DB/env bootstrap tokens. Opaque tokens never authorize **execute**.
pub fn is_capability_token_valid(
    conn: &rusqlite::Connection,
    tenant: &str,
    token: &str,
    desired_authority: &str,
    claimed_directive_id: Option<&str>,
) -> bool {
    if token.is_empty() {
        return false;
    }

    // First, try verifying as a structured capability token
    if let Ok(cap_token) = serde_json::from_str::<sovereign_protocol::types::CapabilityToken>(token)
    {
        // Wire-required identity fields: missing/empty fails closed (no kid/keyset).
        if cap_token.token_id.is_empty() || cap_token.directive_id.is_empty() {
            CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }

        // T4: removed `if true` deadcode wrapper; Ed25519 verify now always runs (no bypass).
        // Guard pre-init panic: if key not ready, fail closed (no elevated action).
        let signing_key = match std::panic::catch_unwind(crate::keymgmt::directive_signing_key) {
            Ok(k) => k,
            Err(_) => return false,
        };
        if !signing_key.verify_capability_token(&cap_token) {
            // Structured parse succeeded: never fall through to opaque.
            return false;
        }

        // Tenant + authority action match
        if cap_token.tenant != tenant
            || !cap_token
                .allowed_actions
                .iter()
                .any(|a| a == desired_authority)
        {
            return false;
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        // T21a: reject immortal tokens (expires_at==0) and tokens with
        // remaining lifetime > 24h. Shared with admin mint so both sides
        // cannot drift.
        if cap_token.expires_at == 0 {
            CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                tenant = tenant,
                actor = %cap_token.actor,
                "T21a: rejected capability token with expires_at=0 (immortal)"
            );
            return false;
        }
        if cap_token.expires_at > now_ms + MAX_CAPABILITY_TOKEN_LIFETIME_MS {
            CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                tenant = tenant,
                actor = %cap_token.actor,
                expires_at = cap_token.expires_at,
                max_allowed = now_ms + MAX_CAPABILITY_TOKEN_LIFETIME_MS,
                "T21a: rejected capability token with remaining validity > 24h"
            );
            return false;
        }
        if cap_token.expires_at <= now_ms {
            return false;
        }

        // Execute-path exact acceptance (PR1 structured execute authority).
        if desired_authority == "execute" {
            if cap_token.subject != "watch-producer" {
                CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
            if !cap_token.approval_required {
                CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
            // Some(0.0) only — reject None, NaN, negative, nonzero.
            match cap_token.max_cost_usd {
                Some(v) if v == 0.0 && v.is_finite() => {}
                _ => {
                    CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return false;
                }
            }
            let Some(claimed) = claimed_directive_id.filter(|id| !id.is_empty()) else {
                CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            };
            if cap_token.directive_id != claimed {
                CAP_TOKEN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
        }

        // Check TenantPolicy allowlist.
        //
        // Pre-seal W2 (opt-a, #3b): this allowlist query must FAIL
        // CLOSED on a DB error, same as the legacy path below. A real
        // prepare/query Err (DB locked, poisoned, table gone) previously
        // skipped the whole check, left worker_allowed=true, and let an
        // (even Ed25519-verified) actor through regardless of policy.
        // We now distinguish a real DB error from "no policy row" /
        // "empty allowlist": the no-row case (QueryReturnedNoRows) and a
        // legitimately empty allowed_workers set keep their current
        // meaning (no restriction configured -> allow); only a real DB
        // ERROR flips to deny + bumps CAP_TOKEN_DB_ERROR_DENY.
        let mut worker_allowed = true; // allow by default if no policy or no allowed_workers set
        let policy_check: Result<(), rusqlite::Error> = (|| {
            let mut stmt =
                conn.prepare("SELECT allowed_workers FROM tenant_policies WHERE tenant = ?1")?;
            let workers_json: Option<String> = match stmt
                .query_row(rusqlite::params![tenant], |r| r.get::<_, Option<String>>(0))
            {
                Ok(v) => v,
                // No policy row for this tenant is NOT an error: no
                // restriction configured -> leave worker_allowed=true.
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
                Err(e) => return Err(e),
            };
            if let Some(workers_json) = workers_json {
                match allowed_worker_policy_allows(&workers_json, &cap_token.actor) {
                    Ok(allowed) => worker_allowed = allowed,
                    Err(e) => {
                        tracing::error!(
                            tenant = tenant,
                            actor = %cap_token.actor,
                            error = %e,
                            "capability-token allowed_workers policy is malformed — denying (fail closed)"
                        );
                        worker_allowed = false;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = policy_check {
            CAP_TOKEN_DB_ERROR_DENY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                tenant = tenant,
                actor = %cap_token.actor,
                error = %e,
                "capability-token allowed_workers DB check errored — denying (fail closed); cap_token_db_error_deny_total bumped"
            );
            worker_allowed = false;
        }
        if !worker_allowed {
            return false;
        }

        // Durable same-directive-only replay for execute (survives restart).
        if desired_authority == "execute" {
            let claimed = claimed_directive_id.expect("execute bind checked above");
            return bind_capability_token_consumption(conn, tenant, &cap_token.token_id, claimed);
        }

        return true;
    }

    // Opaque path: never authorizes execute (structured-only).
    if desired_authority == "execute" {
        return false;
    }

    // Fallback to legacy string-match DB check (prepare / non-execute only).
    //
    // Pre-seal W2 (opt-a): this DB check must FAIL CLOSED on a DB error. A real
    // prepare/query/iteration Err (DB locked, poisoned, schema gone) is NOT the
    // same as a clean empty result set: previously any such error skipped the
    // whole check, left has_db_tokens=false, and fell through to the env
    // allowlist — silently bypassing a tenant's DB tokens. We now distinguish
    // error from empty: on error, deny + bump CAP_TOKEN_DB_ERROR_DENY +
    // tracing::error!. A clean empty result still falls through to env exactly
    // as before (env-allowlist semantics unchanged).
    let mut has_db_tokens = false;

    let db_check: Result<bool, rusqlite::Error> = (|| {
        let mut stmt = conn.prepare(
            "SELECT token FROM tenant_policy_tokens WHERE tenant = ?1 AND authority = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![tenant, desired_authority])?;
        while let Some(row) = rows.next()? {
            has_db_tokens = true;
            let db_token: String = row.get(0).unwrap_or_default();
            // T4: ct compare (reuse arm pattern; subtle + fixed sha for length independence)
            use sha2::{Digest, Sha256};
            use subtle::ConstantTimeEq;
            let a = Sha256::digest(db_token.as_bytes());
            let b = Sha256::digest(token.as_bytes());
            if bool::from(a.ct_eq(&b)) {
                return Ok(true); // matched a DB token
            }
        }
        Ok(false) // queried cleanly, no match (empty or non-matching)
    })();

    match db_check {
        Ok(true) => return true,
        Ok(false) => {
            // Clean query, no match. If the tenant HAS db tokens for this
            // authority but none matched, deny (do not fall through to env).
            if has_db_tokens {
                return false;
            }
            // else: no db tokens at all -> fall through to env allowlist (unchanged).
        }
        Err(e) => {
            // DB error: fail CLOSED. Never fall through to env on an error.
            CAP_TOKEN_DB_ERROR_DENY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                tenant = tenant,
                authority = desired_authority,
                error = %e,
                "capability-token DB check errored — denying (fail closed); cap_token_db_error_deny_total bumped"
            );
            return false;
        }
    }

    // Fallback to env var for bootstrap — prepare only (execute refused above).
    if desired_authority != "prepare" {
        return false;
    }
    if let Ok(val) = std::env::var("WATCH_ALLOWED_PREPARE_TOKENS") {
        for t in val.split(',') {
            // T4: ct for env fallback (boot/loopback)
            use sha2::{Digest, Sha256};
            use subtle::ConstantTimeEq;
            let a = Sha256::digest(t.trim().as_bytes());
            let b = Sha256::digest(token.as_bytes());
            if bool::from(a.ct_eq(&b)) {
                return true;
            }
        }
    }
    false
}
