//! WatchRunner: per-Sentinel spawn loop on the dedicated watch runtime.
//!
//! For each registered sentinel, spawns one async task on the dedicated
//! watch_runtime handle. The task pulls triggers from a bounded
//! `mpsc::channel(64)` and drives [`fire_pipeline`] per trigger.
//!
//! Trigger sources by tier:
//!   - `Tier::Polling | Tier::Deep` — internal ticker pushes one trigger
//!     every `sentinel.cooldown()`. External `kick_sender` calls can also
//!     trigger an immediate fire through the force-wake hook.
//!   - `Tier::Fast` — no internal ticker; fires ONLY on external kicks
//!     (e.g., notify::PollWatcher pushing on file-inbox events).
//!
//! Channel capacity is 64. A full channel drops the trigger. Drops are
//! intentional; the durable escalation path begins after a fire is accepted.
//!
//! Shutdown: drop the [`WatchRunnerHandles::shutdown`] handle (or call
//! `.shutdown()`) — the broadcast wakes every sentinel loop, the kick
//! channels are closed, and the per-task `JoinHandle`s drain via
//! [`WatchRunnerHandles::join_all`].

#![allow(clippy::items_after_test_module)]

use crate::comms::envelope::{CommsEnvelope, EnvelopeKind};
use crate::watch::db::{CommittedFire, WatchDb};
use crate::watch::fire_identity::{causal_fire_id, compute_content_digest};
use crate::watch::quarantine::{QuarantineState, RetryOutcome};
use crate::watch::runtime::{fire_pipeline, FireOutcome};
use crate::watch::{Sentinel, Tier};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

pub const KICK_CHANNEL_CAPACITY: usize = 64;

/// Cadence of the pending-hard-kill retry
/// tick. Sixty seconds is long
/// enough that a wedged SQLite writer isn't hammered, short enough that a
/// recovered DB drains the pending pool within a single operator-visible
/// window.
pub const PENDING_RETRY_TICK: Duration = Duration::from_secs(60);

/// Maximum records retried per tick. Smooths
/// the thundering-herd surge when a stuck DB recovers after many records have
/// accumulated in pending limbo: instead of N parallel `upsert_hard_kill`
/// calls slamming the writer the moment it comes back, we drain 100 per
/// minute and the rest waits one more tick. Acceptable lag for a fail-closed
/// safety state that already serves `HardKilled` to `is_blocked`.
pub const MAX_RETRIES_PER_TICK: usize = 100;

/// Base cadence for the causal sweep and transactional-outbox producer.
/// Sixty seconds mirrors pending_retry
/// precedent for DB kindness under load/recovery. MissedTickBehavior::Skip prevents
/// thundering herd on slow ticks.
pub const CDC_SWEEP_TICK: Duration = Duration::from_secs(60);

/// Bounded work per sweep tick (prevents long-running tx or burst on recovery).
/// Matches spirit of MAX_RETRIES_PER_TICK.
pub const MAX_SWEEP_PER_TICK: usize = 200;

const WATCH_CDC_EMIT_COMMS_ENVELOPE: &str = "WATCH_CDC_EMIT_COMMS_ENVELOPE";
const CDC_ENVELOPE_TTL_SECONDS: u64 = 90;
const CDC_ENVELOPE_BUDGET_HINT: &str = "council:triage";
const CDC_ENVELOPE_REPLY_TO: &str = "gateway://watch/pending_escalations";

/// Defensive telemetry — count of CDC comms-envelope builds that hit the defensive
/// Err arm and fell back to the raw (pre-comms) escalation shape. Unreachable
/// today (every required field is set from non-Option sources), but the D7
/// contract says CDC rows default to the irin.comms.v0.1 wrapper — if a
/// refactor ever makes the fallback fire, rows of mixed shape appear under
/// emit_comms_envelope=true and that must be observable, not silent. Mirrors
/// the CAP_TOKEN_REJECTED pattern (private static + pub accessor).
static CDC_ENVELOPE_BUILD_FALLBACK: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn cdc_envelope_build_fallback_total() -> u64 {
    CDC_ENVELOPE_BUILD_FALLBACK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Unified producer gate (P0-I / plan §6 / phase1-producer-seam §5): default OFF
/// in prod. True only when explicitly armed for D9 (WATCH_PRODUCER_ENABLED=1/true,
/// EXECUTION_MODE=LIVE, and the dispatcher key present for auth). Mirrors
/// should_spawn_live_dispatcher logic (reference, not mutation of that CRITICAL
/// symbol).
fn is_producer_gate_armed() -> bool {
    producer_gate_armed_from(
        std::env::var("WATCH_PRODUCER_ENABLED").ok().as_deref(),
        // Require the same gateway key as dispatcher for the authenticated
        // probe/enqueue path.
        std::env::var("WATCH_DISPATCHER_GATEWAY_KEY").is_ok(),
        std::env::var("EXECUTION_MODE").ok().as_deref(),
    )
}

/// Pure producer-gate predicate. The producer is
/// armed ONLY when `WATCH_PRODUCER_ENABLED` is explicitly truthy (`1`/`true`)
/// AND `EXECUTION_MODE` is exactly `LIVE` AND the gateway key is present.
/// Extracted from the env reads so the default-OFF invariant is unit-testable
/// WITHOUT mutating process env (which would race parallel tests). The CDC
/// sweep spawn site (`has_db() && is_producer_gate_armed()`) is the only caller,
/// so proving this returns `false` under the default (absent flag) proves the
/// sweep cannot be spawned UNARMED.
pub fn producer_gate_armed_from(
    enabled_var: Option<&str>,
    key_present: bool,
    execution_mode_var: Option<&str>,
) -> bool {
    let enabled = matches!(enabled_var, Some(v) if v == "1" || v.eq_ignore_ascii_case("true"));
    let live_execution_mode = matches!(execution_mode_var, Some("LIVE"));
    enabled && key_present && live_execution_mode
}

fn cdc_emit_comms_envelope_enabled() -> bool {
    cdc_emit_comms_envelope_enabled_from(
        std::env::var(WATCH_CDC_EMIT_COMMS_ENVELOPE).ok().as_deref(),
    )
}

/// the boot-time producer-spawn gate as a
/// TESTABLE function (deleting the check now fails tests instead of CI
/// staying green):
///
/// 1. **Single-writer enforcement point 2 (single-writer invariant):** re-acquire the
///    singleton writer claim under this process's uuid. Refused (another
///    LIVE writer) or DB error (#13 fail-closed) → clear
///    `producer_kill_state` so /watch surfaces report unarmed truthfully,
///    and return `false` (caller must NOT spawn the producer).
/// 2. **Boot env-arm audit (p0a):** the WATCH_PRODUCER_ENABLED +
///    WATCH_DISPATCHER_GATEWAY_KEY boot path arms WITHOUT the four-eyes
///    ceremony (one human with env-write + restart). That bypass is now
///    AUDITED: a `boot_env_arm` row is appended to the append-only
///    arm_audit chain before the producer spawns. Fail-closed: if the audit
///    row cannot be written, the producer does not spawn (same posture as
///    the stage/confirm ceremony). The env path itself is declared in the
///    arming-authorization runbook's default-OFF config manifest.
pub async fn boot_producer_claim_check_and_audit(
    quarantine: &Arc<QuarantineState>,
    db: &Arc<WatchDb>,
    now_ms: i64,
) -> bool {
    let execution_mode = std::env::var("EXECUTION_MODE").ok();
    boot_producer_claim_check_and_audit_with_mode(quarantine, db, now_ms, execution_mode.as_deref())
        .await
}

pub async fn boot_producer_claim_check_and_audit_with_mode(
    quarantine: &Arc<QuarantineState>,
    db: &Arc<WatchDb>,
    now_ms: i64,
    execution_mode: Option<&str>,
) -> bool {
    matches!(
        boot_producer_claim_attempt_with_mode(quarantine, db, now_ms, execution_mode).await,
        BootClaimOutcome::Acquired
    )
}

/// Outcome of ONE boot-time producer claim attempt. Carries enough detail for
/// the retry loop to log the current holder and to distinguish a transient
/// foreign claim (retryable — a graceful release or stale takeover will let us
/// in within the 90s window) from the terminal audit-append failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootClaimOutcome {
    /// We acquired the claim and the `boot_env_arm` audit row is written —
    /// the producer may spawn.
    Acquired,
    /// Another LIVE writer holds the claim (`Some(uuid)` when readable). The
    /// boot path RETRIES this (CI regression / previous CI run — a one-shot
    /// refusal turned a transient conflict into permanent producer death).
    RefusedForeignClaim { holder: Option<String> },
    /// Writer-claim check hit a DB error (#13 DB-unavailable = fail-closed).
    /// Retryable: a transient DB blip should not permanently kill the producer.
    RefusedDbError,
    /// We acquired the claim but the append-only `boot_env_arm` audit row could
    /// not be written → fail-closed. We RELEASE the claim we just took (so a
    /// retry — or another instance — can re-acquire cleanly) and refuse.
    RefusedAuditFailed,
}

/// single-writer — ONE boot-time claim attempt (clock injected via
/// `now_ms`, same seam as `try_acquire_writer_claim`, so the refuse→succeed
/// transition is unit-testable without wall-clock). On any refusal the
/// `producer_kill_state` is cleared so /watch surfaces read unarmed truthfully.
pub async fn boot_producer_claim_attempt(
    quarantine: &Arc<QuarantineState>,
    db: &Arc<WatchDb>,
    now_ms: i64,
) -> BootClaimOutcome {
    let execution_mode = std::env::var("EXECUTION_MODE").ok();
    boot_producer_claim_attempt_with_mode(quarantine, db, now_ms, execution_mode.as_deref()).await
}

pub async fn boot_producer_claim_attempt_with_mode(
    quarantine: &Arc<QuarantineState>,
    db: &Arc<WatchDb>,
    now_ms: i64,
    execution_mode: Option<&str>,
) -> BootClaimOutcome {
    match db
        .try_acquire_writer_claim(
            crate::watch::db::process_instance_uuid(),
            now_ms,
            crate::watch::db::writer_claim_stale_ms(),
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            let holder = db
                .writer_claim_holder()
                .await
                .ok()
                .flatten()
                .map(|(u, _)| u);
            tracing::error!(
                ?holder,
                "CDC producer REFUSED to spawn: another live writer holds the single-writer claim (p07 single-writer invariant)"
            );
            *quarantine.producer_kill_state.lock() = None;
            return BootClaimOutcome::RefusedForeignClaim { holder };
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "CDC producer REFUSED to spawn: writer-claim check failed (DB-unavailable = fail-closed, p07)"
            );
            *quarantine.producer_kill_state.lock() = None;
            return BootClaimOutcome::RefusedDbError;
        }
    }

    // audit the env-path arm. Fail-closed on audit failure —
    // an unauditable arm is a refused arm, exactly like stage/confirm. We took
    // the claim above, so on audit failure RELEASE it (uuid-fenced) before
    // refusing — otherwise our own row would block the retry for 90s.
    if let Err(e) = db
        .append_arm_audit(
            "boot_env_arm",
            "env(WATCH_PRODUCER_ENABLED)",
            Some(&format!(
                "boot-time arm via WATCH_PRODUCER_ENABLED + WATCH_DISPATCHER_GATEWAY_KEY + EXECUTION_MODE=LIVE; execution_mode={}; instance_uuid={}; keyset_hash={}",
                execution_mode.unwrap_or("<absent>"),
                crate::watch::db::process_instance_uuid(),
                // B3 (spec §6): the enrolled-keyset hash chains into the boot
                // row so any registry change is tamper-evident and alerted.
                crate::watch::attest::boot_keyset_hash()
            )),
        )
        .await
    {
        tracing::error!(
            error = %e,
            "CDC producer REFUSED to spawn: boot_env_arm audit append failed (fail-closed — an unauditable arm is a refused arm)"
        );
        *quarantine.producer_kill_state.lock() = None;
        let _ = db
            .release_writer_claim(crate::watch::db::process_instance_uuid())
            .await;
        return BootClaimOutcome::RefusedAuditFailed;
    }
    BootClaimOutcome::Acquired
}

/// single-writer — boot-time producer claim RETRY loop (smoke runs
/// previous CI runs). Repeatedly attempts `boot_producer_claim_attempt`
/// every `retry_period`; returns `true` the moment acquisition succeeds, or
/// `false` if the runner `shutdown` signal fires first (clean exit — the
/// producer never spawns). Each foreign-claim refusal is logged at WARN with
/// the holder uuid. Bounded in practice by `WRITER_CLAIM_STALE_MS` (a foreign
/// claim becomes takeover-eligible after the stale window), but we keep
/// retrying indefinitely until shutdown so a long-lived-but-then-released
/// holder is also handled. Council intent: acquisition still only succeeds via
/// release or stale takeover — never two live writers.
///
/// The shutdown channel is the SAME `watch::Receiver<bool>` every runner loop
/// selects on (owned by `WatchRunnerHandles::shutdown_tx`); main.rs now fires
/// it on SIGTERM/SIGINT so a Docker `compose recreate` exits this loop instead
/// of being killed mid-wait.
pub async fn boot_producer_claim_retry_loop(
    quarantine: &Arc<QuarantineState>,
    db: &Arc<WatchDb>,
    retry_period: std::time::Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match boot_producer_claim_attempt(quarantine, db, now_ms).await {
            BootClaimOutcome::Acquired => return true,
            BootClaimOutcome::RefusedForeignClaim { holder } => {
                tracing::warn!(
                    ?holder,
                    retry_in_ms = retry_period.as_millis() as u64,
                    "CDC producer boot claim refused (foreign live writer); will retry — refusal is NOT permanent (restart regression). Bounded by the stale window."
                );
            }
            BootClaimOutcome::RefusedDbError => {
                tracing::warn!(
                    retry_in_ms = retry_period.as_millis() as u64,
                    "CDC producer boot claim refused (DB error, fail-closed); will retry."
                );
            }
            BootClaimOutcome::RefusedAuditFailed => {
                tracing::warn!(
                    retry_in_ms = retry_period.as_millis() as u64,
                    "CDC producer boot claim acquired but boot_env_arm audit failed (fail-closed; claim released); will retry."
                );
            }
        }
        // Already shutting down? Don't sleep — exit now.
        if *shutdown.borrow() {
            return false;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return false;
                }
            }
            _ = tokio::time::sleep(retry_period) => {}
        }
    }
}

fn cdc_emit_comms_envelope_enabled_from(value: Option<&str>) -> bool {
    !matches!(
        value,
        Some(v)
            if v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("legacy")
                || v.eq_ignore_ascii_case("raw")
    )
}

/// Handles returned by [`WatchRunner::start`]. The caller is expected to
/// hold this for the lifetime of the runner; dropping it (or calling
/// `.shutdown()`) signals every per-sentinel loop AND the pending-retry loop
/// to exit.
pub struct WatchRunnerHandles {
    join_handles: Vec<JoinHandle<()>>,
    kicks: HashMap<String, mpsc::Sender<()>>,
    shutdown_tx: watch::Sender<bool>,
    /// Owned handle to the pending-hard-kill
    /// retry loop. `Some(_)` only when `QuarantineState::has_db()` was true
    /// at start time (P1-5 — in-memory mode = no DB to retry against, so the
    /// loop would be dead work). The loop receives the same `shutdown_tx`
    /// signal as the per-sentinel loops; `shutdown()` + `join_all()` drain
    /// it alongside the rest.
    pending_retry: Option<JoinHandle<()>>,
    /// Owned handle to the causal-fire
    /// sweep / transactional-outbox producer loop. Spawned only when has_db()
    /// AND the unified producer gate is armed (default OFF in prod builds/config;
    /// D9 harness arms via WATCH_PRODUCER_ENABLED + key for mock Council path).
    /// Same shutdown signal; drained in join_all(). Boot re-scan + steady ticks
    /// inside the loop (idempotent with ON CONFLICT dedup).
    cdc_sweep: Option<JoinHandle<()>>,
}

impl WatchRunnerHandles {
    /// Signal every sentinel loop AND the pending-retry loop to exit. Safe
    /// to call multiple times.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Await every spawned task to finish. Combine with [`Self::shutdown`]
    /// for clean drain.
    pub async fn join_all(self) {
        for h in self.join_handles {
            let _ = h.await;
        }
        if let Some(h) = self.pending_retry {
            let _ = h.await;
        }
        if let Some(h) = self.cdc_sweep {
            let _ = h.await;
        }
    }

    /// External trigger for a named sentinel. Returns `None` if the
    /// sentinel was not registered. The send is bounded; callers should
    /// `.try_send` if drops on a full channel are acceptable.
    pub fn kick_sender(&self, sentinel_name: &str) -> Option<mpsc::Sender<()>> {
        self.kicks.get(sentinel_name).cloned()
    }

    /// Test helper — true when the pending-hard-kill retry loop was spawned
    /// (i.e. `QuarantineState::has_db()` returned true at start time).
    /// Lets the P1-5 has_db-gate test assert spawn behavior without exposing
    /// the JoinHandle.
    pub fn pending_retry_spawned(&self) -> bool {
        self.pending_retry.is_some()
    }

    /// Phase 1 CDC test helper — true when the causal sweep producer loop was
    /// spawned (has_db + gate armed at start). D9 harness uses to assert
    /// default-OFF + explicit arming.
    pub fn cdc_sweep_spawned(&self) -> bool {
        self.cdc_sweep.is_some()
    }
}

pub struct WatchRunner;

impl WatchRunner {
    /// Spawn one task per sentinel on `rt`. Returns the handles bundle.
    ///
    /// Also spawns the pending-
    /// hard-kill retry loop on the same `rt` when `quarantine.has_db()`.
    /// The retry loop's `JoinHandle` is owned by `WatchRunnerHandles` and
    /// drains on `join_all()`.
    pub fn start(
        rt: Handle,
        sentinels: Vec<Arc<dyn Sentinel>>,
        quarantine: Arc<QuarantineState>,
    ) -> WatchRunnerHandles {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut join_handles = Vec::with_capacity(sentinels.len());
        let mut kicks = HashMap::with_capacity(sentinels.len());

        for sentinel in sentinels {
            let (kick_tx, kick_rx) = mpsc::channel::<()>(KICK_CHANNEL_CAPACITY);
            kicks.insert(sentinel.name().to_string(), kick_tx.clone());

            let s = Arc::clone(&sentinel);
            let q = Arc::clone(&quarantine);
            let shut = shutdown_rx.clone();

            // For Polling/Deep, spawn an internal ticker that pushes on
            // cooldown. Decoupled from the runner loop so a slow fire
            // never delays the next tick (overflowing kicks just drop —
            // bounded backpressure).
            let tier = sentinel.tier();
            if matches!(tier, Tier::Polling | Tier::Deep) {
                let tick_tx = kick_tx.clone();
                let tick_cooldown = sentinel.cooldown();
                let tick_shut = shutdown_rx.clone();
                let tick_handle = rt.spawn(async move {
                    ticker_loop(tick_tx, tick_cooldown, tick_shut).await;
                });
                join_handles.push(tick_handle);
            }

            let h = rt.spawn(async move {
                runner_loop(s, q, kick_rx, shut).await;
            });
            join_handles.push(h);
        }

        // H1 / P1-5 — spawn the pending-hard-kill retry loop only when a
        // durable WatchDb is wired. In-memory mode has no Err path to leave
        // pending set, so the loop would be dead work and emit zero useful
        // telemetry.
        let pending_retry = if quarantine.has_db() {
            let q_for_retry = Arc::clone(&quarantine);
            let shut_for_retry = shutdown_rx.clone();
            Some(rt.spawn(async move {
                pending_retry_loop(q_for_retry, shut_for_retry).await;
            }))
        } else {
            None
        };

        // CDC producer uses the deliberately narrow quarantine database surface.
        // Real separate tokio sweep task + boot re-scan. Spawned only when a durable
        // WatchDb is present **and** the unified producer gate is armed (default OFF
        // in prod; D9 harness arms via WATCH_PRODUCER_ENABLED + key).
        //
        // Uses the limited db_for_cdc_sweep() accessor (explicitly scoped
        // to CDC producer spawn + test wiring; no hot audit path use permitted).
        // Modeled exactly on the pending_retry_loop spawn pattern above.
        let cdc_sweep = if quarantine.has_db() && is_producer_gate_armed() {
            if let Some(db_for_sweep) = quarantine.db_for_cdc_sweep() {
                let mut shut = shutdown_rx.clone();
                let (kill_tx, mut kill_rx) = watch::channel(false);
                let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                *quarantine.producer_kill_state.lock() = Some((kill_tx, ack_rx));
                let q_for_cdc = Arc::clone(&quarantine);
                Some(rt.spawn(async move {
                    // single-writer (single-writer invariant) + the
                    // boot-time claim check AND the 'boot_env_arm' audit row
                    // live in `boot_producer_claim_attempt` — a testable fn
                    // (deleting the check now fails tests). On refusal the kill
                    // state is cleared so /watch surfaces report unarmed.
                    //
                    // RETRY (restart regression): a one-shot
                    // refusal turned ANY transient claim conflict into permanent
                    // producer death — the foreign claim went stale at ~90s but
                    // the producer never re-checked. Now we retry every
                    // writer_claim_heartbeat_ms() until acquisition (bounded by
                    // the 90s stale window) OR runner shutdown. Council intent
                    // preserved: acquisition still only succeeds via a graceful
                    // release or stale takeover — never two live writers.
                    let retry_period = std::time::Duration::from_millis(
                        crate::watch::db::writer_claim_heartbeat_ms(),
                    );
                    if !boot_producer_claim_retry_loop(
                        &q_for_cdc,
                        &db_for_sweep,
                        retry_period,
                        &mut shut,
                    )
                    .await
                    {
                        // Shutdown fired before we ever acquired — exit cleanly.
                        let _ = ack_tx.send(());
                        return;
                    }

                    // single-writer: heartbeat watchdog — keeps the claim
                    // fresh while the producer runs; self-disarms (fail-closed)
                    // if the claim is ever lost. Listens to the same runner
                    // shutdown signal as the producer.
                    tokio::spawn(crate::watch::api::writer_claim_heartbeat_loop(
                        Arc::clone(&q_for_cdc),
                        Arc::clone(&db_for_sweep),
                        crate::watch::db::process_instance_uuid().to_string(),
                        std::time::Duration::from_millis(
                            crate::watch::db::writer_claim_heartbeat_ms(),
                        ),
                        Some(shut.clone()),
                    ));

                    let (unified_tx, unified_rx) = watch::channel(false);
                    let mut unified_tx = Some(unified_tx);

                    let loop_fut = cdc_sweep_loop(db_for_sweep, unified_rx);
                    let mut loop_fut = std::pin::pin!(loop_fut);

                    loop {
                        tokio::select! {
                            _ = &mut loop_fut => { break; }
                            _ = shut.changed() => {
                                if *shut.borrow() {
                                    if let Some(tx) = unified_tx.take() { let _ = tx.send(true); }
                                }
                            }
                            _ = kill_rx.changed() => {
                                if *kill_rx.borrow() {
                                    tracing::warn!("Runtime kill-switch activated for CDC producer. Draining in-flight tick...");
                                    if let Some(tx) = unified_tx.take() { let _ = tx.send(true); }
                                }
                            }
                        }
                    }
                    let _ = ack_tx.send(());
                }))
            } else {
                None
            }
        } else {
            None
        };

        if quarantine.has_db() {
            if let Some(db_for_prune) = quarantine.db_for_cdc_sweep() {
                let shut = shutdown_rx.clone();
                join_handles.push(rt.spawn(async move {
                    pruning_loop(db_for_prune, shut).await;
                }));
            }
        }

        // riders (B) — sweep_phantom_claims gets its RUNTIME caller (closes
        // the "zero runtime callers" engine-fact gap; this is what makes the
        // p0c reservation-release real in production). Sibling of
        // pruning_loop, same has_db gate; goes through the quarantine handle
        // so the counted sweep also bumps lease_expired_during_deliberation
        // (p0b) and emits the recon hint. Cadence: lease/2 (75s at the 150s
        // default — faster than hourly pruning so expired leases release
        // their reservations promptly), plus an immediate boot sweep so
        // phantoms left by a crash are reclaimed at startup.
        if quarantine.has_db() {
            let q_for_sweep = Arc::clone(&quarantine);
            let shut = shutdown_rx.clone();
            join_handles.push(rt.spawn(async move {
                phantom_sweep_loop(q_for_sweep, shut).await;
            }));
        }

        // watch telemetry (telemetry invariant) — out-of-band spend recon loop.
        // DEFAULT-OFF: spawns ONLY when RECON_CADENCE_SECS is set AND a
        // source is configured (RECON_IMPORT_PATH file import — the robust
        // default — or a provider usage key for best-effort API recon).
        // No env -> no task -> no surprise provider API calls.
        if quarantine.has_db() {
            if let Some(cfg) = crate::watch::recon::recon_config_from_env() {
                if let Some(db_for_recon) = quarantine.db_for_recon() {
                    let q_for_recon = Arc::clone(&quarantine);
                    let shut = shutdown_rx.clone();
                    tracing::info!(
                        cadence_secs = cfg.cadence.as_secs(),
                        threshold_usd = cfg.threshold_usd,
                        "spawning out-of-band spend recon loop (p0d)"
                    );
                    join_handles.push(rt.spawn(async move {
                        crate::watch::recon::recon_loop(db_for_recon, q_for_recon, cfg, shut).await;
                    }));
                }
            }
        }

        WatchRunnerHandles {
            join_handles,
            kicks,
            shutdown_tx,
            pending_retry,
            cdc_sweep,
        }
    }
}

async fn ticker_loop(
    tx: mpsc::Sender<()>,
    cooldown: std::time::Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = tokio::time::sleep(cooldown) => {
                // try_send: full channel = drop the tick. The runner is
                // already running behind; an extra trigger wouldn't help.
                if tx.try_send(()).is_err() {
                    // TODO(phase6): bump gw_watch_channel_full_total.
                }
            }
        }
    }
}

async fn runner_loop(
    sentinel: Arc<dyn Sentinel>,
    quarantine: Arc<QuarantineState>,
    mut kick_rx: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            msg = kick_rx.recv() => {
                match msg {
                    None => return, // all senders dropped — shut down
                    Some(()) => {
                        let outcome = fire_pipeline(&*sentinel, &quarantine).await;
                        handle_fire_outcome(outcome, &*sentinel, &quarantine).await;
                    }
                }
            }
        }
    }
}

/// T33.P1-B — outcome → quarantine state-machine dispatch. Extracted from
/// `runner_loop` so the routing is unit-testable in isolation: synthetic
/// `FireOutcome` values can be constructed directly (especially
/// audit-pipeline infra failures `AuditWriteErr` / `AuditWorkerCrashed` /
/// `Timeout("audit")` that are hard to drive end-to-end).
///
/// Mapping follows the circuit-breaker fault-domain split:
///   `Fired(_)` | `Uninteresting`                       → record_success
///   `AuditWriteErr(_)` | `AuditWorkerCrashed`
///     | `Timeout("audit")`                             → warn + bump_audit_infra_errors
///   `Gated(_)`                                         → no-op
///   `Panic` | `ObserveErr(_)` | `EscalateErr(_)`
///     | `Timeout(other)` | `BudgetViolation(_)`        → record_failure
///
/// `Uninteresting` counts as a healthy tick: observe ran, interesting()
/// returned None — a successful pass deserving recovery credit. Audit-infra
/// errors do NOT punish the sentinel; they're operator-attention faults in
/// the SQLite audit pipeline that should not flip a healthy sentinel into
/// quarantine (P1-B wrong-invariant finding, supersedes original T33.5
/// wiring at runner.rs:182-190).
pub async fn handle_fire_outcome(
    outcome: FireOutcome,
    sentinel: &dyn Sentinel,
    quarantine: &QuarantineState,
) {
    // Arm order matters: `Timeout("audit")` must precede the `Timeout(_)`
    // catch-all in the record_failure arm so audit-infra routes correctly.
    match outcome {
        FireOutcome::Fired(_) | FireOutcome::Uninteresting => {
            quarantine
                .record_success(sentinel.tenant(), sentinel.name())
                .await;
        }
        FireOutcome::Gated(_) => {} // already gated — no state change.
        FireOutcome::AuditWriteErr(_)
        | FireOutcome::AuditWorkerCrashed
        | FireOutcome::Timeout("audit") => {
            warn!(
                sentinel = sentinel.name(),
                tenant = sentinel.tenant(),
                outcome = ?outcome,
                "watch::runner: audit-pipeline infrastructure error — NOT quarantining sentinel"
            );
            quarantine.bump_audit_infra_errors();
        }
        FireOutcome::Panic => {
            warn!(
                sentinel = sentinel.name(),
                tenant = sentinel.tenant(),
                "watch::runner: fire_pipeline panicked"
            );
            quarantine
                .record_failure(sentinel.tenant(), sentinel.name())
                .await;
        }
        FireOutcome::BudgetViolation(_)
        | FireOutcome::Timeout(_)
        | FireOutcome::ObserveErr(_)
        | FireOutcome::EscalateErr(_) => {
            quarantine
                .record_failure(sentinel.tenant(), sentinel.name())
                .await;
        }
    }
}

/// Background retry loop for records parked in
/// `pending_hard_kill_persist = Some(_)` limbo.
///
/// Why it exists: when `record_failure` crosses the hard-kill threshold and
/// `db.upsert_hard_kill` returns `Err`, the sentinel is left fail-closed via
/// `pending_hard_kill_persist`. Without a retry, that flag stays set forever
/// (admin clear is the only escape) and `gw_watch_pending_pending_records`
/// only grows.
///
/// Cadence: ticks every [`PENDING_RETRY_TICK`] (60s) with
/// [`MissedTickBehavior::Skip`] (P0-4) so a tick that overruns 60s does NOT
/// stack up a burst of replays — under DB recovery, burst behavior would
/// re-stampede every backed-up tick.
///
/// Per-tick work: snapshots up to [`MAX_RETRIES_PER_TICK`] (100, P1-2) pending
/// keys, calls `retry_pending_hard_kill_once` per key. Each call internally
/// wraps `upsert_hard_kill` in a 5s `tokio::time::timeout` (P0-3) and holds
/// the per-key op_lock for the duration; admin clear lands on the same lane.
///
/// Logging (P1-3): one structured WARN per tick **only when there is pending
/// work**, with summary counters; per-record outcomes are DEBUG to keep the
/// log volume bounded during a recovery wave.
async fn pending_retry_loop(quarantine: Arc<QuarantineState>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(PENDING_RETRY_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // First tick fires immediately by default; consume it so we wait one full
    // PENDING_RETRY_TICK before the first sweep. After a fresh process boot
    // no records can yet be in pending limbo (the flag is in-memory only and
    // is set inside `record_failure`'s Err arm), so the immediate tick would
    // only do an empty walk. Records that newly enter pending in the first
    // PENDING_RETRY_TICK after boot wait at most one tick — acceptable for
    // the fail-closed safety state.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                pending_retry_tick(&quarantine, &mut shutdown).await;
            }
        }
    }
}

/// One pass of the retry loop, factored out so the loop body and the test
/// harness can drive it the same way. Snapshots pending keys, retries each
/// up to [`MAX_RETRIES_PER_TICK`], emits one structured summary WARN if any
/// failures occurred (INFO when every attempt either persisted or hit an
/// admin-clear race — that's the recovery pattern, not an operator-attention
/// event).
///
/// **Shutdown** (adversarial-review hardening): the tick checks `shutdown`
/// between per-record retries so a worst-case shutdown is bounded to one
/// in-flight `retry_pending_hard_kill_once` (≤ `RETRY_DB_BUDGET` + worker
/// drain), not to `MAX_RETRIES_PER_TICK × RETRY_DB_BUDGET`.
///
/// `#[doc(hidden)] pub` so integration tests in `tests/watch_pending_retry.rs`
/// can drive the tick directly without waiting on the 60s interval — testing
/// the loop body without testing tokio's `Interval` is the goal.
#[doc(hidden)]
pub async fn pending_retry_tick(
    quarantine: &Arc<QuarantineState>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let snapshot_at_tick_start = quarantine.pending_snapshot();
    if snapshot_at_tick_start.count == 0 {
        // Common case — no pending, no work, no log line.
        return;
    }

    // Snapshot keys (cap to MAX_RETRIES_PER_TICK; remainder drains next tick).
    let mut keys = quarantine.pending_hard_kill_keys();
    keys.truncate(MAX_RETRIES_PER_TICK);

    let total_in_tick = keys.len();
    let mut attempted: usize = 0;
    let mut persisted: u64 = 0;
    let mut still_failing: u64 = 0;
    let mut timed_out: u64 = 0;
    let mut admin_cleared: u64 = 0;
    let mut shutdown_observed = false;

    for (tenant, sentinel) in keys {
        if *shutdown.borrow() {
            shutdown_observed = true;
            break;
        }
        attempted += 1;
        let outcome = quarantine
            .retry_pending_hard_kill_once(&tenant, &sentinel)
            .await;
        debug!(
            tenant = %tenant,
            sentinel = %sentinel,
            ?outcome,
            "watch::pending_retry: per-record outcome"
        );
        match outcome {
            RetryOutcome::Persisted => persisted += 1,
            RetryOutcome::StillFailing => still_failing += 1,
            RetryOutcome::TimedOut => timed_out += 1,
            RetryOutcome::AdminCleared | RetryOutcome::AdminClearedDuringRetry => {
                admin_cleared += 1;
            }
            RetryOutcome::NoDb => {
                // Loop is gated on has_db(); reaching this is a contract bug,
                // not normal flow. Count as still_failing so the operator
                // sees the anomaly via the WARN counters.
                still_failing += 1;
            }
        }
    }

    if still_failing + timed_out > 0 {
        // Real DB-side failures — operator attention. WARN level.
        warn!(
            pending_count = snapshot_at_tick_start.count,
            oldest_age_ms = snapshot_at_tick_start.oldest_age_ms,
            attempted,
            persisted,
            still_failing,
            timed_out,
            admin_cleared,
            shutdown = shutdown_observed,
            total_in_tick,
            "watch::pending_retry: tick summary (failures present)"
        );
    } else {
        // Every attempt either persisted or raced admin-clear. Normal recovery
        // pattern — INFO level keeps it out of oncall alert flow.
        info!(
            pending_count = snapshot_at_tick_start.count,
            oldest_age_ms = snapshot_at_tick_start.oldest_age_ms,
            attempted,
            persisted,
            admin_cleared,
            shutdown = shutdown_observed,
            total_in_tick,
            "watch::pending_retry: tick summary (recovery/clean)"
        );
    }
}

// ============================================================================
// Phase 1 CDC sweep (modeled exactly on pending_retry_loop / _tick per contract)
// Separate task, ticker Skip, shutdown watch, bounded per-record, outside hot path.
// D7 the invariant: watch_fires.envelope_json remains the immutable raw
// sentinel Escalation audit payload, but new CDC-produced pending_escalations
// rows default to an irin.comms.v0.1 EnvelopeWrapper. Compatibility shim:
// WATCH_CDC_EMIT_COMMS_ENVELOPE=0|false|legacy|raw preserves legacy raw enqueue
// for old fixtures or rollback. Exact transform for COMMS mode:
//   watch_fires.tenant                  -> envelope.data.tenant
//   watch_fires.sentinel                -> envelope.source urn + payload.sentinel_name
//   watch_fires.id                      -> payload.watch_fire_id
//   watch_fires.fired_at                -> payload.fired_at_ms
//   pending_escalations.id              -> payload.pending_escalation_id
//   causal_fire_id                      -> payload.causal_fire_id
//   watch_fires.envelope_json parsed    -> payload.raw_sentinel_escalation
// The council-triage consumer still treats envelope_json as untrusted data and
// takes id/tenant from pending_escalations columns (build_council_triage_user_prompt).
// Dedup is causal (ON CONFLICT in db helper). Boot re-scan on first tick (idempotent).
// Obs: sweep lag in logs + (future gauge); mpsc drop precedent noted in ticker.
// Gate default-OFF enforced at spawn site + harness.
// ============================================================================

/// Consecutive genuine-DB-error count on the SAME fire before the CDC producer
/// skips it (advances the cursor past it) instead of retrying forever. Bounds
/// the poison-row head-of-line block; skips are logged LOUD, never silent.
const POISON_SKIP_THRESHOLD: u32 = 3;

/// Pure decision for the poison-row guard: given the count of CONSECUTIVE
/// genuine enqueue failures on one fire id, should the CDC producer skip it
/// (advance the cursor past it) rather than retry forever? Extracted from the
/// `cdc_sweep_tick` Err arm purely so the head-of-line-block boundary is
/// unit-testable without a fault-injecting DB harness.
fn cdc_poison_should_skip(consecutive_fails: u32) -> bool {
    consecutive_fails >= POISON_SKIP_THRESHOLD
}

/// Process-global count of CDC poison-row skips.
/// Parity with the council money-path atomics (`persist_failures` /
/// `slow_mirror`): a non-zero value means the producer advanced its cursor PAST
/// a row after repeated DETERMINISTIC enqueue failures — an escalation was
/// dropped on a safety primitive and needs investigation. The Phase-6 watch
/// metrics surface (`gw_watch_*`) reads it via [`cdc_poison_skips`].
static CDC_POISON_SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Cumulative CDC poison-row skip count (A2). Exposed for the metrics surface.
pub fn cdc_poison_skips() -> u64 {
    CDC_POISON_SKIPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// True when a CDC enqueue error is transient/environmental (DB busy, locked,
/// disk IO, dropped connection) rather than a deterministic defect in the row's
/// own bytes. Transient errors must NEVER count toward the poison skip — that
/// would drop a real escalation on a safety primitive the moment the writer is
/// briefly contended (F6 — Hardening).
///
/// Fail-safe: anything we cannot positively attribute to a SQLite row-level
/// fault (unknown error shapes, connection-level `tokio_rusqlite` errors) is
/// treated as transient so the producer stalls-and-retries (loud, recoverable)
/// rather than dropping (silent, final).
fn cdc_error_is_transient(e: &anyhow::Error) -> bool {
    // tokio-rusqlite 0.7: `Error<E = rusqlite::Error>` with variants
    // `ConnectionClosed`, `Close((Connection, rusqlite::Error))`, `Error(E)`.
    match e.downcast_ref::<tokio_rusqlite::Error>() {
        Some(tokio_rusqlite::Error::Error(re)) => rusqlite_error_is_transient(re),
        // Mid-close failure carries the underlying rusqlite error; classify it.
        Some(tokio_rusqlite::Error::Close((_, re))) => rusqlite_error_is_transient(re),
        // The connection is gone — environmental, never a row defect.
        Some(tokio_rusqlite::Error::ConnectionClosed) => true,
        // `Error` is #[non_exhaustive]; any future variant is unattributable to
        // the row → fail-safe transient (stall-and-retry, never drop).
        Some(_) => true,
        // Not a tokio_rusqlite error at all — cannot attribute to the row.
        None => true,
    }
}

/// Classifies a raw `rusqlite::Error`. Only the SQLite codes that mean "try
/// again, the row is fine" are transient; every other shape (constraint not
/// covered by ON CONFLICT, type/encoding faults, oversized payload) is
/// deterministic for this row and MAY trip the poison skip.
fn rusqlite_error_is_transient(re: &rusqlite::Error) -> bool {
    match re {
        rusqlite::Error::SqliteFailure(err, _) => matches!(
            err.code,
            rusqlite::ErrorCode::DatabaseBusy
                | rusqlite::ErrorCode::DatabaseLocked
                | rusqlite::ErrorCode::SystemIoFailure
                | rusqlite::ErrorCode::OperationInterrupted
                | rusqlite::ErrorCode::CannotOpen
                | rusqlite::ErrorCode::DiskFull
        ),
        _ => false,
    }
}

fn cdc_pending_envelope_json(
    fire: &CommittedFire,
    pending_escalation_id: &str,
    causal_fire_id: &str,
    emit_comms_envelope: bool,
) -> String {
    if !emit_comms_envelope {
        return fire.envelope_json.clone();
    }

    let raw_sentinel_escalation = serde_json::from_str::<Value>(&fire.envelope_json)
        .unwrap_or_else(|_| Value::String(fire.envelope_json.clone()));
    let payload = serde_json::json!({
        "cdc_transform": "watch_fires_to_pending_escalations.v1",
        "watch_fire_id": fire.id,
        "sentinel_name": fire.sentinel,
        "fired_at_ms": fire.fired_at_ms,
        "pending_escalation_id": pending_escalation_id,
        "causal_fire_id": causal_fire_id,
        "raw_sentinel_escalation": raw_sentinel_escalation,
    });

    let envelope = match CommsEnvelope::builder(EnvelopeKind::Escalation)
        .sentinel_name(&fire.sentinel)
        .tenant(&fire.tenant)
        .ttl_seconds(CDC_ENVELOPE_TTL_SECONDS)
        .budget_hint(CDC_ENVELOPE_BUDGET_HINT)
        .reply_to(CDC_ENVELOPE_REPLY_TO)
        .data(payload)
        .build()
    {
        Ok(envelope) => envelope,
        Err(e) => {
            // T33: unreachable today — every required field is set above (three
            // via constants). If a refactor ever drops a setter, degrade to the
            // raw (pre-comms) envelope shape the pipeline already accepts (the
            // emit_comms_envelope=false path) instead of crashing the sidecar
            // mid-dispatch.
            CDC_ENVELOPE_BUILD_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                watch_fire_id = fire.id,
                error = %e,
                "CDC comms-envelope build failed; falling back to raw escalation envelope"
            );
            return fire.envelope_json.clone();
        }
    };
    let wrapper = envelope.wrap();

    serde_json::to_string(&wrapper).expect("CommsEnvelope serialization is infallible")
}

pub(crate) async fn cdc_sweep_loop(db: Arc<WatchDb>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(CDC_SWEEP_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume immediate first tick (like pending); boot re-scan happens on first real tick.
    ticker.tick().await;

    // Real advancing high-water for the cursor path (prevents starvation of older fires).
    // Advanced from the max id seen in each successful batch.
    let mut last_id: Option<i64> = None;

    // Per-fire consecutive-failure counts for poison-row detection. Persists
    // across ticks (process-memory only; a poison row that survives a restart
    // is re-tried then re-skipped — bounded, never wedged). Cleared per id on
    // a successful enqueue/dedup.
    let mut poison_fails: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                if let Some(max_id) = cdc_sweep_tick(&db, &mut shutdown, last_id, &mut poison_fails).await {
                    // Real advancement using high-water returned by tick (only advanced on Ok enqueues).
                    // First tick (None) reaches oldest via ASC; on Err returns prior so retry happens.
                    // Satisfies boot re-scan + no older-backlog starvation + P1 failed-enqueue rule.
                    last_id = Some(max_id);
                }
            }
        }
    }
}

/// One bounded sweep tick + boot re-scan (first tick after boot does recovery).
/// Computes causal from CommittedFire (state_json as payload or .payload extract
/// for SentinelState shape), attempts dedup insert via db helper (ON CONFLICT is
/// the exactly-once source of truth). Logs causal + lag for obs. Bounded work.
/// No manual INSERT in production path (D9 assertion).
#[doc(hidden)]
pub async fn cdc_sweep_tick(
    db: &Arc<WatchDb>,
    shutdown: &mut watch::Receiver<bool>,
    after_id: Option<i64>,
    poison_fails: &mut std::collections::HashMap<i64, u32>,
) -> Option<i64> {
    // a deposed producer — its writer
    // claim taken over while it was stalled (SIGSTOP > stale window, then
    // resumed) — could previously keep sweeping for up to one heartbeat
    // period before the watchdog self-disarmed. Re-check claim ownership at
    // the top of EVERY tick: a LIVE claim held by a DIFFERENT instance means
    // we were deposed — skip the tick (the heartbeat watchdog will
    // self-disarm shortly). No claim row (unarmed test/dev paths) or a
    // STALE foreign claim proceeds: refusing there would deadlock recovery.
    {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match db.writer_claim_holder().await {
            Ok(Some((holder_uuid, heartbeat_at_ms)))
                if holder_uuid != crate::watch::db::process_instance_uuid()
                    && heartbeat_at_ms >= now_ms - crate::watch::db::writer_claim_stale_ms() =>
            {
                warn!(
                    holder_uuid = %holder_uuid,
                    "watch::cdc_sweep: tick refused — writer claim held LIVE by another instance (deposed producer fencing, p07); awaiting watchdog self-disarm"
                );
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                // DB-unavailable = fail-closed (#13): cannot PROVE we hold
                // the claim, so do not sweep this tick.
                warn!(error = %e, "watch::cdc_sweep: tick refused — writer-claim ownership check failed (fail-closed)");
                return None;
            }
        }
    }

    // Real advancing cursor (design §4). The loop (and driving tests) pass the high-water from the
    // previous batch. This makes the "no starvation of older un-enqueued fires" behavior real,
    // not just an unused parameter. When after_id is Some, the query uses WHERE id > after_id ASC.
    let candidates = match db
        .get_recent_committed_fires(MAX_SWEEP_PER_TICK as i64, after_id)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "watch::cdc_sweep: query failed");
            return None;
        }
    };
    if candidates.is_empty() {
        return None;
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut enqueued: u64 = 0;
    let mut deduped: u64 = 0;
    let mut oldest_lag_ms: i64 = 0;
    let emit_comms_envelope = cdc_emit_comms_envelope_enabled();
    // High-water advanced only on Ok from insert (P1: never over failed enqueue).
    // On Err we break early and return prior high-water so next tick retries.
    // Retry/backoff applies to every attempted row, including errors.
    let mut high_water: Option<i64> = None;

    for fire in candidates.into_iter().take(MAX_SWEEP_PER_TICK) {
        if *shutdown.borrow() {
            break;
        }

        // Re-derive causal (pure, from stored state; matches simulate harness contract).
        // Per causal_fire_id.md invariants (stability across re-observations, no wall time in digest)
        // we deliberately exclude wall-clock `observed_at` (written by sentinels from SystemTime::now())
        // from the digest used for dedup. observed_at is treated as non-identifying metadata here.
        // Stable identity comes from tenant + sentinel + canonical payload content only.
        // This fixes duplicate logical fires collapsing correctly even when wall observed times differ.
        let state_val: Value = serde_json::from_str(&fire.state_json).unwrap_or(Value::Null);
        let payload = state_val
            .get("payload")
            .cloned()
            .unwrap_or(state_val.clone());

        let canonical_tenant = crate::watch::dispatcher::safe_tenant_token(&fire.tenant);
        // Note: observed_at intentionally *not* passed into digest (was causing non-stable causals for
        // real sentinel observations). Payload + sentinel + tenant provide the stable causal content.
        let digest = compute_content_digest(&fire.sentinel, &canonical_tenant, "", &payload);
        let causal = causal_fire_id(&canonical_tenant, &fire.sentinel, &digest);

        // Stable id derivation per phase1 spec §8 rec (causal-derived for roundtrip).
        let esc_id = format!("causal-{}", causal);
        let pending_envelope_json =
            cdc_pending_envelope_json(&fire, &esc_id, &causal, emit_comms_envelope);

        // The ON CONFLICT is the dedup + exactly-once (D9 proof).
        let replay_epoch = crate::watch::dispatcher::current_replay_epoch();
        match db
            .insert_pending_escalation_with_causal_dedup(
                &esc_id,
                &fire.tenant,
                &fire.sentinel,
                &pending_envelope_json,
                &causal,
                now_ms,
                replay_epoch,
            )
            .await
        {
            Ok(inserted) => {
                // Success clears any prior failure streak for this fire id.
                poison_fails.remove(&fire.id);
                if inserted {
                    enqueued += 1;
                    let lag = now_ms.saturating_sub(fire.fired_at_ms);
                    if lag > oldest_lag_ms {
                        oldest_lag_ms = lag;
                    }
                    debug!(
                        tenant = %fire.tenant,
                        sentinel = %fire.sentinel,
                        causal_fire_id = %causal,
                        lag_ms = lag,
                        "watch::cdc_sweep: enqueued (new causal)"
                    );
                } else {
                    deduped += 1;
                }
                // Advance cursor only on Ok (P1 fix): failed enqueues must not advance
                // so next tick retries the row (design retry/backoff @ phase1-producer-seam:58).
                high_water = Some(high_water.map_or(fire.id, |h| h.max(fire.id)));
            }
            Err(e) => {
                // Poison-row guard. The cursor re-queries oldest-first (WHERE id >
                // after_id ASC) every tick, so a row that always errors would
                // wedge the producer forever (head-of-line block). BUT a
                // transient/environmental error (DB busy, locked, IO, dropped
                // connection) is NOT the row's fault — counting it toward the
                // skip would DROP a real escalation on a safety primitive the
                // moment the writer is briefly contended. So:
                //   - transient  → break + retry next tick, DO NOT count, DO NOT
                //                   advance. (A stalled producer is loud and
                //                   recoverable; a dropped safety escalation is
                //                   silent and final.)
                //   - deterministic (the row's own bytes can't be inserted, e.g.
                //                   a constraint ON CONFLICT doesn't cover, an
                //                   encoding/size fault) → count; after
                //                   POISON_SKIP_THRESHOLD consecutive such
                //                   failures, advance the cursor PAST it (LOUD).
                // Errors we cannot positively classify as a row defect are
                // treated as transient — fail-safe: never drop on uncertainty.
                if cdc_error_is_transient(&e) {
                    warn!(
                        error = %e,
                        causal = %causal,
                        fire_id = fire.id,
                        "watch::cdc_sweep: transient enqueue error — will retry this row next tick (NOT counted toward poison skip)"
                    );
                    break;
                }
                let n = *poison_fails
                    .entry(fire.id)
                    .and_modify(|c| *c += 1)
                    .or_insert(1);
                if cdc_poison_should_skip(n) {
                    CDC_POISON_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    warn!(
                        error = %e,
                        causal = %causal,
                        fire_id = fire.id,
                        fails = n,
                        poison_skips_total = cdc_poison_skips(),
                        "watch::cdc_sweep: POISON ROW skipped after repeated DETERMINISTIC enqueue failures — advancing cursor past it (escalation dropped; investigate)"
                    );
                    poison_fails.remove(&fire.id);
                    high_water = Some(high_water.map_or(fire.id, |h| h.max(fire.id)));
                    continue;
                }
                warn!(
                    error = %e,
                    causal = %causal,
                    fire_id = fire.id,
                    fails = n,
                    "watch::cdc_sweep: deterministic enqueue failure — will retry this row next tick"
                );
                break;
            }
        }
    }

    if enqueued + deduped > 0 {
        info!(
            enqueued,
            deduped,
            oldest_lag_ms,
            "watch::cdc_sweep: tick summary (causal dedup exercised; bounded lag observable)"
        );
    }

    // Return the high-water (advanced only on Ok paths in this tick) or None.
    // On enqueue Err we broke early without advancing past it; caller keeps prior last_id.
    // The next tick retries the failed row using the returned or unchanged cursor.
    high_water
}

#[cfg(test)]
mod poison_tests {
    use super::*;

    /// The skip boundary: retry below the threshold, skip at-or-above it.
    /// A transient blip (1–2 fails) must NOT drop the escalation; a genuinely
    /// poisoned row (>=3) must be skipped so it cannot wedge the producer.
    #[test]
    fn poison_skip_boundary() {
        assert!(!cdc_poison_should_skip(0));
        assert!(!cdc_poison_should_skip(1));
        assert!(!cdc_poison_should_skip(POISON_SKIP_THRESHOLD - 1));
        assert!(cdc_poison_should_skip(POISON_SKIP_THRESHOLD));
        assert!(cdc_poison_should_skip(POISON_SKIP_THRESHOLD + 5));
    }

    /// Replays the exact `poison_fails` counting the Err arm performs: three
    /// CONSECUTIVE failures on the same id reach the skip decision, and a
    /// successful enqueue in between resets the count so a flaky (not poisoned)
    /// row is never skipped.
    #[test]
    fn poison_counting_requires_consecutive_failures() {
        let mut poison_fails: std::collections::HashMap<i64, u32> =
            std::collections::HashMap::new();
        let id = 42i64;

        // Two failures: still under threshold, producer would retry (break).
        for _ in 0..2 {
            let n = *poison_fails.entry(id).and_modify(|c| *c += 1).or_insert(1);
            assert!(!cdc_poison_should_skip(n));
        }

        // A successful enqueue clears the streak (Ok arm: poison_fails.remove).
        poison_fails.remove(&id);
        assert!(!poison_fails.contains_key(&id), "Ok resets the streak");

        // Now three consecutive failures → the third trips the skip.
        let mut tripped = false;
        for _ in 0..3 {
            let n = *poison_fails.entry(id).and_modify(|c| *c += 1).or_insert(1);
            if cdc_poison_should_skip(n) {
                tripped = true;
            }
        }
        assert!(tripped, "3 consecutive failures must trip the poison skip");
    }

    // ----- F6: transient-vs-deterministic error classification -----

    fn sqlite_failure(primary_code: i32) -> anyhow::Error {
        anyhow::Error::new(tokio_rusqlite::Error::Error(
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(primary_code), None),
        ))
    }

    /// SQLITE_BUSY / SQLITE_LOCKED / IO are transient — a briefly contended
    /// writer must NEVER be counted as poison and drop a safety escalation.
    #[test]
    fn transient_sqlite_errors_are_not_poison() {
        assert!(
            cdc_error_is_transient(&sqlite_failure(5)), // SQLITE_BUSY
            "SQLITE_BUSY must be transient"
        );
        assert!(
            cdc_error_is_transient(&sqlite_failure(6)), // SQLITE_LOCKED
            "SQLITE_LOCKED must be transient"
        );
        assert!(
            cdc_error_is_transient(&sqlite_failure(10)), // SQLITE_IOERR
            "SQLITE_IOERR must be transient"
        );
    }

    /// A constraint fault ON CONFLICT does not cover is deterministic for the
    /// row's bytes — retrying cannot help, so it MAY trip the poison skip.
    #[test]
    fn deterministic_row_errors_are_poison_eligible() {
        assert!(
            !cdc_error_is_transient(&sqlite_failure(19)), // SQLITE_CONSTRAINT
            "a constraint violation is deterministic for this row"
        );
    }

    /// Fail-safe: an error we cannot positively attribute to a SQLite row fault
    /// (here, a bare non-sqlite error) is treated as transient so the producer
    /// stalls-and-retries rather than dropping an escalation on uncertainty.
    #[test]
    fn unclassifiable_errors_fail_safe_to_transient() {
        let weird = anyhow::anyhow!("connection reset by peer");
        assert!(
            cdc_error_is_transient(&weird),
            "unknown errors must NOT drop escalations"
        );
    }

    /// The connection-level `tokio_rusqlite::Error::ConnectionClosed` is
    /// environmental, never a row defect.
    #[test]
    fn connection_closed_is_transient() {
        let e = anyhow::Error::new(tokio_rusqlite::Error::<rusqlite::Error>::ConnectionClosed);
        assert!(cdc_error_is_transient(&e));
    }

    // ----- A0: CDC producer gate is default-OFF (council blind-spot #1) -----

    /// The CDC sweep is spawned only when `has_db() && is_producer_gate_armed()`.
    /// This proves the gate predicate is OFF by default: it arms ONLY when the
    /// flag is explicitly truthy AND the gateway key is present. Any other
    /// combination — and crucially the absent-flag default — stays OFF, so the
    /// sweep cannot run UNARMED.
    #[test]
    fn producer_gate_is_default_off() {
        // Default / absent flag → OFF, even if the key happens to be present.
        assert!(
            !producer_gate_armed_from(None, true, Some("LIVE")),
            "absent flag must be OFF even with a key present"
        );
        assert!(!producer_gate_armed_from(None, false, Some("LIVE")));
        // Falsy / empty flag values → OFF.
        assert!(!producer_gate_armed_from(Some(""), true, Some("LIVE")));
        assert!(!producer_gate_armed_from(Some("0"), true, Some("LIVE")));
        assert!(!producer_gate_armed_from(Some("false"), true, Some("LIVE")));
        assert!(!producer_gate_armed_from(Some("no"), true, Some("LIVE")));
        // Truthy flag but NO key → still OFF (auth required).
        assert!(
            !producer_gate_armed_from(Some("1"), false, Some("LIVE")),
            "truthy flag without the gateway key must stay OFF"
        );
        assert!(!producer_gate_armed_from(Some("true"), false, Some("LIVE")));
        // EXECUTION_MODE must be exactly LIVE.
        assert!(!producer_gate_armed_from(Some("true"), true, None));
        assert!(!producer_gate_armed_from(Some("true"), true, Some("live")));
        assert!(!producer_gate_armed_from(Some("true"), true, Some("true")));
        assert!(!producer_gate_armed_from(Some("true"), true, Some("other")));
        // Armed ONLY when explicitly truthy, key present, and EXECUTION_MODE=LIVE.
        assert!(producer_gate_armed_from(Some("1"), true, Some("LIVE")));
        assert!(producer_gate_armed_from(Some("true"), true, Some("LIVE")));
        assert!(producer_gate_armed_from(Some("TRUE"), true, Some("LIVE")));
    }

    #[test]
    fn d7_emit_comms_envelope_flag_defaults_to_formal_envelope() {
        assert!(cdc_emit_comms_envelope_enabled_from(None));
        assert!(cdc_emit_comms_envelope_enabled_from(Some("1")));
        assert!(cdc_emit_comms_envelope_enabled_from(Some("true")));
        assert!(!cdc_emit_comms_envelope_enabled_from(Some("0")));
        assert!(!cdc_emit_comms_envelope_enabled_from(Some("false")));
        assert!(!cdc_emit_comms_envelope_enabled_from(Some("legacy")));
        assert!(!cdc_emit_comms_envelope_enabled_from(Some("raw")));
    }

    #[test]
    fn d7_legacy_mode_preserves_raw_envelope_json() {
        let fire = CommittedFire {
            id: 7,
            tenant: "sovereign".to_string(),
            sentinel: "file_inbox".to_string(),
            fired_at_ms: 1_717_000_000_000,
            state_json: "{}".to_string(),
            envelope_json: r#"{"reason":"legacy raw"}"#.to_string(),
        };

        assert_eq!(
            cdc_pending_envelope_json(&fire, "causal-abc", "abc", false),
            fire.envelope_json
        );
    }

    #[test]
    fn d7_comms_mode_wraps_raw_escalation_with_mapping() {
        let fire = CommittedFire {
            id: 7,
            tenant: "sovereign".to_string(),
            sentinel: "file_inbox".to_string(),
            fired_at_ms: 1_717_000_000_000,
            state_json: "{}".to_string(),
            envelope_json: r#"{"reason":"wrapped","payload":{"path":"/inbox/a.txt"}}"#.to_string(),
        };

        let wrapped = cdc_pending_envelope_json(&fire, "causal-abc", "abc", true);
        let v: Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(v["v"], serde_json::json!(1));
        assert_eq!(
            v["envelope"]["type"],
            serde_json::json!("irin.escalation.v0.1")
        );
        let data = &v["envelope"]["data"];
        assert_eq!(data["contract"], serde_json::json!("irin.comms.v0.1"));
        assert_eq!(data["kind"], serde_json::json!("Escalation"));
        assert_eq!(data["tenant"], serde_json::json!("sovereign"));
        assert_eq!(
            data["payload"]["cdc_transform"],
            serde_json::json!("watch_fires_to_pending_escalations.v1")
        );
        assert_eq!(data["payload"]["watch_fire_id"], serde_json::json!(7));
        assert_eq!(
            data["payload"]["sentinel_name"],
            serde_json::json!("file_inbox")
        );
        assert_eq!(
            data["payload"]["pending_escalation_id"],
            serde_json::json!("causal-abc")
        );
        assert_eq!(data["payload"]["causal_fire_id"], serde_json::json!("abc"));
        assert_eq!(
            data["payload"]["raw_sentinel_escalation"]["reason"],
            serde_json::json!("wrapped")
        );
    }
}
/// riders (B) — periodic phantom-claim sweep: the runtime caller for
/// `sweep_phantom_claims` (via the counted quarantine wrapper, so the p0b
/// `lease_expired_during_deliberation` counter and the orphan-charge recon
/// hint fire alongside the p0c reservation release). Cadence is half the
/// deliberation lease (LEASE_DURATION_MS/2 = 75s at the 150s default,
/// floored at 1s when tests compress WATCH_LEASE_DURATION_MS) so a dead
/// dispatcher's reservation is back in the day-cap budget within ~one lease
/// of its crash. Unlike pruning_loop, the FIRST tick fires immediately —
/// a boot sweep that reclaims phantoms left behind by a previous crash.
async fn phantom_sweep_loop(
    quarantine: Arc<QuarantineState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let period_ms = (crate::watch::db::lease_duration_ms() / 2).max(1_000) as u64;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(period_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // No leading ticker.tick() drain here (contrast pruning_loop): the
    // interval's immediate first tick IS the boot sweep.

    loop {
        tokio::select! {
            biased;
            // a dropped shutdown sender makes
            // changed() return Err immediately forever — treat it as
            // shutdown instead of spinning the loop hot.
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                match quarantine.sweep_phantom_claims_counted().await {
                    Ok(report) if report.swept > 0 => {
                        info!(
                            swept = report.swept,
                            in_flight_expired = report.in_flight_expired,
                            "watch::phantom_sweep reclaimed expired claim(s); reservations released"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("watch::phantom_sweep: sweep failed: {}", e);
                    }
                }
            }
        }
    }
}

async fn pruning_loop(
    db: Arc<crate::watch::db::WatchDb>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // 1 hour tick for pruning
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            // a dropped shutdown sender makes
            // changed() return Err immediately forever — treat it as
            // shutdown instead of spinning the loop hot.
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                // 7 days retention
                let older_than_ms = now_ms - (7 * 24 * 3600 * 1000);

                match db.prune_terminal_rows(older_than_ms).await {
                    Ok((pe_pruned, do_pruned, aged_staged_blocked)) => {
                        // W3 P1: loud retention signal — aged terminal parents
                        // still pinned by a live staged directive past retention.
                        // They deliberately do NOT prune (never delete a live
                        // staged child); their accumulation is the alarm, not a
                        // silent loss. Dead-letter transition = fast-follow #28.
                        if aged_staged_blocked > 0 {
                            tracing::warn!(
                                aged_staged_directive = aged_staged_blocked,
                                "watch::pruning: aged terminal escalations pinned by staged \
                                 directives past retention — not pruned (retention alarm)"
                            );
                        }
                        tracing::debug!(
                            pe_pruned,
                            do_pruned,
                            "watch::pruning: terminal rows pruned"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("watch::pruning: failed to prune terminal rows: {}", e);
                    }
                }
            }
        }
    }
}
