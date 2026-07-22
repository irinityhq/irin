//! Stats, audit log, and UI snapshot read surfaces.

use crate::watch::db::WatchDb;
use crate::watch::quarantine::QuarantineState;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::sync::Arc;

use super::helpers::{
    admin_token_matches, assert_canary_tenant, json_response, problem, problem_with_tenant,
};

/// Audit-endpoint pagination cap. The endpoint accepts any `limit` query
/// param but the SQL runs with `min(requested, AUDIT_LIMIT_CAP)`.
pub const AUDIT_LIMIT_CAP: i64 = 500;
pub const AUDIT_LIMIT_DEFAULT: i64 = 50;

/// T29: `GET /watch/audit/{tenant}?limit=&before_id=` — descending fire log
/// with cursor pagination. `limit` defaults to 50, capped at `AUDIT_LIMIT_CAP`.
pub async fn audit_json(
    db: Arc<WatchDb>,
    tenant: String,
    limit: Option<i64>,
    before_id: Option<i64>,
) -> Response {
    let requested = limit.unwrap_or(AUDIT_LIMIT_DEFAULT).max(1);
    let applied = requested.min(AUDIT_LIMIT_CAP);
    match db.list_fires_descending(&tenant, applied, before_id).await {
        Ok(rows) => {
            let fires: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "sentinel": r.sentinel,
                        "fired_at": r.fired_at,
                        "state_json": r.state_json,
                        "reason": r.reason,
                        "prev_hash": r.prev_hash,
                        "hash": r.hash,
                    })
                })
                .collect();
            let next_before_id = fires.last().and_then(|f| f["id"].as_i64());
            json_response(
                StatusCode::OK,
                json!({
                    "tenant": tenant,
                    "applied_limit": applied,
                    "next_before_id": next_before_id,
                    "fires": fires,
                }),
            )
        }
        Err(e) => problem_with_tenant(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal-error",
            &e.to_string(),
            &tenant,
        ),
    }
}

/// T33.P1-D — JSON response shape for `GET /watch/stats`. The Lua-side
/// prometheus poller scrapes this endpoint and emits each field as the
/// matching `gw_watch_*_total` counter on /metrics. New counters added
/// here MUST also be added to the Lua poller (separate repo); the
/// silent-unscrape gap closes when both sides see the field.
// watch telemetry design-review amendment: `Eq` dropped (PartialEq retained)
// because the spend gauge fields are f64 — money stays in USD float form so
// the Lua poller renders gw_watch_spend_*_usd without a cents conversion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct WatchStats {
    /// Increments when the runner sees `FireOutcome::AuditWriteErr` /
    /// `AuditWorkerCrashed` / `Timeout("audit")` (T33.P1-B). Emitted as
    /// `gw_watch_audit_infra_errors_total`.
    pub audit_infra_errors_total: u64,
    /// Increments when `record_failure`'s `db.upsert_hard_kill` call
    /// returns `Err`, leaving the sentinel in `pending_hard_kill_persist`
    /// limbo (T33.P1-D). Emitted as `gw_watch_persist_failures_total`.
    pub persist_failures_total: u64,
    /// Current count of records parked in
    /// `pending_hard_kill_persist = Some(_)`
    /// limbo. Snapshot gauge, not a counter — value can rise and fall as
    /// records flow into and out of pending. Emitted as
    /// `gw_watch_pending_pending_records` (gauge) on /metrics by the
    /// Lua poller. This makes the retry backlog visible to operators.
    #[serde(default)]
    pub pending_pending_records: u64,
    /// Count of pending hard-kill retry attempts that ended in `Err` or a
    /// 5s timeout inside
    /// `retry_pending_hard_kill_once`. Sibling counter to
    /// `persist_failures_total` (which counts FIRST-fail events inside
    /// `record_failure`); this counts subsequent retries that also failed.
    /// Emitted as `gw_watch_pending_retry_failures_total` (counter).
    #[serde(default)]
    pub pending_retry_failures_total: u64,
    /// Age in ms of the oldest record currently parked in pending limbo.
    /// Zero when no records are pending.
    /// First-set Instant semantics (see quarantine.rs module-doc INVARIANT):
    /// a retry that fails again does NOT restamp, so this gauge
    /// monotonically rises until the record persists or is admin-cleared.
    /// Emitted as `gw_watch_pending_oldest_age_ms` (gauge).
    #[serde(default)]
    pub pending_oldest_age_ms: u64,
    /// lease liveness (telemetry invariant / lease-loss path) — count of deliberation
    /// leases lost while a council call was (or may have been) in flight:
    /// mid-flight `RenewOutcome::Lost` in the dispatcher renewal driver, or
    /// an expired real in-flight claim reclaimed by
    /// `sweep_phantom_claims_counted`. Every increment pairs with a RECON
    /// HINT warn carrying the escalation id (possible orphan provider
    /// charge for p0d's out-of-band recon). Emitted as
    /// `gw_watch_lease_expired_during_deliberation_total`.
    #[serde(default)]
    pub lease_expired_during_deliberation: u64,
    /// watch telemetry (telemetry invariant) — idempotency-dedup MISS detector: count of
    /// settles that wrote a realized cost for a (tenant, id) that already
    /// had one (see `db::SettleReport`). The OCC fence makes this impossible
    /// in normal operation — any non-zero value is an alarm. Emitted as
    /// `gw_watch_dup_charge_alarm_total`.
    #[serde(default)]
    pub dup_charge_alarm_total: u64,
    /// T21a: capability tokens rejected (immortal or lifetime > 24h).
    /// Emitted as `gw_watch_cap_token_rejected_total`.
    #[serde(default)]
    pub cap_token_rejected_total: u64,

    /// A4a/T21 — staged directives swept to `expired` because their absolute TTL
    /// (`expires_at_ms`) elapsed before the worker could claim/dispatch them.
    /// Emitted as `gw_watch_directive_ttl_expired_total`.
    pub directive_ttl_expired_total: u64,
    /// T21d — staged directives dead-lettered (swept to `expired`) for exceeding
    /// `DIRECTIVE_MAX_DELIVERY_ATTEMPTS` re-claims (poison directive / flapping worker).
    /// Distinct from the TTL counter so attempt-exhaustion is visible apart from clock-TTL.
    /// Emitted as `gw_watch_directive_max_delivery_exceeded_total`.
    #[serde(default)]
    pub directive_max_delivery_exceeded_total: u64,
    /// P2 — directives REFUSED at stage time because the created-time normalization
    /// delta exceeded `MAX_ALLOWED_SKEW_MS` (clock-skew circuit-breaker). The breaker fails
    /// safe (never spends), so a poisoned per-tenant `prior_max` silently rejects every later
    /// directive for that tenant — this counter is the page-on-it signal.
    /// Emitted as `gw_watch_directive_clock_skew_rejected_total`.
    #[serde(default)]
    pub directive_clock_skew_rejected_total: u64,
    /// watch telemetry (telemetry invariant) — today's UTC-bucket council spend
    /// (reserved + settled) read from the p0c `spend_ledger` via
    /// `get_daily_council_spend`. Gauge; pairs with `spend_cap_usd`. Emitted
    /// as `gw_watch_spend_today_usd`.
    #[serde(default)]
    pub spend_today_usd: f64,
    /// watch telemetry — the enforced UTC-day cap (`db::daily_spend_cap()`),
    /// surfaced so dashboards can plot spend vs cap from one scrape. Emitted
    /// as `gw_watch_spend_cap_usd`.
    #[serde(default)]
    pub spend_cap_usd: f64,
    /// watch telemetry (telemetry invariant) — last observed kill-switch drain latency:
    /// wall ms from the disarm signal (`tx.send(true)`) to the producer's
    /// drain ack. 0 = no disarm recorded yet (sub-ms drains round up to 1).
    /// The Lua poller owns histogram bucketing per the council_stats
    /// precedent; the sidecar ships last + max.
    #[serde(default)]
    pub kill_switch_latency_ms: u64,
    /// watch telemetry — max observed kill-switch drain latency (ms) since
    /// boot, so a slow historical drain stays visible between scrapes.
    #[serde(default)]
    pub kill_switch_latency_max_ms: u64,
    /// Count of out-of-band reconciliation ticks
    /// whose |local settled - external billing| divergence exceeded the
    /// threshold. Each increment pairs with a `recon_alarm` row in watch.db.
    /// Emitted as `gw_watch_recon_divergence_total`.
    #[serde(default)]
    pub recon_divergence_total: u64,
    /// Count of reconciliation ticks where reserved_usd > daily_cap
    /// (orphaned reservation / ledger leak). Page-only; emitted as
    /// `gw_watch_recon_cap_breach_total`.
    #[serde(default)]
    pub recon_cap_breach_total: u64,
    /// Settles whose valid realized cost exceeded the
    /// per-directive reservation ceiling. Emitted as
    /// `gw_watch_settle_ceiling_overshoot_total`.
    #[serde(default)]
    pub settle_ceiling_overshoot_total: u64,
    /// `/watch/stats` assemblies whose spend_ledger
    /// gauge read failed (spend_today_usd degraded to 0.0). Non-zero means
    /// the spend gauge is blind, not that spend is zero. Emitted as
    /// `gw_watch_spend_gauge_read_failures_total`.
    #[serde(default)]
    pub spend_gauge_read_failures_total: u64,
    /// Kill-switch drains that hit the 5s timeout.
    /// Each one also recorded a 5000ms floor observation into the latency
    /// last/max. Emitted as `gw_watch_kill_switch_drain_timeout_total`.
    #[serde(default)]
    pub kill_switch_drain_timeout_total: u64,
    /// Count of unauthenticated arm
    /// stage/confirm rejections (401). Counted in this prunable metric instead
    /// of a permanent `arm_audit` row so the engine-unprunable ceremony chain
    /// cannot be grown one row per request by an unauthenticated caller.
    /// Emitted as `gw_watch_arm_rejected_unauth_total`.
    #[serde(default)]
    pub arm_rejected_unauth_total: u64,
}

/// Single assembly point for the `/watch/stats` JSON snapshot.
/// Used by BOTH the main.rs handler and the integration tests so the scrape
/// surface cannot drift between them. `db: None` (in-memory test path) reads
/// the spend gauge as 0.0; a db read failure also degrades to 0.0 with a
/// warn — the stats endpoint must never 500 over a gauge.
pub async fn build_watch_stats(quarantine: &QuarantineState, db: Option<&WatchDb>) -> WatchStats {
    let snapshot = quarantine.pending_snapshot();
    let spend_today_usd = match db {
        Some(db) => match db.get_daily_council_spend().await {
            Ok(v) => v,
            Err(e) => {
                // A degraded gauge must be
                // distinguishable from genuinely-zero spend on the scrape
                // surface — bump the read-failure counter alongside the warn.
                quarantine.bump_spend_gauge_read_failure();
                tracing::warn!(error = %e, "watch/stats: spend_ledger gauge read failed; reporting 0.0 (spend_gauge_read_failures_total bumped)");
                0.0
            }
        },
        None => 0.0,
    };
    WatchStats {
        audit_infra_errors_total: quarantine.audit_infra_errors_total(),
        persist_failures_total: quarantine.persist_failures_total(),
        pending_pending_records: snapshot.count,
        pending_retry_failures_total: quarantine.pending_retry_failures_total(),
        pending_oldest_age_ms: snapshot.oldest_age_ms,
        lease_expired_during_deliberation: quarantine.lease_expired_during_deliberation(),
        dup_charge_alarm_total: quarantine.dup_charge_alarm_total(),
        cap_token_rejected_total: crate::watch::dispatcher::cap_token_rejected_total(),
        directive_ttl_expired_total: crate::watch::dispatcher::directive_ttl_expired_total(),
        directive_max_delivery_exceeded_total:
            crate::watch::dispatcher::directive_max_delivery_exceeded_total(),
        directive_clock_skew_rejected_total:
            crate::watch::dispatcher::directive_clock_skew_rejected_total(),
        spend_today_usd,
        spend_cap_usd: crate::watch::db::daily_spend_cap(),
        kill_switch_latency_ms: quarantine.kill_switch_latency_last_ms(),
        kill_switch_latency_max_ms: quarantine.kill_switch_latency_max_ms(),
        recon_divergence_total: quarantine.recon_divergence_total(),
        recon_cap_breach_total: quarantine.recon_cap_breach_total(),
        settle_ceiling_overshoot_total: quarantine.settle_ceiling_overshoot_total(),
        spend_gauge_read_failures_total: quarantine.spend_gauge_read_failures_total(),
        kill_switch_drain_timeout_total: quarantine.kill_switch_drain_timeout_total(),
        arm_rejected_unauth_total: quarantine.arm_rejected_unauth_total(),
    }
}

/// Operator-facing Watch snapshot. This is intentionally a separate typed
/// projection rather than a composition of the existing internal read APIs:
/// adding a field to `RegistryRow`, `FireRow`, or `WatchStats` must never
/// widen the UI surface by accident.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UiWatchSnapshot {
    pub tenant: String,
    pub canary_tenant: String,
    /// Actual CDC producer task state, not merely the configured env flag.
    /// False is the default public posture: Watch can observe without turning
    /// fires into Council/outbox work.
    pub action_production_armed: bool,
    pub sentinels: Vec<UiSentinelReadiness>,
    pub temperature: UiWatchTemperature,
    pub recent_fires: Vec<UiRecentFire>,
    pub budget: UiWatchBudget,
    pub degradation: UiWatchDegradation,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiSentinelReadiness {
    pub name: String,
    pub tier: String,
    pub cooldown_ms: i64,
    pub enabled: bool,
    pub hard_killed_at: Option<i64>,
    pub last_fire_at: Option<i64>,
    pub fires_last_hour: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiWatchTemperature {
    pub value: f64,
    pub level: &'static str,
    pub fires_last_hour: i64,
    pub fires_last_24h: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiRecentFire {
    pub id: i64,
    pub sentinel: String,
    pub fired_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiWatchBudget {
    pub spend_today_usd: f64,
    pub spend_cap_usd: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiWatchDegradation {
    pub audit_infra_errors_total: u64,
    pub persist_failures_total: u64,
    pub pending_records: u64,
    pub pending_retry_failures_total: u64,
    pub pending_oldest_age_ms: u64,
    pub lease_expired_during_deliberation_total: u64,
    pub duplicate_charge_alarms_total: u64,
    pub directive_ttl_expired_total: u64,
    pub directive_max_delivery_exceeded_total: u64,
    pub directive_clock_skew_rejected_total: u64,
    pub recon_divergence_total: u64,
    pub recon_cap_breach_total: u64,
    pub settle_ceiling_overshoot_total: u64,
    pub spend_gauge_read_failures_total: u64,
    pub kill_switch_drain_timeout_total: u64,
}

const UI_RECENT_FIRE_LIMIT: i64 = 50;

/// `GET /watch/ui-snapshot/{tenant}` — the only Watch read intended for a
/// human UI. It is admin-authenticated, canary-guarded, read-only, and emits a
/// strict projection with no configs, raw state/reasons/payloads, provider
/// data, prompts, credentials, paths, envelopes, or mutation capabilities.
pub async fn ui_snapshot_json(
    db: Arc<WatchDb>,
    quarantine: Arc<QuarantineState>,
    admin_token: String,
    bearer: Option<String>,
    tenant: String,
    canary_tenant: &str,
) -> Response {
    if !admin_token_matches(&admin_token, bearer.as_deref()) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "request is missing valid credentials",
        );
    }
    if let Some(resp) = assert_canary_tenant(&tenant, canary_tenant) {
        return resp;
    }

    let sentinels = match db.list_registered(&tenant).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| UiSentinelReadiness {
                name: row.name,
                tier: row.tier,
                cooldown_ms: row.cooldown_ms,
                enabled: row.enabled,
                hard_killed_at: row.hard_killed_at,
                last_fire_at: row.last_fire_at,
                fires_last_hour: row.fires_last_hour,
            })
            .collect(),
        Err(error) => {
            tracing::error!(%error, %tenant, "watch UI snapshot registry read failed");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot-unavailable",
                "watch snapshot is temporarily unavailable",
            );
        }
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let fires_last_hour = match db.count_fires_since(&tenant, now_ms - 3_600_000).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, %tenant, "watch UI snapshot hourly count failed");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot-unavailable",
                "watch snapshot is temporarily unavailable",
            );
        }
    };
    let fires_last_24h = match db.count_fires_since(&tenant, now_ms - 86_400_000).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, %tenant, "watch UI snapshot daily count failed");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot-unavailable",
                "watch snapshot is temporarily unavailable",
            );
        }
    };
    let raw_temperature =
        0.7 * (fires_last_hour as f64 / 5.0) + 0.3 * (fires_last_24h as f64 / 24.0);
    let temperature_value = raw_temperature.clamp(0.0, 1.0);
    let temperature_level = if temperature_value < 0.15 {
        "cold"
    } else if temperature_value < 0.6 {
        "warm"
    } else {
        "hot"
    };

    let recent_fires = match db
        .list_fires_descending(&tenant, UI_RECENT_FIRE_LIMIT, None)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|row| UiRecentFire {
                id: row.id,
                sentinel: row.sentinel,
                fired_at: row.fired_at,
            })
            .collect(),
        Err(error) => {
            tracing::error!(%error, %tenant, "watch UI snapshot fire-tail read failed");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot-unavailable",
                "watch snapshot is temporarily unavailable",
            );
        }
    };

    let stats = build_watch_stats(&quarantine, Some(db.as_ref())).await;
    json_response(
        StatusCode::OK,
        UiWatchSnapshot {
            tenant,
            canary_tenant: canary_tenant.to_string(),
            action_production_armed: quarantine.producer_kill_state.lock().is_some(),
            sentinels,
            temperature: UiWatchTemperature {
                value: temperature_value,
                level: temperature_level,
                fires_last_hour,
                fires_last_24h,
            },
            recent_fires,
            budget: UiWatchBudget {
                spend_today_usd: stats.spend_today_usd,
                spend_cap_usd: stats.spend_cap_usd,
            },
            degradation: UiWatchDegradation {
                audit_infra_errors_total: stats.audit_infra_errors_total,
                persist_failures_total: stats.persist_failures_total,
                pending_records: stats.pending_pending_records,
                pending_retry_failures_total: stats.pending_retry_failures_total,
                pending_oldest_age_ms: stats.pending_oldest_age_ms,
                lease_expired_during_deliberation_total: stats.lease_expired_during_deliberation,
                duplicate_charge_alarms_total: stats.dup_charge_alarm_total,
                directive_ttl_expired_total: stats.directive_ttl_expired_total,
                directive_max_delivery_exceeded_total: stats.directive_max_delivery_exceeded_total,
                directive_clock_skew_rejected_total: stats.directive_clock_skew_rejected_total,
                recon_divergence_total: stats.recon_divergence_total,
                recon_cap_breach_total: stats.recon_cap_breach_total,
                settle_ceiling_overshoot_total: stats.settle_ceiling_overshoot_total,
                spend_gauge_read_failures_total: stats.spend_gauge_read_failures_total,
                kill_switch_drain_timeout_total: stats.kill_switch_drain_timeout_total,
            },
        },
    )
}
