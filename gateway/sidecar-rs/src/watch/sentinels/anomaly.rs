//! Phase C1 `anomaly-watch` — escalation failure-rate spike detector (experimental).
//!
//! Fires when the tenant's `pending_escalations` failure rate in a sliding window
//! rises above an EWMA baseline by `threshold_pct`, with absolute floors on sample
//! count and failures. Cheap stats only — no LLM in `observe()` / `interesting()`.
//!
//! Read-only `watch.db` via `spawn_blocking`. Latency baselines deferred to v1.1
//! (no durable latency histogram in watch.db today).

use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
struct WindowCounts {
    failures: i64,
    total: i64,
}

#[derive(Debug, Clone)]
struct RateSnapshot {
    ewma_error_rate: Option<f64>,
    consecutive_high: u8,
}

#[derive(Debug, Clone)]
pub struct AnomalyConfig {
    pub watch_db_path: PathBuf,
    pub window_ms: i64,
    pub threshold_pct: f64,
    pub min_samples: i64,
    pub min_failures: i64,
    pub min_error_rate: f64,
    pub ewma_alpha: f64,
    pub consecutive_windows_required: u8,
}

pub struct AnomalySentinel {
    name: String,
    tenant: String,
    watch_db_path: PathBuf,
    window_ms: i64,
    threshold_pct: f64,
    min_samples: i64,
    min_failures: i64,
    min_error_rate: f64,
    ewma_alpha: f64,
    consecutive_windows_required: u8,
    cooldown: Duration,
    snapshot: Mutex<RateSnapshot>,
}

impl AnomalySentinel {
    pub fn new(name: &str, tenant: &str, config: AnomalyConfig) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            watch_db_path: config.watch_db_path,
            window_ms: config.window_ms,
            threshold_pct: config.threshold_pct,
            min_samples: config.min_samples,
            min_failures: config.min_failures,
            min_error_rate: config.min_error_rate,
            ewma_alpha: config.ewma_alpha,
            consecutive_windows_required: config.consecutive_windows_required,
            cooldown: Duration::from_secs(60),
            snapshot: Mutex::new(RateSnapshot {
                ewma_error_rate: None,
                consecutive_high: 0,
            }),
        }
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
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

    fn read_window_counts(
        watch_db_path: &Path,
        tenant: &str,
        since_ms: i64,
    ) -> Result<WindowCounts, String> {
        let path = watch_db_path.to_path_buf();
        let tenant = tenant.to_string();
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("open watch.db: {e}"))?;
        let (failures, total): (i64, i64) = conn
            .query_row(
                "SELECT
                    COALESCE(SUM(CASE WHEN status IN ('failed', 'dead_lettered') THEN 1 ELSE 0 END), 0),
                    COUNT(*)
                 FROM pending_escalations
                 WHERE tenant = ?1 AND created_at_ms >= ?2",
                rusqlite::params![tenant, since_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| format!("query pending_escalations: {e}"))?;
        Ok(WindowCounts { failures, total })
    }

    fn error_rate(counts: WindowCounts) -> f64 {
        if counts.total <= 0 {
            0.0
        } else {
            counts.failures as f64 / counts.total as f64
        }
    }
}

#[async_trait]
impl Sentinel for AnomalySentinel {
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
        let since_ms = now_ms.saturating_sub(self.window_ms);

        let watch_path = self.watch_db_path.clone();
        let tenant = self.tenant.clone();
        let counts = tokio::task::spawn_blocking(move || {
            Self::read_window_counts(&watch_path, &tenant, since_ms)
        })
        .await
        .map_err(|e| ObserveError::Fatal(format!("join: {e}")))?
        .map_err(ObserveError::TransientUpstream)?;

        let current_rate = Self::error_rate(counts);

        let mut snap = self
            .snapshot
            .lock()
            .map_err(|e| ObserveError::Fatal(format!("snapshot lock poisoned: {e}")))?;

        let (baseline_established, ewma_before) = match snap.ewma_error_rate {
            Some(v) => (true, v),
            None => (false, 0.0),
        };

        let mut spike = false;
        if counts.total >= self.min_samples {
            if snap.ewma_error_rate.is_none() {
                snap.ewma_error_rate = Some(current_rate);
                snap.consecutive_high = 0;
            } else {
                let ewma = snap.ewma_error_rate.unwrap_or(current_rate);
                let threshold_rate = ewma * (1.0 + self.threshold_pct / 100.0);
                let absolute_spike = current_rate >= self.min_error_rate
                    && counts.failures >= self.min_failures
                    && current_rate > threshold_rate;
                if absolute_spike {
                    snap.consecutive_high = snap.consecutive_high.saturating_add(1);
                    spike = snap.consecutive_high >= self.consecutive_windows_required;
                } else {
                    snap.consecutive_high = 0;
                }
                snap.ewma_error_rate =
                    Some(self.ewma_alpha * current_rate + (1.0 - self.ewma_alpha) * ewma);
            }
        } else {
            snap.consecutive_high = 0;
        }

        let ewma_after = snap.ewma_error_rate.unwrap_or(0.0);

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: serde_json::json!({
                "window_ms": self.window_ms,
                "failures": counts.failures,
                "total": counts.total,
                "error_rate": current_rate,
                "ewma_error_rate": ewma_after,
                "ewma_before": ewma_before,
                "baseline_established": baseline_established || snap.ewma_error_rate.is_some(),
                "consecutive_high": snap.consecutive_high,
                "consecutive_required": self.consecutive_windows_required,
                "spike_detected": spike,
                "threshold_pct": self.threshold_pct,
                "min_error_rate": self.min_error_rate,
                "min_samples": self.min_samples,
                "min_failures": self.min_failures,
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        if !state.payload["spike_detected"].as_bool().unwrap_or(false) {
            return None;
        }
        let rate = state.payload["error_rate"].as_f64().unwrap_or(0.0);
        let ewma = state.payload["ewma_before"].as_f64().unwrap_or(0.0);
        let failures = state.payload["failures"].as_i64().unwrap_or(0);
        let total = state.payload["total"].as_i64().unwrap_or(0);
        Some(format!(
            "escalation error rate {:.1}% ({}/{} failed) exceeds EWMA baseline {:.1}% by >{:.0}% for {} consecutive windows",
            rate * 100.0,
            failures,
            total,
            ewma * 100.0,
            self.threshold_pct,
            state.payload["consecutive_high"].as_u64().unwrap_or(0)
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
            urgency: Urgency::High,
        })
    }
}
