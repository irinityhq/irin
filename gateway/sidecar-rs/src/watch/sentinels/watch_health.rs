//! Phase 2 watch-health-watch meta-sentinel — Grok G5.
//!
//! Watches the watch plane itself. Fires if ANY of:
//!   1. There is a sentinel currently quarantined past `now()` (other than self).
//!   2. There is a sentinel with `hard_killed_at` set (other than self).
//!   3. `WatchDb::verify_chain(tenant)` returns ok=false.
//!
//! Self-exclusion is structural: queries pass `self.name()` to the WHERE
//! clause so a hard-killed or quarantined watch-health-watch row won't
//! cause this sentinel to fire on itself in an infinite loop.
//!
//! Urgency::High — a degraded watch plane is a meta-failure; the operator
//! needs to see it before downstream sentinels start under-reporting.

use crate::watch::{
    db::WatchDb, EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct WatchHealthSentinel {
    name: String,
    tenant: String,
    watch_db_path: PathBuf,
    cooldown: Duration,
}

impl WatchHealthSentinel {
    pub fn new(name: &str, tenant: &str, watch_db_path: &Path) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            watch_db_path: watch_db_path.to_path_buf(),
            cooldown: Duration::from_secs(60),
        }
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }
}

#[async_trait]
impl Sentinel for WatchHealthSentinel {
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

        let quarantined_count = db
            .count_quarantined_excluding(now_ms, &self.name)
            .await
            .map_err(|e| ObserveError::TransientUpstream(format!("count quarantine: {e}")))?;

        let hard_killed_count = db
            .count_hard_killed_excluding(&self.name)
            .await
            .map_err(|e| ObserveError::TransientUpstream(format!("count hard-killed: {e}")))?;

        let chain = db
            .verify_chain(&self.tenant)
            .await
            .map_err(|e| ObserveError::TransientUpstream(format!("verify_chain: {e}")))?;

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: serde_json::json!({
                "quarantined_count": quarantined_count,
                "hard_killed_count": hard_killed_count,
                "chain_ok": chain.ok,
                "chain_broken_at_id": chain.broken_at_id,
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        let q = state.payload["quarantined_count"].as_i64().unwrap_or(0);
        let k = state.payload["hard_killed_count"].as_i64().unwrap_or(0);
        let chain_ok = state.payload["chain_ok"].as_bool().unwrap_or(true);

        if q == 0 && k == 0 && chain_ok {
            return None;
        }

        let mut parts = Vec::new();
        if k > 0 {
            parts.push(format!("{k} hard-killed sentinel(s)"));
        }
        if q > 0 {
            parts.push(format!("{q} quarantined sentinel(s)"));
        }
        if !chain_ok {
            parts.push("watch.db chain BROKEN".to_string());
        }
        Some(format!("watch plane degraded: {}", parts.join(", ")))
    }

    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError> {
        Ok(Escalation {
            state,
            reason,
            urgency: Urgency::High,
        })
    }
}
