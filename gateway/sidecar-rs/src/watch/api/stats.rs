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
    /// Worker pre-act Ed25519 verification refusals. Emitted as
    /// `gw_watch_directive_verify_failed_total`.
    #[serde(default)]
    pub directive_verify_failed_total: u64,
    /// Capability-token checks denied because the backing DB query errored
    /// (fail-closed). Emitted as `gw_watch_cap_token_db_error_deny_total`.
    #[serde(default)]
    pub cap_token_db_error_deny_total: u64,
    /// 1 when a live producer kill-switch channel is held; 0 otherwise.
    /// Emitted as `gw_watch_action_production_armed`.
    #[serde(default)]
    pub action_production_armed: bool,
    /// Per-tenant temperature gauges (canary / registered tenants).
    /// Emitted as labeled `gw_watch_temperature`.
    #[serde(default)]
    pub temperatures: Vec<WatchTemperatureStat>,
    /// Per-sentinel fires (from `watch_fires`) + runner ticks (choke point).
    /// Emitted as labeled `gw_watch_sentinel_*` families.
    #[serde(default)]
    pub sentinels: Vec<WatchSentinelStat>,
}

/// Per-tenant temperature slice for `/watch/stats` → Prometheus.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct WatchTemperatureStat {
    pub tenant: String,
    pub value: f64,
    pub level: String,
    pub fires_last_hour: u64,
    pub fires_last_24h: u64,
}

/// Per-sentinel runtime stats: durable fire count + in-process runner ticks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
pub struct WatchSentinelStat {
    pub tenant: String,
    pub sentinel: String,
    /// Lifetime fire count from `watch_fires`.
    pub fires_total: u64,
    pub ticks_fired: u64,
    pub ticks_uninteresting: u64,
    pub ticks_failure: u64,
    pub ticks_gated: u64,
    /// Wall-clock ms of last runner tick (0 = never).
    pub last_tick_ms: u64,
}

/// Single assembly point for the `/watch/stats` JSON snapshot.
/// Used by BOTH the main.rs handler and the integration tests so the scrape
/// surface cannot drift between them. `db: None` (in-memory test path) reads
/// the spend gauge as 0.0; a db read failure also degrades to 0.0 with a
/// warn — the stats endpoint must never 500 over a gauge.
///
/// **UiWatchSnapshot contract:** this struct may grow fields for metrics; the
/// UI snapshot projection is assembled separately and must stay byte-stable.
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

    let mut fire_counts: std::collections::HashMap<(String, String), u64> =
        std::collections::HashMap::new();
    let mut temperatures: Vec<WatchTemperatureStat> = Vec::new();
    if let Some(db) = db {
        if let Ok(rows) = db.count_fires_by_sentinel().await {
            for (tenant, sentinel, n) in rows {
                fire_counts.insert((tenant, sentinel), n);
            }
        }
        if let Ok(tenants) = db.list_sentinel_tenants().await {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let one_hour_ago = now_ms - 3_600_000;
            let one_day_ago = now_ms - 86_400_000;
            for tenant in tenants {
                let fires_1h = db
                    .count_fires_since(&tenant, one_hour_ago)
                    .await
                    .unwrap_or(0) as u64;
                let fires_24h = db
                    .count_fires_since(&tenant, one_day_ago)
                    .await
                    .unwrap_or(0) as u64;
                let raw = 0.7 * (fires_1h as f64 / 5.0) + 0.3 * (fires_24h as f64 / 24.0);
                let value = raw.clamp(0.0, 1.0);
                let level = if value < 0.15 {
                    "cold"
                } else if value < 0.6 {
                    "warm"
                } else {
                    "hot"
                }
                .to_string();
                temperatures.push(WatchTemperatureStat {
                    tenant,
                    value,
                    level,
                    fires_last_hour: fires_1h,
                    fires_last_24h: fires_24h,
                });
            }
        }
    }

    // Merge tick cells with fire counts; include fire-only sentinels too.
    let mut sentinels: Vec<WatchSentinelStat> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for tick in quarantine.sentinel_tick_snapshot() {
        let key = (tick.tenant.clone(), tick.sentinel.clone());
        let fires_total = fire_counts.get(&key).copied().unwrap_or(0);
        seen.insert(key);
        sentinels.push(WatchSentinelStat {
            tenant: tick.tenant,
            sentinel: tick.sentinel,
            fires_total,
            ticks_fired: tick.ticks_fired,
            ticks_uninteresting: tick.ticks_uninteresting,
            ticks_failure: tick.ticks_failure,
            ticks_gated: tick.ticks_gated,
            last_tick_ms: tick.last_tick_ms,
        });
    }
    for ((tenant, sentinel), fires_total) in fire_counts {
        if seen.contains(&(tenant.clone(), sentinel.clone())) {
            continue;
        }
        sentinels.push(WatchSentinelStat {
            tenant,
            sentinel,
            fires_total,
            ticks_fired: 0,
            ticks_uninteresting: 0,
            ticks_failure: 0,
            ticks_gated: 0,
            last_tick_ms: 0,
        });
    }
    sentinels.sort_by(|a, b| (&a.tenant, &a.sentinel).cmp(&(&b.tenant, &b.sentinel)));

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
        directive_verify_failed_total: crate::watch::dispatcher::directive_verify_failed_total(),
        cap_token_db_error_deny_total: crate::watch::dispatcher::cap_token_db_error_deny_total(),
        action_production_armed: quarantine.action_production_armed(),
        temperatures,
        sentinels,
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
    /// Bounded redacted execute receipts (token id + outcome only; no secrets).
    pub recent_execute_receipts: Vec<UiExecuteReceipt>,
    pub budget: UiWatchBudget,
    pub degradation: UiWatchDegradation,
}

/// Finite lifecycle decision for a redacted execute receipt.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiExecuteDecision {
    Completed,
    Refused,
    Pending,
    Expired,
    Dismissed,
    Bound,
}

/// v1 sole earned-execution action.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
pub enum UiExecuteAction {
    #[serde(rename = "quarantine_producer")]
    QuarantineProducer,
}

/// Redacted Earned Execution receipt for the UI snapshot.
///
/// Exact fields: token_id, decision, action, result, directive_id,
/// in_response_to, at_ms. Never raw capability token, signature, envelope,
/// or last_error detail text.
///
/// `result` is only `Some("acked")` or `Some(ProblemDetails title)`; null
/// for lifecycle-only decisions (pending/expired/dismissed/bound).
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct UiExecuteReceipt {
    pub token_id: String,
    pub decision: UiExecuteDecision,
    pub action: UiExecuteAction,
    /// Only `"acked"` or a ProblemDetails title; null otherwise.
    pub result: Option<String>,
    pub directive_id: String,
    /// Escalation / fire correlation id when the outbox row is still present.
    pub in_response_to: Option<String>,
    pub at_ms: i64,
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
/// Fixed small tail of redacted execute receipts (not full history).
const UI_RECENT_EXECUTE_RECEIPT_LIMIT: i64 = 20;

/// Map durable consumption + optional outbox row into a strict UI receipt.
///
/// Lifecycle stays in `decision`. `result` is only `"acked"` or a
/// ProblemDetails title — never outbox lifecycle strings. Free-form
/// last_error detail is dropped.
pub(crate) fn project_execute_receipt(
    row: crate::watch::db::ExecuteReceiptRow,
) -> UiExecuteReceipt {
    let problem_title = row.last_error.as_deref().and_then(redacted_problem_title);
    let status = row.outbox_status.as_deref();
    let (decision, result) = match (status, problem_title) {
        (Some("acked"), _) => (UiExecuteDecision::Completed, Some("acked".to_string())),
        (Some("expired"), _) => (UiExecuteDecision::Expired, None),
        (Some("dismissed"), _) => (UiExecuteDecision::Dismissed, None),
        (Some("staged"), Some(title)) => (UiExecuteDecision::Refused, Some(title)),
        (Some("staged"), None) => (UiExecuteDecision::Pending, None),
        (Some(_), Some(title)) => (UiExecuteDecision::Refused, Some(title)),
        (Some(_), None) => (UiExecuteDecision::Pending, None),
        (None, Some(title)) => (UiExecuteDecision::Refused, Some(title)),
        (None, None) => (UiExecuteDecision::Bound, None),
    };
    UiExecuteReceipt {
        token_id: row.token_id,
        decision,
        action: UiExecuteAction::QuarantineProducer,
        result,
        directive_id: row.directive_id,
        in_response_to: row.in_response_to,
        at_ms: row.consumed_at_ms,
    }
}

/// Extract ProblemDetails `title` only — never `detail` (may be sensitive).
fn redacted_problem_title(last_error: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(last_error).ok()?;
    let title = value.get("title")?.as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    // Bound length so a hostile row cannot bloat the snapshot.
    let capped: String = title.chars().take(96).collect();
    Some(capped)
}

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

    let recent_execute_receipts = match db
        .list_recent_execute_receipts(&tenant, UI_RECENT_EXECUTE_RECEIPT_LIMIT)
        .await
    {
        Ok(rows) => rows.into_iter().map(project_execute_receipt).collect(),
        Err(error) => {
            tracing::error!(%error, %tenant, "watch UI snapshot execute-receipt read failed");
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
            recent_execute_receipts,
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

#[cfg(test)]
mod project_execute_receipt_tests {
    use super::*;
    use crate::watch::db::ExecuteReceiptRow;

    #[test]
    fn completed_ack_projects_clean_receipt() {
        let receipt = project_execute_receipt(ExecuteReceiptRow {
            token_id: "tok-1".into(),
            directive_id: "dir-1".into(),
            consumed_at_ms: 42,
            outbox_status: Some("acked".into()),
            last_error: None,
            in_response_to: Some("esc-1".into()),
        });
        assert_eq!(receipt.decision, UiExecuteDecision::Completed);
        assert_eq!(receipt.result.as_deref(), Some("acked"));
        assert_eq!(receipt.action, UiExecuteAction::QuarantineProducer);
        assert_eq!(receipt.token_id, "tok-1");
        assert_eq!(receipt.directive_id, "dir-1");
        assert_eq!(receipt.in_response_to.as_deref(), Some("esc-1"));
    }

    #[test]
    fn refused_projects_problem_title_never_detail() {
        let last_error = serde_json::json!({
            "title": "invalid-capability-token",
            "detail": "raw capability_token SECRET_TOKEN signature_b64=abc must not leak",
        })
        .to_string();
        let receipt = project_execute_receipt(ExecuteReceiptRow {
            token_id: "tok-2".into(),
            directive_id: "dir-2".into(),
            consumed_at_ms: 99,
            outbox_status: Some("staged".into()),
            last_error: Some(last_error),
            in_response_to: Some("esc-2".into()),
        });
        assert_eq!(receipt.decision, UiExecuteDecision::Refused);
        assert_eq!(receipt.result.as_deref(), Some("invalid-capability-token"));
        let dumped = serde_json::to_string(&receipt).unwrap();
        assert!(!dumped.contains("SECRET_TOKEN"));
        assert!(!dumped.contains("signature_b64"));
        assert!(!dumped.contains("capability_token"));
    }

    #[test]
    fn freeform_last_error_is_dropped_and_lifecycle_stays_out_of_result() {
        let receipt = project_execute_receipt(ExecuteReceiptRow {
            token_id: "tok-3".into(),
            directive_id: "dir-3".into(),
            consumed_at_ms: 1,
            outbox_status: Some("staged".into()),
            last_error: Some("not-json SECRET_BLOB".into()),
            in_response_to: None,
        });
        assert_eq!(receipt.decision, UiExecuteDecision::Pending);
        assert_eq!(receipt.result, None);
        let dumped = serde_json::to_string(&receipt).unwrap();
        assert!(!dumped.contains("SECRET_BLOB"));
        assert!(!dumped.contains("\"staged\""));
    }

    #[test]
    fn lifecycle_decisions_do_not_duplicate_into_result() {
        for (status, decision) in [
            ("expired", UiExecuteDecision::Expired),
            ("dismissed", UiExecuteDecision::Dismissed),
        ] {
            let receipt = project_execute_receipt(ExecuteReceiptRow {
                token_id: "tok-life".into(),
                directive_id: "dir-life".into(),
                consumed_at_ms: 7,
                outbox_status: Some(status.into()),
                last_error: None,
                in_response_to: Some("esc-life".into()),
            });
            assert_eq!(receipt.decision, decision);
            assert_eq!(receipt.result, None, "lifecycle must not appear in result");
        }
        let bound = project_execute_receipt(ExecuteReceiptRow {
            token_id: "tok-bound".into(),
            directive_id: "dir-bound".into(),
            consumed_at_ms: 8,
            outbox_status: None,
            last_error: None,
            in_response_to: None,
        });
        assert_eq!(bound.decision, UiExecuteDecision::Bound);
        assert_eq!(bound.result, None);
    }
}
