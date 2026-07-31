//! Host-authoritative desktop status snapshot.
//!
//! The Tauri host is the single status authority for Gateway Pack, Touch ID,
//! and private phone access. Ordering is a property of the data: each committed
//! sample carries a process-local `authority_epoch` and a strictly monotonic
//! `seq` assigned under one lock that also covers input gathering. The
//! renderer only applies a sample when it is newer (`applyIfNewer`).

use crate::gateway_pack::{self, GatewayPackStatus};
use crate::keychain::KeychainSecretStore;
use crate::phone_access::{self, LiveTailscaleRunner, PhoneAccessStatus};
use crate::touch_id::{self, GatewayReadySticky, TouchIdStatus};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

/// Event name for host-pushed status snapshots.
pub const DESKTOP_STATUS_EVENT: &str = "desktop-status";

/// Background tick interval.
const BACKGROUND_TICK: Duration = Duration::from_secs(5);
/// Unconditional heartbeat emit interval (even when content is unchanged).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How the authority samples underlying systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Presentation path: warm pack cache + readiness sticky are allowed.
    Background,
    /// Fail-closed action path: bypasses the warm presentation cache.
    Action,
}

/// Committed, ordered snapshot of every Settings status domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopStatusSnapshot {
    /// Random per process; renderer resets its seq guard when this changes.
    pub authority_epoch: String,
    /// Strictly monotonic within the epoch, assigned under the authority lock.
    pub seq: u64,
    pub pack: GatewayPackStatus,
    pub touch_id: TouchIdStatus,
    pub phone: PhoneAccessStatus,
}

impl DesktopStatusSnapshot {
    /// Content equality used for emit-on-change (seq is intentionally excluded).
    pub fn content_eq(&self, other: &Self) -> bool {
        self.authority_epoch == other.authority_epoch
            && self.pack == other.pack
            && self.touch_id == other.touch_id
            && self.phone == other.phone
    }
}

struct AuthorityState {
    seq: u64,
    last: Option<DesktopStatusSnapshot>,
    last_emit_at: Option<Instant>,
    /// When an armed lease ends, the background loop wakes to repaint.
    armed_exp_at_ms: Option<i64>,
    computing: bool,
    dirty: bool,
}

struct SharedAuthority {
    epoch: String,
    state: Mutex<AuthorityState>,
    cv: Condvar,
    sticky: Mutex<GatewayReadySticky>,
    /// Single-flight wake for the background loop.
    wake: Condvar,
    wake_mutex: Mutex<()>,
    started: AtomicBool,
}

impl SharedAuthority {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            epoch: format!("{}-{:x}", std::process::id(), nanos),
            state: Mutex::new(AuthorityState {
                seq: 0,
                last: None,
                last_emit_at: None,
                armed_exp_at_ms: None,
                computing: false,
                dirty: false,
            }),
            cv: Condvar::new(),
            sticky: Mutex::new(GatewayReadySticky::new()),
            wake: Condvar::new(),
            wake_mutex: Mutex::new(()),
            started: AtomicBool::new(false),
        }
    }
}

static AUTHORITY: OnceLock<SharedAuthority> = OnceLock::new();

fn authority() -> &'static SharedAuthority {
    AUTHORITY.get_or_init(SharedAuthority::new)
}

/// Mark the background loop dirty so it recomputes soon (best-effort).
pub fn mark_dirty() {
    let auth = authority();
    if let Ok(mut guard) = auth.state.lock() {
        guard.dirty = true;
    }
    if let Ok(g) = auth.wake_mutex.lock() {
        drop(g);
        auth.wake.notify_all();
    }
}

/// Monotonic milliseconds since process start (sticky expiry must not use wall clock).
fn monotonic_ms() -> i64 {
    use std::sync::OnceLock as OL;
    static ORIGIN: OL<Instant> = OL::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn project_gateway_ready(sample: bool, hard_down: bool) -> bool {
    let auth = authority();
    let now_ms = monotonic_ms();
    match auth.sticky.lock() {
        Ok(mut guard) => guard.project(
            sample,
            hard_down,
            now_ms,
            GatewayReadySticky::DEFAULT_HOLD_MS,
        ),
        Err(_) => sample,
    }
}

/// Gather one ordered snapshot. The lock covers both gathering and seq assignment
/// so gather order == commit order == seq order.
pub fn recompute(app: &AppHandle, freshness: Freshness) -> DesktopStatusSnapshot {
    let auth = authority();
    let store = KeychainSecretStore;

    // Single-flight for Background: if another gather is in progress, wait for
    // it and return the committed sample (or re-run if marked dirty).
    // Action always waits for exclusive access, then gathers fresh.
    let mut guard = auth.state.lock().unwrap_or_else(|e| e.into_inner());
    while guard.computing {
        guard = auth.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
    }
    guard.computing = true;
    guard.dirty = false;
    let previous_touch_id = guard
        .last
        .as_ref()
        .map(|snapshot| snapshot.touch_id.clone());
    drop(guard);

    // --- gather (serialized by computing flag + re-lock at commit) ---
    let pack = match freshness {
        Freshness::Background => gateway_pack::gateway_pack_status(&store),
        Freshness::Action => gateway_pack::gateway_pack_status_fresh(&store),
    };
    let gateway_ready = match freshness {
        Freshness::Background => project_gateway_ready(pack.governed_ready, pack.hard_down),
        Freshness::Action => pack.governed_ready,
    };
    let touch_id = match freshness {
        Freshness::Background => {
            touch_id::touch_id_status_background(&store, gateway_ready, previous_touch_id.as_ref())
        }
        Freshness::Action => touch_id::touch_id_status(&store, gateway_ready),
    };
    let council_ready = council_backend_ready_for_status(app);
    let phone =
        phone_access::phone_access_status(&LiveTailscaleRunner, pack.enabled, council_ready);

    // --- commit under lock ---
    let mut guard = auth.state.lock().unwrap_or_else(|e| e.into_inner());
    guard.seq = guard.seq.saturating_add(1);
    let snap = DesktopStatusSnapshot {
        authority_epoch: auth.epoch.clone(),
        seq: guard.seq,
        pack,
        touch_id,
        phone,
    };
    guard.armed_exp_at_ms = snap.touch_id.armed_exp_at_ms.filter(|&ms| ms > 0);
    guard.last = Some(snap.clone());
    guard.computing = false;
    let was_dirty = guard.dirty;
    auth.cv.notify_all();
    drop(guard);

    if was_dirty {
        // Another request arrived mid-gather; schedule a follow-up.
        mark_dirty();
    }
    snap
}

/// Start the background status loop once per process. Safe to call multiple times.
pub fn start_background_loop(app: AppHandle) {
    let auth = authority();
    if auth
        .started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    std::thread::Builder::new()
        .name("desktop-status-authority".into())
        .spawn(move || background_loop(app))
        .ok();
}

fn background_loop(app: AppHandle) {
    let auth = authority();
    let mut last_emitted: Option<DesktopStatusSnapshot> = None;
    loop {
        let wait = next_wait_duration(auth);
        if let Ok(guard) = auth.wake_mutex.lock() {
            let _ = auth.wake.wait_timeout(guard, wait);
        }

        let snap = recompute(&app, Freshness::Background);
        let content_changed = last_emitted
            .as_ref()
            .map(|prev| !prev.content_eq(&snap))
            .unwrap_or(true);
        let heartbeat = {
            let guard = auth.state.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .last_emit_at
                .map(|t| Instant::now().duration_since(t) >= HEARTBEAT_INTERVAL)
                .unwrap_or(true)
        };
        if content_changed || heartbeat {
            if let Ok(mut guard) = auth.state.lock() {
                guard.last_emit_at = Some(Instant::now());
            }
            let _ = app.emit(DESKTOP_STATUS_EVENT, &snap);
            last_emitted = Some(snap);
        }
    }
}

fn next_wait_duration(auth: &SharedAuthority) -> Duration {
    let guard = match auth.state.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    if guard.dirty {
        return Duration::from_millis(50);
    }
    let mut wait = BACKGROUND_TICK;
    if let Some(exp_ms) = guard.armed_exp_at_ms {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if exp_ms > now_ms {
            let until = Duration::from_millis((exp_ms - now_ms) as u64);
            if until < wait {
                wait = until;
            }
        } else {
            // Already expired — recompute ASAP so armed→expired paints.
            wait = Duration::from_millis(50);
        }
    }
    wait
}

fn council_backend_ready_for_status(app: &AppHandle) -> bool {
    // Mirror lib.rs::council_backend_ready without circular private access:
    // probe via the same spawn-config + build identity path when available.
    crate::council_backend_ready_probe(app)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_pack::GatewayPackState;
    use crate::phone_access::PhoneAccessState;
    use crate::touch_id::{TouchIdReason, TouchIdState};
    use std::sync::Arc;
    use std::thread;

    fn sample_pack(state: GatewayPackState) -> GatewayPackStatus {
        let mut st = GatewayPackStatus {
            state,
            message: "test".into(),
            pack_version: None,
            manifest_mode: None,
            gateway_url: "http://127.0.0.1:18080".into(),
            project: "irin-desktop-gateway".into(),
            key_id: None,
            enabled: matches!(state, GatewayPackState::AuthenticatedReady),
            docker: "ready".into(),
            watch_producer_enabled: false,
            watch_dispatcher_enabled: false,
            authenticated: matches!(state, GatewayPackState::AuthenticatedReady),
            council_governed: matches!(state, GatewayPackState::AuthenticatedReady),
            gateway_url_configured: true,
            support_matrix_summary: String::new(),
            spawn_capable: matches!(state, GatewayPackState::AuthenticatedReady),
            governed_ready: matches!(state, GatewayPackState::AuthenticatedReady),
            hard_down: !matches!(
                state,
                GatewayPackState::AuthenticatedReady | GatewayPackState::Degraded
            ),
        };
        // refresh_predicates is private; fields are set explicitly above.
        let _ = &mut st;
        st
    }

    fn sample_touch(state: TouchIdState) -> TouchIdStatus {
        TouchIdStatus {
            state,
            reason: None,
            armed_exp_at_ms: None,
            armed_expires_in_ms: None,
            stage_expires_in_ms: None,
            enrolled: true,
            allow_real_arm: true,
            can_enroll: false,
            can_arm: state == TouchIdState::Ready,
            can_renew: state == TouchIdState::Armed,
            can_disarm: state == TouchIdState::Armed,
            rehearsal_passed: false,
        }
    }

    fn sample_phone(state: PhoneAccessState) -> PhoneAccessStatus {
        PhoneAccessStatus {
            state,
            message: "test".into(),
            tailnet_url: None,
            enabled: state == PhoneAccessState::Ready,
            ownership: "none".into(),
            interrupted: false,
            gateway_routes: false,
            funnel_present: false,
        }
    }

    /// Commit a synthetic snapshot under the same lock/seq rules as recompute.
    fn commit_synthetic(
        auth: &SharedAuthority,
        pack: GatewayPackStatus,
        touch: TouchIdStatus,
        phone: PhoneAccessStatus,
    ) -> DesktopStatusSnapshot {
        let mut guard = auth.state.lock().unwrap();
        while guard.computing {
            guard = auth.cv.wait(guard).unwrap();
        }
        guard.computing = true;
        // Simulate gather holding the serial section.
        drop(guard);
        let mut guard = auth.state.lock().unwrap();
        guard.seq = guard.seq.saturating_add(1);
        let snap = DesktopStatusSnapshot {
            authority_epoch: auth.epoch.clone(),
            seq: guard.seq,
            pack,
            touch_id: touch,
            phone,
        };
        guard.armed_exp_at_ms = snap.touch_id.armed_exp_at_ms;
        guard.last = Some(snap.clone());
        guard.computing = false;
        auth.cv.notify_all();
        snap
    }

    #[test]
    fn seq_is_strictly_monotonic_under_concurrent_commit() {
        let auth = Arc::new(SharedAuthority::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let a = Arc::clone(&auth);
            handles.push(thread::spawn(move || {
                commit_synthetic(
                    &a,
                    sample_pack(GatewayPackState::Disabled),
                    sample_touch(TouchIdState::Ready),
                    sample_phone(PhoneAccessState::Off),
                )
                .seq
            }));
        }
        let mut seqs: Vec<u64> = handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
        assert_eq!(auth.state.lock().unwrap().seq, 8);
    }

    #[test]
    fn gather_under_lock_orders_commits() {
        let auth = SharedAuthority::new();
        // Two serial commits: second must have higher seq and replace last.
        let a = commit_synthetic(
            &auth,
            sample_pack(GatewayPackState::Disabled),
            sample_touch(TouchIdState::Ready),
            sample_phone(PhoneAccessState::Off),
        );
        let mut touch = sample_touch(TouchIdState::Armed);
        touch.armed_exp_at_ms = Some(9_999_999_999_999);
        let b = commit_synthetic(
            &auth,
            sample_pack(GatewayPackState::AuthenticatedReady),
            touch,
            sample_phone(PhoneAccessState::Ready),
        );
        assert!(b.seq > a.seq);
        let last = auth.state.lock().unwrap().last.clone().unwrap();
        assert_eq!(last.seq, b.seq);
        assert_eq!(last.touch_id.state, TouchIdState::Armed);
        assert_eq!(
            auth.state.lock().unwrap().armed_exp_at_ms,
            Some(9_999_999_999_999)
        );
    }

    #[test]
    fn content_eq_excludes_seq() {
        let mut a = DesktopStatusSnapshot {
            authority_epoch: "e".into(),
            seq: 1,
            pack: sample_pack(GatewayPackState::Disabled),
            touch_id: sample_touch(TouchIdState::Ready),
            phone: sample_phone(PhoneAccessState::Off),
        };
        let mut b = a.clone();
        b.seq = 99;
        assert!(
            a.content_eq(&b),
            "seq alone must not count as content change"
        );
        b.touch_id.state = TouchIdState::Armed;
        assert!(!a.content_eq(&b));
        a.touch_id.reason = Some(TouchIdReason::LeaseExpired);
        assert!(!a.content_eq(&b));
    }

    #[test]
    fn snapshot_carries_no_secret_fields() {
        let snap = DesktopStatusSnapshot {
            authority_epoch: "epoch".into(),
            seq: 1,
            pack: sample_pack(GatewayPackState::AuthenticatedReady),
            touch_id: {
                let mut t = sample_touch(TouchIdState::Armed);
                t.armed_exp_at_ms = Some(1_700_000_000_000);
                t.armed_expires_in_ms = Some(60_000);
                t
            },
            phone: sample_phone(PhoneAccessState::Ready),
        };
        let json = serde_json::to_string(&snap).unwrap();
        for field in [
            "\"challenge\"",
            "\"signature\"",
            "\"credential_id\"",
            "\"public_key\"",
            "\"keyset_hash\"",
            "\"token\"",
            "\"admin_token\"",
            "\"principal\"",
            "\"attestation\"",
            "\"authenticator_data\"",
            "\"helper_sha256\"",
            "\"private_key\"",
            "\"api_key\"",
            "\"bearer\"",
            "GW_API_KEY",
        ] {
            assert!(
                !json.contains(field),
                "snapshot must not carry {field}: {json}"
            );
        }
    }

    #[test]
    fn action_freshness_invalidates_presentation_cache_generation() {
        // Action path uses gateway_pack_status_fresh, which bumps generation
        // before probing so a warm sample cannot be acted on. Prove the
        // generation contract without running a live Docker probe.
        let before = gateway_pack::status_cache_generation_for_test();
        gateway_pack::invalidate_status_cache();
        let after = gateway_pack::status_cache_generation_for_test();
        assert!(
            after > before,
            "fresh/action sampling invalidates presentation cache ({before} → {after})"
        );
        // Freshness discriminants used by recompute.
        assert_ne!(Freshness::Background, Freshness::Action);
        // recompute(Action) is the only path that must call status_fresh;
        // recompute(Background) uses the cached presentation path. Source
        // contract is held by the match arms in recompute().
        let source = include_str!("status_authority.rs");
        assert!(
            source.contains("Freshness::Action => gateway_pack::gateway_pack_status_fresh"),
            "Action freshness must call gateway_pack_status_fresh"
        );
        assert!(
            source.contains("Freshness::Background => gateway_pack::gateway_pack_status("),
            "Background freshness must call gateway_pack_status (cached)"
        );
    }
}
