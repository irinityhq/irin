//! Phase 2 completion-verify-watch sentinel — unverified completion detector.
//!
//! Fires on `directive_outbox` rows in 'acked' status where the attached
//! `worker_provenance` (derived from claim_handle) is missing or != VerifiedExact.
//! This makes false reporting observable: worker claims "done" but no
//! cryptographic verification of the work product / correlation.
//!
//! Non-goals (Phase 1):
//! - No LLM / content verification of the ack result.
//! - Does not block the ack (post-hoc detection only).
//! - Worker provenance now persisted in dedicated `worker_provenance` column on ack (VerifiedExact set by worker after verification). Legacy rows fall back via claim_handle.
//!
//! Escalation carries directive_id + ack_provenance_status + question for Council.
//! Cooldown (default 5m) is enforced by runner/registry (per-tenant instance).
//!
//! Storage contract: outbox.rs and db.rs.

use crate::watch::outbox::DirectiveOutboxRecord;
use crate::watch::{
    db::WatchDb, EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use sovereign_protocol::types::WorkerProvenanceStatus;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct CompletionVerifySentinel {
    name: String,
    tenant: String,
    watch_db_path: PathBuf,
    cooldown: Duration,
}

impl CompletionVerifySentinel {
    pub fn new(name: &str, tenant: &str, watch_db_path: &Path) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            watch_db_path: watch_db_path.to_path_buf(),
            cooldown: Duration::from_secs(300),
        }
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }
}

#[async_trait]
impl Sentinel for CompletionVerifySentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Polling
    }
    fn cooldown(&self) -> Duration {
        self.cooldown
    }

    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        let db = WatchDb::open(&self.watch_db_path)
            .await
            .map_err(|e| ObserveError::TransientUpstream(format!("open watch.db: {e}")))?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        // P2 pagination fix: walk up to 5 pages (250 newest acked) using cursor support
        // so that unverified acks are not starved by a long tail of recent verified acks.
        // After the inner for loop over rows and the cursor update, early-break:
        //   if rows.len() < PAGE_SIZE as usize { break; }
        // Payload samples up to 50; count reports how many unverified were seen in window.
        // (Full historical scan remains admin/list_outbox job; sentinel bounds work.)
        let mut unverified_count: usize = 0;
        let mut unverified_sample: Vec<serde_json::Value> = vec![];
        let mut cursor: Option<(i64, String)> = None;
        const MAX_PAGES: usize = 5;
        const PAGE_SIZE: i64 = 50;
        for _ in 0..MAX_PAGES {
            let rows: Vec<DirectiveOutboxRecord> = db
                .list_outbox(&self.tenant, Some("acked"), PAGE_SIZE, cursor.clone())
                .await
                .map_err(|e| ObserveError::TransientUpstream(format!("list_outbox acked: {e}")))?;
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                let is_unverified = match &r.worker_provenance {
                    None => true,
                    Some(p) => p.status != WorkerProvenanceStatus::VerifiedExact,
                };
                if is_unverified {
                    unverified_count += 1;
                    if unverified_sample.len() < 50 {
                        unverified_sample.push(serde_json::json!({
                            "id": r.id,
                            "in_response_to": r.in_response_to,
                            "created_at_ms": r.created_at_ms,
                            "acked_at_ms": r.acked_at_ms,
                            "ack_provenance_status": r.worker_provenance.as_ref().map(|p| {
                                match p.status {
                                    WorkerProvenanceStatus::VerifiedExact => "verified_exact",
                                    WorkerProvenanceStatus::OpaqueHandleOnly => "opaque_handle_only",
                                    WorkerProvenanceStatus::Unavailable => "unavailable",
                                }
                            }).unwrap_or("none"),
                        }));
                    }
                }
            }
            if let Some(last) = rows.last() {
                cursor = Some((last.created_at_ms, last.id.clone()));
            }
            if rows.len() < PAGE_SIZE as usize {
                break;
            }
        }

        let unverified = unverified_sample;

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: serde_json::json!({
                "unverified_acked_count": unverified_count,
                "unverified_acked": unverified,
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        let count = state.payload["unverified_acked_count"]
            .as_u64()
            .unwrap_or(0);
        if count == 0 {
            return None;
        }
        if let Some(arr) = state
            .payload
            .get("unverified_acked")
            .and_then(|v| v.as_array())
        {
            if let Some(first) = arr.first() {
                let id = first["id"].as_str().unwrap_or("?");
                let status = first["ack_provenance_status"].as_str().unwrap_or("none");
                if status == "none" {
                    return Some(format!("acked directive {} has no worker provenance", id));
                } else {
                    return Some(format!(
                        "acked directive {} provenance is {} — unverified completion",
                        id, status
                    ));
                }
            }
        }
        Some(format!(
            "{} unverified acked directive(s) for tenant",
            count
        ))
    }

    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError> {
        Ok(Escalation {
            state,
            reason,
            urgency: Urgency::Medium,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interesting_verified_exact_no_fire() {
        // Unit test per AC: VerifiedExact → no fire
        let state = SentinelState {
            tenant: "test".into(),
            sentinel: "completion-verify-watch".into(),
            observed_at: 12345,
            payload: serde_json::json!({
                "unverified_acked_count": 0,
                "unverified_acked": []
            }),
        };
        let s = CompletionVerifySentinel::new(
            "completion-verify-watch",
            "test",
            std::path::Path::new("/tmp"),
        );
        assert!(s.interesting(&state).is_none());
    }

    #[test]
    fn test_interesting_unverified_fires() {
        // Opaque / None → fire , exact messages
        let s = CompletionVerifySentinel::new(
            "completion-verify-watch",
            "test",
            std::path::Path::new("/tmp"),
        );

        // Case None
        let state_none = SentinelState {
            tenant: "test".into(),
            sentinel: "completion-verify-watch".into(),
            observed_at: 12345,
            payload: serde_json::json!({
                "unverified_acked_count": 1,
                "unverified_acked": [{
                    "id": "dir-42",
                    "in_response_to": "esc-1",
                    "created_at_ms": 1000,
                    "acked_at_ms": 2000,
                    "ack_provenance_status": "none"
                }]
            }),
        };
        let r = s.interesting(&state_none).unwrap();
        assert!(
            r.contains("acked directive dir-42 has no worker provenance"),
            "got: {}",
            r
        );

        // Case OpaqueHandleOnly
        let state_opaque = SentinelState {
            tenant: "test".into(),
            sentinel: "completion-verify-watch".into(),
            observed_at: 12345,
            payload: serde_json::json!({
                "unverified_acked_count": 1,
                "unverified_acked": [{
                    "id": "dir-99",
                    "in_response_to": "esc-2",
                    "created_at_ms": 1000,
                    "acked_at_ms": 2000,
                    "ack_provenance_status": "opaque_handle_only"
                }]
            }),
        };
        let r2 = s.interesting(&state_opaque).unwrap();
        assert!(
            r2.contains("provenance is opaque_handle_only — unverified completion"),
            "got: {}",
            r2
        );
    }
}
