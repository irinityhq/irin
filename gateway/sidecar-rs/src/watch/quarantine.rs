//! Quarantine state machine with monotonic timing, hysteresis, OCC, and probation.
//!
//! The state machine provides:
//! - Monotonic `Instant` values for in-process control (NTP-slew safe).
//! - Hysteresis: a single success does not fully reset state; reset requires
//!   hysteresis_successes (default 3) AND elapsed ≥ 2× current backoff.
//! - OCC inside the fire-write transaction against `watch.db`.
//!
//! ## INVARIANT — `pending_hard_kill_persist` first-set semantics (H1 / P0-2)
//!
//! `pending_hard_kill_persist: Option<Instant>` is **load-bearing safety state**,
//! not a transient cache. The Instant is the first-set timestamp — it must
//! NEVER be restamped on subsequent failures. The age of the oldest pending
//! record is an operator-visible signal (see `pending_oldest_age_ms()` →
//! `gw_watch_pending_oldest_age_ms` gauge); restamping would silently reset
//! that signal every time the retry loop misses, hiding the true age of the
//! failure. The full rule:
//!
//!   - Only `record_failure` may **stamp** `pending_hard_kill_persist`, and
//!     only when the field is currently `None` (first-set; never restamped).
//!   - Only a **successful** DB upsert (in `record_failure`'s Ok arm or
//!     `retry_pending_hard_kill_once`'s Ok arm) or `admin_clear_quarantine`
//!     may **clear** it.
//!   - A retry that ends in `Err` / `TimedOut` MUST leave the Instant
//!     untouched.
//!
//! These transitions are permanent state-machine invariants.

use crate::watch::db::WatchDb;
use crate::watch::runtime::QuarantineGate;
use crate::watch::Escalation;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Per-retry budget on **our await** of
/// one `db.upsert_hard_kill` call.
///
/// IMPORTANT — what this timeout does NOT do: `tokio::time::timeout` cancels
/// only the awaiting Future. `db.upsert_hard_kill(...)` dispatches a closure
/// to the dedicated tokio-rusqlite worker thread via `Connection::call(F)`;
/// when our await is dropped, the closure CONTINUES to run on the worker
/// thread to completion. The Ok/Err result is silently discarded.
///
/// Correctness holds because:
///   (a) `upsert_hard_kill` SQL is idempotent — `INSERT ... ON CONFLICT(tenant,
///       name) DO UPDATE SET hard_killed_at = excluded.hard_killed_at`. A
///       delayed-but-eventually-Ok closure landing on the worker thread, even
///       though our caller reported `TimedOut`, is a no-op on next retry.
///   (b) The tokio-rusqlite worker serializes all `conn.call` invocations
///       FIFO. The next retry's closure will land AFTER the prior one drains.
///   (c) DB-first / memory-mirror invariant — our caller leaves
///       `pending_hard_kill_persist` set when reporting `TimedOut`, so the
///       next retry observes the pending flag and catches up regardless of
///       whether the prior closure later succeeded on the worker. The
///       in-memory mirror lags durable state during the gap; `is_blocked`
///       continues to fail closed via the pending flag.
///
/// The 5s budget primarily bounds how long the CALLER waits, not how long the
/// SQLite execute can take. Worker-thread backpressure is a real risk under a
/// genuine writer wedge (busy_timeout=50ms in db.rs:183 handles normal
/// contention).
pub const RETRY_DB_BUDGET: Duration = Duration::from_secs(5);

/// H1 / P1-1 — `(count, oldest_age_ms)` pair returned by
/// `QuarantineState::pending_snapshot`. Caller wires both into the
/// `gw_watch_pending_pending_records` (gauge) and `gw_watch_pending_oldest_age_ms`
/// (gauge) metrics from a single records-lock walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PendingSnapshot {
    pub count: u64,
    pub oldest_age_ms: u64,
}

/// H1 / P0-1 — result of one `retry_pending_hard_kill_once` attempt. Drives
/// the retry loop's per-tick counter aggregation (P1-3) without leaking
/// `tokio::time::timeout` / DB error types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryOutcome {
    /// DB upsert returned `Ok` — hard_killed_at stamped in-memory, pending
    /// cleared. The healthy terminal state.
    Persisted,
    /// DB upsert returned `Err` — pending preserved, counter incremented,
    /// Instant untouched (P0-2 first-set invariant).
    StillFailing,
    /// 5s timeout elapsed — counter incremented, pending preserved, Instant
    /// untouched. Same operator effect as `StillFailing` plus the timeout
    /// fact is logged at DEBUG.
    TimedOut,
    /// Record was removed entirely between the snapshot and the retry. Under
    /// op_lock this is rare (record_success won't remove pending records);
    /// defensive arm.
    AdminCleared,
    /// `pending_hard_kill_persist` flipped to `None` between the snapshot
    /// and the under-lock re-check — `admin_clear_quarantine` won the race.
    /// No DB call made. No counter bump.
    AdminClearedDuringRetry,
    /// `QuarantineState` has no `WatchDb` wired (in-memory mode). The retry
    /// loop is gated on `has_db()` and should never call this in production;
    /// returned defensively so a future misuse returns a typed result instead
    /// of silently pretending to persist.
    NoDb,
}

#[derive(Debug, Clone)]
pub struct QuarantineConfig {
    pub cooldown: Duration,
    pub fails_to_trigger: u32,          // P0-3: was 3, now 2
    pub backoff_ms_per_cycle: Vec<u64>, // [60_000, 300_000, 1_800_000, 3_600_000]
    pub hard_kill_after_cycles: u32,    // 5
    pub hard_kill_window_ms: u64,       // 3_600_000
    pub hysteresis_successes: u32,      // P0-3b: 3
    pub hysteresis_elapsed_mult: u64,   // P0-3b: ≥ 2× backoff
    pub probation_ms: u64,              // Grok EB: 600_000
}

impl Default for QuarantineConfig {
    fn default() -> Self {
        Self {
            cooldown: Duration::from_secs(5),
            fails_to_trigger: 2,
            backoff_ms_per_cycle: vec![60_000, 300_000, 1_800_000, 3_600_000],
            hard_kill_after_cycles: 5,
            hard_kill_window_ms: 3_600_000,
            hysteresis_successes: 3,
            hysteresis_elapsed_mult: 2,
            probation_ms: 600_000,
        }
    }
}

impl QuarantineConfig {
    pub fn test_with_cooldown(cooldown: Duration) -> Self {
        Self {
            cooldown,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuarantineRecord {
    pub tenant: String,
    pub sentinel: String,
    pub quarantined_until: Instant,
    pub duration_ms: u64,
    pub consecutive_fails: u32,
    pub consecutive_successes: u32,
    pub cycle_count: u32,
    pub cycles_window_start: Option<Instant>,
    pub last_error: Option<String>,
    pub last_quarantine_end: Option<Instant>,
    pub hard_killed_at: Option<Instant>,
    pub probation_until: Option<Instant>,
    /// T33.P1-D — fail-closed "we tried to persist hard-kill but the DB
    /// write failed" flag. Set inside `record_failure` when
    /// `db.upsert_hard_kill` returns `Err`, alongside an increment of
    /// `persist_failures_total`. `is_blocked` treats `Some(_)` as
    /// `HardKilled` (fail-closed) so the safety ladder doesn't silently
    /// lose ground while we wait for the next persist attempt. The
    /// rolling-window reset in `record_failure` is also gated on
    /// `is_none()` so a pending hard-kill can't be papered over by a
    /// cycle-window expiry. Cleared on the next successful persist, on
    /// `record_success`, and on `admin_clear_quarantine`.
    pub pending_hard_kill_persist: Option<Instant>,
}

/// p0a-four-eyes (the dual-custody invariant) — an arming ceremony staged by one
/// principal, awaiting confirmation. Since dual-custody-local-attest B1
/// (spec §4.3, restart-recovery invariant) this in-memory slot is a CACHE of the
/// durable `arm_pending` row: the stage is persisted in the same tx as its
/// audit row, and an unexpired row is rehydrated at boot
/// (`rehydrate_arm_pending`) so a restart mid-ceremony no longer drops the
/// stage. Armed state itself still does NOT survive restart (env-gate
/// unchanged, fail-closed). `stage_id` is a random nonce echoed back at
/// stage time and REQUIRED on confirm (design-review amendment: binds the
/// confirm to the exact stage it intends, so a confirm can never ratify a
/// different/older stage after an overwrite race). `staged_at` is monotonic
/// `Instant` (NTP-slew safe); the wall-clock truth is `arm_pending.exp_at_ms`.
#[derive(Debug, Clone)]
pub struct StagedArm {
    pub stage_id: String,
    pub staged_by: String,
    pub staged_at: Instant,
    pub ttl: Duration,
}

/// T33.P0-A — per-(tenant, sentinel) async serialization lane map. Each
/// key gets one `tokio::sync::Mutex` held across the SQLite await on
/// paths that mutate both DB and memory (`record_failure`,
/// `admin_clear_quarantine`). Aliased so the `QuarantineState` field
/// type stays inside clippy's `type_complexity` budget.
type OpLockMap = HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

pub struct QuarantineState {
    cfg: QuarantineConfig,
    /// (tenant, sentinel) -> record
    records: Mutex<HashMap<(String, String), QuarantineRecord>>,
    /// (tenant, sentinel) -> last fire time (for cooldown)
    last_fires: Mutex<HashMap<(String, String), Instant>>,
    /// T33.P0-A — per-(tenant, sentinel) async serialization lane. Held
    /// across the SQLite await on paths that mutate both DB and memory
    /// (`record_failure`, `admin_clear_quarantine`) so a concurrent caller
    /// on the same key cannot land a DB write between phase-2 and the
    /// phase-3 in-memory mirror. The `records` Mutex (parking_lot, sync)
    /// is still acquired/released within these fns as needed — `op_locks`
    /// is the cross-await lock; `records` is the structural lock.
    ///
    /// Pattern: named-key serialization lane (cf. Kafka partition locks,
    /// council-rs SessionStore). Lock-map pruning is deferred.
    op_locks: Mutex<OpLockMap>,
    /// T33.P1-B — audit-pipeline infrastructure errors. Incremented by the
    /// runner when `fire_pipeline` returns `AuditWriteErr`, `AuditWorkerCrashed`,
    /// or `Timeout("audit")` — the sentinel itself fired correctly but the
    /// audit write or its worker failed. These are operator-attention infra
    /// faults, NOT sentinel-health signals; routing them through `record_failure`
    /// (Patch 4 superseded) would punish a healthy sentinel for downstream
    /// SQLite/runtime trouble. Counter is internal to v0.1; Patch 5 / T36
    /// wires the Prometheus `register_counter!` for `gw_watch_audit_infra_errors_total`
    /// and `gw_watch_persist_failures_total` together (silent-unscrape risk
    /// per council B4).
    audit_infra_errors_total: AtomicU64,
    /// T33.P1-D — count of failed `db.upsert_hard_kill` attempts inside
    /// `record_failure`. Each failure leaves the sentinel in the
    /// fail-closed "pending_hard_kill_persist" limbo (is_blocked returns
    /// `HardKilled`) and increments this counter so the silent-degradation
    /// path the wall-line "action is final" worried about becomes audible
    /// via /watch/stats. Internal AtomicU64; exposed to scrape via the
    /// JSON `/watch/stats` endpoint (Lua poller emits
    /// `gw_watch_persist_failures_total` on /metrics, matching the
    /// council_stats precedent).
    persist_failures_total: AtomicU64,
    /// Count of `db.upsert_hard_kill` retry attempts that ended in `Err` or a
    /// 5s timeout inside `retry_pending_hard_kill_once`. Sibling counter to
    /// `persist_failures_total`: that counts FIRST-fail moments inside
    /// `record_failure`; this counts subsequent retries that also failed.
    /// Exposed as `gw_watch_pending_retry_failures_total` on /metrics via
    /// `/watch/stats`.
    pending_retry_failures_total: AtomicU64,
    /// lease liveness (telemetry invariant / lease-loss path) — count of deliberation
    /// leases lost while a council call was (or may have been) in flight.
    /// Incremented when (a) `renew_deliberation_lease` returns `Lost` mid
    /// deliberation in the dispatcher renewal driver, or (b)
    /// `sweep_phantom_claims_counted` reclaims an expired 'claimed' row that
    /// was a real in-flight claim (non-null claim_token AND attempts > 0).
    /// Each increment is a potential ORPHAN PROVIDER CHARGE recon hint: the
    /// lost holder's council call may have already incurred spend that the
    /// reclaimer's reservation does not cover — p0d's out-of-band recon
    /// cross-checks these against provider invoices. Exposed via JSON
    /// `/watch/stats` as `lease_expired_during_deliberation`; the Lua poller
    /// emits `gw_watch_lease_expired_during_deliberation_total` on /metrics.
    ///
    /// OVERCOUNT CAVEAT : a single
    /// lost lease can be counted twice — the sweep counts the expired
    /// in-flight row, and the still-alive slow holder's next renew also
    /// returns `Lost` and bumps again. Overcounting is the SAFE direction
    /// for an orphan-charge alarm (it can only make recon look harder, never
    /// hide a charge); treat the counter as an upper bound, not an exact
    /// orphan count.
    lease_expired_during_deliberation: AtomicU64,
    /// watch telemetry (telemetry invariant) — idempotency-dedup MISS detector. Bumped
    /// (via `dispatcher::note_settle_report`) when
    /// `store_council_response_and_stage` settled a realized cost for a
    /// (tenant, id) whose row ALREADY carried a realized_cost_usd from a
    /// previous settle. The OCC claim_token fence makes that impossible in
    /// normal operation — any increment is an invariant breach worth paging
    /// on. Exposed via `/watch/stats` as `dup_charge_alarm_total`; Lua emits
    /// `gw_watch_dup_charge_alarm_total`.
    dup_charge_alarm_total: AtomicU64,
    /// watch telemetry (telemetry invariant) — wall-time ms from the disarm signal
    /// (`tx.send(true)` in `admin_disarm_producer_json`) to the drain ack.
    /// Last observed value; 0 = no disarm recorded yet (sub-ms drains round
    /// up to 1 so a real disarm is never confused with "never"). The full
    /// histogram is the Lua poller's job per the council_stats precedent —
    /// the sidecar ships last + max (design risk note: bucket store deferred).
    kill_switch_latency_last_ms: AtomicU64,
    /// watch telemetry — max observed kill-switch drain latency (ms). Pairs
    /// with `kill_switch_latency_last_ms` so a slow historical drain stays
    /// visible between scrapes.
    kill_switch_latency_max_ms: AtomicU64,
    /// watch telemetry (telemetry invariant) — count of out-of-band recon ticks
    /// whose |local settled - external billing| divergence exceeded
    /// RECON_DIVERGENCE_THRESHOLD_USD. Each increment pairs with a
    /// recon_alarm row in watch.db carrying both sides of the comparison.
    /// Exposed via `/watch/stats` as `recon_divergence_total`.
    recon_divergence_total: AtomicU64,
    /// T2 shadow — count of recon TICKS observed with reserved_usd > daily_cap
    /// (orphaned reservation after stale-reclaim; nothing reclaims into
    /// negative). This is a rate gauge: a single persistent breach increments it
    /// every tick (today + yesterday buckets), so it counts ticks-in-breach, NOT
    /// distinct breach episodes — the PAGE dedups per bucket separately (see
    /// `recon::cap_breach_page_edge`). Page-only in shadow mode. Exposed via
    /// `/watch/stats` as `recon_cap_breach_total`.
    recon_cap_breach_total: AtomicU64,
    /// count of settles whose VALID realized cost
    /// exceeded the per-directive reservation ceiling (MAX_FANOUT_COST_USD).
    /// Settle-at-realized stays the truth; this makes the overshoot audible
    /// (input to the p0d alarm path). Exposed via `/watch/stats` as
    /// `settle_ceiling_overshoot_total`.
    settle_ceiling_overshoot_total: AtomicU64,
    /// count of `/watch/stats` assemblies where the
    /// spend_ledger gauge read FAILED and `spend_today_usd` degraded to 0.0.
    /// Distinguishes "genuinely zero spend" from "the gauge is blind" on the
    /// scrape surface. Exposed as `spend_gauge_read_failures_total`.
    spend_gauge_read_failures_total: AtomicU64,
    /// count of kill-switch drains that hit the 5s
    /// timeout (the producer did not ack). Each timeout also records a
    /// 5000ms floor observation into the latency last/max so the scraped
    /// distribution can no longer systematically exclude the worst cases.
    /// Exposed as `kill_switch_drain_timeout_total`.
    kill_switch_drain_timeout_total: AtomicU64,
    /// P1  — count of UNAUTHENTICATED arm
    /// stage/confirm rejections (401: bad/missing principal bearer). Counted
    /// here in a prunable metric INSTEAD of appending a permanent row to the
    /// engine-unprunable `arm_audit` hash chain: an attacker who can reach the
    /// UDS must not be able to grow that integrity-critical, trigger-unprunable
    /// table one row per request with no in-governance remediation (DELETE is
    /// trigger-blocked). Permanent `arm_audit` rows are reserved for
    /// AUTHENTICATED-but-unauthorized events where the principal identity is
    /// real. Exposed via `/watch/stats` as `arm_rejected_unauth_total`.
    arm_rejected_unauth_total: AtomicU64,
    /// Phase 2 §4 — durable watch.db. None in pure in-memory unit tests
    /// (T4 / T5*); Some when wired through main.rs (production) or through
    /// the T21 OCC test. Drives write_fire_row → WatchDb::insert_fire.
    db: Option<Arc<WatchDb>>,
    /// Phase 1 Weld: Atomic runtime kill-switch (Invariant).
    /// Stores the shutdown Sender for the cdc_sweep_loop so it can be dynamically
    /// torn down without a deploy via the admin API.
    pub producer_kill_state: Mutex<
        Option<(
            tokio::sync::watch::Sender<bool>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    /// p0a-four-eyes — the single in-flight arming stage, if any. A new
    /// stage overwrites any prior unexpired stage (re-stage allowed by the
    /// same or a different principal); confirm consumes it. In-memory only
    /// by design (see `StagedArm`). NEVER hold this lock across an await.
    pub arm_staging: Mutex<Option<StagedArm>>,
}

impl QuarantineState {
    pub fn new_in_memory(cfg: QuarantineConfig) -> Self {
        Self {
            cfg,
            records: Mutex::new(HashMap::new()),
            last_fires: Mutex::new(HashMap::new()),
            op_locks: Mutex::new(HashMap::new()),
            audit_infra_errors_total: AtomicU64::new(0),
            persist_failures_total: AtomicU64::new(0),
            pending_retry_failures_total: AtomicU64::new(0),
            lease_expired_during_deliberation: AtomicU64::new(0),
            dup_charge_alarm_total: AtomicU64::new(0),
            kill_switch_latency_last_ms: AtomicU64::new(0),
            kill_switch_latency_max_ms: AtomicU64::new(0),
            recon_divergence_total: AtomicU64::new(0),
            recon_cap_breach_total: AtomicU64::new(0),
            settle_ceiling_overshoot_total: AtomicU64::new(0),
            spend_gauge_read_failures_total: AtomicU64::new(0),
            kill_switch_drain_timeout_total: AtomicU64::new(0),
            arm_rejected_unauth_total: AtomicU64::new(0),
            db: None,
            producer_kill_state: Mutex::new(None),
            arm_staging: Mutex::new(None),
        }
    }

    /// Production constructor — wires the durable watch.db so write_fire_row
    /// flows through insert_fire's BEGIN IMMEDIATE + OCC tx.
    pub fn new_with_db(cfg: QuarantineConfig, db: Arc<WatchDb>) -> Self {
        Self {
            cfg,
            records: Mutex::new(HashMap::new()),
            last_fires: Mutex::new(HashMap::new()),
            op_locks: Mutex::new(HashMap::new()),
            audit_infra_errors_total: AtomicU64::new(0),
            persist_failures_total: AtomicU64::new(0),
            pending_retry_failures_total: AtomicU64::new(0),
            lease_expired_during_deliberation: AtomicU64::new(0),
            dup_charge_alarm_total: AtomicU64::new(0),
            kill_switch_latency_last_ms: AtomicU64::new(0),
            kill_switch_latency_max_ms: AtomicU64::new(0),
            recon_divergence_total: AtomicU64::new(0),
            recon_cap_breach_total: AtomicU64::new(0),
            settle_ceiling_overshoot_total: AtomicU64::new(0),
            spend_gauge_read_failures_total: AtomicU64::new(0),
            kill_switch_drain_timeout_total: AtomicU64::new(0),
            arm_rejected_unauth_total: AtomicU64::new(0),
            db: Some(db),
            producer_kill_state: Mutex::new(None),
            arm_staging: Mutex::new(None),
        }
    }

    /// H1 / P1-5 — true when a durable `WatchDb` is wired (production /
    /// integration-test path). The pending-hard-kill retry loop is spawned
    /// only when this is true: the in-memory-only path treats the hard-kill
    /// transition as immediately persisted (no Err arm to leave pending set),
    /// so the loop would be dead work and emit zero useful telemetry.
    pub fn has_db(&self) -> bool {
        self.db.is_some()
    }

    /// CDC producer surface — returns a clone of the
    /// durable WatchDb handle **only** for spawning the cdc_sweep_loop producer task
    /// when the unified producer gate is armed. This is the minimal surface needed
    /// to deliver a real separate tokio sweep task + boot re-scan (design §2 / plan §5 Step 2).
    ///
    /// Guardrails (non-negotiable):
    /// - Must NOT be used for hot audit paths (fire_pipeline / write_fire_row / insert_fire).
    /// - Must NOT be used to mutate watch_fires schema or columns.
    /// - Caller must respect the producer gate (default OFF; D9 arm only).
    ///
    /// Keep broader database access out of the producer.
    pub(crate) fn db_for_cdc_sweep(&self) -> Option<Arc<WatchDb>> {
        self.db.clone()
    }

    /// p0a-four-eyes — narrowly-scoped durable handle for arming-ceremony
    /// audit writes (`WatchDb::append_arm_audit` / `list_arm_audit`) and,
    /// since dual-custody-local-attest B1, the `arm_pending` persisted-stage
    /// reads/writes that ride the same ceremony surface (`stage_arm_pending`
    /// / `get_arm_pending` / `clear_arm_pending`) ONLY.
    /// Same guardrail shape as `db_for_cdc_sweep`: must not touch hot audit
    /// paths or mutate watch_fires. None only on the in-memory test path —
    /// production always wires the durable db, so the arm ceremony is
    /// always audited there.
    pub(crate) fn db_for_arm_audit(&self) -> Option<Arc<WatchDb>> {
        self.db.clone()
    }

    /// dual-custody-local-attest B1 (spec §4.3, restart-recovery invariant) — boot
    /// rehydration of a persisted, unexpired pending stage into the
    /// in-memory staging slot. The slot is a CACHE of the durable
    /// `arm_pending` row: a sidecar restart mid-ceremony no longer drops the
    /// stage. Expired rows are never rehydrated (wall-clock `exp_at_ms` is
    /// the truth; the rehydrated `StagedArm` carries the REMAINING ttl as a
    /// monotonic-Instant cache of it). DB errors rehydrate nothing —
    /// fail-closed: no stage is strictly safer than a wrong stage, and the
    /// operator simply re-stages. Returns the rehydrated stage_id.
    pub async fn rehydrate_arm_pending(&self) -> Option<String> {
        let db = self.db.clone()?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let row = match db.get_arm_pending(now_ms).await {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "arm_pending rehydrate read failed — no stage rehydrated (fail-closed; re-stage to recover)"
                );
                return None;
            }
        };
        let remaining_ms = (row.exp_at_ms - now_ms).max(0) as u64;
        let stage_id = row.stage_id.clone();
        *self.arm_staging.lock() = Some(StagedArm {
            stage_id: row.stage_id,
            staged_by: row.staged_by,
            staged_at: Instant::now(),
            ttl: Duration::from_millis(remaining_ms),
        });
        tracing::info!(
            stage_id = %stage_id,
            remaining_ms,
            "arm: pending stage rehydrated from arm_pending (crash-resume, spec §4.3)"
        );
        Some(stage_id)
    }

    /// watch telemetry — narrowly-scoped durable handle for the out-of-band
    /// recon loop (`recon::recon_loop` -> `get_daily_settled_council_spend` +
    /// `insert_recon_alarm`) ONLY. Same guardrail shape as `db_for_cdc_sweep`
    /// / `db_for_arm_audit`: must not touch hot audit paths or mutate
    /// watch_fires. None only on the in-memory test path.
    pub(crate) fn db_for_recon(&self) -> Option<Arc<WatchDb>> {
        self.db.clone()
    }

    /// T33.P0-A — return the per-(tenant, sentinel) async serialization lane,
    /// allocating one on first touch. Held across phase-2 SQLite awaits on
    /// paths that mutate (DB, memory) pairs.
    fn op_lock_for(&self, tenant: &str, sentinel: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.op_locks.lock();
        locks
            .entry((tenant.to_string(), sentinel.to_string()))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// T33.P1-B — increment the audit-infra error counter. Called by the
    /// runner for `AuditWriteErr` / `AuditWorkerCrashed` / `Timeout("audit")`
    /// instead of `record_failure` — those outcomes mean the audit pipeline
    /// (SQLite, worker thread) failed, not the sentinel; quarantining a
    /// healthy sentinel for infra trouble is a circuit-breaker fault-domain
    /// mistake (council P1-B). The increment is currently observable only
    /// via `audit_infra_errors_total()` for tests; Prometheus exporter wiring
    /// belongs to Patch 5 / T36 (`register_counter!` for `gw_watch_audit_infra_errors_total`).
    pub fn bump_audit_infra_errors(&self) {
        self.audit_infra_errors_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// T33.P1-B — current value of the audit-infra error counter. Exposed
    /// via JSON `/watch/stats` (T33.P1-D) for the Lua poller to scrape and
    /// emit as `gw_watch_audit_infra_errors_total` on /metrics.
    pub fn audit_infra_errors_total(&self) -> u64 {
        self.audit_infra_errors_total.load(Ordering::Relaxed)
    }

    /// T33.P1-D — current value of the hard-kill persist failure counter.
    /// Exposed via JSON `/watch/stats` for the Lua poller to scrape and
    /// emit as `gw_watch_persist_failures_total` on /metrics (council B4
    /// silent-unscrape pair with `gw_watch_audit_infra_errors_total`).
    pub fn persist_failures_total(&self) -> u64 {
        self.persist_failures_total.load(Ordering::Relaxed)
    }

    /// lease liveness (telemetry invariant) — increment the lost-deliberation-lease
    /// counter. Called by the dispatcher renewal driver on
    /// `RenewOutcome::Lost` and by `sweep_phantom_claims_counted` for each
    /// expired real in-flight claim. Every increment is also an orphan-charge
    /// recon hint (see field doc): pair it with a tracing event carrying the
    /// escalation id so p0d's out-of-band recon can settle the books.
    pub fn bump_lease_expired_during_deliberation(&self) {
        self.lease_expired_during_deliberation
            .fetch_add(1, Ordering::Relaxed);
    }

    /// lease liveness — current value of the lost-deliberation-lease counter.
    /// Exposed via JSON `/watch/stats` (field `lease_expired_during_deliberation`)
    /// for the Lua poller to scrape and emit as
    /// `gw_watch_lease_expired_during_deliberation_total` on /metrics.
    pub fn lease_expired_during_deliberation(&self) -> u64 {
        self.lease_expired_during_deliberation
            .load(Ordering::Relaxed)
    }

    /// watch telemetry (telemetry invariant) — increment the dup-charge alarm. Called by
    /// `dispatcher::note_settle_report` when a settle wrote a realized cost
    /// over a row that already carried one (idempotency-dedup MISS; the OCC
    /// fence should make this impossible — any increment is alarm-worthy).
    pub fn bump_dup_charge_alarm(&self) {
        self.dup_charge_alarm_total.fetch_add(1, Ordering::Relaxed);
    }

    /// watch telemetry — current dup-charge alarm count. Exposed via JSON
    /// `/watch/stats` as `dup_charge_alarm_total` (Lua poller emits
    /// `gw_watch_dup_charge_alarm_total`).
    pub fn dup_charge_alarm_total(&self) -> u64 {
        self.dup_charge_alarm_total.load(Ordering::Relaxed)
    }

    /// watch telemetry (telemetry invariant) — record one kill-switch drain latency
    /// observation (ms). Stores the last value and folds the max. Callers
    /// pass `elapsed_ms.max(1)` so a sub-ms drain is distinguishable from
    /// "no disarm recorded yet" (0).
    pub fn record_kill_switch_latency_ms(&self, ms: u64) {
        self.kill_switch_latency_last_ms
            .store(ms, Ordering::Relaxed);
        self.kill_switch_latency_max_ms
            .fetch_max(ms, Ordering::Relaxed);
    }

    /// watch telemetry — last observed kill-switch drain latency (ms); 0 when
    /// no disarm has been recorded. `/watch/stats` field
    /// `kill_switch_latency_ms`.
    pub fn kill_switch_latency_last_ms(&self) -> u64 {
        self.kill_switch_latency_last_ms.load(Ordering::Relaxed)
    }

    /// watch telemetry — max observed kill-switch drain latency (ms).
    /// `/watch/stats` field `kill_switch_latency_max_ms`.
    pub fn kill_switch_latency_max_ms(&self) -> u64 {
        self.kill_switch_latency_max_ms.load(Ordering::Relaxed)
    }

    /// watch telemetry (telemetry invariant) — increment the recon-divergence
    /// counter. Called by `recon::run_recon_once` alongside the recon_alarm
    /// row write when |local - external| exceeds the threshold.
    pub fn bump_recon_divergence(&self) {
        self.recon_divergence_total.fetch_add(1, Ordering::Relaxed);
    }

    /// watch telemetry — current recon-divergence count. Exposed via JSON
    /// `/watch/stats` as `recon_divergence_total`.
    pub fn recon_divergence_total(&self) -> u64 {
        self.recon_divergence_total.load(Ordering::Relaxed)
    }

    /// T2 shadow — increment the recon cap-breach counter. Sibling to
    /// bump_recon_divergence. Called from recon when reserved_usd > cap.
    pub fn bump_recon_cap_breach(&self) {
        self.recon_cap_breach_total.fetch_add(1, Ordering::Relaxed);
    }

    /// T2 shadow — current recon cap-breach count. Exposed via JSON
    /// `/watch/stats` as `recon_cap_breach_total`.
    pub fn recon_cap_breach_total(&self) -> u64 {
        self.recon_cap_breach_total.load(Ordering::Relaxed)
    }

    /// increment the settle ceiling-overshoot counter.
    /// Called by `dispatcher::note_settle_report` when a settle's valid
    /// realized cost exceeded the per-directive reservation ceiling.
    pub fn bump_settle_ceiling_overshoot(&self) {
        self.settle_ceiling_overshoot_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// current ceiling-overshoot count. `/watch/stats`
    /// field `settle_ceiling_overshoot_total`.
    pub fn settle_ceiling_overshoot_total(&self) -> u64 {
        self.settle_ceiling_overshoot_total.load(Ordering::Relaxed)
    }

    /// increment the spend-gauge read-failure counter.
    /// Called by `build_watch_stats` when the spend_ledger read fails and
    /// `spend_today_usd` degrades to 0.0.
    pub fn bump_spend_gauge_read_failure(&self) {
        self.spend_gauge_read_failures_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// current spend-gauge read-failure count.
    /// `/watch/stats` field `spend_gauge_read_failures_total`.
    pub fn spend_gauge_read_failures_total(&self) -> u64 {
        self.spend_gauge_read_failures_total.load(Ordering::Relaxed)
    }

    /// record one kill-switch drain TIMEOUT: bump the
    /// timeout counter AND fold a floor observation (the 5000ms timeout
    /// bound) into the latency last/max so the scraped distribution includes
    /// the worst cases that feed single-writer invariant's max_loss number.
    pub fn record_kill_switch_drain_timeout(&self, floor_ms: u64) {
        self.kill_switch_drain_timeout_total
            .fetch_add(1, Ordering::Relaxed);
        self.record_kill_switch_latency_ms(floor_ms);
    }

    /// current kill-switch drain-timeout count.
    /// `/watch/stats` field `kill_switch_drain_timeout_total`.
    pub fn kill_switch_drain_timeout_total(&self) -> u64 {
        self.kill_switch_drain_timeout_total.load(Ordering::Relaxed)
    }

    /// P1  — increment the unauthenticated
    /// arm-rejection counter. Called by `admin_arm_stage_json` /
    /// `admin_arm_confirm_json` on the 401 (bad/missing principal bearer) path
    /// in place of a permanent (engine-unprunable) `arm_audit` row, so an
    /// unauthenticated caller cannot grow the ceremony chain.
    pub fn bump_arm_rejected_unauth(&self) {
        self.arm_rejected_unauth_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// P1  — current count of unauthenticated arm
    /// rejections. `/watch/stats` field `arm_rejected_unauth_total`.
    pub fn arm_rejected_unauth_total(&self) -> u64 {
        self.arm_rejected_unauth_total.load(Ordering::Relaxed)
    }

    /// lease liveness — sweep expired phantom claims through the durable
    /// WatchDb AND bump `lease_expired_during_deliberation` by the number of
    /// real in-flight claims among them (non-null claim_token, attempts > 0 —
    /// a dispatcher actually held the row, so its council call may be an
    /// orphan charge). Never-started phantom rows are swept but do NOT count.
    /// Requires the durable DB (production / integration-test wiring).
    pub async fn sweep_phantom_claims_counted(
        &self,
    ) -> anyhow::Result<crate::watch::db::PhantomSweepReport> {
        let db = self.db.as_ref().ok_or_else(|| {
            anyhow::anyhow!("sweep_phantom_claims_counted requires a durable WatchDb")
        })?;
        let report = db.sweep_phantom_claims_report().await?;
        for _ in 0..report.in_flight_expired {
            self.bump_lease_expired_during_deliberation();
        }
        if report.in_flight_expired > 0 {
            tracing::warn!(
                in_flight_expired = report.in_flight_expired,
                swept = report.swept,
                "RECON HINT: swept expired deliberation lease(s) that were in flight — possible orphan provider charge(s); reservation released by sweep, cross-check via out-of-band spend recon (p0d)"
            );
        }
        Ok(report)
    }

    /// Count of records currently parked in
    /// `pending_hard_kill_persist = Some(_)` limbo. Operators can see, via
    /// `gw_watch_pending_pending_records` gauge on /metrics, how many
    /// sentinels are fail-closed-blocked waiting for the next DB persist
    /// to clear them. A non-zero, slowly-rising gauge with
    /// `gw_watch_persist_failures_total` flat is the signature of a
    /// stuck database situation that the retry loop needs to drain.
    ///
    /// Walks the `records` Mutex once per scrape (30s tick in the Lua
    /// poller). Lock-acquire is hot-path concern only at scrape time,
    /// not on request path. Returns `u64` to match the JSON contract
    /// even though it's logically a count.
    pub fn pending_pending_records(&self) -> u64 {
        let recs = self.records.lock();
        recs.values()
            .filter(|r| r.pending_hard_kill_persist.is_some())
            .count() as u64
    }

    /// Age in ms of the oldest pending hard-kill persist Instant. Returns 0
    /// when no records are pending. The
    /// gauge that pairs with `pending_pending_records` (count) — answers the
    /// urgent operator question "how stuck is the worst record" with a single
    /// scrape, without the shape-and-bucket burden of a histogram.
    ///
    /// First-set Instant semantics (see module-doc INVARIANT): the value
    /// reflects the time elapsed since the FIRST failed persist for that
    /// record. A retry that fails again does not restamp, so this gauge
    /// monotonically rises until the record is either persisted (Ok arm
    /// clears) or admin-cleared.
    pub fn pending_oldest_age_ms(&self) -> u64 {
        let recs = self.records.lock();
        recs.values()
            .filter_map(|r| r.pending_hard_kill_persist)
            .map(|t| t.elapsed().as_millis() as u64)
            .max()
            .unwrap_or(0)
    }

    /// H1 / P1-1 — single-lock-walk snapshot returning both `count` and
    /// `oldest_age_ms` in one pass. Cheaper than two separate accessor calls
    /// when both are needed (e.g. the retry-loop per-tick WARN line).
    pub fn pending_snapshot(&self) -> PendingSnapshot {
        let recs = self.records.lock();
        let mut count: u64 = 0;
        let mut oldest_age_ms: u64 = 0;
        for rec in recs.values() {
            if let Some(t) = rec.pending_hard_kill_persist {
                count += 1;
                let age = t.elapsed().as_millis() as u64;
                if age > oldest_age_ms {
                    oldest_age_ms = age;
                }
            }
        }
        PendingSnapshot {
            count,
            oldest_age_ms,
        }
    }

    /// Snapshot of `(tenant, sentinel)`
    /// keys currently in `pending_hard_kill_persist = Some(_)` limbo. Returned
    /// by value so the caller doesn't hold the records mutex across the awaited
    /// DB retry calls (the records mutex is a parking_lot sync Mutex; bridging
    /// it across an await would deadlock with `record_failure` /
    /// `admin_clear_quarantine`).
    ///
    /// The snapshot is intentionally stale-tolerant: between this call and
    /// `retry_pending_hard_kill_once`, an `admin_clear_quarantine` may
    /// clear pending out from under us. That is handled inside
    /// `retry_pending_hard_kill_once` under op_lock + records re-check, which
    /// returns `RetryOutcome::AdminClearedDuringRetry` when the pending flag
    /// is no longer set.
    pub fn pending_hard_kill_keys(&self) -> Vec<(String, String)> {
        let recs = self.records.lock();
        recs.iter()
            .filter(|(_, r)| r.pending_hard_kill_persist.is_some())
            .map(|((t, s), _)| (t.clone(), s.clone()))
            .collect()
    }

    /// Current value of the pending-retry failure counter. Exposed via
    /// `/watch/stats` JSON and emitted as `gw_watch_pending_retry_failures_total`
    /// on /metrics by the Lua poller.
    pub fn pending_retry_failures_total(&self) -> u64 {
        self.pending_retry_failures_total.load(Ordering::Relaxed)
    }

    /// Single retry attempt for one
    /// `(tenant, sentinel)` key. The retry loop calls this per key per tick.
    ///
    /// Serialization: holds the per-key `op_lock` for the full attempt so
    /// admin clear and `record_failure` cannot interleave. This preserves the
    /// H3 TOCTOU bound proved on `fire_pipeline` and the same DB-first /
    /// memory-mirror discipline that `record_failure` uses on its Err path.
    ///
    /// Stale-snapshot handling: under op_lock, re-checks `pending_hard_kill_persist`
    /// before calling `db.upsert_hard_kill`. If admin already cleared it, returns
    /// `AdminClearedDuringRetry` without touching the DB — admin wins the race.
    ///
    /// DB call timeout (P0-3): wraps `upsert_hard_kill` in a 5s
    /// `tokio::time::timeout`. Timeout outcome is `TimedOut`, treated as
    /// `StillFailing` for the counter (still leaves pending set; the
    /// load-bearing safety state is preserved).
    ///
    /// Instant semantics (P0-2): on `Err` / `TimedOut`, the Instant in
    /// `pending_hard_kill_persist` is **left untouched**. First-set semantics
    /// are load-bearing for the `pending_oldest_age_ms` gauge.
    pub async fn retry_pending_hard_kill_once(&self, tenant: &str, sentinel: &str) -> RetryOutcome {
        // P1-5 in spirit: callers should gate via `has_db()` before spawning,
        // but defend the contract here so a misuse returns `NoDb` instead of
        // a silent Ok pretending to persist.
        let Some(db) = self.db.as_ref() else {
            return RetryOutcome::NoDb;
        };

        let op_lock = self.op_lock_for(tenant, sentinel);
        let _op_guard = op_lock.lock().await;

        // Stale-snapshot re-check under op_lock — admin clear holds the same
        // op_lock for its DB+memory span, so once we have the lock either
        // the admin clear has fully committed (and our pending is None) or it
        // has not yet started (and our pending is still Some).
        let still_pending = {
            let recs = self.records.lock();
            let key = (tenant.to_string(), sentinel.to_string());
            match recs.get(&key) {
                Some(rec) => rec.pending_hard_kill_persist.is_some(),
                None => false,
            }
        };
        if !still_pending {
            return RetryOutcome::AdminClearedDuringRetry;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // NB: `tokio::time::timeout` bounds OUR await only. tokio-rusqlite
        // dispatches the closure to a dedicated worker thread; dropping our
        // Future does NOT cancel that closure (see `RETRY_DB_BUDGET` doc).
        // Correctness across the timeout boundary relies on idempotent SQL +
        // single-connection FIFO + DB-first / memory-mirror.
        let result = tokio::time::timeout(
            RETRY_DB_BUDGET,
            db.upsert_hard_kill(
                tenant,
                sentinel,
                now_ms,
                "h1: retry pending hard-kill persist",
            ),
        )
        .await;

        match result {
            Err(_elapsed) => {
                // P0-3 timeout → counter + StillFailing. P0-2: do NOT restamp
                // the Instant. Leave pending exactly as record_failure first
                // set it. The worker-thread closure may still complete; the
                // next retry tolerates a delayed-Ok via idempotent SQL.
                self.pending_retry_failures_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    tenant = tenant,
                    sentinel = sentinel,
                    "watch::pending_retry: upsert_hard_kill exceeded 5s budget (TimedOut)"
                );
                RetryOutcome::TimedOut
            }
            Ok(Err(e)) => {
                self.pending_retry_failures_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    tenant = tenant,
                    sentinel = sentinel,
                    error = %e,
                    "watch::pending_retry: upsert_hard_kill failed; pending preserved"
                );
                RetryOutcome::StillFailing
            }
            Ok(Ok(())) => {
                // DB persisted; mirror into memory and clear pending. Under
                // op_lock so admin clear cannot interleave. DB-first /
                // memory-mirror discipline matches `record_failure`'s Ok arm.
                let mut recs = self.records.lock();
                let key = (tenant.to_string(), sentinel.to_string());
                if let Some(rec) = recs.get_mut(&key) {
                    rec.hard_killed_at = Some(Instant::now());
                    rec.pending_hard_kill_persist = None;
                    RetryOutcome::Persisted
                } else {
                    // Record vanished between our pending re-check and now —
                    // under op_lock this can only happen via record_success
                    // hysteresis remove, which is gated on pending.is_none(),
                    // so it cannot happen mid-retry. Defensive arm.
                    RetryOutcome::AdminCleared
                }
            }
        }
    }

    /// Used by tests that don't care about cfg.
    pub fn test_default() -> Self {
        Self::new_in_memory(QuarantineConfig::default())
    }

    /// T33.7 P1-5 — read active probation windows from `watch_sentinels` and
    /// seed `records` so `is_blocked()` returns `ProbationLogOnly` and
    /// `fire_pipeline` applies the `[PROBATION] ` reason prefix on scheduled
    /// fires during the residual window. Called once at boot from `main.rs`
    /// after the registry upsert loop.
    ///
    /// Translates wall-clock `probation_until_ms` (Unix ms, NTP-slewable)
    /// into a monotonic `Instant` via `Instant::now() + remaining_ms`. The
    /// in-memory deadline is therefore a snapshot of "remaining window at
    /// boot time"; if the wall clock jumps forward post-boot, the in-memory
    /// deadline doesn't move — fires will keep getting `[PROBATION]` until
    /// the monotonic deadline expires. A wall-clock jump backward similarly
    /// doesn't extend the window. Both behaviors are correct (probation is
    /// a soft window, not a hard contract).
    ///
    /// In-memory test path (db = None) returns Ok(0) without touching state.
    /// Existing in-memory records take precedence — the entry::or_insert_with
    /// guard means a record produced earlier (e.g., from record_failure
    /// during the same boot) is not clobbered. The single legitimate caller
    /// is `main.rs` and it runs this exactly once before the runner spawns.
    pub async fn hydrate_probation_from_db(&self) -> anyhow::Result<usize> {
        let Some(db) = &self.db else {
            return Ok(0);
        };
        let rows = db.list_active_probation().await?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let now = Instant::now();
        let mut recs = self.records.lock();
        let mut hydrated = 0;
        for (tenant, name, probation_until_ms) in rows {
            let remaining_ms = (probation_until_ms - now_ms).max(0) as u64;
            if remaining_ms == 0 {
                continue;
            }
            let key = (tenant.clone(), name.clone());
            let mut inserted = false;
            recs.entry(key).or_insert_with(|| {
                inserted = true;
                QuarantineRecord {
                    tenant,
                    sentinel: name,
                    quarantined_until: now,
                    duration_ms: 0,
                    consecutive_fails: 0,
                    consecutive_successes: 0,
                    cycle_count: 0,
                    cycles_window_start: None,
                    last_error: None,
                    last_quarantine_end: None,
                    hard_killed_at: None,
                    probation_until: Some(now + Duration::from_millis(remaining_ms)),
                    pending_hard_kill_persist: None,
                }
            });
            if inserted {
                hydrated += 1;
            }
        }
        Ok(hydrated)
    }

    /// T33.P0-B (review) — mirror durable hard-kills from
    /// `watch_sentinels.hard_killed_at` into the in-memory records on boot.
    /// Without this, `is_blocked` returns `None` post-restart for known-bad
    /// sentinels; `runner_loop` drives observe/interesting/escalate and only
    /// the OCC in `insert_fire` rejects the write. Hydrating the hard-kill
    /// state keeps the gate and OCC layers in agreement.
    ///
    /// Fail-closed semantics at the caller: hard-kill is the safety rail
    /// ("Action is final"); a hydrate failure on boot must block runner
    /// spawn (`main` returns Err, process exits 1). Probation hydrate stays
    /// log-and-continue — that's the bifurcated hydration policy from the
    /// the invariant (durability invariant).
    ///
    /// In-memory test path (db = None) returns Ok(0) without touching state.
    /// Existing in-memory records are not clobbered (entry::or_insert_with).
    pub async fn hydrate_hard_kill_from_db(&self) -> anyhow::Result<usize> {
        let Some(db) = &self.db else {
            return Ok(0);
        };
        let rows = db.list_active_hard_killed().await?;
        let now = Instant::now();
        let mut recs = self.records.lock();
        let mut hydrated = 0;
        // The DB's i64 `hard_killed_at_ms` is intentionally discarded: the
        // in-memory record uses `Option<Instant>` (monotonic cleave, durability invariant);
        // `is_blocked` only checks `.is_some()`. Canonical Unix-ms lives in
        // watch.db; hydrate restores the flag, not the timestamp.
        for (tenant, name, _hard_killed_at_ms) in rows {
            let key = (tenant.clone(), name.clone());
            let mut inserted = false;
            recs.entry(key).or_insert_with(|| {
                inserted = true;
                QuarantineRecord {
                    tenant,
                    sentinel: name,
                    quarantined_until: now,
                    duration_ms: 0,
                    consecutive_fails: 0,
                    consecutive_successes: 0,
                    cycle_count: 0,
                    cycles_window_start: None,
                    last_error: None,
                    last_quarantine_end: None,
                    hard_killed_at: Some(now),
                    probation_until: None,
                    pending_hard_kill_persist: None,
                }
            });
            if inserted {
                hydrated += 1;
            }
        }
        Ok(hydrated)
    }

    pub async fn note_fire(&self, tenant: &str, sentinel: &str) {
        self.last_fires
            .lock()
            .insert((tenant.to_string(), sentinel.to_string()), Instant::now());
    }

    pub async fn in_cooldown(&self, tenant: &str, sentinel: &str, elapsed: Duration) -> bool {
        let last = self
            .last_fires
            .lock()
            .get(&(tenant.to_string(), sentinel.to_string()))
            .copied();
        match last {
            None => false,
            Some(t) => elapsed < self.cfg.cooldown && t.elapsed() < self.cfg.cooldown,
        }
    }

    pub async fn record_failure(&self, tenant: &str, sentinel: &str) {
        // T33.P0-A — hold the per-(tenant, sentinel) op_lock across the
        // SQLite await so admin_clear_quarantine on the same key cannot
        // land between phase-2's DB upsert and phase-3's in-memory mirror.
        // Drop pattern: the lock is released only when this future returns,
        // so the (DB, memory) pair is atomic against admin_clear on the
        // same key. The runner_loop already serializes record_failure on
        // any single sentinel; this lock guards admin-vs-runtime concurrency.
        let op_lock = self.op_lock_for(tenant, sentinel);
        let _op_guard = op_lock.lock().await;

        // Phase 1 — mutate in-memory state under the lock. The hard-kill
        // transition is decided here but the `rec.hard_killed_at` write is
        // deferred until phase 2's DB persistence succeeds (T33.6 P0-2):
        // if we set it in-memory before the DB write, a crash between the
        // two leaves the OCC silent — fire_pipeline thinks the sentinel is
        // hard-killed but the OCC in insert_fire would accept fires.
        // DB-first then memory mirrors the T32 admin-clear pattern.
        let needs_db_hard_kill_persist = {
            let mut recs = self.records.lock();
            let key = (tenant.to_string(), sentinel.to_string());
            let rec = recs.entry(key.clone()).or_insert_with(|| QuarantineRecord {
                tenant: tenant.into(),
                sentinel: sentinel.into(),
                quarantined_until: Instant::now(),
                duration_ms: 0,
                consecutive_fails: 0,
                consecutive_successes: 0,
                cycle_count: 0,
                cycles_window_start: None,
                last_error: None,
                last_quarantine_end: None,
                hard_killed_at: None,
                probation_until: None,
                pending_hard_kill_persist: None,
            });

            rec.consecutive_fails += 1;
            rec.consecutive_successes = 0;

            let mut hard_kill_triggered = false;
            if rec.consecutive_fails >= self.cfg.fails_to_trigger {
                // Trigger or extend quarantine. Advance cycle.
                let new_cycle = if rec.duration_ms == 0 {
                    0
                } else {
                    rec.cycle_count + 1
                };
                let cap_idx =
                    std::cmp::min(new_cycle as usize, self.cfg.backoff_ms_per_cycle.len() - 1);
                let duration_ms = self.cfg.backoff_ms_per_cycle[cap_idx];

                rec.cycle_count = new_cycle;
                rec.duration_ms = duration_ms;
                rec.quarantined_until = Instant::now() + Duration::from_millis(duration_ms);
                rec.consecutive_fails = 0;
                rec.cycles_window_start = rec.cycles_window_start.or(Some(Instant::now()));

                // Hard-kill check: 5 cycles within 1h window?
                if let Some(window_start) = rec.cycles_window_start {
                    let in_window =
                        window_start.elapsed().as_millis() as u64 <= self.cfg.hard_kill_window_ms;
                    if in_window && new_cycle + 1 >= self.cfg.hard_kill_after_cycles {
                        // Decision recorded; in-memory write deferred until
                        // phase 3 after DB persistence (or immediate for the
                        // in-memory-only test path with db = None).
                        hard_kill_triggered = true;
                    }
                    // T33.P1-D — only reset the rolling window if there is
                    // no pending hard-kill persist. Resetting under a
                    // pending persist would paper over the safety-ladder
                    // gap: cycle_count → 0 effectively hides the prior
                    // 5-in-window from the next hard-kill check, even
                    // though the DB never recorded the previous hard-kill
                    // due to upsert failure. Fail-closed.
                    if !in_window && rec.pending_hard_kill_persist.is_none() {
                        rec.cycles_window_start = Some(Instant::now());
                        rec.cycle_count = 0;
                        rec.duration_ms = self.cfg.backoff_ms_per_cycle[0];
                        rec.quarantined_until =
                            Instant::now() + Duration::from_millis(rec.duration_ms);
                    }
                }
            }
            hard_kill_triggered
        };

        if !needs_db_hard_kill_persist {
            return;
        }

        // Phase 2 — DB persistence. If `db` is None (in-memory test path)
        // we skip the await and treat the transition as persisted: legacy
        // unit tests in tests/watch_quarantine.rs that use
        // `new_in_memory(...)` expect `rec.hard_killed_at` to be set after
        // the 5th cycle without any DB plumbing. If `db` is Some and the
        // write fails: bump `persist_failures_total` (T33.P1-D, exposed
        // via /watch/stats → gw_watch_persist_failures_total), warn, and
        // signal phase 3 to set `pending_hard_kill_persist` so the safety
        // ladder fails closed via `is_blocked` until the next successful
        // persist.
        let persisted = if let Some(db) = &self.db {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            match db
                .upsert_hard_kill(
                    tenant,
                    sentinel,
                    now_ms,
                    "runtime: hard-kill at 5 cycles within 1h window",
                )
                .await
            {
                Ok(()) => true,
                Err(e) => {
                    self.persist_failures_total.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        tenant = tenant,
                        sentinel = sentinel,
                        error = %e,
                        "watch::quarantine: hard-kill DB persist failed; setting pending_hard_kill_persist so is_blocked fails closed until next successful persist (T33.P1-D)"
                    );
                    false
                }
            }
        } else {
            true
        };

        // Phase 3 — mirror the durable outcome into memory.
        //   persisted == true  → DB has the hard-kill (or no DB at all);
        //                        set `hard_killed_at`, clear any stale
        //                        `pending_hard_kill_persist` from a
        //                        previous-cycle failure that this call
        //                        has now recovered.
        //   persisted == false → DB write failed; first-set
        //                        `pending_hard_kill_persist` to fail
        //                        closed via `is_blocked` until the next
        //                        persist attempt succeeds. Leave
        //                        `hard_killed_at` None so the DB-first
        //                        invariant holds. **Never restamp** an
        //                        existing Instant (H1 / P0-2 first-set
        //                        invariant — see module-doc); restamping
        //                        would silently reset the
        //                        `gw_watch_pending_oldest_age_ms` gauge
        //                        every time the retry loop misses, hiding
        //                        the true age of the failure.
        // Concurrent record_failure on the same sentinel cannot race with
        // this: runner_loop serializes per sentinel and op_lock guards
        // admin-vs-runtime concurrency.
        {
            let mut recs = self.records.lock();
            let key = (tenant.to_string(), sentinel.to_string());
            if let Some(rec) = recs.get_mut(&key) {
                if persisted {
                    rec.hard_killed_at = Some(Instant::now());
                    rec.pending_hard_kill_persist = None;
                } else if rec.pending_hard_kill_persist.is_none() {
                    rec.pending_hard_kill_persist = Some(Instant::now());
                }
            }
        }
    }

    pub async fn get_state(&self, tenant: &str, sentinel: &str) -> Option<QuarantineRecord> {
        self.records
            .lock()
            .get(&(tenant.to_string(), sentinel.to_string()))
            .cloned()
    }

    /// Test-only: simulate clock advance past current quarantine duration.
    /// Sets `last_quarantine_end` so the hysteresis path can engage.
    pub async fn test_advance_past_quarantine(&self, tenant: &str, sentinel: &str) {
        let mut recs = self.records.lock();
        if let Some(rec) = recs.get_mut(&(tenant.to_string(), sentinel.to_string())) {
            rec.last_quarantine_end = Some(Instant::now());
            rec.quarantined_until = Instant::now() - Duration::from_secs(1);
        }
    }

    /// Phase 2 §9.2: full reset only if N successes AND ≥ N× backoff elapsed.
    ///
    /// T33.P0.1 (review): `record_success` MUST NOT clear
    /// `pending_hard_kill_persist`, and MUST NOT remove the record while
    /// pending is set. Pending is the in-process fail-closed flag for "we
    /// crossed the hard-kill threshold but the DB upsert failed" — clearing
    /// it on a successful tick (e.g. `FireOutcome::Uninteresting` routed
    /// through `handle_fire_outcome`) silently drops the safety ladder
    /// without any DB persist ever succeeding. Pending is only cleared by
    /// (a) a successful DB upsert (record_failure's Ok arm), or (b)
    /// `admin_clear_quarantine`. Removing the record under pending would
    /// have the same effect since `is_blocked` returns None for absent
    /// records.
    pub async fn record_success(&self, tenant: &str, sentinel: &str) {
        let mut recs = self.records.lock();
        let key = (tenant.to_string(), sentinel.to_string());
        let should_remove = {
            let Some(rec) = recs.get_mut(&key) else {
                return;
            };
            rec.consecutive_successes += 1;
            if rec.consecutive_fails > 0 {
                rec.consecutive_fails -= 1;
            }

            let elapsed_ok = match rec.last_quarantine_end {
                None => false,
                Some(t) => {
                    t.elapsed().as_millis() as u64
                        >= self.cfg.hysteresis_elapsed_mult * rec.duration_ms
                }
            };
            rec.consecutive_successes >= self.cfg.hysteresis_successes
                && elapsed_ok
                && rec.pending_hard_kill_persist.is_none()
        };
        if should_remove {
            recs.remove(&key);
        }
    }

    /// Pipeline gate check — called from runtime::fire_pipeline step 3.
    ///
    /// T33.P1-A: on natural expiry, stamp `last_quarantine_end` once so
    /// `record_success`'s hysteresis gate (`elapsed >= 2× duration_ms`)
    /// can engage. Without this stamp, a sentinel whose quarantine window
    /// elapses without an admin clear stays orphaned forever — three
    /// successes do nothing because `last_quarantine_end.is_none()`
    /// short-circuits `elapsed_ok` to false. Guarded on `duration_ms > 0`
    /// so fresh records (consecutive_fails=1, pre-trigger) don't get
    /// stamped as "recovering" — they were never quarantined.
    ///
    /// T33.P1-D: `pending_hard_kill_persist.is_some()` is treated as
    /// `HardKilled` — fail-closed safety ladder. A pending-but-not-yet-
    /// persisted hard-kill means we crossed the threshold but the DB
    /// write failed; until the next persist succeeds, we MUST keep the
    /// sentinel blocked. Otherwise the gap between threshold and
    /// successful persist silently lets fires through, breaking the
    /// wall-line guarantee that "action is final".
    pub async fn is_blocked(&self, tenant: &str, sentinel: &str) -> Option<QuarantineGate> {
        let mut recs = self.records.lock();
        if let Some(rec) = recs.get_mut(&(tenant.to_string(), sentinel.to_string())) {
            if rec.hard_killed_at.is_some() || rec.pending_hard_kill_persist.is_some() {
                return Some(QuarantineGate::HardKilled);
            }
            if let Some(p) = rec.probation_until {
                if p > Instant::now() {
                    return Some(QuarantineGate::ProbationLogOnly);
                }
            }
            let now = Instant::now();
            if rec.quarantined_until > now {
                return Some(QuarantineGate::Quarantined);
            }
            // Natural expiry — stamp once.
            if rec.last_quarantine_end.is_none() && rec.duration_ms > 0 {
                rec.last_quarantine_end = Some(now);
            }
        }
        None
    }

    /// Audit row writer. When `db` is wired (production path), delegates
    /// to `WatchDb::insert_fire` which holds BEGIN IMMEDIATE + OCC against
    /// `watch_sentinels.hard_killed_at`. When `db` is None (legacy
    /// in-memory tests), returns Ok(0).
    ///
    /// Return contract:
    ///   Ok(id)               — row inserted at rowid `id`.
    ///   Err("hard_killed_race") — OCC detected hard-kill set on the
    ///                              sentinel between is_blocked and insert.
    ///                              The fire is silently dropped on disk;
    ///                              the runtime maps this to a distinct outcome.
    ///   Err(other)           — SQLite or serialization failure.
    pub async fn write_fire_row(&self, esc: Escalation) -> Result<i64, String> {
        let Some(db) = &self.db else {
            return Ok(0); // tests-only in-memory path.
        };
        let envelope_json = serde_json::to_string(&esc).map_err(|e| e.to_string())?;
        let state_json = serde_json::to_string(&esc.state).map_err(|e| e.to_string())?;
        match db
            .insert_fire(
                &esc.state.tenant,
                &esc.state.sentinel,
                esc.state.observed_at,
                &state_json,
                &esc.reason,
                &envelope_json,
                1,
            )
            .await
        {
            Ok(Some(id)) => Ok(id),
            Ok(None) => Err("hard_killed_race".to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Test-only hook for the `/watch/stats` HTTP test (T33.P1-D). Bumps
    /// `persist_failures_total` without driving a real DB failure. Production
    /// callers MUST go through `record_failure`'s Err arm so the pending
    /// state machine flag is also set; this helper is intentionally bypass.
    #[doc(hidden)]
    pub fn test_bump_persist_failures(&self) {
        self.persist_failures_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Test-only — expose the per-key `op_lock` so tests can hold the
    /// serialization lane externally and verify other paths (e.g. retry)
    /// block on it. Required by the H1 P0-6 deterministic concurrent
    /// contention test that proves `retry_pending_hard_kill_once` actually
    /// uses the lock — without this accessor, "race" tests would only
    /// exercise the under-lock pending re-check, not the lock itself.
    #[doc(hidden)]
    pub fn test_op_lock_for(&self, tenant: &str, sentinel: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.op_lock_for(tenant, sentinel)
    }

    /// Test-only — clear `pending_hard_kill_persist` on the in-memory record
    /// without going through `admin_clear_quarantine` (which would itself
    /// take the op_lock, deadlocking a test that already holds it). Used
    /// inside the H1 P0-6 contention test to simulate the memory-mirror
    /// phase of admin clear under an externally-held op_lock.
    #[doc(hidden)]
    pub fn test_clear_pending(&self, tenant: &str, sentinel: &str) {
        let mut recs = self.records.lock();
        if let Some(rec) = recs.get_mut(&(tenant.to_string(), sentinel.to_string())) {
            rec.pending_hard_kill_persist = None;
            rec.hard_killed_at = None;
        }
    }

    /// Test-only hook for hysteresis tests. Forces `last_quarantine_end` into
    /// the past so the 2× backoff elapsed check engages. `#[doc(hidden)]` +
    /// `pub` because integration tests (in `tests/`) live outside the crate
    /// and can't see `#[cfg(test)]` items.
    #[doc(hidden)]
    pub async fn test_set_last_quarantine_end(&self, tenant: &str, sentinel: &str, when: Instant) {
        let mut recs = self.records.lock();
        if let Some(rec) = recs.get_mut(&(tenant.to_string(), sentinel.to_string())) {
            rec.last_quarantine_end = Some(when);
        }
    }

    /// Test-only: stamp `probation_until` directly on the in-memory record,
    /// inserting a healthy-shape record if one doesn't exist yet. Used by
    /// T30 probation-path tests to exercise `force_wake_pipeline`'s
    /// [PROBATION] reason-prefix path without driving the full
    /// hard-kill → admin-clear cycle.
    #[doc(hidden)]
    pub async fn test_set_probation_until(&self, tenant: &str, sentinel: &str, when: Instant) {
        let mut recs = self.records.lock();
        let key = (tenant.to_string(), sentinel.to_string());
        let rec = recs.entry(key).or_insert_with(|| QuarantineRecord {
            tenant: tenant.into(),
            sentinel: sentinel.into(),
            quarantined_until: Instant::now(),
            duration_ms: 0,
            consecutive_fails: 0,
            consecutive_successes: 0,
            cycle_count: 0,
            cycles_window_start: None,
            last_error: None,
            last_quarantine_end: None,
            hard_killed_at: None,
            probation_until: None,
            pending_hard_kill_persist: None,
        });
        rec.probation_until = Some(when);
    }

    /// Test-only: stamp `hard_killed_at` directly, inserting a healthy-shape
    /// record if absent. Used by T33.P0.1 pre-observe gate tests to assert
    /// fire_pipeline returns `Gated(HardKilled)` without calling observe()
    /// or interesting().
    #[doc(hidden)]
    pub async fn test_set_hard_killed_at(&self, tenant: &str, sentinel: &str, when: Instant) {
        let mut recs = self.records.lock();
        let key = (tenant.to_string(), sentinel.to_string());
        let rec = recs.entry(key).or_insert_with(|| QuarantineRecord {
            tenant: tenant.into(),
            sentinel: sentinel.into(),
            quarantined_until: Instant::now(),
            duration_ms: 0,
            consecutive_fails: 0,
            consecutive_successes: 0,
            cycle_count: 0,
            cycles_window_start: None,
            last_error: None,
            last_quarantine_end: None,
            hard_killed_at: None,
            probation_until: None,
            pending_hard_kill_persist: None,
        });
        rec.hard_killed_at = Some(when);
    }

    /// Test-only: stamp `pending_hard_kill_persist` directly, inserting a
    /// healthy-shape record if absent. Used by T33.P0.1 pre-observe gate
    /// tests + the "Uninteresting must not clear pending" invariant test.
    #[doc(hidden)]
    pub async fn test_set_pending_hard_kill_persist(
        &self,
        tenant: &str,
        sentinel: &str,
        when: Instant,
    ) {
        let mut recs = self.records.lock();
        let key = (tenant.to_string(), sentinel.to_string());
        let rec = recs.entry(key).or_insert_with(|| QuarantineRecord {
            tenant: tenant.into(),
            sentinel: sentinel.into(),
            quarantined_until: Instant::now(),
            duration_ms: 0,
            consecutive_fails: 0,
            consecutive_successes: 0,
            cycle_count: 0,
            cycles_window_start: None,
            last_error: None,
            last_quarantine_end: None,
            hard_killed_at: None,
            probation_until: None,
            pending_hard_kill_persist: None,
        });
        rec.pending_hard_kill_persist = Some(when);
    }

    /// Test-only: stamp `quarantined_until` directly, inserting a fresh
    /// record if absent. Used by T33.P0.1 pre-observe gate tests to assert
    /// fire_pipeline returns `Gated(Quarantined)` without driving the
    /// natural failure → trigger path.
    #[doc(hidden)]
    pub async fn test_set_quarantined_until(&self, tenant: &str, sentinel: &str, when: Instant) {
        let mut recs = self.records.lock();
        let key = (tenant.to_string(), sentinel.to_string());
        let rec = recs.entry(key).or_insert_with(|| QuarantineRecord {
            tenant: tenant.into(),
            sentinel: sentinel.into(),
            quarantined_until: Instant::now(),
            duration_ms: 0,
            consecutive_fails: 0,
            consecutive_successes: 0,
            cycle_count: 0,
            cycles_window_start: None,
            last_error: None,
            last_quarantine_end: None,
            hard_killed_at: None,
            probation_until: None,
            pending_hard_kill_persist: None,
        });
        rec.quarantined_until = when;
    }

    /// T32 admin path — clear quarantine + hard-kill (and optionally
    /// probation) for a sentinel. Inspects the in-memory record BEFORE
    /// mutation to populate `cleared` with the labels of states that were
    /// actually active; writes the durable `watch_sentinels` row FIRST
    /// (clears `hard_killed_at`, sets/clears `probation_until`), then
    /// mutates in-memory state under the same lock. If the DB write fails,
    /// in-memory state is NOT touched — DB is authoritative for hard_kill
    /// (insert_fire's OCC reads it) and a partial mutation would leave the
    /// sentinel in a split-brain "unblocked in memory, still hard-killed on
    /// disk" state that silently fails every fire.
    ///
    /// `skip_probation = true` (admin sets `reset_probation: true`) means
    /// the sentinel re-arms immediately; `probation_until` is cleared on
    /// both sides. `skip_probation = false` (default) means a hard-kill
    /// recovery enters the 10-min log-only window per spec §9.2.
    ///
    /// CONCURRENCY: this path is serialized on the per-(tenant, sentinel)
    /// `op_lock` (T33.P0-A). The op_lock is held for the full phase1-3 span,
    /// so neither a concurrent admin clear NOR a concurrent record_failure
    /// on the same key can interleave between the DB write and the in-memory
    /// mirror — the (DB, memory) pair is atomic on a per-key basis. The
    /// `records` mutex is still acquired/released within phases as needed
    /// (it's the structural lock); `op_lock` is the cross-await lock.
    pub async fn admin_clear_quarantine(
        &self,
        tenant: &str,
        sentinel: &str,
        skip_probation: bool,
    ) -> anyhow::Result<ClearOutcome> {
        // T33.P0-A — hold the per-(tenant, sentinel) op_lock across the
        // SQLite await. Without this, a concurrent record_failure at the
        // hard-kill threshold can land a DB upsert + memory mirror that
        // either overwrites our clear (DB ends hard-killed, memory cleared)
        // or our memory clear happens before its memory mirror (memory
        // ends hard-killed, DB cleared) — both observed under load.
        // This lock prevents the database and in-memory state from diverging.
        let op_lock = self.op_lock_for(tenant, sentinel);
        let _op_guard = op_lock.lock().await;

        // Phase 1 — inspect in-memory under lock. We capture per-cycle
        // quarantine state and in-memory hard-kill (which may differ from
        // the DB column if record was just hard-killed and hasn't been
        // mirrored, or absent post-restart). Lock is held only across the
        // inspect; the await happens after.
        let (in_mem_quarantine_cleared, in_mem_hard_kill, in_mem_probation_cleared) = {
            let recs = self.records.lock();
            let key = (tenant.to_string(), sentinel.to_string());
            match recs.get(&key) {
                None => (false, false, false),
                Some(rec) => {
                    let now = Instant::now();
                    (
                        rec.quarantined_until > now,
                        rec.hard_killed_at.is_some(),
                        skip_probation && rec.probation_until.is_some(),
                    )
                }
            }
        };

        // Phase 2 — durable atomic read+update. The DB function returns
        // whether `watch_sentinels.hard_killed_at` was set BEFORE the
        // clear, and the probation_until that landed. This is what makes
        // the post-restart DB-only-hard-kill path enter probation correctly:
        // in-memory is empty, but DB still gates
        // insert_fire via OCC. If the DB write fails, in-memory is
        // untouched — DB is authoritative for hard_kill.
        let durable = if let Some(db) = &self.db {
            db.clear_hard_kill_and_set_probation(
                tenant,
                sentinel,
                skip_probation,
                self.cfg.probation_ms,
                in_mem_hard_kill,
            )
            .await?
        } else {
            // In-memory-only test path: synthesize what the DB would have
            // said based on in-memory state. Shares the
            // `probation_target_for_clear` helper with the DB tx so the
            // two paths can't drift on the timestamp computation.
            crate::watch::db::DurableClearOutcome {
                was_hard_killed: in_mem_hard_kill,
                probation_until_ms: crate::watch::db::probation_target_for_clear(
                    in_mem_hard_kill,
                    skip_probation,
                    self.cfg.probation_ms,
                ),
            }
        };

        // Phase 3 — in-memory mutation. Mirror the DB state so the gate
        // check (`is_blocked`) sees the same shape on the next fire. If
        // record was absent AND the DB cleared a hard-kill that will enter
        // probation, we INSERT a healthy-shape record carrying the
        // probation window — without this, fire_pipeline's gate check
        // returns None and the `[PROBATION]` prefix never applies.
        let any_db_state_to_carry = durable.was_hard_killed || durable.probation_until_ms.is_some();
        {
            let mut recs = self.records.lock();
            let key = (tenant.to_string(), sentinel.to_string());
            let exists = recs.contains_key(&key);
            if !exists && any_db_state_to_carry {
                recs.insert(
                    key.clone(),
                    QuarantineRecord {
                        tenant: tenant.into(),
                        sentinel: sentinel.into(),
                        quarantined_until: Instant::now(),
                        duration_ms: 0,
                        consecutive_fails: 0,
                        consecutive_successes: 0,
                        cycle_count: 0,
                        cycles_window_start: None,
                        last_error: None,
                        last_quarantine_end: None,
                        hard_killed_at: None,
                        probation_until: durable
                            .probation_until_ms
                            .map(|_| Instant::now() + Duration::from_millis(self.cfg.probation_ms)),
                        pending_hard_kill_persist: None,
                    },
                );
            } else if let Some(rec) = recs.get_mut(&key) {
                rec.hard_killed_at = None;
                // T33.P1-D — admin override clears the pending-persist
                // flag. The DB has been UPDATEd to set hard_killed_at = NULL
                // already (phase 2), so any pending limbo from a failed
                // earlier persist attempt is logically resolved.
                rec.pending_hard_kill_persist = None;
                rec.quarantined_until = Instant::now();
                rec.cycle_count = 0;
                rec.cycles_window_start = None;
                rec.consecutive_fails = 0;
                rec.consecutive_successes = 0;
                if durable.probation_until_ms.is_some() {
                    rec.probation_until =
                        Some(Instant::now() + Duration::from_millis(self.cfg.probation_ms));
                } else {
                    rec.probation_until = None;
                    // Only remove the record if it has decayed to its
                    // zero-state with no quarantine history to preserve.
                    if rec.last_quarantine_end.is_none() {
                        recs.remove(&key);
                    }
                }
            }
        }

        // Phase 4 — assemble the cleared list. "hard_kill" comes from the
        // DB (authoritative); "quarantine" + "probation" come from
        // in-memory (DB doesn't track per-cycle state in this v0.1 schema).
        let mut cleared = Vec::new();
        if in_mem_quarantine_cleared {
            cleared.push("quarantine".to_string());
        }
        if durable.was_hard_killed || in_mem_hard_kill {
            cleared.push("hard_kill".to_string());
        }
        if in_mem_probation_cleared {
            cleared.push("probation".to_string());
        }

        Ok(ClearOutcome {
            cleared,
            probation_until_ms: durable.probation_until_ms,
        })
    }
}

/// T32 — return value of `admin_clear_quarantine`. `cleared` lists labels of
/// states that were actually active before the call ("quarantine",
/// "hard_kill", "probation"); empty list means the sentinel was already
/// healthy. `probation_until_ms` is the Unix-ms wall-clock deadline of the
/// post-hard-kill 10-min log-only window — Some only when the call cleared a
/// hard-kill AND `skip_probation = false`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClearOutcome {
    pub cleared: Vec<String>,
    pub probation_until_ms: Option<i64>,
}
