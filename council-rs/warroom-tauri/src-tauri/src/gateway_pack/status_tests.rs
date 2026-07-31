use super::*;
use crate::docker_cli::DESKTOP_COMPOSE_PROJECT;
use crate::gateway_pack::health::compose_ls_reports_running;
use crate::gateway_pack::launch::gateway_child_env_if_ready;
use crate::gateway_pack::types::{GatewayPackState, GatewayPackStatus};
use crate::keychain::{
    store_gw_api_key, MemorySecretStore, SecretStore, GW_API_KEY_ACCOUNT, KEYCHAIN_SERVICE,
};
use crate::private_config::test_env_lock;
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn state_ready_allows_governed_only() {
    assert!(GatewayPackState::AuthenticatedReady.allows_governed());
    assert!(!GatewayPackState::InstalledStopped.allows_governed());
    assert!(!GatewayPackState::DockerMissing.allows_governed());
    assert!(!GatewayPackState::Degraded.allows_governed());
}

#[test]
fn status_docker_missing_is_actionable() {
    let st = GatewayPackStatus::base(GatewayPackState::DockerMissing, "Docker CLI not found");
    assert!(!st.state.allows_governed());
    assert!(!st.spawn_capable);
    assert!(!st.authenticated);
}

#[test]
fn governed_spawn_requires_proven_pack_auth() {
    // Full ready state permits the governed spawn.
    let mut st = GatewayPackStatus::base(GatewayPackState::AuthenticatedReady, "ready");
    st.enabled = true;
    st.authenticated = true;
    st.council_governed = true;
    st.refresh_predicates(false);
    assert!(st.spawn_capable);
    assert!(st.governed_ready);
    assert!(st.state.allows_governed());
    assert!(!st.hard_down);

    // A ready-shaped state without owned-child route proof is not full
    // governed readiness.
    let mut st = GatewayPackStatus::base(GatewayPackState::AuthenticatedReady, "unproven child");
    st.enabled = true;
    st.authenticated = true;
    st.refresh_predicates(false);
    assert!(st.spawn_capable);
    assert!(!st.governed_ready);

    // So does the enable / relaunch-restore position: pack auth is proven
    // but the owned child is not governed yet (state Degraded) — the
    // gated spawn is what creates that proof.
    let mut st = GatewayPackStatus::base(GatewayPackState::Degraded, "child not proven");
    st.enabled = true;
    st.authenticated = true;
    st.refresh_predicates(false);
    assert!(st.spawn_capable);
    assert!(!st.governed_ready);
    assert!(!st.state.allows_governed());
    assert!(!st.hard_down);

    // An authenticating key with governed mode disabled must not permit
    // a governed spawn (the old gate's hole).
    let mut st = GatewayPackStatus::base(GatewayPackState::Disabled, "disabled");
    st.authenticated = true;
    st.refresh_predicates(false);
    assert!(!st.spawn_capable);
    assert!(st.hard_down);

    // Enabled but the key failed / is missing: no proof, no spawn.
    let mut st = GatewayPackStatus::base(GatewayPackState::Degraded, "key failed");
    st.enabled = true;
    st.refresh_predicates(false);
    assert!(!st.spawn_capable);

    // Neither enabled nor authenticated: base defaults refuse.
    let st = GatewayPackStatus::base(GatewayPackState::NotInstalled, "absent");
    assert!(!st.spawn_capable);
    assert!(st.hard_down);
}

#[test]
fn governed_ready_implies_spawn_capable() {
    // Vary council_governed too — without it governed_ready is never true and
    // the implication is vacuous.
    let mut saw_governed_ready = false;
    for state in [
        GatewayPackState::NotInstalled,
        GatewayPackState::DockerMissing,
        GatewayPackState::DockerDaemonDown,
        GatewayPackState::Installing,
        GatewayPackState::InstalledStopped,
        GatewayPackState::Starting,
        GatewayPackState::AuthenticatedReady,
        GatewayPackState::Degraded,
        GatewayPackState::Disabled,
    ] {
        for enabled in [false, true] {
            for authenticated in [false, true] {
                for council_governed in [false, true] {
                    for pack_not_running in [false, true] {
                        let mut st = GatewayPackStatus::base(state, "probe");
                        st.enabled = enabled;
                        st.authenticated = authenticated;
                        st.council_governed = council_governed;
                        st.refresh_predicates(pack_not_running);
                        if st.governed_ready {
                            saw_governed_ready = true;
                            assert!(
                                st.spawn_capable,
                                "governed_ready without spawn_capable for {:?}",
                                state
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(
        saw_governed_ready,
        "fixture must produce at least one governed_ready case"
    );
}

#[test]
fn hard_down_classifier_is_exhaustive_and_treats_stopped_as_hard() {
    assert!(GatewayPackStatus::classify_hard_down(
        false,
        GatewayPackState::AuthenticatedReady,
        false
    ));
    assert!(GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::Disabled,
        false
    ));
    assert!(GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::DockerMissing,
        false
    ));
    assert!(GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::InstalledStopped,
        false
    ));
    // Enabled pack whose containers are down is Degraded + not_running.
    assert!(GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::Degraded,
        true
    ));
    // Soft degraded (auth/health flake, ungoverned child) is not hard-down.
    assert!(!GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::Degraded,
        false
    ));
    assert!(!GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::AuthenticatedReady,
        false
    ));
    assert!(!GatewayPackStatus::classify_hard_down(
        true,
        GatewayPackState::Starting,
        false
    ));
}

fn test_status_ready() -> GatewayPackStatus {
    let mut st = GatewayPackStatus::base(GatewayPackState::AuthenticatedReady, "ready");
    st.enabled = true;
    st.authenticated = true;
    st.council_governed = true;
    st.refresh_predicates(false);
    st
}

fn test_status_stopped() -> GatewayPackStatus {
    let mut st = GatewayPackStatus::base(
        GatewayPackState::Degraded,
        "Gateway was enabled but the pack is not running.",
    );
    st.enabled = true;
    st.authenticated = false;
    st.refresh_predicates(true);
    st
}

#[test]
fn status_cache_coalesces_concurrent_misses_to_one_probe() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static ENTER: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
    static GO: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

    fn slow_probe() -> GatewayPackStatus {
        CALLS.fetch_add(1, Ordering::SeqCst);
        if let Some(tx) = ENTER.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(());
        }
        if let Some(rx) = GO.lock().ok().and_then(|mut g| g.take()) {
            let _ = rx.recv();
        }
        test_status_ready()
    }

    let (enter_tx, enter_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    *ENTER.lock().expect("enter") = Some(enter_tx);
    *GO.lock().expect("go") = Some(go_rx);
    CALLS.store(0, Ordering::SeqCst);
    with_test_status_probe(slow_probe, || {
        let store = MemorySecretStore::default();
        let h1 = std::thread::spawn(|| {
            let store = MemorySecretStore::default();
            gateway_pack_status(&store)
        });
        enter_rx.recv().expect("leader entered probe");
        // Second miss while the leader is still probing must join single-flight.
        let h2 = std::thread::spawn(|| {
            let store = MemorySecretStore::default();
            gateway_pack_status(&store)
        });
        std::thread::sleep(Duration::from_millis(50));
        go_tx.send(()).expect("release leader");
        let s1 = h1.join().expect("join h1");
        let s2 = h2.join().expect("join h2");
        assert!(s1.governed_ready);
        assert!(s2.governed_ready);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "concurrent misses must share one probe"
        );
        let _ = gateway_pack_status(&store);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "cache hit");
        let _ = store;
    });
    *ENTER.lock().expect("enter") = None;
    *GO.lock().expect("go") = None;
}

#[test]
fn status_cache_inflight_invalidation_discards_stale_commit() {
    // Generation-guard unit proof: a sample captured under gen N cannot
    // commit after invalidate advances the generation. Concurrent hang
    // coverage lives in status_cache_coalesces_concurrent_misses and
    // status_cache_leader_reprobes_after_inflight_invalidation.
    let _serial = status_cache_test_lock();
    invalidate_status_cache();
    let gen_before = STATUS_CACHE
        .lock()
        .map(|s| s.generation)
        .expect("cache lock");
    let ready = test_status_ready();
    // Simulate a pre-invalidation probe finishing after invalidate.
    invalidate_status_cache();
    commit_status_cache(gen_before, &ready);
    let store = MemorySecretStore::default();
    // Cache must be empty; live probe (no override) returns whatever the
    // environment has — assert only that ready was not committed.
    if let Ok(slot) = STATUS_CACHE.lock() {
        assert!(
            slot.cached.is_none(),
            "stale generation must not repopulate the cache"
        );
        assert_ne!(slot.generation, gen_before);
    }
    // Fresh path also bumps generation and installs a new sample.
    {
        let mut guard = TEST_STATUS_PROBE.lock().expect("probe lock");
        *guard = Some(test_status_stopped);
    }
    let fresh = gateway_pack_status_fresh(&store);
    assert!(fresh.hard_down);
    assert!(!fresh.governed_ready);
    let cached = gateway_pack_status(&store);
    assert!(cached.hard_down);
    {
        let mut guard = TEST_STATUS_PROBE.lock().expect("probe lock");
        *guard = None;
    }
    invalidate_status_cache();
    let _ = store;
}

#[test]
fn status_cache_leader_reprobes_after_inflight_invalidation() {
    // Leader + waiter both enlisted under gen N before invalidate. Both
    // must re-enter, share one gen N+1 probe, return hard-down, and leave
    // only that sample in the presentation cache. Coordination uses
    // timeout-bounded channels so a hang is a test failure, not a stall.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    // Nested probe fn cannot capture outer locals; keep timeout as a literal.
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static ENTER: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
    static GO: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

    fn phased_probe() -> GatewayPackStatus {
        let n = CALLS.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            if let Some(tx) = ENTER.lock().ok().and_then(|mut g| g.take()) {
                let _ = tx.send(());
            }
            if let Some(rx) = GO.lock().ok().and_then(|mut g| g.take()) {
                // Timeout-bounded: a stuck release is a hard failure.
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(()) => {}
                    Err(_) => panic!("old-generation probe release timed out"),
                }
            }
            // Pre-invalidation sample — must not be returned by either caller.
            test_status_ready()
        } else {
            test_status_stopped()
        }
    }

    let (enter_tx, enter_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    *ENTER.lock().expect("enter") = Some(enter_tx);
    *GO.lock().expect("go") = Some(go_rx);
    CALLS.store(0, Ordering::SeqCst);
    with_test_status_probe(phased_probe, || {
        let coord_timeout = Duration::from_secs(5);
        let (leader_tx, leader_rx) = mpsc::channel();
        let (waiter_tx, waiter_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();
        *TEST_INFLIGHT_WAITER_JOINED.lock().expect("join sink") = Some(join_tx);

        let gen_n = STATUS_CACHE
            .lock()
            .map(|s| s.generation)
            .expect("status cache lock");

        let leader = std::thread::spawn(move || {
            let store = MemorySecretStore::default();
            let st = gateway_pack_status(&store);
            let _ = leader_tx.send(st);
        });

        enter_rx
            .recv_timeout(coord_timeout)
            .expect("leader entered first probe");

        // Enlist a waiter under the same in-flight generation before invalidate.
        let waiter = std::thread::spawn(move || {
            let store = MemorySecretStore::default();
            let st = gateway_pack_status(&store);
            let _ = waiter_tx.send(st);
        });
        // Positive, timeout-bounded enlistment proof — not a scheduler sleep.
        let joined_gen = join_rx
            .recv_timeout(coord_timeout)
            .expect("waiter join acknowledgement timed out");
        assert_eq!(
            joined_gen, gen_n,
            "waiter must join generation-N in-flight branch before invalidate"
        );
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "waiter must join single-flight under gen N (no second probe)"
        );

        // Invalidate while the old-generation probe is still in flight.
        invalidate_status_cache();
        go_tx.send(()).expect("release old-generation leader probe");

        let leader_sample = leader_rx
            .recv_timeout(coord_timeout)
            .expect("leader result timed out");
        let waiter_sample = waiter_rx
            .recv_timeout(coord_timeout)
            .expect("waiter result timed out");
        leader.join().expect("leader thread panicked");
        waiter.join().expect("waiter thread panicked");

        assert!(
            !leader_sample.governed_ready && leader_sample.hard_down,
            "leader must return post-invalidation hard-down, not ready"
        );
        assert!(
            !waiter_sample.governed_ready && waiter_sample.hard_down,
            "waiter must return post-invalidation hard-down, not ready"
        );
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            2,
            "gen N+1 re-probe must be single-flight (1 old + 1 shared new), got {}",
            CALLS.load(Ordering::SeqCst)
        );

        // Cache holds only the post-invalidation sample; next read is a hit.
        if let Ok(slot) = STATUS_CACHE.lock() {
            let cached = slot
                .cached
                .as_ref()
                .expect("post-invalidation sample must be cached");
            assert!(
                cached.1.hard_down && !cached.1.governed_ready,
                "cache must contain only the hard-down re-probe sample"
            );
        } else {
            panic!("status cache lock poisoned");
        }
        let store = MemorySecretStore::default();
        let hit = gateway_pack_status(&store);
        assert!(hit.hard_down && !hit.governed_ready);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            2,
            "warm cache hit must not re-probe"
        );
        let _ = store;
    });
    *ENTER.lock().expect("enter") = None;
    *GO.lock().expect("go") = None;
}

#[test]
fn arm_action_gate_sample_bypasses_warm_ready_presentation_cache() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static READY: AtomicBool = AtomicBool::new(true);

    fn dual_probe() -> GatewayPackStatus {
        CALLS.fetch_add(1, Ordering::SeqCst);
        if READY.load(Ordering::SeqCst) {
            test_status_ready()
        } else {
            // Ungoverned-but-auth path also fails the arm gate.
            let mut st = GatewayPackStatus::base(
                GatewayPackState::Degraded,
                "Gateway is up but Council is not governed.",
            );
            st.enabled = true;
            st.authenticated = true;
            st.council_governed = false;
            st.refresh_predicates(false);
            st
        }
    }

    CALLS.store(0, Ordering::SeqCst);
    READY.store(true, Ordering::SeqCst);
    with_test_status_probe(dual_probe, || {
        let store = MemorySecretStore::default();
        let warm = gateway_pack_status(&store);
        assert!(warm.governed_ready, "warm presentation cache is ready");
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        // World loses governed child while presentation cache is warm.
        READY.store(false, Ordering::SeqCst);
        // Presentation path may still show the warm ready sample.
        let presentation = gateway_pack_status(&store);
        assert!(
            presentation.governed_ready,
            "warm presentation cache still ready within TTL"
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        // Arm/renew gate uses gateway_pack_status_fresh (via gateway_ready_for_arm).
        let fresh = gateway_pack_status_fresh(&store);
        assert!(
            !fresh.governed_ready,
            "fresh arm gate must fail closed on ungoverned sample"
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        let _ = store;
    });
}

#[test]
fn owned_council_route_record_invalidates_status_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn counting_probe() -> GatewayPackStatus {
        CALLS.fetch_add(1, Ordering::SeqCst);
        test_status_ready()
    }

    CALLS.store(0, Ordering::SeqCst);
    with_test_status_probe(counting_probe, || {
        let store = MemorySecretStore::default();
        let _ = gateway_pack_status(&store);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        let _ = gateway_pack_status(&store);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "cache hit");
        record_owned_council_route(Some(true));
        let _ = gateway_pack_status(&store);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            2,
            "route write must invalidate"
        );
        record_owned_council_route(None);
        let _ = store;
    });
}

#[test]
fn credential_authority_path_bypasses_warm_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    fn dual_probe() -> GatewayPackStatus {
        CALLS.fetch_add(1, Ordering::SeqCst);
        if READY.load(Ordering::SeqCst) {
            test_status_ready()
        } else {
            test_status_stopped()
        }
    }

    CALLS.store(0, Ordering::SeqCst);
    READY.store(true, Ordering::SeqCst);
    with_test_status_probe(dual_probe, || {
        let store = MemorySecretStore::default();
        let warm = gateway_pack_status(&store);
        assert!(warm.governed_ready);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        // World dies while cache is warm.
        READY.store(false, Ordering::SeqCst);
        // The credential-returning helper must take a fresh sample rather
        // than returning a key based on the warm presentation cache.
        let env = gateway_child_env_if_ready(&store).expect("authority sample");
        assert!(env.is_none());
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        // The authority sample also re-caches the new truth for presentation.
        let after = gateway_pack_status(&store);
        assert!(after.hard_down);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        let _ = store;
    });
}

#[test]
fn memory_store_status_without_pack() {
    let _g = test_env_lock();
    let prev = std::env::var("HOME").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-st-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);
    let store = MemorySecretStore::default();
    let st = gateway_pack_status(&store);
    assert!(
        matches!(
            st.state,
            GatewayPackState::NotInstalled
                | GatewayPackState::DockerMissing
                | GatewayPackState::DockerDaemonDown
                | GatewayPackState::InstalledStopped
                | GatewayPackState::Disabled
        ),
        "unexpected {:?}",
        st.state
    );
    assert!(!st.state.allows_governed());
    assert!(!st.authenticated);
    assert!(!st.council_governed);
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn compose_ls_reports_running_requires_exact_project_running() {
    let ours = "/x/docker-compose.yml";
    let running = r#"[{"Name":"irin-desktop-gateway","Status":"running(2)","ConfigFiles":"/x/docker-compose.yml"}]"#;
    assert!(compose_ls_reports_running(running, ours));

    // Stopped project is not running.
    let exited = r#"[{"Name":"irin-desktop-gateway","Status":"exited(1)","ConfigFiles":"/x/docker-compose.yml"}]"#;
    assert!(!compose_ls_reports_running(exited, ours));

    // Lookalike names never count as ours (exact match only).
    let lookalike = r#"[{"Name":"irin-desktop-gateway-evil","Status":"running(1)","ConfigFiles":"/x/docker-compose.yml"}]"#;
    assert!(!compose_ls_reports_running(lookalike, ours));

    // A foreign project with OUR exact name but a different config file
    // is not ours either — the Keychain key must never reach it.
    let foreign_same_name = r#"[{"Name":"irin-desktop-gateway","Status":"running(2)","ConfigFiles":"/evil/docker-compose.yml"}]"#;
    assert!(!compose_ls_reports_running(foreign_same_name, ours));

    // Empty, malformed, or non-array output is not running.
    assert!(!compose_ls_reports_running("[]", ours));
    assert!(!compose_ls_reports_running("not json", ours));
    assert!(!compose_ls_reports_running(
        r#"{"Name":"irin-desktop-gateway"}"#,
        ours
    ));
    assert!(!compose_ls_reports_running("", ours));
}

#[test]
fn owned_council_route_proof_records_and_clears() {
    // Default for a fresh process: no owned child proof.
    record_owned_council_route(None);
    assert_eq!(owned_council_route(), None);
    record_owned_council_route(Some(true));
    assert_eq!(owned_council_route(), Some(true));
    record_owned_council_route(Some(false));
    assert_eq!(owned_council_route(), Some(false));
    // Child stop/death clears the proof again.
    record_owned_council_route(None);
    assert_eq!(owned_council_route(), None);
}

// ---------------------------------------------------------------------------
// Auth-observation cache: the status background loop must not re-read
// GW_API_KEY from Keychain every 5s tick. These tests exercise the cache via
// `resolve_auth_observation` directly (with_test_status_probe short-circuits
// before gateway_pack_status_uncached, so it cannot observe Keychain reads).
// `models_authenticated` returns false in the test env (no Docker/gateway),
// which is fine — these tests prove *read-count* behavior, not auth results.
// ---------------------------------------------------------------------------

/// Records Keychain get order without logging secret values. Mirrors the
/// pattern in launch_tests.rs so status-loop read counts are observable.
struct CountingKeychainStore {
    inner: Mutex<HashMap<(String, String), String>>,
    gets: Arc<AtomicUsize>,
}

impl CountingKeychainStore {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            gets: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_gw_key() -> Self {
        let store = Self::new();
        store_gw_api_key(&store, &format!("gw_{:032x}", 1u128)).unwrap();
        store.gets.store(0, AtomicOrdering::SeqCst);
        store
    }

    fn get_count(&self) -> usize {
        self.gets.load(AtomicOrdering::SeqCst)
    }
}

impl SecretStore for CountingKeychainStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        self.inner
            .lock()
            .map_err(|_| "lock poisoned".to_string())?
            .insert(
                (service.to_string(), account.to_string()),
                password.to_string(),
            );
        Ok(())
    }

    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        if service == KEYCHAIN_SERVICE && account == GW_API_KEY_ACCOUNT {
            self.gets.fetch_add(1, AtomicOrdering::SeqCst);
        }
        Ok(self
            .inner
            .lock()
            .map_err(|_| "lock poisoned".to_string())?
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        self.inner
            .lock()
            .map_err(|_| "lock poisoned".to_string())?
            .remove(&(service.to_string(), account.to_string()));
        Ok(())
    }
}

#[test]
fn auth_observation_cache_hit_avoids_second_keychain_read() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::with_gw_key();

    // First call: cache miss → one Keychain get.
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(store.get_count(), 1, "first call should read Keychain once");

    // Second call: cache hit → no Keychain get.
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(store.get_count(), 1, "second call should serve from cache");
}

#[test]
fn auth_observation_cached_absence_is_not_re_probed() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::new(); // no key seeded

    // Absence is cached just like presence.
    let (present, _) = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert!(!present, "empty store => key absent");
    assert_eq!(store.get_count(), 1, "first call probes once");

    // Cached absence: no re-probe on the next tick.
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(
        store.get_count(),
        1,
        "absence must be cached, not re-probed"
    );
}

#[test]
fn auth_observation_invalidation_forces_re_read() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::with_gw_key();

    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(store.get_count(), 1);

    // Invalidate drops the cache; next call re-probes.
    invalidate_auth_observation();
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(store.get_count(), 2, "post-invalidation must re-read");
}

#[test]
fn auth_observation_held_key_skips_keychain_entirely() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::with_gw_key();

    // Authority path: caller holds the key → zero Keychain gets.
    let (present, _) =
        resolve_auth_observation(&store, AuthProbeMode::AuthorityLive(Some("gw_deadbeef")));
    assert!(present, "held key => key_present is true");
    assert_eq!(
        store.get_count(),
        0,
        "held-key path must not touch Keychain"
    );
}

#[test]
fn cold_launch_seed_makes_first_background_tick_keychain_free() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::with_gw_key();

    seed_auth_observation_from_preloaded_key(Some("gw_deadbeef"));
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);

    assert_eq!(
        store.get_count(),
        0,
        "preloaded cold-launch key must seed the first background tick"
    );
}

#[test]
fn cold_launch_absence_seed_makes_first_background_tick_keychain_free() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::new();

    seed_auth_observation_from_preloaded_key(None);
    let (present, _) = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);

    assert!(!present);
    assert_eq!(
        store.get_count(),
        0,
        "known cold-launch absence must seed the first background tick"
    );
}

#[test]
fn auth_observation_stale_generation_rejected_at_commit() {
    let _lock = test_env_lock();
    invalidate_auth_observation();

    // Simulate the generation advancing between read and commit: read the
    // generation, then invalidate (bump it) before committing.
    let gen_before = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    invalidate_auth_observation(); // advances generation
    let gen_after = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    assert_ne!(
        gen_before, gen_after,
        "invalidation must advance generation"
    );

    // A commit with the stale generation is rejected.
    let committed = commit_auth_observation(gen_before, true, true);
    assert!(!committed, "stale-generation commit must be rejected");
    assert!(
        !auth_observation_present_for_test(),
        "rejected commit must not populate the cache"
    );

    // A commit with the current generation succeeds.
    let committed = commit_auth_observation(gen_after, true, false);
    assert!(committed, "current-generation commit must succeed");
    assert!(auth_observation_present_for_test());
}

#[test]
fn auth_observation_concurrent_misses_cannot_double_commit() {
    let _lock = test_env_lock();
    invalidate_auth_observation();

    // Two threads each read a distinct generation, then both try to commit.
    // The generation guard ensures only the one matching the current value
    // writes through; the other is rejected. This is a structural proof: the
    // commit function's generation check is the concurrency guard.
    let gen_a = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    invalidate_auth_observation();
    let gen_b = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);

    let committed_a = commit_auth_observation(gen_a, true, true);
    let committed_b = commit_auth_observation(gen_b, false, false);

    // Exactly one commit wins.
    assert!(
        committed_a ^ committed_b,
        "exactly one of the two generations must commit (a={committed_a}, b={committed_b})"
    );
    assert!(auth_observation_present_for_test());

    // The winning observation is the one with the current generation.
    let (cached_present, _) = cached_auth_observation().unwrap();
    if committed_b {
        assert!(!cached_present, "gen_b won => its observation must hold");
    }
}

// ---------------------------------------------------------------------------
// Reviewer-requested tests (Issue 3): prove the authority/background contract
// boundary holds. These are the three scenarios the reviewer identified as gaps
// in the first iteration.
// ---------------------------------------------------------------------------

/// Issue 1 proof: cached presentation auth cannot influence the authority path.
///
/// Seeds the observation cache with `authenticated=true` (via the internal
/// commit helper), then calls `resolve_auth_observation` with a held key. The
/// held-key path must run `models_authenticated` live (returns false in the test
/// env — no Docker) and NOT serve the cached `true`. This is the Touch ID
/// arm/renew contract: a cached `true` must never permit the ceremony after
/// Gateway auth has actually failed.
#[test]
fn authority_path_ignores_cached_auth_observation() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::with_gw_key();

    // Warm the cache with authenticated=true via the internal commit seam.
    let gen = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        commit_auth_observation(gen, true, true),
        "seed cache with authenticated=true"
    );

    // Verify the cache is warm.
    let (cached_present, cached_authed) = cached_auth_observation().expect("cache should be warm");
    assert!(
        cached_present && cached_authed,
        "cache seeded with true/true"
    );

    // Authority path with a held key: must NOT serve from cache. In the test
    // env models_authenticated returns false (no Docker), so the live result
    // must be authenticated=false — the cached true must NOT leak through.
    let (present, authed) =
        resolve_auth_observation(&store, AuthProbeMode::AuthorityLive(Some("gw_deadbeef")));
    assert!(present, "held key => present");
    assert!(
        !authed,
        "authority path must run live models_authenticated (false in test env), \
         not serve cached authenticated=true"
    );
    assert_eq!(
        store.get_count(),
        0,
        "held-key path must not touch Keychain at all"
    );
}

/// Issue 2 proof: background reads remain constant until explicit invalidation
/// (no TTL). Warm the cache, then call the background path many times and prove
/// zero additional Keychain reads — the observation persists indefinitely.
#[test]
fn background_reads_constant_until_lifecycle_invalidation() {
    let _lock = test_env_lock();
    invalidate_auth_observation();
    let store = CountingKeychainStore::with_gw_key();

    // First background call: cache miss → one read.
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(store.get_count(), 1);

    // Many subsequent background calls: cache persists, zero additional reads.
    // (Previously a 30s TTL would have re-read every 30s; now it's lifecycle-only.)
    for _ in 0..20 {
        let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    }
    assert_eq!(
        store.get_count(),
        1,
        "background path must not re-read Keychain until lifecycle invalidation"
    );

    // Only an explicit lifecycle invalidation forces a re-read.
    invalidate_auth_observation();
    let _ = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert_eq!(store.get_count(), 2, "post-invalidation re-reads once");
}

/// Issue 3 proof: a probe whose generation was superseded by a lifecycle
/// invalidation mid-flight cannot repopulate the observation with stale auth.
///
/// Simulates the race: read generation G0, invalidate (advance to G1), then
/// attempt to commit the G0 observation. The generation guard must reject it
/// so a stale "authenticated=true" cannot overwrite the invalidated cache.
#[test]
fn stale_generation_cannot_repopulate_observation() {
    let _lock = test_env_lock();
    invalidate_auth_observation();

    // Step 1: a normal background probe commits at G0.
    let store = CountingKeychainStore::with_gw_key();
    let (present, _) = resolve_auth_observation(&store, AuthProbeMode::BackgroundCached);
    assert!(present, "key is present");
    assert!(auth_observation_present_for_test());

    // Step 2: read G0, then a lifecycle mutation invalidates (advances to G1).
    let stale_gen = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    invalidate_auth_observation();
    let current_gen = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    assert_ne!(
        stale_gen, current_gen,
        "invalidation must advance generation"
    );

    // Step 3: a probe that computed its observation at G0 tries to commit.
    // The guard must reject it — the key may have been rotated by the lifecycle
    // mutation between read and commit.
    let rejected = commit_auth_observation(stale_gen, true, true);
    assert!(
        !rejected,
        "stale-generation commit must be rejected even if auth was true"
    );

    // The cache remains cleared by the invalidation, not repopulated by the
    // stale probe.
    assert!(
        !auth_observation_present_for_test(),
        "rejected stale commit must not repopulate the cache"
    );
}

// ---------------------------------------------------------------------------
// P1 regression: authority path must return unauthenticated when the key is
// absent or the Keychain read fails, even if the background cache holds a warm
// authenticated=true. This is the exact failure path the reviewer identified:
// gateway_pack_status_fresh converts Ok(None)/Err to AuthorityLive(None), and
// resolve_auth_observation must NOT fall through to the cache.
// ---------------------------------------------------------------------------

/// SecretStore whose get_password always returns Err (simulates Keychain ACL
/// failure / unreadable item).
struct FailingKeychainStore {
    gets: AtomicUsize,
}

impl FailingKeychainStore {
    fn new() -> Self {
        Self {
            gets: AtomicUsize::new(0),
        }
    }

    fn get_count(&self) -> usize {
        self.gets.load(AtomicOrdering::SeqCst)
    }
}

impl SecretStore for FailingKeychainStore {
    fn set_password(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        Err("keychain unavailable".into())
    }
    fn get_password(&self, _: &str, _: &str) -> Result<Option<String>, String> {
        self.gets.fetch_add(1, AtomicOrdering::SeqCst);
        Err("keychain read failed".into())
    }
    fn delete_password(&self, _: &str, _: &str) -> Result<(), String> {
        Err("keychain unavailable".into())
    }
}

/// Helper: seed the observation cache with (key_present=true, authenticated=true)
/// at the current generation.
fn seed_cache_authenticated_true() {
    let gen = AUTH_OBSERVATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        commit_auth_observation(gen, true, true),
        "seed cache with authenticated=true"
    );
}

#[test]
fn authority_absent_key_returns_unauthenticated_despite_warm_cache() {
    let _lock = test_env_lock();
    invalidate_auth_observation();

    // Warm the background cache with authenticated=true.
    seed_cache_authenticated_true();
    let (_, cached_authed) = cached_auth_observation().expect("cache warm");
    assert!(cached_authed, "cache seeded with authenticated=true");

    // Exercise the same Keychain-load-to-AuthProbeMode seam used by
    // gateway_pack_status_fresh. Ok(None) must become AuthorityLive(None)
    // without serving the cached true.
    let store = CountingKeychainStore::new(); // no key seeded
    let (present, authed) =
        with_loaded_authority_mode(&store, |mode| resolve_auth_observation(&store, mode));
    assert_eq!(
        store.get_count(),
        1,
        "authority path must read Keychain once"
    );
    assert!(!present, "absent key => key_present must be false");
    assert!(
        !authed,
        "authority path must return unauthenticated despite warm cached=true"
    );
}

#[test]
fn authority_failing_keychain_returns_unauthenticated_despite_warm_cache() {
    let _lock = test_env_lock();
    invalidate_auth_observation();

    // Warm the background cache with authenticated=true.
    seed_cache_authenticated_true();
    let (_, cached_authed) = cached_auth_observation().expect("cache warm");
    assert!(cached_authed, "cache seeded with authenticated=true");

    // Exercise the exact production seam: an actual SecretStore error must be
    // read once, collapse to AuthorityLive(None), and fail closed without
    // serving the cached true.
    let store = FailingKeychainStore::new();
    let (present, authed) =
        with_loaded_authority_mode(&store, |mode| resolve_auth_observation(&store, mode));
    assert_eq!(
        store.get_count(),
        1,
        "authority path must read Keychain once"
    );
    assert!(
        !present,
        "failed keychain read => key_present must be false"
    );
    assert!(
        !authed,
        "authority path must return unauthenticated despite warm cached=true"
    );
}
