//! Pre-observe gate adversarial tests.
//!
//! Regression guard: `fire_pipeline` at `src/watch/runtime.rs:114` runs
//! `observe()` → `interesting()` BEFORE the quarantine gate at line 140.
//! Consequence: when `interesting()` returns `None`, `fire_pipeline` returns
//! `FireOutcome::Uninteresting`, which the P1-B mapping in
//! `runner::handle_fire_outcome` then routes to `record_success`. The current
//! `record_success` unconditionally clears `pending_hard_kill_persist` — so a
//! sentinel that was marked pending (fail-closed safety ladder, because the
//! DB hard-kill upsert failed) is silently cleared the next time its
//! `interesting()` returns None.
//!
//! These tests exercise the fail-closed path directly.

use async_trait::async_trait;
use gateway_sidecar::watch::quarantine::{QuarantineConfig, QuarantineState};
use gateway_sidecar::watch::runtime::{fire_pipeline, FireOutcome, QuarantineGate};
use gateway_sidecar::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Sentinel that counts observe() + interesting() invocations so we can assert
/// fire_pipeline's pre-observe gate short-circuits before either is called.
struct ObserveCountingSentinel {
    name: String,
    tenant: String,
    observes: Arc<AtomicU32>,
    interestings: Arc<AtomicU32>,
}

#[async_trait]
impl Sentinel for ObserveCountingSentinel {
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
        Duration::from_millis(0)
    }
    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        self.observes.fetch_add(1, Ordering::SeqCst);
        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: 0,
            payload: serde_json::Value::Null,
        })
    }
    fn interesting(&self, _: &SentinelState) -> Option<String> {
        self.interestings.fetch_add(1, Ordering::SeqCst);
        None // Uninteresting — drives the bug path.
    }
    async fn escalate(&self, _: SentinelState, _: String) -> Result<Escalation, EscalateError> {
        unreachable!("escalate must not be reached: interesting() always returns None")
    }
}

fn make_sentinel(name: &str, tenant: &str) -> ObserveCountingSentinel {
    ObserveCountingSentinel {
        name: name.into(),
        tenant: tenant.into(),
        observes: Arc::new(AtomicU32::new(0)),
        interestings: Arc::new(AtomicU32::new(0)),
    }
}

/// HardKilled (durable, set via `record_failure` Ok arm post-upsert) MUST
/// short-circuit fire_pipeline before observe() runs.
#[tokio::test]
async fn t33_p01_pipeline_pre_gate_skips_observe_on_hard_killed() {
    let q = QuarantineState::new_in_memory(QuarantineConfig::test_with_cooldown(
        std::time::Duration::from_millis(0),
    ));
    q.test_set_hard_killed_at("sovereign", "s", Instant::now())
        .await;
    let sentinel = make_sentinel("s", "sovereign");
    let observes = sentinel.observes.clone();
    let interestings = sentinel.interestings.clone();

    let outcome = fire_pipeline(&sentinel, &q).await;

    assert!(
        matches!(outcome, FireOutcome::Gated(QuarantineGate::HardKilled)),
        "expected Gated(HardKilled), got {outcome:?}"
    );
    assert_eq!(
        observes.load(Ordering::SeqCst),
        0,
        "observe() must not run while hard-killed"
    );
    assert_eq!(
        interestings.load(Ordering::SeqCst),
        0,
        "interesting() must not run while hard-killed"
    );
}

/// `pending_hard_kill_persist` (process-local fail-closed pending state)
/// MUST also short-circuit fire_pipeline. `is_blocked` already maps this to
/// `Gated(HardKilled)`; the bug is that the current pipeline runs observe()
/// + interesting() FIRST, so the gate never fires when interesting()→None.
#[tokio::test]
async fn t33_p01_pipeline_pre_gate_skips_observe_on_pending_hard_kill_persist() {
    let q = QuarantineState::new_in_memory(QuarantineConfig::test_with_cooldown(
        std::time::Duration::from_millis(0),
    ));
    q.test_set_pending_hard_kill_persist("sovereign", "s", Instant::now())
        .await;
    let sentinel = make_sentinel("s", "sovereign");
    let observes = sentinel.observes.clone();
    let interestings = sentinel.interestings.clone();

    let outcome = fire_pipeline(&sentinel, &q).await;

    assert!(
        matches!(outcome, FireOutcome::Gated(QuarantineGate::HardKilled)),
        "expected Gated(HardKilled) on pending, got {outcome:?}"
    );
    assert_eq!(
        observes.load(Ordering::SeqCst),
        0,
        "observe() must not run while pending_hard_kill_persist is set"
    );
    assert_eq!(
        interestings.load(Ordering::SeqCst),
        0,
        "interesting() must not run while pending_hard_kill_persist is set"
    );
}

/// Active quarantine window MUST short-circuit fire_pipeline.
#[tokio::test]
async fn t33_p01_pipeline_pre_gate_skips_observe_on_quarantined() {
    let q = QuarantineState::new_in_memory(QuarantineConfig::test_with_cooldown(
        std::time::Duration::from_millis(0),
    ));
    q.test_set_quarantined_until("sovereign", "s", Instant::now() + Duration::from_secs(60))
        .await;
    let sentinel = make_sentinel("s", "sovereign");
    let observes = sentinel.observes.clone();
    let interestings = sentinel.interestings.clone();

    let outcome = fire_pipeline(&sentinel, &q).await;

    assert!(
        matches!(outcome, FireOutcome::Gated(QuarantineGate::Quarantined)),
        "expected Gated(Quarantined), got {outcome:?}"
    );
    assert_eq!(
        observes.load(Ordering::SeqCst),
        0,
        "observe() must not run while quarantined"
    );
    assert_eq!(
        interestings.load(Ordering::SeqCst),
        0,
        "interesting() must not run while quarantined"
    );
}

/// Probation does NOT block — observe() runs, interesting() runs. (If
/// interesting()→Some(reason), escalate() would prepend [PROBATION] per
/// spec §9.2. Here interesting()→None, so we expect Uninteresting and
/// no gating short-circuit — confirms pre-observe gate does not over-block
/// the probation path.)
#[tokio::test]
async fn t33_p01_pipeline_does_not_pre_gate_on_probation() {
    let q = QuarantineState::new_in_memory(QuarantineConfig::test_with_cooldown(
        std::time::Duration::from_millis(0),
    ));
    q.test_set_probation_until("sovereign", "s", Instant::now() + Duration::from_secs(60))
        .await;
    let sentinel = make_sentinel("s", "sovereign");
    let observes = sentinel.observes.clone();
    let interestings = sentinel.interestings.clone();

    let outcome = fire_pipeline(&sentinel, &q).await;

    assert!(
        matches!(outcome, FireOutcome::Uninteresting),
        "expected Uninteresting under probation (probation does not block), got {outcome:?}"
    );
    assert_eq!(
        observes.load(Ordering::SeqCst),
        1,
        "observe() MUST run under probation"
    );
    assert_eq!(
        interestings.load(Ordering::SeqCst),
        1,
        "interesting() MUST run under probation"
    );
}
