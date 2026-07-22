//! Sentinel registry, hard-kill, probation, and tenant policy.

use rusqlite::OptionalExtension;

use super::WatchDb;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RegistryRow {
    pub name: String,
    pub tier: String,
    pub cooldown_ms: i64,
    pub enabled: bool,
    pub hard_killed_at: Option<i64>,
    pub config_json: String,
    pub last_fire_at: Option<i64>,
    pub fires_last_hour: i64,
}

/// T32 — return value of `clear_hard_kill_and_set_probation`. Lets the
/// caller know whether the DB row was actually hard-killed before the
/// clear (so `cleared` lists "hard_kill" accurately) and what
/// `probation_until` was written (so the in-memory record can mirror it).
#[derive(Debug, Clone)]
pub struct DurableClearOutcome {
    pub was_hard_killed: bool,
    pub probation_until_ms: Option<i64>,
}

/// T32 — compute the Unix-ms `probation_until` for an admin clear when a
/// hard-kill is being lifted. Returns `Some(now + probation_ms)` only when
/// the hard-kill is actually being removed AND admin didn't opt out via
/// `reset_probation: true`. Shared by the durable DB tx and the in-memory
/// fallback path in `QuarantineState::admin_clear_quarantine` so they
/// can't drift.
pub(crate) fn probation_target_for_clear(
    was_hard_killed: bool,
    skip_probation: bool,
    probation_ms: u64,
) -> Option<i64> {
    if was_hard_killed && !skip_probation {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Some(unix_ms + probation_ms as i64)
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TenantPolicy {
    pub tenant: String,
    pub allowed_models: Option<Vec<String>>,
    pub max_cost_usd: Option<f64>,
    pub max_latency_ms: Option<u64>,
    pub allowed_sentinels: Option<Vec<String>>,
    pub allowed_workers: Option<Vec<String>>,
    pub retention_days: Option<u32>,
}

impl WatchDb {
    pub async fn get_tenant_policy(&self, tenant: &str) -> anyhow::Result<Option<TenantPolicy>> {
        let t = tenant.to_string();
        self.conn
            .call(move |conn| -> Result<Option<TenantPolicy>, rusqlite::Error> {
                let mut stmt = conn.prepare(
                    "SELECT allowed_models, max_cost_usd, max_latency_ms, allowed_sentinels, allowed_workers, retention_days
                     FROM tenant_policies
                     WHERE tenant = ?1",
                )?;
                let policy = stmt.query_row(rusqlite::params![t], |r| {
                    let allowed_models_str: Option<String> = r.get(0)?;
                    let allowed_sentinels_str: Option<String> = r.get(3)?;
                    let allowed_workers_str: Option<String> = r.get(4)?;

                    Ok(TenantPolicy {
                        tenant: t.clone(),
                        allowed_models: allowed_models_str.and_then(|s| serde_json::from_str(&s).unwrap_or(None)),
                        max_cost_usd: r.get(1)?,
                        max_latency_ms: r.get(2)?,
                        allowed_sentinels: allowed_sentinels_str.and_then(|s| serde_json::from_str(&s).unwrap_or(None)),
                        allowed_workers: allowed_workers_str.and_then(|s| serde_json::from_str(&s).unwrap_or(None)),
                        retention_days: r.get(5)?,
                    })
                }).optional()?;
                Ok(policy)
            })
            .await
            .map_err(Into::into)
    }

    pub async fn set_tenant_policy(&self, policy: TenantPolicy) -> anyhow::Result<()> {
        self.conn
            .call(move |conn| -> Result<(), rusqlite::Error> {
                let allowed_models_str = policy.allowed_models.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default());
                let allowed_sentinels_str = policy.allowed_sentinels.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default());
                let allowed_workers_str = policy.allowed_workers.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default());

                conn.execute(
                    "INSERT INTO tenant_policies (tenant, allowed_models, max_cost_usd, max_latency_ms, allowed_sentinels, allowed_workers, retention_days)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(tenant) DO UPDATE SET
                        allowed_models = excluded.allowed_models,
                        max_cost_usd = excluded.max_cost_usd,
                        max_latency_ms = excluded.max_latency_ms,
                        allowed_sentinels = excluded.allowed_sentinels,
                        allowed_workers = excluded.allowed_workers,
                        retention_days = excluded.retention_days",
                    rusqlite::params![
                        policy.tenant,
                        allowed_models_str,
                        policy.max_cost_usd,
                        policy.max_latency_ms,
                        allowed_sentinels_str,
                        allowed_workers_str,
                        policy.retention_days
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    /// Count sentinels currently quarantined past `now_ms`, excluding any
    /// whose `sentinel` matches `exclude_sentinel`. Used by the
    /// watch-health-watch meta-sentinel so it doesn't fire on itself.
    pub async fn count_quarantined_excluding(
        &self,
        now_ms: i64,
        exclude_sentinel: &str,
    ) -> anyhow::Result<i64> {
        let exclude = exclude_sentinel.to_string();
        let n = self
            .conn
            .call(move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM watch_quarantine
                     WHERE quarantined_until > ?1 AND sentinel != ?2",
                    rusqlite::params![now_ms, exclude],
                    |r| r.get(0),
                )?;
                Ok::<i64, rusqlite::Error>(n)
            })
            .await?;
        Ok(n)
    }

    /// Count sentinels with a non-NULL `hard_killed_at`, excluding
    /// `exclude_sentinel`. Used by the watch-health-watch meta-sentinel.
    pub async fn count_hard_killed_excluding(&self, exclude_sentinel: &str) -> anyhow::Result<i64> {
        let exclude = exclude_sentinel.to_string();
        let n = self
            .conn
            .call(move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM watch_sentinels
                     WHERE hard_killed_at IS NOT NULL AND name != ?1",
                    rusqlite::params![exclude],
                    |r| r.get(0),
                )?;
                Ok::<i64, rusqlite::Error>(n)
            })
            .await?;
        Ok(n)
    }

    /// Insert a hard-kill marker for a sentinel. Test/admin helper —
    /// the runtime normally writes this via the quarantine module, but
    /// the meta-sentinel test needs a way to plant a row.
    pub async fn upsert_hard_kill(
        &self,
        tenant: &str,
        name: &str,
        hard_killed_at_ms: i64,
        reason: &str,
    ) -> anyhow::Result<()> {
        let tenant = tenant.to_string();
        let name = name.to_string();
        let reason = reason.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO watch_sentinels
                        (name, tenant, tier, cooldown_ms, config_json,
                         enabled, hard_killed_at, hard_kill_reason)
                     VALUES (?1, ?2, 'polling', 0, '{}', 1, ?3, ?4)
                     ON CONFLICT(tenant, name) DO UPDATE SET
                        hard_killed_at = excluded.hard_killed_at,
                        hard_kill_reason = excluded.hard_kill_reason",
                    rusqlite::params![name, tenant, hard_killed_at_ms, reason],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// T32 — admin clears the hard-kill (and optionally sets probation) for a
    /// sentinel. Single tx: SELECT prior `hard_killed_at` → compute target
    /// `probation_until` → UPDATE atomically. Returns whether the DB row
    /// was actually hard-killed before the clear (so the caller can populate
    /// its `cleared` audit list accurately) and the probation_until that
    /// landed (so the caller can mirror it into the in-memory record and
    /// echo it in the API response). The OCC tx in `insert_fire` reads
    /// `hard_killed_at`; clearing this column is what actually unblocks
    /// fires. The atomic read+update is necessary because in-memory state
    /// can be absent post-restart, so the in-memory inspection alone cannot
    /// tell us whether the DB was hard-killed. This prevents the silent
    /// "DB-only hard-kill cleared but no probation set" path otherwise.
    pub async fn clear_hard_kill_and_set_probation(
        &self,
        tenant: &str,
        name: &str,
        skip_probation: bool,
        probation_ms: u64,
        inmem_hard_killed: bool,
    ) -> anyhow::Result<DurableClearOutcome> {
        let tenant = tenant.to_string();
        let name = name.to_string();
        let outcome = self
            .conn
            .call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let db_hard_killed: bool = tx
                    .query_row(
                        "SELECT hard_killed_at FROM watch_sentinels
                         WHERE tenant=?1 AND name=?2",
                        rusqlite::params![tenant, name],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten()
                    .is_some();
                // OR'd view: caller may have observed in-memory hard-kill
                // even when the DB column was never mirrored (the
                // runtime hard-kill path doesn't currently persist to DB
                // for non-OCC reasons). Either way, probation kicks in.
                let was_hard_killed = db_hard_killed || inmem_hard_killed;
                let probation_until_ms =
                    probation_target_for_clear(was_hard_killed, skip_probation, probation_ms);
                tx.execute(
                    "UPDATE watch_sentinels SET
                        hard_killed_at = NULL,
                        hard_kill_reason = NULL,
                        probation_until = ?3
                     WHERE tenant = ?1 AND name = ?2",
                    rusqlite::params![tenant, name, probation_until_ms],
                )?;
                tx.commit()?;
                Ok::<DurableClearOutcome, rusqlite::Error>(DurableClearOutcome {
                    was_hard_killed,
                    probation_until_ms,
                })
            })
            .await?;
        Ok(outcome)
    }

    /// T27 — upsert a sentinel's registration row at boot from sentinels.yaml.
    /// Idempotent re runtime state: ON CONFLICT touches only `tier`,
    /// `cooldown_ms`, `config_json` — never resets `hard_killed_at`,
    /// `hard_kill_reason`, `probation_until`, or `enabled`. Restarting the
    /// sidecar must not wipe quarantine progress.
    pub async fn upsert_sentinel_registration(
        &self,
        tenant: &str,
        name: &str,
        tier: &str,
        cooldown_ms: i64,
        config_json: &str,
    ) -> anyhow::Result<()> {
        let tenant = tenant.to_string();
        let name = name.to_string();
        let tier = tier.to_string();
        let config_json = config_json.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO watch_sentinels
                        (name, tenant, tier, cooldown_ms, config_json, enabled)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1)
                     ON CONFLICT(tenant, name) DO UPDATE SET
                        tier        = excluded.tier,
                        cooldown_ms = excluded.cooldown_ms,
                        config_json = excluded.config_json",
                    rusqlite::params![name, tenant, tier, cooldown_ms, config_json],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// T33.7 P1-5 — list sentinel rows whose `probation_until` is still in
    /// the future AND `hard_killed_at` is NULL. Used by
    /// `QuarantineState::hydrate_probation_from_db` on boot to mirror the
    /// durable probation window into the in-memory record so `is_blocked`
    /// returns `ProbationLogOnly` and `fire_pipeline` applies the
    /// `[PROBATION] ` reason prefix on scheduled fires during the residual
    /// window.
    ///
    /// Hard-killed rows are intentionally skipped: the insert_fire OCC reads
    /// `hard_killed_at` from DB on every write, so hard-kill gating survives
    /// restart without in-memory hydration. Hydrating a (hard-killed,
    /// probation-set) row would produce a split-brain where `is_blocked`
    /// returns `ProbationLogOnly` but the OCC silently rejects every fire.
    pub async fn list_active_probation(&self) -> anyhow::Result<Vec<(String, String, i64)>> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let rows = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT tenant, name, probation_until
                     FROM watch_sentinels
                     WHERE probation_until IS NOT NULL
                       AND probation_until > ?1
                       AND hard_killed_at IS NULL",
                )?;
                let rows: Vec<(String, String, i64)> = stmt
                    .query_map(rusqlite::params![now_ms], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<Vec<(String, String, i64)>, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }

    /// T33.P0-B (review) — list rows where `hard_killed_at IS
    /// NOT NULL`. Used by `QuarantineState::hydrate_hard_kill_from_db` on
    /// boot to mirror durable hard-kills into the in-memory record so
    /// `is_blocked` returns `HardKilled` at the gate (step 3 of
    /// fire_pipeline). Without hydration the gate silently misses, the
    /// pipeline runs observe/interesting/escalate for a known-bad sentinel,
    /// and only the OCC in `insert_fire` catches the disagreement.
    ///
    /// Returns `(tenant, name, hard_killed_at_ms)` tuples. Failure here is
    /// fail-closed at the boot caller: the hard-kill ladder is the
    /// "Action is final" rail and silently losing it on restart violates
    /// the wall-line.
    pub async fn list_active_hard_killed(&self) -> anyhow::Result<Vec<(String, String, i64)>> {
        let rows = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT tenant, name, hard_killed_at
                     FROM watch_sentinels
                     WHERE hard_killed_at IS NOT NULL",
                )?;
                let rows: Vec<(String, String, i64)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<Vec<(String, String, i64)>, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }

    /// T27 — per-tenant registry view with stats:
    /// LEFT JOIN watch_fires for last_fire_at + fires_last_hour.
    pub async fn list_registered(&self, tenant: &str) -> anyhow::Result<Vec<RegistryRow>> {
        let tenant_owned = tenant.to_string();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let one_hour_ago = now_ms - 3_600_000;
        let rows = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT s.name,
                            s.tier,
                            s.cooldown_ms,
                            s.enabled,
                            s.hard_killed_at,
                            s.config_json,
                            (SELECT MAX(f.fired_at)
                             FROM watch_fires f
                             WHERE f.tenant = s.tenant AND f.sentinel = s.name) AS last_fire_at,
                            (SELECT COUNT(*)
                             FROM watch_fires f
                             WHERE f.tenant = s.tenant AND f.sentinel = s.name
                               AND f.fired_at > ?2) AS fires_last_hour
                     FROM watch_sentinels s
                     WHERE s.tenant = ?1
                     ORDER BY s.name ASC",
                )?;
                let rows: Vec<RegistryRow> = stmt
                    .query_map(rusqlite::params![tenant_owned, one_hour_ago], |r| {
                        Ok(RegistryRow {
                            name: r.get(0)?,
                            tier: r.get(1)?,
                            cooldown_ms: r.get(2)?,
                            enabled: r.get::<_, i64>(3)? != 0,
                            hard_killed_at: r.get(4)?,
                            config_json: r.get(5)?,
                            last_fire_at: r.get(6)?,
                            fires_last_hour: r.get(7)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<Vec<RegistryRow>, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }
}
