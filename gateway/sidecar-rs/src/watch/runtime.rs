//! Isolated Tokio runtime and total-deadline pipeline.
//!
//! A dedicated 2-worker + 8-blocking runtime is isolated
//! from the sidecar hot-path runtime. 200ms total-deadline pipeline with
//! per-phase sub-budgets (observe 50 / interesting 10 / escalate 100 /
//! audit 40 — sum = 200 = total). The fire-decision leg
//! (gate + observe + interesting + gate) has its own 100ms wall-clock budget
//! to catch blocking sentinel code that tokio timeouts cannot preempt.

use crate::watch::quarantine::QuarantineState;
use crate::watch::{EscalateError, ObserveError, Sentinel, SentinelState};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};

const TOTAL_BUDGET: Duration = Duration::from_millis(200);
const FIRE_DECISION_BUDGET: Duration = Duration::from_millis(100);
const OBSERVE_BUDGET: Duration = Duration::from_millis(50);
const INTERESTING_BUDGET: Duration = Duration::from_millis(10);
const ESCALATE_BUDGET: Duration = Duration::from_millis(100);
const AUDIT_BUDGET: Duration = Duration::from_millis(40);

/// Dedicated runtime for the watch plane. 2 worker threads + 8 blocking threads,
/// isolated from the sidecar hot-path runtime. Owned by main.rs at startup.
pub fn build_watch_runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_name("watch-rt")
        .enable_all()
        .build()
        .expect("failed to build watch_runtime")
}

/// Outcome of one fire-pipeline run. Discriminator for both metric labeling
/// and quarantine state-machine input.
#[derive(Debug)]
pub enum FireOutcome {
    Fired(i64), // audit row id
    Uninteresting,
    Gated(QuarantineGate),
    ObserveErr(ObserveError),
    EscalateErr(EscalateError),
    AuditWriteErr(String),
    AuditWorkerCrashed,
    Panic,
    Timeout(&'static str), // phase name: observe|interesting|escalate|audit|total
    BudgetViolation(&'static str), // budget name: fire_decision
}

/// Outcome of `QuarantineState::is_blocked` — the gate state a sentinel
/// is in at fire time. Removed `CooldownActive` in T33.10 (zero return
/// sites, misleading semantics) — cooldown is enforced by the runner's
/// ticker cadence, not by `is_blocked`. Adding a new variant must come
/// with a `is_blocked` return site or it's defensive dead code.
#[derive(Debug)]
pub enum QuarantineGate {
    Quarantined,
    HardKilled,
    ProbationLogOnly,
}

/// T30 — synthetic-state pipeline for `POST /watch/force-wake/{sentinel}`.
///
/// Skips `observe()` and `interesting()` (spec §4.4); jumps straight to the
/// quarantine gate, then `escalate()`, then audit-write. Uses the same
/// timeout + panic-guard + audit-budget guarantees as [`fire_pipeline`] so
/// the resulting fire is hash-chained identically to a natural fire.
pub async fn force_wake_pipeline<S: Sentinel + ?Sized>(
    sentinel: &S,
    quarantine: &QuarantineState,
    synthetic_state: SentinelState,
    reason: String,
) -> FireOutcome {
    let outer = tokio::time::timeout(TOTAL_BUDGET, async {
        // Quarantined / HardKilled block force-wake (409 at the handler).
        // ProbationLogOnly does NOT block — probation is log-only per spec
        // §9.2. The fire runs, but `reason` is prefixed `[PROBATION] ` so
        // Phase 3's dispatcher filters the audit row out.
        let reason = match quarantine
            .is_blocked(sentinel.tenant(), sentinel.name())
            .await
        {
            Some(QuarantineGate::Quarantined) => {
                return FireOutcome::Gated(QuarantineGate::Quarantined)
            }
            Some(QuarantineGate::HardKilled) => {
                return FireOutcome::Gated(QuarantineGate::HardKilled)
            }
            Some(QuarantineGate::ProbationLogOnly) => format!("[PROBATION] {reason}"),
            None => reason,
        };

        let escalation = match tokio::time::timeout(
            ESCALATE_BUDGET,
            AssertUnwindSafe(sentinel.escalate(synthetic_state, reason)).catch_unwind(),
        )
        .await
        {
            Err(_) => return FireOutcome::Timeout("escalate"),
            Ok(Err(_panic)) => return FireOutcome::Panic,
            Ok(Ok(Err(e))) => return FireOutcome::EscalateErr(e),
            Ok(Ok(Ok(escalation))) => escalation,
        };

        match tokio::time::timeout(AUDIT_BUDGET, quarantine.write_fire_row(escalation)).await {
            Err(_) => FireOutcome::Timeout("audit"),
            Ok(Err(e)) => FireOutcome::AuditWriteErr(e.to_string()),
            Ok(Ok(fire_id)) => FireOutcome::Fired(fire_id),
        }
    })
    .await;

    outer.unwrap_or(FireOutcome::Timeout("total"))
}

/// One end-to-end fire pipeline. Total-deadline 200ms, sub-budgets per phase.
/// Pattern: Linkerd hedging budgets (sum of sub-budgets = total).
///
/// The gate is split. The Quarantined / HardKilled /
/// pending_hard_kill_persist check runs BEFORE observe() so a
/// blocked sentinel cannot leak `Uninteresting` outcomes that downstream
/// `record_success` would translate into a silent pending-clear. ProbationLogOnly
/// is NOT short-circuited here — its decorator (`[PROBATION] ` reason prefix)
/// is applied post-interesting per spec §9.2.
///
/// # TOCTOU race bound
///
/// An `admin_clear_quarantine` or `admin_hard_kill` call arriving
/// concurrently with a `fire_pipeline` invocation cannot produce more
/// than **one** audit row per race per sentinel. The bound is enforced
/// by three independent serialization points:
///
/// 1. **Pre-observe gate (step 0)** — `is_blocked()` short-circuits
///    `HardKilled` / `Quarantined` / `pending_hard_kill_persist = Some(_)`
///    before `observe()` runs. Wins the race if admin's commit lands
///    before the pipeline starts.
/// 2. **Post-interesting re-check (step 3)** — same `is_blocked()` call
///    re-evaluates after `observe()` + `interesting()` but before
///    `escalate()` + audit-write. Catches admin kills that landed
///    during the observe/interesting window (cheap reads, no audit cost).
/// 3. **`insert_fire` OCC (step 5)** — `BEGIN IMMEDIATE` + in-tx
///    `SELECT hard_killed_at FROM watch_sentinels` rejects the insert
///    with `Ok(None)` if admin's commit landed during escalate+audit.
///    The watch_sentinels row is the OCC anchor; the tx is exclusive
///    on the SQLite write lane so the read-then-insert is atomic.
///
/// Case analysis for an admin kill K racing a pipeline P on the same
/// `(tenant, sentinel)`:
///
/// | K commits | P sees           | Audit rows |
/// |---|---|---|
/// | before step 0    | step 0 gate fires `Gated(HardKilled)`         | 0 |
/// | between 0 and 3  | step 3 gate fires `Gated(HardKilled)`         | 0 |
/// | between 3 and 5  | OCC rejects in `insert_fire` → `Ok(None)`     | 0 |
/// | after step 5     | P already committed its row; next P stops     | 1 |
///
/// Worst case = 1 audit row, achieved only when P fully commits before
/// K commits — at that point the row reflects ground truth (the
/// sentinel was alive at audit time). Any subsequent P on the same
/// sentinel hits the step-0 gate. No re-litigation of this bound.
///
/// What this **does not** cover: external observers reading
/// `watch_sentinels.hard_killed_at` and `watch_fires` non-transactionally
/// can see a brief inconsistency (a fire row whose sentinel is now
/// hard-killed). This is correct under the SQLite isolation model —
/// the row was a real fire at its `fired_at_ms`; subsequent kill is a
/// separate event.
pub async fn fire_pipeline<S: Sentinel + ?Sized>(
    sentinel: &S,
    quarantine: &QuarantineState,
) -> FireOutcome {
    let outer = tokio::time::timeout(TOTAL_BUDGET, async {
        let decision_started = tokio::time::Instant::now();

        // 0. Pre-observe gate. HardKilled (incl. pending) + Quarantined
        //    short-circuit; ProbationLogOnly proceeds (decorator applied
        //    after interesting); None proceeds.
        match quarantine
            .is_blocked(sentinel.tenant(), sentinel.name())
            .await
        {
            Some(QuarantineGate::HardKilled) => {
                return FireOutcome::Gated(QuarantineGate::HardKilled)
            }
            Some(QuarantineGate::Quarantined) => {
                return FireOutcome::Gated(QuarantineGate::Quarantined)
            }
            Some(QuarantineGate::ProbationLogOnly) | None => {}
        }
        if decision_started.elapsed() > FIRE_DECISION_BUDGET {
            return FireOutcome::BudgetViolation("fire_decision");
        }

        // 1. observe (50ms).
        let observed = match tokio::time::timeout(OBSERVE_BUDGET, sentinel.observe()).await {
            Err(_) => return FireOutcome::Timeout("observe"),
            Ok(result) => result,
        };
        if decision_started.elapsed() > FIRE_DECISION_BUDGET {
            return FireOutcome::BudgetViolation("fire_decision");
        }
        let state = match observed {
            Err(e) => return FireOutcome::ObserveErr(e),
            Ok(state) => state,
        };

        // 2. interesting (10ms — sync predicate; timeout defends against pathology).
        let interesting =
            match tokio::time::timeout(INTERESTING_BUDGET, async { sentinel.interesting(&state) })
                .await
            {
                Err(_) => return FireOutcome::Timeout("interesting"),
                Ok(reason) => reason,
            };
        if decision_started.elapsed() > FIRE_DECISION_BUDGET {
            return FireOutcome::BudgetViolation("fire_decision");
        }
        let reason = match interesting {
            None => return FireOutcome::Uninteresting,
            Some(r) => r,
        };

        // 3. Re-check gate (admin may have killed mid-pipeline; probation
        // decorator applied here). Spec §9.2 — probation does NOT block;
        // it tags the reason so Phase 3's dispatcher filters log-only fires.
        let reason = match quarantine
            .is_blocked(sentinel.tenant(), sentinel.name())
            .await
        {
            Some(QuarantineGate::Quarantined) => {
                return FireOutcome::Gated(QuarantineGate::Quarantined)
            }
            Some(QuarantineGate::HardKilled) => {
                return FireOutcome::Gated(QuarantineGate::HardKilled)
            }
            Some(QuarantineGate::ProbationLogOnly) => format!("[PROBATION] {reason}"),
            None => reason,
        };
        if decision_started.elapsed() > FIRE_DECISION_BUDGET {
            return FireOutcome::BudgetViolation("fire_decision");
        }

        // 4. escalate (100ms) — panic-isolated via catch_unwind.
        let escalation = match tokio::time::timeout(
            ESCALATE_BUDGET,
            AssertUnwindSafe(sentinel.escalate(state, reason)).catch_unwind(),
        )
        .await
        {
            Err(_) => return FireOutcome::Timeout("escalate"),
            Ok(Err(_panic)) => return FireOutcome::Panic,
            Ok(Ok(Err(e))) => return FireOutcome::EscalateErr(e),
            Ok(Ok(Ok(escalation))) => escalation,
        };

        // 5. audit-write (40ms) — written via callback into quarantine layer.
        match tokio::time::timeout(AUDIT_BUDGET, quarantine.write_fire_row(escalation)).await {
            Err(_) => FireOutcome::Timeout("audit"),
            Ok(Err(e)) => FireOutcome::AuditWriteErr(e.to_string()),
            Ok(Ok(fire_id)) => FireOutcome::Fired(fire_id),
        }
    })
    .await;

    outer.unwrap_or(FireOutcome::Timeout("total"))
}
