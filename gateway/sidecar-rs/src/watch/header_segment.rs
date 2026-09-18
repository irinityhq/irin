//! Shared sanitization for Watch header segments.
//!
//! Tenant and escalation-id segments use the same character set, trim, and
//! SHA-256 fallback. Bounds, prefixes, and empty-input tokens differ. Outputs
//! stay byte-identical to the previous dispatcher-local copies.

use sha2::{Digest, Sha256};

/// Sanitize one Idempotency-Key segment.
///
/// Empty input (after trim) returns `anon`. A segment made only of ASCII
/// alphanumeric characters plus `-`, `_`, and `.`, and no longer than
/// `max_len`, is returned trimmed. Anything else — including `:`, controls,
/// and over-long values — becomes `{prefix}` plus the first 12 hex chars of
/// SHA-256(trimmed).
pub fn safe_header_segment(raw: &str, anon: &str, prefix: &str, max_len: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return anon.to_string();
    }
    let is_safe = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if is_safe && trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let mut hasher = Sha256::new();
        hasher.update(trimmed.as_bytes());
        let hash = hex::encode(hasher.finalize());
        format!("{prefix}{}", &hash[..12])
    }
}

/// Derives a canonical, stable, non-empty safe tenant token for use in
/// the `Idempotency-Key` header.
///
/// Rules (C11):
/// - Must not contain ':' or any control characters.
/// - Must be non-empty.
/// - Must be deterministic / stable for the same tenant.
/// - For safe tenants (alphanumeric + limited punctuation), the token is
///   the tenant itself (trimmed). Otherwise a short stable hash is used.
///
/// This is the single source of truth for safe-tenant-token derivation.
pub fn safe_tenant_token(tenant: &str) -> String {
    safe_header_segment(tenant, "t-anon", "t-", 64)
}

/// Sanitizes the escalation-id leg of the C11 Idempotency-Key (D8).
///
/// Mirrors [`safe_tenant_token`]. The live producer derives escalation ids as
/// `causal-<hex>` (see `cdc_sweep_tick`), which are `[a-z0-9-]` and pass
/// through byte-for-byte. Any id carrying `:` (the `<tenant>:<esc>` delimiter)
/// or a control char — which would make `HeaderValue::from_str` reject the
/// value and the old `.expect()` PANIC on the dispatch path — is replaced with
/// a stable SHA-256 fallback. There is no reachable trigger today (ids are
/// internally generated hex); this is defensive hardening per the defensive-input invariant.
pub fn safe_escalation_id_segment(raw: &str) -> String {
    // ':' is deliberately EXCLUDED from the safe set so the "<tenant>:<esc>"
    // delimiter stays unambiguous and a crafted id cannot forge another
    // tenant's qualified key. The bound is 128, not the tenant bound of 64.
    safe_header_segment(raw, "e-anon", "e-", 128)
}
