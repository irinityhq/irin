//! Writer-claim heartbeat and fail-closed self-disarm on claim loss.

use crate::watch::db::WatchDb;
use crate::watch::quarantine::QuarantineState;
use std::sync::Arc;

use super::arming::append_arm_audit_best_effort;

/// single-writer (single-writer invariant) — one writer-claim heartbeat step: refresh
/// our liveness; on loss (UPDATE affected 0 rows — another instance took
/// over) OR on DB error (#13 DB-unavailable = fail-closed: a writer that
/// cannot PROVE it still holds the claim must stop writing), self-disarm.
/// Returns `true` when the claim is still held.
pub async fn writer_claim_heartbeat_step(
    quarantine: &Arc<QuarantineState>,
    db: &WatchDb,
    uuid: &str,
    now_ms: i64,
) -> bool {
    match db.heartbeat_writer_claim(uuid, now_ms).await {
        Ok(true) => true,
        Ok(false) => {
            tracing::error!(
                instance_uuid = uuid,
                "writer claim LOST (heartbeat affected 0 rows — taken over by another instance); self-disarming (fail-closed)"
            );
            self_disarm_on_lost_writer_claim(
                quarantine,
                "writer claim lost (heartbeat affected 0 rows)",
            )
            .await;
            false
        }
        Err(e) => {
            tracing::error!(
                instance_uuid = uuid,
                error = %e,
                "writer claim heartbeat FAILED (DB error — cannot prove claim is held); self-disarming (fail-closed, #13)"
            );
            self_disarm_on_lost_writer_claim(
                quarantine,
                "writer claim heartbeat DB error (fail-closed)",
            )
            .await;
            false
        }
    }
}

/// single-writer (single-writer invariant) — RELEASE our writer claim on a GRACEFUL
/// heartbeat-loop exit (shutdown signal or disarm) so a restart inside the
/// stale window can re-acquire immediately instead of waiting out
/// `WRITER_CLAIM_STALE_MS` (the CI smoke lifecycle gap — CI regression:
/// the recreated sidecar saw the prior instance's still-"live" claim and
/// correctly refused, bricking the producer for 90s). Best-effort: a release
/// failure is logged but NEVER blocks shutdown. Fencing-safe by construction —
/// `release_writer_claim` only deletes a row whose `instance_uuid` is ours, so
/// if a successor has already taken over (the self-disarm-on-lost-claim path),
/// the DELETE matches 0 rows and the successor's claim survives. This helper
/// is therefore deliberately NOT called from the claim-lost exit; calling it
/// there would still be a no-op thanks to the uuid guard, but we don't, so the
/// intent stays explicit.
async fn writer_claim_release_step(db: &WatchDb, uuid: &str) {
    match db.release_writer_claim(uuid).await {
        Ok(true) => {
            tracing::info!(
                instance_uuid = uuid,
                "writer claim released on graceful shutdown (successor can re-acquire without the stale wait)"
            );
        }
        Ok(false) => {
            // Already taken over / never held — fencing-safe no-op. Not an
            // error: the self-disarm path or a prior takeover already cleared
            // our ownership.
            tracing::debug!(
                instance_uuid = uuid,
                "writer claim release affected 0 rows (already taken over or not held) — no-op"
            );
        }
        Err(e) => {
            // Best-effort: a release failure must not block shutdown. The
            // stale-takeover predicate is the fallback (same as crash).
            tracing::warn!(
                instance_uuid = uuid,
                error = %e,
                "writer claim release FAILED (best-effort); restart will fall back to the stale window"
            );
        }
    }
}

/// single-writer — fail-closed self-disarm when the writer claim is
/// lost. Same drain mechanics as `admin_disarm_producer_json`, but
/// triggered by the heartbeat watchdog rather than an operator: audit the
/// kill intent (best-effort, never blocks the kill), take the kill state,
/// signal the drain, await the ack with the same 5s bound. Idempotent —
/// an already-disarmed producer is a no-op.
async fn self_disarm_on_lost_writer_claim(quarantine: &Arc<QuarantineState>, why: &str) {
    append_arm_audit_best_effort(
        quarantine,
        "disarm",
        "writer-claim-watchdog(self)",
        &format!("self-disarm: {why}"),
    )
    .await;

    let state = quarantine.producer_kill_state.lock().take();
    if let Some((tx, ack_rx)) = state {
        // the
        // self-disarm drain is part of the SAME population the telemetry and single-writer invariants
        // kill_switch_latency_max_ms / kill_switch_drain_timeout_total series
        // (and the max-loss derivation in the arming runbook) must see. A
        // split-brain recovery drain that hits the 5s timeout is exactly the
        // worst case that bounds single-writer max-loss — it must not be
        // invisible on /watch/stats.
        let kill_sent_at = std::time::Instant::now();
        if tx.send(true).is_err() {
            tracing::error!("self-disarm: kill channel dropped — CDC producer already gone");
            return;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), ack_rx).await {
            Ok(Ok(_)) => {
                let drain_ms = (kill_sent_at.elapsed().as_millis() as u64).max(1);
                quarantine.record_kill_switch_latency_ms(drain_ms);
                tracing::warn!(
                    drain_ms,
                    "self-disarm complete: CDC producer drained after writer-claim loss"
                );
            }
            Ok(Err(_)) => {
                let crash_ms = (kill_sent_at.elapsed().as_millis() as u64).max(1);
                quarantine.record_kill_switch_latency_ms(crash_ms);
                tracing::error!(
                    crash_ms,
                    "self-disarm: producer dropped ack channel without completing drain"
                );
            }
            Err(_) => {
                quarantine.record_kill_switch_drain_timeout(5_000);
                tracing::error!("self-disarm: producer drain timed out after 5 seconds (kill_switch_drain_timeout_total bumped; 5000ms floor recorded)");
            }
        }
    }
}

/// single-writer — the heartbeat watchdog loop (pruning_loop pattern):
/// every `period`, refresh the claim via [`writer_claim_heartbeat_step`];
/// exit when the claim is lost (step already self-disarmed), when the
/// producer is disarmed (stop refreshing so a takeover by another instance
/// can happen after the stale window), or on `shutdown` (runner path).
pub async fn writer_claim_heartbeat_loop(
    quarantine: Arc<QuarantineState>,
    db: std::sync::Arc<WatchDb>,
    uuid: String,
    period: std::time::Duration,
    mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) {
    let stale_ms = crate::watch::db::writer_claim_stale_ms();
    if stale_ms < 3 * period.as_millis() as i64 {
        tracing::warn!(
            stale_ms,
            heartbeat_ms = period.as_millis() as u64,
            "WRITER_CLAIM_STALE_MS < 3x heartbeat interval — a single slow tick risks losing a LIVE claim; raise the stale window"
        );
    }
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // consume the immediate first tick — first refresh after one period

    loop {
        tokio::select! {
            biased;
            _ = async {
                match shutdown.as_mut() {
                    Some(rx) => { let _ = rx.changed().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                if shutdown.as_ref().map(|rx| *rx.borrow()).unwrap_or(false) {
                    // GRACEFUL shutdown (runner path): release our claim so a
                    // restart inside the stale window re-acquires immediately
                    // (smoke lifecycle fix — CI regression). Best-effort.
                    writer_claim_release_step(&db, &uuid).await;
                    return;
                }
            }
            _ = ticker.tick() => {
                if quarantine.producer_kill_state.lock().is_none() {
                    // Disarmed (operator or self) — stop refreshing so the
                    // claim goes stale and another instance can take over.
                    // GRACEFUL: release our claim now so the successor does not
                    // wait out the stale window. The release is uuid-fenced, so
                    // a self-disarm-on-lost-claim (successor already took over)
                    // matches 0 rows and the successor's claim survives.
                    writer_claim_release_step(&db, &uuid).await;
                    return;
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if !writer_claim_heartbeat_step(&quarantine, &db, &uuid, now_ms).await {
                    // CLAIM LOST (taken over by a successor): the claim now
                    // belongs to that successor — we MUST NOT release it (a
                    // fencing violation). The step already self-disarmed.
                    return; // lost — step already self-disarmed (fail-closed)
                }
            }
        }
    }
}
