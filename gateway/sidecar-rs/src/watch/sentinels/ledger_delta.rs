//! Phase C1 `ledger-delta-watch` — spend spike detector (experimental stock sentinel).
//!
//! Fires when today's UTC `spend_ledger` total (reserved + settled) rises more than
//! `threshold_pct` above an established baseline. Baseline is the first sample of
//! the UTC day whose spend is at least `min_baseline_usd`, unless `baseline_usd` is
//! pinned in config.
//!
//! Read-only by construction: `watch.db` is opened with `SQLITE_OPEN_READ_ONLY`.
//! No LLM in `observe()` / `interesting()`; single indexed PK read via `spawn_blocking`.

use crate::watch::db::utc_day_bucket;
use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone)]
struct SpendSnapshot {
    day_bucket: String,
    /// Rolling baseline for the current UTC day (auto mode only).
    auto_baseline_usd: Option<f64>,
}

pub struct LedgerDeltaSentinel {
    name: String,
    tenant: String,
    watch_db_path: PathBuf,
    threshold_pct: f64,
    min_baseline_usd: f64,
    min_absolute_delta_usd: f64,
    fixed_baseline_usd: Option<f64>,
    cooldown: Duration,
    snapshot: Mutex<SpendSnapshot>,
}

impl LedgerDeltaSentinel {
    pub fn new(
        name: &str,
        tenant: &str,
        watch_db_path: &Path,
        threshold_pct: f64,
        min_baseline_usd: f64,
        min_absolute_delta_usd: f64,
        fixed_baseline_usd: Option<f64>,
    ) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            watch_db_path: watch_db_path.to_path_buf(),
            threshold_pct,
            min_baseline_usd,
            min_absolute_delta_usd,
            fixed_baseline_usd,
            cooldown: Duration::from_secs(60),
            snapshot: Mutex::new(SpendSnapshot {
                day_bucket: String::new(),
                auto_baseline_usd: None,
            }),
        }
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }

    /// Boot-time fail-fast: path must exist and be openable read-only.
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

    fn read_daily_spend(watch_db_path: &Path, day_bucket: &str) -> Result<f64, String> {
        let path = watch_db_path.to_path_buf();
        let bucket = day_bucket.to_string();
        let sum = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("open watch.db: {e}"))?;
        let total: f64 = sum
            .query_row(
                "SELECT COALESCE(reserved_usd, 0.0) + COALESCE(settled_usd, 0.0)
                 FROM spend_ledger WHERE day_bucket = ?1",
                rusqlite::params![bucket],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("query spend_ledger: {e}"))?
            .unwrap_or(0.0);
        Ok(total)
    }
}

#[async_trait]
impl Sentinel for LedgerDeltaSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Fast
    }
    fn cooldown(&self) -> Duration {
        self.cooldown
    }

    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let day_bucket = utc_day_bucket(now_ms);

        let watch_path = self.watch_db_path.clone();
        let bucket = day_bucket.clone();
        let current_spend =
            tokio::task::spawn_blocking(move || Self::read_daily_spend(&watch_path, &bucket))
                .await
                .map_err(|e| ObserveError::Fatal(format!("join: {e}")))?
                .map_err(ObserveError::TransientUpstream)?;

        let mut snap = self
            .snapshot
            .lock()
            .map_err(|e| ObserveError::Fatal(format!("snapshot lock poisoned: {e}")))?;

        if snap.day_bucket != day_bucket {
            snap.day_bucket = day_bucket.clone();
            snap.auto_baseline_usd = None;
        }

        let (baseline_usd, baseline_established) = match self.fixed_baseline_usd {
            Some(fixed) => (fixed, true),
            None => {
                if snap.auto_baseline_usd.is_none() && current_spend >= self.min_baseline_usd {
                    snap.auto_baseline_usd = Some(current_spend);
                }
                let baseline = snap.auto_baseline_usd.unwrap_or(0.0);
                let established = snap.auto_baseline_usd.is_some();
                (baseline, established)
            }
        };

        let delta_usd = current_spend - baseline_usd;
        let delta_pct = if baseline_usd > 0.0 {
            (delta_usd / baseline_usd) * 100.0
        } else {
            0.0
        };

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: serde_json::json!({
                "day_bucket": day_bucket,
                "current_spend_usd": current_spend,
                "baseline_usd": baseline_usd,
                "baseline_established": baseline_established,
                "delta_usd": delta_usd,
                "delta_pct": delta_pct,
                "threshold_pct": self.threshold_pct,
                "min_baseline_usd": self.min_baseline_usd,
                "min_absolute_delta_usd": self.min_absolute_delta_usd,
                "fixed_baseline": self.fixed_baseline_usd.is_some(),
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        if !state.payload["baseline_established"]
            .as_bool()
            .unwrap_or(false)
        {
            return None;
        }
        let baseline = state.payload["baseline_usd"].as_f64().unwrap_or(0.0);
        if baseline < self.min_baseline_usd {
            return None;
        }
        let delta_usd = state.payload["delta_usd"].as_f64().unwrap_or(0.0);
        if delta_usd < self.min_absolute_delta_usd {
            return None;
        }
        let delta_pct = state.payload["delta_pct"].as_f64().unwrap_or(0.0);
        if delta_pct > self.threshold_pct {
            Some(format!(
                "spend delta {:.1}% (${:.4} → ${:.4}, +${:.4}) exceeds {:.1}% threshold (day {})",
                delta_pct,
                baseline,
                state.payload["current_spend_usd"].as_f64().unwrap_or(0.0),
                delta_usd,
                self.threshold_pct,
                state.payload["day_bucket"].as_str().unwrap_or("?")
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
