//! Pack lifecycle generation, presentation status cache, and live status probe.

use super::health::{desktop_project_running, gateway_health_ok, models_authenticated};
use super::install::{installed_pack_root, load_validated_manifest};
use super::paths::{bundled_pack_root, is_pack_installed};
use super::types::{GatewayPackState, GatewayPackStatus};
use crate::docker_cli::{probe_docker_daemon, DockerDaemonState};
use crate::keychain::{load_gw_api_key, SecretStore};
use crate::private_config::load_or_create_private_config;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Pack lifecycle generation: advances on enable/disable/stop/uninstall so an
/// in-flight Touch ID ceremony can fail closed if the pack identity changes
/// between stage and confirm. Independent of the presentation STATUS_CACHE
/// generation (which also bumps on route/spawn transitions).
static PACK_LIFECYCLE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Current pack lifecycle generation (non-secret, process-local).
pub fn pack_lifecycle_generation() -> u64 {
    PACK_LIFECYCLE_GENERATION.load(Ordering::SeqCst)
}

/// Advance the pack lifecycle generation. Called at the start of every pack
/// lifecycle mutation under the lifecycle lock.
pub fn bump_pack_lifecycle_generation() {
    PACK_LIFECYCLE_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Collapse concurrent pack-status probes (Settings pack card + Touch ID poll
/// fire on the same interval). A multi-second Docker/HTTP sample is fail-closed
/// but expensive; sharing one sample for a short window prevents probe storms
/// from flipping presentation every few seconds.
///
/// Presentation/status commands only. Authority paths (enroll, governed spawn,
/// phone publication, post-lifecycle checks) must call
/// [`gateway_pack_status_fresh`] so they never act on a cached sample.
///
/// Single-flight + generation-guarded: concurrent misses share one probe, and
/// a probe begun before [`invalidate_status_cache`] cannot commit afterward.
const STATUS_CACHE_TTL: Duration = Duration::from_secs(2);

struct StatusCacheSlot {
    generation: u64,
    cached: Option<(Instant, GatewayPackStatus)>,
    inflight: Option<(u64, Arc<SharedStatusProbe>)>,
}

struct SharedStatusProbe {
    state: Mutex<InflightProbeState>,
    cv: Condvar,
}

// Pre-existing shape (monolith); pure-move keeps the inline status sample.
#[allow(clippy::large_enum_variant)]
enum InflightProbeState {
    Running,
    Done(GatewayPackStatus),
}

impl StatusCacheSlot {
    const fn new() -> Self {
        Self {
            generation: 0,
            cached: None,
            inflight: None,
        }
    }
}

static STATUS_CACHE: Mutex<StatusCacheSlot> = Mutex::new(StatusCacheSlot::new());

/// Drop the cached status sample and bump generation so an in-flight probe
/// started before this call cannot repopulate the cache. Lifecycle mutations
/// and owned-Council-route transitions must call this.
pub fn invalidate_status_cache() {
    if let Ok(mut guard) = STATUS_CACHE.lock() {
        guard.generation = guard.generation.wrapping_add(1);
        guard.cached = None;
    }
}

/// Test-only: presentation cache generation counter (for Action vs Background
/// freshness proofs). Not a pack lifecycle generation.
#[cfg(test)]
pub fn status_cache_generation_for_test() -> u64 {
    STATUS_CACHE.lock().map(|g| g.generation).unwrap_or(0)
}

pub(crate) fn commit_status_cache(generation: u64, st: &GatewayPackStatus) {
    if let Ok(mut guard) = STATUS_CACHE.lock() {
        if guard.generation == generation {
            guard.cached = Some((Instant::now(), st.clone()));
        }
    }
}

/// Proven route of the Council child owned by this shell, recorded by lib.rs
/// at every spawn/stop/adopt/terminate transition:
/// - `Some(true)` — owned child was spawned governed (`COUNCIL_VIA_GATEWAY=1`
///   with the Keychain-held client key)
/// - `Some(false)` — owned child was spawned Direct
/// - `None` — no owned Council child (stopped, died, adopted-external, or
///   never spawned)
///
/// `council_governed` must be proven from this record plus live pack
/// authentication, never inferred from pack health + the persisted enabled
/// flag: those stay true after a failed governed restart or a Direct
/// relaunch while Council is in fact running Direct.
static OWNED_COUNCIL_ROUTE: Mutex<Option<bool>> = Mutex::new(None);

/// Record the owned Council child route. Called only from the shell's
/// spawn/stop paths in lib.rs; the renderer never reaches this.
///
/// Invalidates the status cache: route truth is a field of pack status, so a
/// spawn/stop/death transition must not leave a stale Degraded/Ready sample.
pub fn record_owned_council_route(route: Option<bool>) {
    if let Ok(mut guard) = OWNED_COUNCIL_ROUTE.lock() {
        *guard = route;
    }
    invalidate_status_cache();
}

/// The recorded route of the currently owned Council child, if any.
pub fn owned_council_route() -> Option<bool> {
    OWNED_COUNCIL_ROUTE.lock().ok().and_then(|g| *g)
}

pub fn gateway_pack_status(store: &dyn SecretStore) -> GatewayPackStatus {
    gateway_pack_status_cached(store, probe_live_status)
}

/// Uncached sample that also refreshes the presentation cache when the current
/// generation still matches. Used by enroll/arm, governed spawn/restart, phone
/// publication, and post-lifecycle checks.
pub fn gateway_pack_status_fresh(store: &dyn SecretStore) -> GatewayPackStatus {
    // Bump generation first so any probe begun before this call cannot commit.
    invalidate_status_cache();
    let generation = STATUS_CACHE.lock().map(|g| g.generation).unwrap_or(0);
    let st = probe_live_status(store);
    commit_status_cache(generation, &st);
    st
}

pub(crate) fn gateway_pack_status_cached(
    store: &dyn SecretStore,
    probe: fn(&dyn SecretStore) -> GatewayPackStatus,
) -> GatewayPackStatus {
    loop {
        let (generation, shared) = {
            let Ok(mut slot) = STATUS_CACHE.lock() else {
                return probe(store);
            };
            if let Some((at, st)) = &slot.cached {
                if at.elapsed() < STATUS_CACHE_TTL {
                    return st.clone();
                }
            }
            let generation = slot.generation;
            if let Some((inflight_gen, probe_shared)) = &slot.inflight {
                if *inflight_gen == generation {
                    let shared = Arc::clone(probe_shared);
                    drop(slot);
                    // Test-only: positive proof a caller joined the in-flight
                    // waiter branch (not inferred from sleep). No-op outside tests.
                    #[cfg(test)]
                    notify_test_inflight_waiter_joined(generation);
                    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    while matches!(*state, InflightProbeState::Running) {
                        state = shared.cv.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if let InflightProbeState::Done(st) = &*state {
                        let sample = st.clone();
                        drop(state);
                        // If generation advanced while we waited, re-enter.
                        if STATUS_CACHE
                            .lock()
                            .map(|s| s.generation == generation)
                            .unwrap_or(false)
                        {
                            return sample;
                        }
                        continue;
                    }
                    continue;
                }
            }
            let shared = Arc::new(SharedStatusProbe {
                state: Mutex::new(InflightProbeState::Running),
                cv: Condvar::new(),
            });
            slot.inflight = Some((generation, Arc::clone(&shared)));
            (generation, shared)
        };

        let st = probe(store);

        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            *state = InflightProbeState::Done(st.clone());
            shared.cv.notify_all();
        }

        // Commit only for the generation we probed under. If invalidation
        // advanced the generation mid-probe, clear our inflight slot and
        // re-enter — never return the pre-invalidation sample to the leader.
        let generation_still_current = if let Ok(mut slot) = STATUS_CACHE.lock() {
            let current = slot.generation == generation;
            if current {
                slot.cached = Some((Instant::now(), st.clone()));
            }
            if let Some((g, handle)) = &slot.inflight {
                if *g == generation && Arc::ptr_eq(handle, &shared) {
                    slot.inflight = None;
                }
            }
            current
        } else {
            // Lock poisoned: fall back to the live sample we just took.
            true
        };
        if generation_still_current {
            return st;
        }
        continue;
    }
}

pub(crate) fn probe_live_status(store: &dyn SecretStore) -> GatewayPackStatus {
    #[cfg(test)]
    if let Some(st) = test_status_probe_override() {
        return st;
    }
    gateway_pack_status_uncached(store)
}

#[cfg(test)]
static TEST_STATUS_PROBE: Mutex<Option<fn() -> GatewayPackStatus>> = Mutex::new(None);

/// Test-only sink: when set, each in-flight waiter join sends the generation
/// it joined. Used to positively synchronize concurrency tests before invalidate.
#[cfg(test)]
static TEST_INFLIGHT_WAITER_JOINED: Mutex<Option<std::sync::mpsc::Sender<u64>>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn test_status_probe_override() -> Option<GatewayPackStatus> {
    let probe = TEST_STATUS_PROBE.lock().ok().and_then(|g| *g);
    probe.map(|f| f())
}

#[cfg(test)]
pub(crate) fn notify_test_inflight_waiter_joined(generation: u64) {
    if let Ok(guard) = TEST_INFLIGHT_WAITER_JOINED.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(generation);
        }
    }
}

#[cfg(test)]
pub(crate) fn status_cache_test_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::private_config::test_env_lock()
}

#[cfg(test)]
pub(crate) fn with_test_status_probe<R>(
    probe: fn() -> GatewayPackStatus,
    body: impl FnOnce() -> R,
) -> R {
    let _serial = status_cache_test_lock();
    invalidate_status_cache();
    {
        let mut guard = TEST_STATUS_PROBE.lock().expect("test probe lock");
        *guard = Some(probe);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    {
        let mut guard = TEST_STATUS_PROBE.lock().expect("test probe lock");
        *guard = None;
    }
    // Always drop any waiter-join sink so a panicking test cannot leak into peers.
    if let Ok(mut guard) = TEST_INFLIGHT_WAITER_JOINED.lock() {
        *guard = None;
    }
    invalidate_status_cache();
    match result {
        Ok(v) => v,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

pub(crate) fn gateway_pack_status_uncached(store: &dyn SecretStore) -> GatewayPackStatus {
    let mut st = GatewayPackStatus::base(
        GatewayPackState::NotInstalled,
        "Gateway Pack is not installed. Core War Room works in Direct mode without Docker.",
    );

    let cfg = load_or_create_private_config().ok();
    if let Some(ref c) = cfg {
        st.enabled = c.via_gateway_default;
        st.key_id = c.gateway_key_id.clone();
        st.pack_version = c.gateway_pack_version.clone();
    }

    match probe_docker_daemon() {
        DockerDaemonState::CliMissing => {
            st.state = GatewayPackState::DockerMissing;
            st.docker = "cli_missing".into();
            st.message = "Docker CLI not found. Install Docker Desktop to use the optional Gateway Pack. Core War Room stays healthy in Direct mode.".into();
            st.refresh_predicates(false);
            return st;
        }
        DockerDaemonState::DaemonDown => {
            st.state = GatewayPackState::DockerDaemonDown;
            st.docker = "daemon_down".into();
            st.message = "Docker Desktop is installed but the daemon is not running. Open Docker Desktop, wait until it is ready, then retry. Core War Room stays healthy in Direct mode.".into();
            if is_pack_installed() {
                st.message.push_str(" Pack files are present on disk.");
            }
            st.refresh_predicates(false);
            return st;
        }
        DockerDaemonState::Ready => {
            st.docker = "ready".into();
        }
    }

    let pack_root = match installed_pack_root() {
        Some(p) => p,
        None => {
            // Bundled resources may exist; still not_installed until marker.
            st.state = GatewayPackState::NotInstalled;
            if bundled_pack_root().is_some() {
                st.message = "Gateway Pack assets are bundled but not installed. Use Enable Gateway to install into Application Support.".into();
            }
            st.refresh_predicates(false);
            return st;
        }
    };

    if let Ok(v) = load_validated_manifest(&pack_root) {
        st.pack_version = Some(v.pack_version.clone());
        st.manifest_mode = Some(v.mode.as_str().to_string());
    }

    let running = desktop_project_running();
    let health = gateway_health_ok();

    let key = load_gw_api_key(store).ok().flatten();
    let authenticated = key
        .as_ref()
        .map(|k| models_authenticated(k))
        .unwrap_or(false);
    st.authenticated = authenticated;

    // Council governed is proven from the owned child's recorded spawn route,
    // never inferred: Docker health + stored key + persisted flag stay true
    // after a failed governed restart or a relaunch that started Council
    // Direct, and must not be able to claim a governed route on their own.
    let proven_governed_child = owned_council_route() == Some(true);
    st.council_governed = st.enabled && authenticated && health && running && proven_governed_child;

    if !running {
        if cfg.as_ref().map(|c| c.via_gateway_default) == Some(true) {
            st.state = GatewayPackState::Degraded;
            st.message = "Gateway was enabled but the pack is not running. Start the pack or Disable Gateway for Direct mode.".into();
            st.council_governed = false;
        } else if is_pack_installed() {
            st.state = if cfg.as_ref().map(|c| c.via_gateway_default) == Some(false) {
                GatewayPackState::Disabled
            } else {
                GatewayPackState::InstalledStopped
            };
            st.message = "Gateway Pack is installed and stopped. Enable Gateway to start, provision, and authenticate.".into();
        } else {
            st.state = GatewayPackState::NotInstalled;
        }
        st.refresh_predicates(true);
        return st;
    }

    if !health {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway containers are up but /health failed. Check Docker logs for irin-desktop-gateway.".into();
        st.council_governed = false;
        st.refresh_predicates(false);
        return st;
    }

    if authenticated && st.enabled {
        if proven_governed_child {
            st.state = GatewayPackState::AuthenticatedReady;
            st.message = "Gateway Pack is authenticated and ready for governed proceedings.".into();
            st.council_governed = true;
        } else {
            // Pack-side auth is ready, but the owned Council child is not
            // proven governed (Direct, dead, or never spawned) — the pack
            // alone must not unlock governed proceedings.
            st.state = GatewayPackState::Degraded;
            st.message = "Gateway Pack is authenticated and enabled, but the owned Council child is not in a proven governed route. Use Enable Gateway to restart Council through the Gateway.".into();
            st.council_governed = false;
        }
    } else if authenticated && !st.enabled {
        st.state = GatewayPackState::Disabled;
        st.message =
            "Gateway is up with a stored key, but governed mode is disabled (Direct).".into();
        st.council_governed = false;
    } else if key.is_some() {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway is up but the stored client key failed /v1/models. Re-run Enable Gateway to re-provision.".into();
        st.council_governed = false;
    } else {
        st.state = GatewayPackState::Degraded;
        st.message =
            "Gateway is up but no client key is in Keychain. Run Enable Gateway to provision."
                .into();
        st.council_governed = false;
    }
    st.refresh_predicates(false);
    st
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
