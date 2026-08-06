//! Experimental `sequence-watch` — detects a burst of individually valid Act directives.
//!
//! The existing `directive_outbox` is the sliding window: one tenant-scoped,
//! indexed, read-only query counts recent staged/acked Act directives and their
//! aggregate Council cost. No LLM, schema, background buffer, or auto-quarantine.

use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct SequenceWatchSentinel {
    name: String,
    tenant: String,
    watch_db_path: PathBuf,
    window_ms: i64,
    max_acts_per_window: i64,
    min_aggregate_cost_usd: f64,
    cooldown: Duration,
}

impl SequenceWatchSentinel {
    pub fn new(
        name: &str,
        tenant: &str,
        watch_db_path: &Path,
        window_ms: i64,
        max_acts_per_window: i64,
        min_aggregate_cost_usd: f64,
    ) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            watch_db_path: watch_db_path.to_path_buf(),
            window_ms,
            max_acts_per_window,
            min_aggregate_cost_usd,
            cooldown: Duration::from_secs(60),
        }
    }

    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    pub fn validate_path(&self) -> anyhow::Result<()> {
        if !self.watch_db_path.exists() {
            anyhow::bail!(
                "watch.db missing or unreadable at {} — check bind mount / WATCH_DB_PATH",
                self.watch_db_path.display()
            );
        }
        rusqlite::Connection::open_with_flags(
            &self.watch_db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "watch.db not openable read-only at {}: {e}",
                self.watch_db_path.display()
            )
        })?;
        Ok(())
    }

    fn read_window(
        watch_db_path: &Path,
        tenant: &str,
        window_start_ms: i64,
        now_ms: i64,
    ) -> Result<(i64, f64), String> {
        let conn = rusqlite::Connection::open_with_flags(
            watch_db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("open watch.db: {e}"))?;
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(council_cost_usd), 0.0)
             FROM directive_outbox
             WHERE tenant = ?1
               AND status IN ('staged', 'acked')
               AND created_at_ms >= ?2
               AND created_at_ms <= ?3
               AND verdict = 'Act'",
            rusqlite::params![tenant, window_start_ms, now_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("query directive_outbox sequence window: {e}"))
    }
}

#[async_trait]
impl Sentinel for SequenceWatchSentinel {
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
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let window_start_ms = now_ms.saturating_sub(self.window_ms);
        let path = self.watch_db_path.clone();
        let tenant = self.tenant.clone();
        let (act_count, aggregate_cost_usd) = tokio::task::spawn_blocking(move || {
            Self::read_window(&path, &tenant, window_start_ms, now_ms)
        })
        .await
        .map_err(|e| ObserveError::Fatal(format!("join: {e}")))?
        .map_err(ObserveError::TransientUpstream)?;
        let fired = act_count > self.max_acts_per_window
            && aggregate_cost_usd > self.min_aggregate_cost_usd;

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: serde_json::json!({
                "sequence_alert": fired,
                "heuristics_fired": if fired { vec!["directive_velocity"] } else { Vec::<&str>::new() },
                "window_ms": self.window_ms,
                "window_start_ms": window_start_ms,
                "act_count": act_count,
                "aggregate_cost_usd": aggregate_cost_usd,
                "max_acts_per_window": self.max_acts_per_window,
                "min_aggregate_cost_usd": self.min_aggregate_cost_usd,
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        state.payload["sequence_alert"]
            .as_bool()
            .unwrap_or(false)
            .then(|| {
                format!(
                    "{} Act directives totaling ${:.4} in {}ms exceed sequence limits (max {}, min ${:.4})",
                    state.payload["act_count"].as_i64().unwrap_or(0),
                    state.payload["aggregate_cost_usd"].as_f64().unwrap_or(0.0),
                    self.window_ms,
                    self.max_acts_per_window,
                    self.min_aggregate_cost_usd,
                )
            })
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
