//! Pure, deterministic `causal_fire_id` primitive (Phase 0 per causal_fire_id.md §3).
//!
//! Formula (exact, binding):
//!   causal_fire_id = SHA256( tenant || 0x00 || sentinel || 0x00 || content_digest )
//!
//! Where:
//! - `tenant` = canonical per safe_tenant_token / D8 rules (lowercase, no colons,
//!   stable slug). Caller supplies already-canonical value (see dispatcher).
//! - `sentinel` = registered name (file_inbox, silence, etc.).
//! - `content_digest` = SHA256( canonical_json_bytes ) of the structure in §4.
//!
//! Canonical JSON: serde_json::to_vec on a Value whose Maps are BTree-backed under
//! the current default serde_json configuration (sorted keys, compact, no whitespace;
//! no `preserve_order` feature; see keymgmt.rs precedent and hardening notes).
//! Matches existing Phase 3 canonical practice in keymgmt::sign_directive_envelope.
//!
//! Invariants (§5):
//! 1. Stability across restarts/re-observations for identical (tenant, sentinel, causal content).
//! 2. Chain independence — never reads or depends on watch_fires.hash / prev_hash.
//! 3. Determinism — no wall time, randomness, or process state.
//! 4. Tenant isolation — tenant at root prevents cross-tenant collisions.
//! 5. Audit preservation — never requires mutable column/index on watch_fires.
//!
//! Compute location: sweep (recommended, protects 200 ms fire_pipeline budget).
//! Anti-patterns forbidden: watch_fires hashes, wall ns, raw blobs when digest suffices.
//!
//! This module is intentionally small, pure, and side-effect free. No DB, no I/O.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Compute the stable logical `causal_fire_id` per the exact formula.
/// Precondition (documented per re-review F3): `canonical_tenant` MUST be the output of
/// `safe_tenant_token(...)` (or equivalent per D8 rules); `sentinel` a registered name;
/// `content_digest` a stable hex per §4. Callers (future CDC sweep) are responsible;
/// validation deferred to the sweep layer per P0 minimalism + "compute in sweep" (causal §6).
/// All inputs must be pre-canonicalized by caller (tenant via safe_tenant_token).
pub fn causal_fire_id(canonical_tenant: &str, sentinel: &str, content_digest: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_tenant.as_bytes());
    hasher.update([0x00]);
    hasher.update(sentinel.as_bytes());
    hasher.update([0x00]);
    hasher.update(content_digest.as_bytes());
    hex::encode(hasher.finalize())
}

/// Build the canonical causal content structure (§4) and return its SHA256 hex digest.
/// Precondition: same as `causal_fire_id` (callers responsible for D8 + causal §4 inputs;
/// validation deferred per P0 minimalism + "compute in sweep" rec).
/// `observed_at`: RFC3339 (second precision or better; use fixed value in tests for determinism).
/// `payload`: sentinel-specific per table in causal_fire_id.md (e.g. file_inbox: path+size+mtime).
/// Keys are sorted under the current default serde_json configuration (BTreeMap-backed Map;
/// no `preserve_order` feature enabled in sidecar-rs Cargo.toml; see keymgmt.rs precedent).
pub fn compute_content_digest(
    sentinel: &str,
    canonical_tenant: &str,
    observed_at: &str,
    payload: &Value,
) -> String {
    let causal = serde_json::json!({
        "sentinel": sentinel,
        "tenant": canonical_tenant,
        "observed_at": observed_at,
        "payload": payload
    });
    // Unreachable in practice for controlled callers (known json! shape + BTree Map default).
    // Matches keymgmt.rs:500 infallible pattern for the same reason. Documented here per review.
    let bytes = serde_json::to_vec(&causal).expect(
        "causal content structure must serialize deterministically (controlled inputs only)",
    );
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXED_OBSERVED: &str = "2026-05-29T21:00:00Z";

    #[test]
    fn different_tenants_produce_different_ids() {
        let p = json!({"path": "/tmp/a", "size": 1, "mtime": 100});
        let d1 = compute_content_digest("file_inbox", "tenant-a", FIXED_OBSERVED, &p);
        let id1 = causal_fire_id("tenant-a", "file_inbox", &d1);
        let d2 = compute_content_digest("file_inbox", "tenant-b", FIXED_OBSERVED, &p);
        let id2 = causal_fire_id("tenant-b", "file_inbox", &d2);
        assert_ne!(id1, id2, "tenant isolation violated");
    }

    #[test]
    fn identical_causal_content_produces_identical_id() {
        let p = json!({"backlog_count": 5, "representative": "msg-123"});
        let d1 = compute_content_digest("silence", "acme", FIXED_OBSERVED, &p);
        let id1 = causal_fire_id("acme", "silence", &d1);
        let d2 = compute_content_digest("silence", "acme", FIXED_OBSERVED, &p);
        let id2 = causal_fire_id("acme", "silence", &d2);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 64);
    }

    #[test]
    fn canonical_json_is_stable_across_key_order_in_payload() {
        // Construct two Values that differ only in construction order; serialization must match.
        let p1 = json!({"a": 1, "b": 2, "z": "last"});
        let p2 = json!({"z": "last", "b": 2, "a": 1});
        let d1 = compute_content_digest("queue_depth", "t1", FIXED_OBSERVED, &p1);
        let d2 = compute_content_digest("queue_depth", "t1", FIXED_OBSERVED, &p2);
        assert_eq!(d1, d2, "canonical must ignore input key order");
        let id1 = causal_fire_id("t1", "queue_depth", &d1);
        let id2 = causal_fire_id("t1", "queue_depth", &d2);
        assert_eq!(id1, id2);
    }

    #[test]
    fn different_payloads_produce_different_ids() {
        let p1 = json!({"depth": 10});
        let p2 = json!({"depth": 11});
        let d1 = compute_content_digest("queue_depth", "t1", FIXED_OBSERVED, &p1);
        let id1 = causal_fire_id("t1", "queue_depth", &d1);
        let d2 = compute_content_digest("queue_depth", "t1", FIXED_OBSERVED, &p2);
        let id2 = causal_fire_id("t1", "queue_depth", &d2);
        assert_ne!(id1, id2);
    }

    #[test]
    fn sentinel_specific_payloads_follow_spec_guidance() {
        // file_inbox example (path normalized, size, mtime)
        let fi = json!({"path": "/data/inbox/report.pdf", "size": 4096, "mtime": 1717000000});
        let d = compute_content_digest("file_inbox", "sovereign", FIXED_OBSERVED, &fi);
        let id = causal_fire_id("sovereign", "file_inbox", &d);
        assert_eq!(id.len(), 64);

        // silence example
        let si = json!({"backlog_count": 3, "representative": ["m1", "m2"]});
        let d = compute_content_digest("silence", "sovereign", FIXED_OBSERVED, &si);
        let id = causal_fire_id("sovereign", "silence", &d);
        assert_eq!(id.len(), 64);
    }

    /// Explicit golden for the raw byte concat + outer SHA (per General/General-2 reviews).
    /// Makes the 0x00 separator / prefix-attack invariant machine-checked.
    #[test]
    fn causal_fire_id_formula_exact_concat_golden() {
        let tenant = "acme";
        let sentinel = "file_inbox";
        let content_digest = "deadbeefcafebabe0123456789abcdef0123456789abcdef0123456789abcdef"; // 64 hex
        let id = causal_fire_id(tenant, sentinel, content_digest);

        // Recompute manually to prove the formula (tenant || 0x00 || sentinel || 0x00 || digest)
        let mut preimage = Vec::new();
        preimage.extend_from_slice(tenant.as_bytes());
        preimage.push(0x00);
        preimage.extend_from_slice(sentinel.as_bytes());
        preimage.push(0x00);
        preimage.extend_from_slice(content_digest.as_bytes());
        let expected = hex::encode(Sha256::digest(&preimage));
        assert_eq!(id, expected, "exact formula concat must match");
        assert_eq!(id.len(), 64);
    }
}
