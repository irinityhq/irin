//! watch telemetry — OUT-OF-BAND spend reconciliation (Invariant + P1-G).
//!
//! The ruling's bar: reconciliation must read an out-of-band billing source —
//! a provider usage API or an independent ledger — never the same database
//! that holds the bug. This module compares `spend_ledger.settled_usd` for
//! today's UTC bucket against one of two external sources:
//!
//! * [`FileImportRecon`] — the ROBUST DEFAULT. The operator drops a provider
//!   billing export (JSON or CSV) at `RECON_IMPORT_PATH`; the job parses the
//!   total cost for the UTC day. Default because provider usage APIs are
//!   delayed/async and may not cover the exact window — a manual export is
//!   always reconcilable.
//! * [`ProviderUsageRecon`] — best-effort. Queries the OpenAI Costs API
//!   (`/v1/organization/costs`, the most-documented authenticated usage
//!   endpoint among the keys exposed in .env.example). The key is read from
//!   env at call time and NEVER printed, logged, or echoed in errors.
//!
//! Divergence beyond `RECON_DIVERGENCE_THRESHOLD_USD` writes a row to the
//! `recon_alarm` table (the cross-check artifact: both sides preserved),
//! emits `tracing::error!`, and bumps `recon_divergence_total` on
//! `/watch/stats`.
//!
//! DEFAULT-OFF: the periodic loop ([`recon_loop`], pruning_loop pattern)
//! spawns ONLY when `RECON_CADENCE_SECS` is set AND a source is configured.
//! No env, no task, no surprise provider API calls.

use crate::watch::db::{utc_day_bucket, WatchDb};
use crate::watch::quarantine::QuarantineState;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const RECON_CADENCE_SECS_ENV: &str = "RECON_CADENCE_SECS";
pub const RECON_DIVERGENCE_THRESHOLD_USD_ENV: &str = "RECON_DIVERGENCE_THRESHOLD_USD";
pub const RECON_IMPORT_PATH_ENV: &str = "RECON_IMPORT_PATH";

/// Default divergence threshold when `RECON_DIVERGENCE_THRESHOLD_USD` is
/// unset/unparseable: a dollar of drift in a $50/day cap regime is worth a
/// look; anything tighter false-positives on provider rounding.
pub const RECON_DIVERGENCE_THRESHOLD_USD_DEFAULT: f64 = 1.0;

/// An out-of-band billing source for one UTC day bucket. Implementations
/// MUST NOT read the watch.db that holds the spend_ledger — that is the
/// database whose bugs this trait exists to catch.
#[async_trait]
pub trait ReconSource: Send + Sync {
    /// Stable short name recorded in the `recon_alarm.source` column.
    fn source_name(&self) -> &'static str;
    /// Total realized provider cost (USD) for the given `YYYY-MM-DD` UTC day.
    async fn fetch_period_cost_usd(&self, day_bucket: &str) -> anyhow::Result<f64>;
}

/// Impl B (the robust default): operator-dropped provider export at a fixed
/// path. Accepted shapes (tried in order):
///   1. JSON object map:      `{"2026-06-06": 25.0, ...}`
///   2. JSON single-day form: `{"day_bucket": "2026-06-06", "total_usd": 25.0}`
///   3. CSV lines:            `2026-06-06,25.0` (unparseable lines skipped,
///      so a `day,usd` header is fine)
pub struct FileImportRecon {
    path: PathBuf,
}

impl FileImportRecon {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl ReconSource for FileImportRecon {
    fn source_name(&self) -> &'static str {
        "file_import"
    }

    async fn fetch_period_cost_usd(&self, day_bucket: &str) -> anyhow::Result<f64> {
        let raw = tokio::fs::read_to_string(&self.path).await.map_err(|e| {
            anyhow::anyhow!(
                "recon import file unreadable at {}: {e}",
                self.path.display()
            )
        })?;
        parse_import(&raw, day_bucket)
    }
}

/// Parse an operator-dropped export for one day's total. Pure fn (unit-tested
/// through the integration tests via both JSON and CSV fixtures).
pub fn parse_import(raw: &str, day_bucket: &str) -> anyhow::Result<f64> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(obj) = v.as_object() {
            if let Some(n) = obj.get(day_bucket).and_then(|x| x.as_f64()) {
                return Ok(n);
            }
            if let (Some(d), Some(n)) = (
                obj.get("day_bucket").and_then(|x| x.as_str()),
                obj.get("total_usd").and_then(|x| x.as_f64()),
            ) {
                if d == day_bucket {
                    return Ok(n);
                }
                anyhow::bail!("recon import covers day {d}, wanted {day_bucket}");
            }
            anyhow::bail!("no entry for day {day_bucket} in JSON recon import");
        }
    }
    // CSV fallback: day,usd per line; lines that don't parse are skipped.
    for line in raw.lines() {
        let mut parts = line.splitn(2, ',');
        if let (Some(d), Some(c)) = (parts.next(), parts.next()) {
            if d.trim() == day_bucket {
                if let Ok(n) = c.trim().parse::<f64>() {
                    return Ok(n);
                }
            }
        }
    }
    anyhow::bail!("no entry for day {day_bucket} in recon import file")
}

/// Impl A (best-effort): the OpenAI Costs API
/// (`GET /v1/organization/costs?start_time=&end_time=`) — the most-documented
/// authenticated usage/cost endpoint among the provider keys this deployment
/// exposes. Usage APIs are delayed/async, so this source may legitimately
/// under-report very recent windows; the file import remains the default.
///
/// The key is read from the environment at call time and
/// never printed, logged, or interpolated into errors. Errors carry HTTP
/// status only.
pub struct ProviderUsageRecon {
    base_url: String,
}

impl Default for ProviderUsageRecon {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
        }
    }
}

impl ProviderUsageRecon {
    /// Override the base URL (tests / proxies). Production uses `default()`.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    /// Read the API key from env. The Costs API requires an org/admin-scoped
    /// key, so `OPENAI_ADMIN_KEY` wins over `OPENAI_API_KEY`. The VALUE is
    /// never logged — only which var names were tried.
    fn api_key() -> anyhow::Result<String> {
        for var in ["OPENAI_ADMIN_KEY", "OPENAI_API_KEY"] {
            if let Ok(v) = std::env::var(var) {
                if !v.trim().is_empty() {
                    return Ok(v);
                }
            }
        }
        anyhow::bail!("no provider usage key in env (tried OPENAI_ADMIN_KEY, OPENAI_API_KEY)")
    }
}

/// Inverse of `utc_day_bucket`: `YYYY-MM-DD` -> Unix seconds at 00:00:00 UTC.
/// days_from_civil (Hinnant), mirroring the civil_from_days in db.rs so the
/// two stay drift-free without a chrono dependency.
pub fn day_bucket_to_unix_secs(day_bucket: &str) -> anyhow::Result<i64> {
    let mut it = day_bucket.splitn(3, '-');
    let (y, m, d) = (|| -> Option<(i64, i64, i64)> {
        Some((
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        ))
    })()
    .ok_or_else(|| anyhow::anyhow!("bad day bucket: {day_bucket}"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        anyhow::bail!("bad day bucket: {day_bucket}");
    }
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400)
}

#[async_trait]
impl ReconSource for ProviderUsageRecon {
    fn source_name(&self) -> &'static str {
        "provider_usage"
    }

    async fn fetch_period_cost_usd(&self, day_bucket: &str) -> anyhow::Result<f64> {
        let key = Self::api_key()?;
        let start = day_bucket_to_unix_secs(day_bucket)?;
        let end = start + 86_400;
        let url = format!(
            "{}/v1/organization/costs?start_time={start}&end_time={end}&limit=31",
            self.base_url
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let resp = client
            .get(&url)
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("provider usage request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            // Body deliberately not surfaced (could echo auth context).
            anyhow::bail!("provider usage API returned HTTP {status}");
        }
        let body: serde_json::Value = resp.json().await?;
        let mut total = 0.0f64;
        if let Some(buckets) = body.get("data").and_then(|d| d.as_array()) {
            for bucket in buckets {
                if let Some(results) = bucket.get("results").and_then(|r| r.as_array()) {
                    for r in results {
                        if let Some(v) = r
                            .get("amount")
                            .and_then(|a| a.get("value"))
                            .and_then(|v| v.as_f64())
                        {
                            total += v;
                        }
                    }
                }
            }
        }
        Ok(total)
    }
}

/// Outcome of one recon tick — both sides of the comparison plus whether the
/// alarm path (recon_alarm row + counter + error log) fired. T2 adds the
/// reserved/cap invariant fields (shadow only).
#[derive(Debug, Clone, PartialEq)]
pub struct ReconOutcome {
    pub day_bucket: String,
    pub local_usd: f64,
    pub external_usd: f64,
    pub divergence_usd: f64,
    pub alarmed: bool,
    pub reserved_usd: f64,
    pub cap_usd: f64,
    pub cap_breached: bool,
    pub billed_minus_reserved_usd: f64,
}

/// One reconciliation pass (telemetry invariant): compare today's
/// `spend_ledger.settled_usd` against the out-of-band source. Divergence
/// beyond `threshold_usd` writes a `recon_alarm` row, bumps
/// `recon_divergence_total`, and emits an ERROR. Returns the comparison
/// either way so callers/tests can assert the quiet path too.
pub async fn run_recon_once(
    db: &WatchDb,
    quarantine: &QuarantineState,
    source: &dyn ReconSource,
    threshold_usd: f64,
) -> anyhow::Result<ReconOutcome> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    run_recon_for_day(
        db,
        quarantine,
        source,
        threshold_usd,
        &utc_day_bucket(now_ms),
    )
    .await
}

/// one reconciliation pass for an ARBITRARY
/// `YYYY-MM-DD` UTC bucket. `run_recon_once` delegates here with today's
/// bucket; the loop additionally runs YESTERDAY's bucket each tick so
/// charges landing near the UTC midnight boundary (settled into a bucket
/// that closes before the next recon tick) can no longer permanently escape
/// reconciliation.
pub async fn run_recon_for_day(
    db: &WatchDb,
    quarantine: &QuarantineState,
    source: &dyn ReconSource,
    threshold_usd: f64,
    day_bucket: &str,
) -> anyhow::Result<ReconOutcome> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let local_usd = db.get_settled_council_spend_for_bucket(day_bucket).await?;
    let external_usd = source.fetch_period_cost_usd(day_bucket).await?;
    let divergence_usd = external_usd - local_usd;
    let alarmed = divergence_usd.abs() > threshold_usd;

    if alarmed {
        db.insert_recon_alarm(
            now_ms,
            day_bucket,
            local_usd,
            external_usd,
            divergence_usd,
            source.source_name(),
        )
        .await?;
        quarantine.bump_recon_divergence();
        tracing::error!(
            day_bucket = %day_bucket,
            local_usd,
            external_usd,
            divergence_usd,
            threshold_usd,
            source = source.source_name(),
            "RECON DIVERGENCE: local spend_ledger settled total disagrees with the out-of-band billing source beyond threshold — possible dup charge, orphan in-flight charge (see lease_expired_during_deliberation recon hints), or ledger bug"
        );
    }

    // T2 shadow invariant (Review). KNOWN BLIND SPOT: this fires
    // ONLY on a full breach (reserved > cap). A PARTIAL orphan that leaves
    // reserved elevated but still <= cap (e.g. true=20, leaked=45, cap=50 ->
    // 50% headroom permanently eaten) is headroom EROSION and is NOT detected
    // here — and is plausibly the more common manifestation of the stale-reclaim
    // bug than an outright breach. So this is a breach tripwire, not a proof the
    // ledger is leak-free. Promotion to enforcement + an erosion detector are
    // post-canary follow-ups. `bump_recon_cap_breach` below counts ticks observed
    // in breach (a rate gauge), NOT distinct breach episodes; the PAGE dedups
    // per bucket via `cap_breach_page_edge`.
    let reserved_usd = db.get_reserved_council_spend_for_bucket(day_bucket).await?;
    let cap_usd = crate::watch::db::daily_spend_cap();
    let cap_breached = reserved_usd > cap_usd;
    let billed_minus_reserved_usd = external_usd - reserved_usd;

    if cap_breached {
        quarantine.bump_recon_cap_breach();
        tracing::error!(
            day_bucket = %day_bucket,
            reserved_usd,
            cap_usd,
            source = source.source_name(),
            "reserved exceeds daily cap — orphaned reservation / ledger leak"
        );
    }

    // T2 billed-reserved delta monitor (reuse already-fetched external/local)
    tracing::debug!(
        day_bucket = %day_bucket,
        local_usd,
        external_usd,
        reserved_usd,
        billed_minus_reserved_usd,
        "recon tick"
    );
    if billed_minus_reserved_usd > threshold_usd {
        tracing::warn!(
            day_bucket = %day_bucket,
            external_usd,
            reserved_usd,
            billed_minus_reserved_usd,
            threshold_usd,
            "billed exceeds reserved beyond threshold (settle overshoot aggregate or anomaly)"
        );
    }

    Ok(ReconOutcome {
        day_bucket: day_bucket.to_string(),
        local_usd,
        external_usd,
        divergence_usd,
        alarmed,
        reserved_usd,
        cap_usd,
        cap_breached,
        billed_minus_reserved_usd,
    })
}

/// Which out-of-band source the recon loop should use.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconSourceKind {
    /// Operator-dropped export at this path — the robust default.
    FileImport(PathBuf),
    /// Provider usage API (best-effort; key read from env at call time).
    ProviderUsage,
}

impl ReconSourceKind {
    pub fn build(&self) -> Box<dyn ReconSource> {
        match self {
            ReconSourceKind::FileImport(p) => Box::new(FileImportRecon::new(p.clone())),
            ReconSourceKind::ProviderUsage => Box::new(ProviderUsageRecon::default()),
        }
    }
}

pub const RECON_AUTO_DISARM_ENV: &str = "RECON_AUTO_DISARM";

#[derive(Debug, Clone, PartialEq)]
pub struct ReconConfig {
    pub cadence: Duration,
    pub threshold_usd: f64,
    pub source: ReconSourceKind,
    /// H7a: when true, a divergence alarm auto-disarms the producer.
    /// Default OFF (page-only). Explicit RECON_AUTO_DISARM=on to enable.
    pub auto_disarm: bool,
}

/// Env wrapper around [`recon_config_from_values`]. `None` = recon loop OFF
/// (the default-OFF discipline: unset cadence means no task is spawned).
pub fn recon_config_from_env() -> Option<ReconConfig> {
    let provider_key_present = ["OPENAI_ADMIN_KEY", "OPENAI_API_KEY"].iter().any(|v| {
        std::env::var(v)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    });
    // H7a: auto-disarm on divergence defaults OFF (page-only) until recon
    // source trust is proven (council P1). Explicit "1"/"true"/"yes"/"on"
    // enables auto-disarm for operators who have authenticated their source.
    let auto_disarm = matches!(
        std::env::var(RECON_AUTO_DISARM_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    );
    recon_config_from_values(
        std::env::var(RECON_CADENCE_SECS_ENV).ok().as_deref(),
        std::env::var(RECON_DIVERGENCE_THRESHOLD_USD_ENV)
            .ok()
            .as_deref(),
        std::env::var(RECON_IMPORT_PATH_ENV).ok().as_deref(),
        provider_key_present,
        auto_disarm,
    )
}

/// Pure config assembly (testable without process-global env mutation):
/// * cadence unset / unparseable / 0 -> `None` (default-OFF; ruling risk note:
///   no surprise provider API calls).
/// * `RECON_IMPORT_PATH` set -> file import (the robust default source).
/// * else provider key present -> provider usage (best-effort).
/// * else -> warn + `None` (cadence configured but nothing to read).
pub fn recon_config_from_values(
    cadence_secs: Option<&str>,
    threshold_usd: Option<&str>,
    import_path: Option<&str>,
    provider_key_present: bool,
    auto_disarm: bool,
) -> Option<ReconConfig> {
    let cadence_secs = cadence_secs?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|s| *s > 0)?;
    let threshold_usd = threshold_usd
        .and_then(|t| t.trim().parse::<f64>().ok())
        .filter(|t| t.is_finite() && *t >= 0.0)
        .unwrap_or(RECON_DIVERGENCE_THRESHOLD_USD_DEFAULT);
    let source = match import_path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => ReconSourceKind::FileImport(PathBuf::from(p)),
        None if provider_key_present => ReconSourceKind::ProviderUsage,
        None => {
            tracing::warn!(
                "RECON_CADENCE_SECS is set but no recon source is configured (no RECON_IMPORT_PATH, no OPENAI_ADMIN_KEY/OPENAI_API_KEY) — recon loop NOT spawned"
            );
            return None;
        }
    };
    Some(ReconConfig {
        cadence: Duration::from_secs(cadence_secs),
        threshold_usd,
        source,
        auto_disarm,
    })
}

/// Periodic reconciliation loop — pruning_loop pattern (runner.rs): interval
/// tick with Skip on missed ticks, first (immediate) tick consumed so the
/// initial pass happens one full cadence after boot, biased shutdown select.
/// Best-effort: a failed tick warns and waits for the next cadence.
/// T2 shadow edge-trigger for the cap-breach PAGE (Review).
/// Returns true exactly when `day_bucket` transitions into breach, so the page
/// fires once per breach episode rather than once per recon tick. A cleared
/// breach is removed so a later re-breach pages again. The breach COUNTER is
/// independent (bumped every tick in `run_recon_for_day` as a rate gauge);
/// only the page dedups here. `breached` holds at most a handful of live
/// buckets (today/yesterday churn).
pub fn cap_breach_page_edge(
    breached: &mut std::collections::HashSet<String>,
    o: &ReconOutcome,
) -> bool {
    if o.cap_breached {
        // HashSet::insert returns true iff the bucket was not already breached.
        breached.insert(o.day_bucket.clone())
    } else {
        breached.remove(&o.day_bucket);
        false
    }
}

pub async fn recon_loop(
    db: Arc<WatchDb>,
    quarantine: Arc<QuarantineState>,
    cfg: ReconConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let source = cfg.source.build();
    // H7a: quiet notifier (the recon spawn already logged in runner.rs).
    let notifier = crate::watch::api::ArmNotifier::from_env_quiet();
    // T2 shadow-mode: cap-breach is page-only (never auto_disarm, never a hard
    // block); hard-block promotion is deferred to post-canary per premortem.
    // Council merge-gate (8a413c69-c2d) blocker: the PAGE must edge-trigger per
    // day_bucket, otherwise sticky cap_breached x per-tick x (today+yesterday)
    // is a self-DoS on the on-call. The counter still bumps every tick (rate
    // gauge); only the page dedups. Folded into one FnMut so all four recon-tick
    // arms (today/yesterday x alarmed/quiet) page identically.
    let mut breached_buckets: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut page_cap_breach = |o: &ReconOutcome| {
        if cap_breach_page_edge(&mut breached_buckets, o) {
            let reason = format!(
                "recon cap-breach day={} reserved={:.4} cap={:.4} (source={})",
                o.day_bucket,
                o.reserved_usd,
                o.cap_usd,
                source.source_name()
            );
            notifier.notify("recon-cap-breach", "recon(page-only)", &reason);
        }
    };
    let mut ticker = tokio::time::interval(cfg.cadence);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            // a dropped shutdown sender makes
            // changed() return Err immediately forever — treat it as
            // shutdown instead of spinning the loop hot (fixed in
            // pruning_loop / phantom_sweep_loop too).
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                let mut alarm_detail: Option<ReconOutcome> = None;
                match run_recon_once(&db, &quarantine, source.as_ref(), cfg.threshold_usd).await {
                    // Divergence already alarmed (row+counter+error) inside run_recon_once.
                    Ok(o) if o.alarmed => {
                        page_cap_breach(&o);
                        alarm_detail = Some(o);
                    }
                    Ok(o) => {
                        tracing::debug!(
                            day_bucket = %o.day_bucket,
                            local_usd = o.local_usd,
                            external_usd = o.external_usd,
                            "recon tick: within threshold"
                        );
                        page_cap_breach(&o);
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        source = source.source_name(),
                        "recon tick failed (best-effort; retrying next cadence)"
                    ),
                }

                // also reconcile
                // YESTERDAY's (closed) bucket. Best-effort with a softer
                // failure mode — exports/usage APIs legitimately may not
                // carry the prior day yet, so a fetch error here is debug,
                // not warn.
                let yesterday = utc_day_bucket(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64
                        - 86_400_000,
                );
                match run_recon_for_day(&db, &quarantine, source.as_ref(), cfg.threshold_usd, &yesterday).await {
                    Ok(o) if o.alarmed => {
                        page_cap_breach(&o);
                        if alarm_detail.is_none() { alarm_detail = Some(o); }
                    }
                    Ok(o) => {
                        page_cap_breach(&o);
                    }
                    Err(e) => tracing::debug!(
                        error = %e,
                        day_bucket = %yesterday,
                        "recon lookback tick (yesterday) skipped — source has no data for the closed bucket yet"
                    ),
                }

                // H7a: machine-actionable response to divergence. The alarm
                // (row+counter+error) already fired inside run_recon_*. Here we
                // either auto-disarm (default) or page-only (rollback).
                if let Some(o) = alarm_detail {
                    let reason = format!(
                        "recon divergence day={} local={:.4} external={:.4} delta={:.4} (threshold={:.4}, source={})",
                        o.day_bucket, o.local_usd, o.external_usd, o.divergence_usd,
                        cfg.threshold_usd, source.source_name()
                    );
                    if cfg.auto_disarm {
                        crate::watch::api::auto_disarm_producer(
                            &quarantine,
                            &notifier,
                            "recon-divergence(auto)",
                            &reason,
                        ).await;
                    } else {
                        // Page-only rollback path: still surface out-of-band.
                        notifier.notify("recon-divergence", "recon(page-only)", &reason);
                        tracing::error!(reason, "RECON DIVERGENCE: auto-disarm DISABLED (RECON_AUTO_DISARM=off) — paged only, producer left armed");
                    }
                }
            }
        }
    }
}
