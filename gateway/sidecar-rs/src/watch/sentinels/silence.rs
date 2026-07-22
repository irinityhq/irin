//! Phase 2 silence-watch sentinel — backlog-gated silence detector.
//!
//! Pattern: warroom Gem "backlog-gated silence". Fires only when BOTH
//!   (a) silence beyond `threshold_hours` since the last audit event AND
//!   (b) the `backlog_path` directory has at least one entry.
//!
//! Rationale: silence alone is not interesting — a quiet ledger is fine if
//! there is no work waiting. Silence + backlog = something is broken.
//!
//! Read-only by construction: the ledger.db connection is opened with
//! `OpenFlags::SQLITE_OPEN_READ_ONLY`, and the backlog_path is only stat'd
//! via `read_dir`.

use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct SilenceSentinel {
    name: String,
    tenant: String,
    threshold_hours: i64,
    ledger_db_path: PathBuf,
    backlog_path: PathBuf,
    cooldown: Duration,
}

impl SilenceSentinel {
    pub fn new(
        name: &str,
        tenant: &str,
        threshold_hours: i64,
        ledger_db_path: &Path,
        backlog_path: &Path,
    ) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            threshold_hours,
            ledger_db_path: ledger_db_path.to_path_buf(),
            backlog_path: backlog_path.to_path_buf(),
            cooldown: Duration::from_secs(300),
        }
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }
}

#[async_trait]
impl Sentinel for SilenceSentinel {
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
        // Read-only ledger.db open. We use blocking rusqlite via spawn_blocking
        // — keeps the sentinel self-contained (no shared connection state)
        // and avoids any risk of a writer lock contending against the
        // sidecar's own ledger writes.
        let ledger_path = self.ledger_db_path.clone();
        let last_event_ms: Option<i64> = tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open_with_flags(
                &ledger_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )?;
            // `audit_events.timestamp` is recorded as unix seconds in the
            // ledger writer. We normalize to milliseconds below.
            let ts: Option<i64> = conn
                .query_row("SELECT MAX(timestamp) FROM audit_events", [], |r| r.get(0))
                .ok();
            Ok::<Option<i64>, rusqlite::Error>(ts)
        })
        .await
        .map_err(|e| ObserveError::Fatal(format!("join: {e}")))?
        .map_err(|e| ObserveError::TransientUpstream(format!("ledger.db: {e}")))?;

        // Detect backlog presence (directory may be missing → 0). We only care
        // whether work exists, so stop after the first readable entry instead
        // of walking an unbounded inbox.
        // Wrapped in spawn_blocking (exact parallel to ledger.db block above) per
        // design ruling: "Priority 3: Protect the Executor (spawn_blocking).
        // Tear-down explicitly rejected placebo timeouts." and rollback:
        // "Any change that leaves a blocking call on the 2-worker watch-rt runtime
        // must be backed out. Add spawn_blocking or move to helper process."
        // This is the smallest change satisfying Gateway watch-runtime stress/
        // threading isolation gates (simulated hanging sentinel work must not starve
        // the dedicated 2w+8b rt). Non-goals respected (no dispatcher/outbox touch).
        let backlog_count: i64 = {
            let p = self.backlog_path.clone();
            tokio::task::spawn_blocking(move || match std::fs::read_dir(&p) {
                Ok(rd) => rd.filter_map(|e| e.ok()).take(1).count() as i64,
                Err(_) => 0,
            })
            .await
            .map_err(|e| ObserveError::Fatal(format!("join fs: {e}")))?
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        // ledger.timestamp is unix seconds — convert to ms for symmetry.
        let last_event_ms_norm = last_event_ms.map(|s| s * 1000);
        let hours_since = match last_event_ms_norm {
            Some(t) => ((now_ms - t).max(0) as f64) / 3_600_000.0,
            None => f64::INFINITY,
        };

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: serde_json::json!({
                "last_event_ms": last_event_ms_norm,
                "backlog_count": backlog_count,
                "hours_since": if hours_since.is_finite() { hours_since } else { -1.0 },
                "hours_since_infinite": !hours_since.is_finite(),
                "threshold_hours": self.threshold_hours,
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        let backlog_count = state.payload["backlog_count"].as_i64().unwrap_or(0);
        if backlog_count <= 0 {
            return None;
        }
        let hours_since = state.payload["hours_since"].as_f64().unwrap_or(-1.0);
        let infinite = state.payload["hours_since_infinite"]
            .as_bool()
            .unwrap_or(false);
        let exceeds = infinite || hours_since > self.threshold_hours as f64;
        if exceeds {
            Some(format!(
                "silence with backlog: {backlog_count} pending, {} since last event (threshold {}h)",
                if infinite {
                    "infinite".to_string()
                } else {
                    format!("{hours_since:.2}h")
                },
                self.threshold_hours
            ))
        } else {
            None
        }
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
