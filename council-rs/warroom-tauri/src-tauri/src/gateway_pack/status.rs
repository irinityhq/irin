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


/// Serialize tests that observe or bump the process-global pack lifecycle
/// generation. Shared by launch_tests and enable_tests in the same lib test binary.
#[cfg(test)]
pub fn lifecycle_gen_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap()
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

/// Derived Keychain-key auth observation for the *presentation* status path.
///
/// The status background loop ticks every 5s and `STATUS_CACHE` TTL is 2s, so
/// without this guard `load_gw_api_key` would re-enter Keychain ~12×/min while
/// the app is open — each read can surface a macOS authorization dialog under
/// the signed DMG's ACL policy. This cache memoizes the *derived* observation
/// `{ key_present, authenticated }`, never the raw key value.
///
/// Lifecycle-invalidated only (no TTL): once committed, the observation
/// persists until [`invalidate_auth_observation`] fires at a credential or
/// pack-lifecycle mutation. This bounds Keychain reads to one per
/// first-access, then zero until enable/disable/uninstall — the presentation
/// path never re-reads on its own. Authority paths bypass this cache entirely
/// via [`gateway_pack_status_fresh`] / [`gateway_pack_status_fresh_with_key`]
/// (see [`AuthProbeMode`]).
///
/// Generation-guarded: a probe that read an older generation cannot commit a
/// stale observation back over a fresher cache (the key may have been rotated
/// by enable/uninstall between read and commit). Concurrency is handled by the
/// outer `STATUS_CACHE` single-flight, which serializes callers of
/// `gateway_pack_status_uncached`; this cache is a plain guarded read/write.
static AUTH_OBSERVATION_GENERATION: AtomicU64 = AtomicU64::new(1);
static AUTH_OBSERVATION: Mutex<Option<AuthObservation>> = Mutex::new(None);

struct AuthObservation {
    #[allow(dead_code)]
    generation: u64,
    key_present: bool,
    authenticated: bool,
}

/// Invalidate the auth observation cache. Call only at genuine credential or
/// pack-lifecycle mutations (enable/disable/stop/uninstall, post-migration) —
/// not on every fresh status sample or route transition, which fire far more
/// often than the key actually changes.
pub fn invalidate_auth_observation() {
    AUTH_OBSERVATION_GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut guard) = AUTH_OBSERVATION.lock() {
        *guard = None;
    }
}

/// Present-cached auth observation if one has been committed and not yet
/// invalidated. Lifecycle-scoped — no TTL expiry.
fn cached_auth_observation() -> Option<(bool, bool)> {
    let guard = AUTH_OBSERVATION.lock().ok()?;
    let obs = guard.as_ref()?;
    Some((obs.key_present, obs.authenticated))
}

/// Commit a fresh observation iff the generation still matches the one read at
/// probe start (the key cannot have been rotated mid-probe without us seeing a
/// newer generation). Returns true if committed, false if a concurrent
/// invalidation advanced the generation mid-probe.
fn commit_auth_observation(generation: u64, key_present: bool, authenticated: bool) -> bool {
    if let Ok(mut guard) = AUTH_OBSERVATION.lock() {
        let current = AUTH_OBSERVATION_GENERATION.load(Ordering::SeqCst);
        if current != generation {
            return false;
        }
        *guard = Some(AuthObservation {
            generation,
            key_present,
            authenticated,
        });
        true
    } else {
        false
    }
}

/// Current auth-observation generation (non-secret, process-local).
///
/// Capture this at the same time as a cold-launch Keychain flight and pass it
/// into [`seed_auth_observation_from_preloaded_key`] so a concurrent
/// enable/disable/uninstall invalidation cannot commit the old key under a
/// newer generation.
pub fn auth_observation_generation() -> u64 {
    AUTH_OBSERVATION_GENERATION.load(Ordering::SeqCst)
}

/// Seed the background presentation cache from the GW key already owned by a
/// cold-launch flight. Never re-enters Keychain.
///
/// `preload_generation` must be the generation captured when the key was
/// loaded. If lifecycle invalidation advanced generation since then, this is a
/// no-op (stale key must not overwrite a fresher cache).
///
/// Commits a **provisional** observation (key presence, `authenticated=false`)
/// before the live `/v1/models` probe so a concurrent Background tick cannot
/// re-get `GW_API_KEY` while the HTTP probe is in flight. The provisional is
/// then upgraded to the live result under the same generation.
pub(crate) fn seed_auth_observation_from_preloaded_key(key: Option<&str>, preload_generation: u64) {
    if AUTH_OBSERVATION_GENERATION.load(Ordering::SeqCst) != preload_generation {
        return;
    }
    let key_present = key.is_some();
    // Provisional: presence alone closes the Keychain race during the probe.
    let _ = commit_auth_observation(preload_generation, key_present, false);
    let authenticated = key.map(models_authenticated).unwrap_or(false);
    let _ = commit_auth_observation(preload_generation, key_present, authenticated);
}

#[cfg(test)]
fn auth_observation_present_for_test() -> bool {
    AUTH_OBSERVATION
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
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
/// at every spawn/stop/terminate transition:
/// - `Some(true)` — owned child was spawned governed (`COUNCIL_VIA_GATEWAY=1`
///   with the Keychain-held client key)
/// - `Some(false)` — owned child was spawned Direct
/// - `None` — no owned Council child (stopped, died, never started, or
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
    gateway_pack_status_cached(store, |s| {
        probe_live_status(s, AuthProbeMode::BackgroundCached)
    })
}

/// Uncached, **live-auth** sample. Used by authority paths: enroll/arm, governed
/// spawn/restart, phone publication, and post-lifecycle checks. Never serves
/// from the auth-observation cache — the Touch ID arm/renew ceremony and the
/// governed-spawn gate must see the real `/v1/models` result at decision time.
///
/// Loads `GW_API_KEY` once from Keychain and threads it through the probe. When
/// the caller already holds the key (governed spawn, resume flight), use
/// [`gateway_pack_status_fresh_with_key`] to avoid the redundant Keychain get.
///
/// A missing or failed Keychain read produces `AuthorityLive(None)`, which
/// returns unauthenticated without consulting the cache — a cached
/// `authenticated=true` from a prior background probe must never authorize a
/// ceremony after the key is gone.
pub fn gateway_pack_status_fresh(store: &dyn SecretStore) -> GatewayPackStatus {
    with_loaded_authority_mode(store, |mode| {
        gateway_pack_status_fresh_with_mode(store, mode)
    })
}

/// Load the authority key exactly once, convert absent/unreadable results to
/// the fail-closed authority mode, and keep the owned key alive for the probe.
/// Tests use this same seam to prove real Keychain failures cannot fall back to
/// the background auth-observation cache.
fn with_loaded_authority_mode<R>(
    store: &dyn SecretStore,
    probe: impl FnOnce(AuthProbeMode<'_>) -> R,
) -> R {
    let held = load_gw_api_key(store).ok().flatten();
    probe(AuthProbeMode::AuthorityLive(held.as_deref()))
}

/// Fresh sample using a caller-held client key (no Keychain re-entry for
/// `GW_API_KEY`). Mirrors the `pack_auth_revalidated` / `_with_key` pair: when
/// the spawn or resume flight already loaded the key, pass `Some(key)` so the
/// fresh auth probe skips a redundant Keychain get.
///
/// Authority path: always performs live `/v1/models` authentication — never
/// the auth-observation cache.
pub fn gateway_pack_status_fresh_with_key(
    store: &dyn SecretStore,
    held_key: Option<&str>,
) -> GatewayPackStatus {
    gateway_pack_status_fresh_with_mode(store, AuthProbeMode::AuthorityLive(held_key))
}

fn gateway_pack_status_fresh_with_mode(
    store: &dyn SecretStore,
    mode: AuthProbeMode<'_>,
) -> GatewayPackStatus {
    // Bump generation first so any probe begun before this call cannot commit.
    invalidate_status_cache();
    let generation = STATUS_CACHE.lock().map(|g| g.generation).unwrap_or(0);
    let st = probe_live_status(store, mode);
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

pub(crate) fn probe_live_status(store: &dyn SecretStore, mode: AuthProbeMode) -> GatewayPackStatus {
    #[cfg(test)]
    if let Some(st) = test_status_probe_override() {
        return st;
    }
    gateway_pack_status_uncached_with_key(store, mode)
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

#[allow(dead_code)] // retained for status-loop test probes with no held key
pub(crate) fn gateway_pack_status_uncached(store: &dyn SecretStore) -> GatewayPackStatus {
    gateway_pack_status_uncached_with_key(store, AuthProbeMode::BackgroundCached)
}

/// How the status gather should resolve `{ key_present, authenticated }`.
///
/// The distinction is load-bearing: the **background** presentation path may
/// serve from the lifecycle-scoped auth-observation cache; the **authority**
/// path (Touch ID arm/renew, governed spawn, post-lifecycle checks) must
/// always run `/v1/models` live and must **never** fall back to cached auth —
/// a cached `true` must not authorize a ceremony after the key disappears or
/// becomes unreadable.
///
/// `AuthorityLive(None)` explicitly means "the authority path tried to load
/// the key and it was absent or unreadable" — it returns unauthenticated
/// without consulting the cache, unlike `BackgroundCached`.
pub(crate) enum AuthProbeMode<'a> {
    /// Background presentation path: consult the lifecycle cache first, probe
    /// once on a miss, and commit the result.
    BackgroundCached,
    /// Authority path: run `/v1/models` live with a caller-held key (no
    /// Keychain get). `None` means absent/unreadable — return unauthenticated,
    /// never the cache.
    AuthorityLive(Option<&'a str>),
}

/// Resolve `{ key_present, authenticated }` for the status gather.
///
/// See [`AuthProbeMode`] for the authority/background contract.
pub(crate) fn resolve_auth_observation(
    store: &dyn SecretStore,
    mode: AuthProbeMode,
) -> (bool, bool) {
    let auth_generation = AUTH_OBSERVATION_GENERATION.load(Ordering::SeqCst);
    match mode {
        AuthProbeMode::AuthorityLive(Some(k)) => (true, models_authenticated(k)),
        AuthProbeMode::AuthorityLive(None) => {
            // Key absent or Keychain read failed on the authority path.
            // Must NOT consult the cache — return unauthenticated. A cached
            // authenticated=true from a prior background probe must never
            // authorize a ceremony after the key is gone.
            (false, false)
        }
        AuthProbeMode::BackgroundCached => match cached_auth_observation() {
            Some(cached) => cached,
            None => {
                let key = load_gw_api_key(store).ok().flatten();
                let present = key.is_some();
                let authed = key
                    .as_ref()
                    .map(|k| models_authenticated(k))
                    .unwrap_or(false);
                commit_auth_observation(auth_generation, present, authed);
                (present, authed)
            }
        },
    }
}

/// Uncached pack-status gather.
///
/// `mode`: authority paths pass [`AuthProbeMode::AuthorityLive`] so the auth
/// probe always runs `/v1/models` live (never the observation cache). The
/// background path passes [`AuthProbeMode::BackgroundCached`] to consult the
/// lifecycle cache first. See [`AuthProbeMode`] for the contract.
pub(crate) fn gateway_pack_status_uncached_with_key(
    store: &dyn SecretStore,
    mode: AuthProbeMode,
) -> GatewayPackStatus {
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

    let (key_present, authenticated) = resolve_auth_observation(store, mode);
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
    } else if key_present {
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
