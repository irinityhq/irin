use super::super::cli_adapters::{
    ensure_cli_adapters, ensure_cli_adapters_with_tokens, ensure_proxy_tokens,
};
use super::super::status::{bump_pack_lifecycle_generation, lifecycle_gen_test_lock, pack_lifecycle_generation};
use super::*;
use crate::keychain::{
    load_gw_api_key, migrate_legacy_secrets_with_values, store_arm_principal_token,
    store_auth_pepper, store_claude_proxy_token, store_codex_proxy_token, store_gw_api_key,
    store_watch_admin_token, SecretStore, ARM_PRINCIPAL_TOKEN_ACCOUNT, AUTH_PEPPER_ACCOUNT,
    CLAUDE_PROXY_TOKEN_ACCOUNT, CODEX_PROXY_TOKEN_ACCOUNT, GW_API_KEY_ACCOUNT, KEYCHAIN_SERVICE,
    WATCH_ADMIN_TOKEN_ACCOUNT,
};
use std::collections::HashMap;
use std::sync::Mutex;

/// Records Keychain get order without logging secret values.
struct CountingSecretStore {
    inner: Mutex<HashMap<(String, String), String>>,
    gets: Mutex<Vec<String>>,
}

impl CountingSecretStore {
    fn with_seeded_pack_secrets() -> Self {
        let store = Self {
            inner: Mutex::new(HashMap::new()),
            gets: Mutex::new(Vec::new()),
        };
        // Shapes match production validators (values never asserted/logged).
        // Fixed-width hex pads so lengths match is_valid_* exactly.
        store_gw_api_key(&store, &format!("gw_{:032x}", 1u128)).unwrap();
        store_auth_pepper(&store, &format!("{:064x}", 2u128)).unwrap();
        store_arm_principal_token(&store, &format!("tok_{:032x}", 3u128)).unwrap();
        // 64 hex chars required for proxy tokens.
        store_claude_proxy_token(&store, &format!("{:064x}", 4u128)).unwrap();
        store_codex_proxy_token(&store, &format!("{:064x}", 5u128)).unwrap();
        store_watch_admin_token(&store, &format!("{:064x}", 6u128)).unwrap();
        // Clear set-side noise; only measure gets during the resume path under test.
        store.gets.lock().unwrap().clear();
        store
    }

    fn get_accounts(&self) -> Vec<String> {
        self.gets.lock().unwrap().clone()
    }

    fn get_count_for(&self, account: &str) -> usize {
        self.gets
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.as_str() == account)
            .count()
    }
}

impl SecretStore for CountingSecretStore {
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
        if service == KEYCHAIN_SERVICE {
            self.gets
                .lock()
                .map_err(|_| "lock poisoned".to_string())?
                .push(account.to_string());
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

/// Exercise the FullStart resume Keychain sequence without Docker/compose:
/// one GW key load for the flight, one proxy-token pair load shared by
/// adapters + compose secret env, plus pepper + watch admin + arm principal.
///
/// The pre-fix cold-launch path double-got GW + Claude + Codex (8 gets for
/// 5 distinct accounts) and produced eight sequential authorization dialogs.
fn full_start_resume_keychain_sequence(store: &dyn SecretStore) -> Result<(), String> {
    // Migration returns the GW/pepper values from its one account read each.
    let migrated = migrate_legacy_secrets_with_values(store);
    let key = migrated
        .gw_api_key
        .ok_or_else(|| "GW_API_KEY missing".to_string())?;
    // Remaining four accounts are loaded exactly once for the launch flight.
    let launch_secrets = super::super::env::load_launch_secrets(store, migrated.auth_pepper)?;
    let _ = pack_auth_revalidated_with_key(&key); // no Keychain re-entry
    let _ = ensure_cli_adapters_with_tokens(
        &launch_secrets.proxy_tokens.0,
        &launch_secrets.proxy_tokens.1,
    );
    let _env =
        super::super::env::build_compose_secret_env_with_launch_secrets(None, &launch_secrets)?;
    // Final proof reuses held key (no GW re-get).
    let _ = pack_auth_revalidated_with_key(&key);
    Ok(())
}

/// Documents the *buggy* double-load sequence that produced 8 Keychain gets.
fn buggy_full_start_double_load_sequence(store: &dyn SecretStore) -> Result<(), String> {
    let _ = pack_auth_revalidated(store); // GW #1
    let _ = ensure_cli_adapters(store); // Claude #1, Codex #1
    let _ = load_gw_api_key(store)?; // GW #2
    let _ = super::super::env::build_compose_secret_env(store, None, None)?; // pepper, arm, Claude #2, Codex #2
    let _ = pack_auth_revalidated(store); // GW #3 (post-fix path also revalidated)
    Ok(())
}

#[test]
fn full_start_resume_keychain_gets_each_account_at_most_once() {
    let store = CountingSecretStore::with_seeded_pack_secrets();
    full_start_resume_keychain_sequence(&store).unwrap();
    let gets = store.get_accounts();
    // Exactly the six expected distinct accounts, one get each.
    assert_eq!(
        gets.len(),
        6,
        "expected 6 Keychain gets (one per account), got {gets:?}"
    );
    for account in [
        GW_API_KEY_ACCOUNT,
        CLAUDE_PROXY_TOKEN_ACCOUNT,
        CODEX_PROXY_TOKEN_ACCOUNT,
        AUTH_PEPPER_ACCOUNT,
        WATCH_ADMIN_TOKEN_ACCOUNT,
        ARM_PRINCIPAL_TOKEN_ACCOUNT,
    ] {
        assert_eq!(
            store.get_count_for(account),
            1,
            "account {account} should be got exactly once; sequence={gets:?}"
        );
    }
}

#[test]
fn buggy_full_start_sequence_repeats_gw_and_proxy_accounts() {
    // Regression anchor: prove the pre-fix call pattern repeats accounts.
    let store = CountingSecretStore::with_seeded_pack_secrets();
    buggy_full_start_double_load_sequence(&store).unwrap();
    let gets = store.get_accounts();
    assert!(
        gets.len() >= 8,
        "buggy sequence should perform >=8 gets, got {gets:?}"
    );
    assert!(
        store.get_count_for(GW_API_KEY_ACCOUNT) >= 2,
        "buggy path re-gets GW_API_KEY; sequence={gets:?}"
    );
    assert!(
        store.get_count_for(CLAUDE_PROXY_TOKEN_ACCOUNT) >= 2,
        "buggy path re-gets Claude proxy token; sequence={gets:?}"
    );
    assert!(
        store.get_count_for(CODEX_PROXY_TOKEN_ACCOUNT) >= 2,
        "buggy path re-gets Codex proxy token; sequence={gets:?}"
    );
}

/// Cold flight + promote-style reuse of held key/secrets: still 6 gets total.
fn promote_with_held_secrets_adds_zero_keychain_gets(
    store: &dyn SecretStore,
) -> Result<(), String> {
    let migrated = migrate_legacy_secrets_with_values(store);
    let key = migrated
        .gw_api_key
        .ok_or_else(|| "GW_API_KEY missing".to_string())?;
    let launch_secrets = super::super::env::load_launch_secrets(store, migrated.auth_pepper)?;
    let _ = pack_auth_revalidated_with_key(&key);
    let _ = ensure_cli_adapters_with_tokens(
        &launch_secrets.proxy_tokens.0,
        &launch_secrets.proxy_tokens.1,
    );
    let _env =
        super::super::env::build_compose_secret_env_with_launch_secrets(None, &launch_secrets)?;
    // Promote retry: same held values (no Keychain re-entry).
    let _ = pack_auth_revalidated_with_key(&key);
    let _ = ensure_cli_adapters_with_tokens(
        &launch_secrets.proxy_tokens.0,
        &launch_secrets.proxy_tokens.1,
    );
    let _env2 =
        super::super::env::build_compose_secret_env_with_launch_secrets(None, &launch_secrets)?;
    let _ = pack_auth_revalidated_with_key(&key);
    Ok(())
}

/// Bare promote (no held secrets) re-gets accounts the way schedule used to.
fn buggy_promote_without_held_secrets_sequence(store: &dyn SecretStore) -> Result<(), String> {
    let _ = pack_auth_revalidated(store); // GW
    let _ = ensure_cli_adapters(store); // proxies
    let _ = super::super::env::build_compose_secret_env(store, None, None)?; // pepper/arm/watch/proxies
    Ok(())
}

#[test]
fn promote_with_held_secrets_does_not_reenter_keychain() {
    let store = CountingSecretStore::with_seeded_pack_secrets();
    promote_with_held_secrets_adds_zero_keychain_gets(&store).unwrap();
    let gets = store.get_accounts();
    assert_eq!(
        gets.len(),
        6,
        "cold+promote held secrets must stay at 6 gets, got {gets:?}"
    );
    for account in [
        GW_API_KEY_ACCOUNT,
        CLAUDE_PROXY_TOKEN_ACCOUNT,
        CODEX_PROXY_TOKEN_ACCOUNT,
        AUTH_PEPPER_ACCOUNT,
        WATCH_ADMIN_TOKEN_ACCOUNT,
        ARM_PRINCIPAL_TOKEN_ACCOUNT,
    ] {
        assert_eq!(
            store.get_count_for(account),
            1,
            "account {account} re-entered on promote; sequence={gets:?}"
        );
    }
}

/// Orchestration path used by `schedule_governed_promote_attempts`: after a
/// cold Keychain flight, the production promote helper + held-key status must
/// perform zero additional store reads. If the scheduler stops forwarding
/// held secrets, this fails (legacy dispatch re-gets accounts).
#[test]
fn promote_pack_ready_held_secret_path_does_not_reenter_keychain() {
    // `status_with_council_route_with_key` bumps the status-cache generation;
    // serialize with the coalescing tests in status_tests.rs (B-17).
    let _serial = crate::private_config::test_env_lock();
    let store = CountingSecretStore::with_seeded_pack_secrets();
    let migrated = migrate_legacy_secrets_with_values(&store);
    let key = migrated.gw_api_key.expect("seeded GW_API_KEY");
    let launch_secrets =
        super::super::env::load_launch_secrets(&store, migrated.auth_pepper).unwrap();
    let after_cold = store.get_accounts().len();
    assert_eq!(after_cold, 6, "cold flight should be exactly 6 gets");

    // Same dispatch schedule_governed_promote_attempts uses per attempt.
    let _step = promote_pack_ready_for_attempt(&store, Some(&key), Some(&launch_secrets), 0, 4);
    // Post-success status sample on the held-key authority path.
    let _ = status_with_council_route_with_key(&store, Some(&key), true, false);

    let gets = store.get_accounts();
    assert_eq!(
        gets.len(),
        after_cold,
        "scheduler held-secret path must add zero Keychain gets; sequence={gets:?}"
    );
}

#[test]
fn promote_held_secrets_invalid_after_lifecycle_bump() {
    let _g = lifecycle_gen_test_lock();
    let at = pack_lifecycle_generation();
    assert!(promote_held_secrets_still_valid(at));
    bump_pack_lifecycle_generation();
    assert!(!promote_held_secrets_still_valid(at));
}

#[test]
fn buggy_promote_without_held_secrets_reenters_accounts() {
    let store = CountingSecretStore::with_seeded_pack_secrets();
    full_start_resume_keychain_sequence(&store).unwrap();
    let after_cold = store.get_accounts().len();
    buggy_promote_without_held_secrets_sequence(&store).unwrap();
    let gets = store.get_accounts();
    assert!(
        gets.len() > after_cold,
        "bare promote should add Keychain gets beyond cold flight; got {gets:?}"
    );
    assert!(
        store.get_count_for(GW_API_KEY_ACCOUNT) >= 2,
        "bare promote re-gets GW; sequence={gets:?}"
    );
}

#[test]
fn packaged_frontend_must_not_start_council() {
    assert!(!frontend_may_start_council(true));
    assert!(frontend_may_start_council(false));
}

#[test]
fn cold_launch_single_governed_owner_when_frontend_defers() {
    // Policy: packaged + frontend deferred → exactly one native-owned route.
    assert!(!frontend_may_start_council(true));
    let route = cold_launch_owned_via_gateway(
        true,  // packaged
        true,  // via_gateway_default
        true,  // pack_auth_ok
        false, // frontend does not start
        true,  // would-have-won is irrelevant
    );
    assert_eq!(route, Some(true));
    // Only one owner: native governed, no second Direct child.
    assert_eq!(
        cold_launch_owned_via_gateway(true, true, true, false, false),
        Some(true)
    );
}

#[test]
fn cold_launch_race_sticks_direct_if_frontend_still_starts() {
    // Documents the fixed bug path: frontend Direct wins the lock and
    // native cannot correct ownership ("already tracked as running").
    let stuck = cold_launch_owned_via_gateway(true, true, true, true, true);
    assert_eq!(stuck, Some(false));
}

#[test]
fn decide_launch_via_gateway_requires_persisted_and_auth() {
    assert!(!decide_launch_via_gateway(false, true));
    assert!(!decide_launch_via_gateway(true, false));
    assert!(decide_launch_via_gateway(true, true));
}

#[test]
fn launch_resume_success_and_fail_closed() {
    assert_eq!(
        decide_launch_resume_outcome(true, true, false, true),
        LaunchResumeOutcome::Governed
    );
    assert_eq!(
        decide_launch_resume_outcome(true, false, true, true),
        LaunchResumeOutcome::Governed
    );
    // Pack never ready → Direct fail-closed (truthful; not invented governed).
    assert_eq!(
        decide_launch_resume_outcome(true, false, false, true),
        LaunchResumeOutcome::DirectFailClosed
    );
    // Pack ready but governed spawn fails → Direct fail-closed.
    assert_eq!(
        decide_launch_resume_outcome(true, true, false, false),
        LaunchResumeOutcome::DirectFailClosed
    );
    // Not enabled → Direct.
    assert_eq!(
        decide_launch_resume_outcome(false, true, true, true),
        LaunchResumeOutcome::DirectFailClosed
    );
}

#[test]
fn later_promote_without_manual_reenable() {
    assert!(may_promote_to_governed(true, Some(false), true));
    // Already governed: no promote needed.
    assert!(!may_promote_to_governed(true, Some(true), true));
    // No owned child.
    assert!(!may_promote_to_governed(true, None, true));
    // Operator disabled.
    assert!(!may_promote_to_governed(false, Some(false), true));
    // Pack still not ready.
    assert!(!may_promote_to_governed(true, Some(false), false));
}

#[test]
fn resume_action_avoids_full_start_when_project_running() {
    assert_eq!(
        decide_resume_pack_action(true, false),
        ResumePackAction::AlreadyReady
    );
    assert_eq!(
        decide_resume_pack_action(true, true),
        ResumePackAction::AlreadyReady
    );
    // Running but not yet auth-ready → wait/poll only (no Keychain compose rebuild).
    assert_eq!(
        decide_resume_pack_action(false, true),
        ResumePackAction::WaitOnly
    );
    // Down → full compose start once callers enter resume.
    assert_eq!(
        decide_resume_pack_action(false, false),
        ResumePackAction::FullStart
    );
}

#[test]
fn adapter_reconcile_is_bounded_to_readiness_transition_or_failed_retry() {
    let not_ready = CliAdaptersStatus::default();
    let mut claude_ready = not_ready;
    claude_ready.claude = super::super::cli_adapters::AdapterHealth::Ready;
    claude_ready.claude_reason = super::super::cli_adapters::AdapterNotReadyReason::None;
    let mut codex_ready = not_ready;
    codex_ready.codex = super::super::cli_adapters::AdapterHealth::Ready;
    codex_ready.codex_reason = super::super::cli_adapters::AdapterNotReadyReason::None;

    assert!(adapter_became_ready(not_ready, claude_ready));
    assert!(adapter_became_ready(not_ready, codex_ready));
    assert!(!adapter_became_ready(claude_ready, claude_ready));
    assert!(!adapter_became_ready(claude_ready, not_ready));
    assert!(!adapter_reconcile_required(
        claude_ready,
        claude_ready,
        false
    ));
    assert!(!adapter_reconcile_required(not_ready, not_ready, false));
    assert!(adapter_reconcile_required(claude_ready, claude_ready, true));
}

#[test]
fn watch_token_upgrade_reconciles_once_and_requires_both_admin_surfaces() {
    assert!(governed_launch_after_watch_reconciliation(true, true));
    assert!(!governed_launch_after_watch_reconciliation(true, false));
    assert!(!governed_launch_after_watch_reconciliation(false, true));
    assert!(watch_admin_surfaces_ready(Some(200), Some(200)));
    assert!(!watch_admin_surfaces_ready(Some(200), Some(403)));
    assert!(!watch_admin_surfaces_ready(Some(503), Some(200)));
    assert!(!watch_admin_surfaces_ready(None, Some(200)));

    assert!(decide_watch_token_reconciliation(
        true,
        Some(401),
        Some(401),
        false
    ));
    assert!(decide_watch_token_reconciliation(
        true,
        Some(200),
        Some(401),
        false
    ));
    assert!(decide_watch_token_reconciliation(
        true,
        Some(401),
        Some(200),
        false
    ));
    assert!(!decide_watch_token_reconciliation(
        true,
        Some(200),
        Some(200),
        false
    ));
    assert!(!decide_watch_token_reconciliation(
        true,
        Some(503),
        None,
        false
    ));
    assert!(!decide_watch_token_reconciliation(
        false,
        Some(401),
        Some(401),
        false
    ));
    assert!(!decide_watch_token_reconciliation(
        true,
        Some(401),
        Some(401),
        true
    ));
}

#[test]
fn promote_early_window_bounds_resume_calls() {
    // Attempts 0..3 may resume; 4+ revalidate-only.
    assert!(promote_may_call_resume(0, 4, false));
    assert!(promote_may_call_resume(3, 4, false));
    assert!(!promote_may_call_resume(4, 4, false));
    assert!(!promote_may_call_resume(11, 4, false));
    // Ready pack never re-enters resume.
    assert!(!promote_may_call_resume(0, 4, true));
}

// --- PR4 characterization: launch outcomes (secret snapshot, lifecycle fence, Direct) ---

/// One LaunchSecrets snapshot per flight is reused for adapters + compose secret
/// env without re-entering Keychain for proxy/watch/pepper accounts.
#[test]
fn one_secret_snapshot_per_flight_reused_for_adapters_and_compose() {
    let store = CountingSecretStore::with_seeded_pack_secrets();
    full_start_resume_keychain_sequence(&store).expect("flight");
    // GW once (via migration), each other pack secret at most once.
    assert_eq!(store.get_count_for(GW_API_KEY_ACCOUNT), 1);
    assert_eq!(store.get_count_for(AUTH_PEPPER_ACCOUNT), 1);
    assert_eq!(store.get_count_for(CLAUDE_PROXY_TOKEN_ACCOUNT), 1);
    assert_eq!(store.get_count_for(CODEX_PROXY_TOKEN_ACCOUNT), 1);
    assert_eq!(store.get_count_for(WATCH_ADMIN_TOKEN_ACCOUNT), 1);
    assert_eq!(store.get_count_for(ARM_PRINCIPAL_TOKEN_ACCOUNT), 1);
}

/// Lifecycle generation change after pack-ready proof must refuse promote commit.
#[test]
fn lifecycle_generation_change_aborts_before_promote_commit() {
    let at = pack_lifecycle_generation();
    assert!(promote_may_commit_after_pack_ready(
        at, at, true, true
    ));
    // Simulate Enable/disable advancing generation after adapters finished.
    assert!(!promote_may_commit_after_pack_ready(
        at,
        at.wrapping_add(1),
        true,
        true
    ));
    // Pack not ready never commits even if generation matches.
    assert!(!promote_may_commit_after_pack_ready(at, at, false, true));
    // may_promote false never commits.
    assert!(!promote_may_commit_after_pack_ready(at, at, true, false));
}

/// Failed authenticated readiness (pack auth false) leaves launch Direct.
#[test]
fn failed_authenticated_readiness_leaves_council_direct() {
    assert_eq!(
        decide_launch_resume_outcome(true, false, false, false),
        LaunchResumeOutcome::DirectFailClosed
    );
    // Governed spawn only when pack ready.
    assert_eq!(
        decide_launch_via_gateway(true, false),
        false,
        "no pack auth → not governed"
    );
    assert_eq!(
        decide_launch_via_gateway(true, true),
        true,
        "pack auth + persisted → governed allowed"
    );
    // Pure promote eligibility: Direct-owned child + pack ok required.
    assert!(!may_promote_to_governed(true, Some(false), false));
    assert!(may_promote_to_governed(true, Some(false), true));
}

/// evaluate_promote_flight_attempt aborts when lifecycle advances mid-flight.
#[test]
fn evaluate_promote_flight_aborts_on_lifecycle_bump() {
    let _g = lifecycle_gen_test_lock();
    let store = CountingSecretStore::with_seeded_pack_secrets();
    let at = pack_lifecycle_generation();
    // Force generation change so entry fence aborts without resume work.
    bump_pack_lifecycle_generation();
    let decision = evaluate_promote_flight_attempt(
        &store,
        at,
        None,
        None,
        0,
        4,
        true,
        Some(false),
    );
    assert_eq!(decision, PromoteFlightDecision::AbortLifecycleChanged);
    // No Keychain work after abort.
    assert!(
        store.get_accounts().is_empty(),
        "abort before pack step must not touch Keychain, got {:?}",
        store.get_accounts()
    );
}


/// Scheduler commit boundary: generation bump during stop/port-wait must not
/// invoke governed start (Codex residual — fence after wait, before spawn).
#[test]
fn promote_commit_aborts_if_generation_bumps_during_stop_wait() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _g = lifecycle_gen_test_lock();

    let at = pack_lifecycle_generation();
    let started = AtomicBool::new(false);
    let stop_calls = AtomicUsize::new(0);
    let wait_calls = AtomicUsize::new(0);

    let result = promote_commit_after_stop_wait_detailed(
        at,
        || {
            stop_calls.fetch_add(1, Ordering::SeqCst);
        },
        || {
            wait_calls.fetch_add(1, Ordering::SeqCst);
            // Simulate Enable/disable/uninstall advancing generation while
            // the shell waits for the Direct child port to release.
            bump_pack_lifecycle_generation();
        },
        || {
            started.store(true, Ordering::SeqCst);
            Ok("must-not-run".into())
        },
    );

    assert_eq!(result, Err(PromoteCommitError::LifecycleChangedAfterStop));
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1, "stop must run");
    assert_eq!(wait_calls.load(Ordering::SeqCst), 1, "wait must run");
    assert!(
        !started.load(Ordering::SeqCst),
        "governed start must not run after generation bump in stop/wait gap"
    );
}

/// Happy commit path: matching generation after stop/wait allows spawn.
#[test]
fn promote_commit_proceeds_when_generation_stable() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _g = lifecycle_gen_test_lock();

    let at = pack_lifecycle_generation();
    let started = AtomicBool::new(false);
    let result = promote_commit_after_stop_wait_detailed(
        at,
        || {},
        || {},
        || {
            started.store(true, Ordering::SeqCst);
            Ok("governed-ok".into())
        },
    );
    assert_eq!(result, Ok("governed-ok".into()));
    assert!(started.load(Ordering::SeqCst));
}

/// Post-pack generation mismatch is AbortLifecycleChanged, never WaitNotReady
/// (Copilot: treat lifecycle bump as hard abort, not transient readiness).
#[test]
fn post_pack_gen_mismatch_classifies_as_abort_not_wait() {
    let decision = classify_post_pack_promote_decision(
        1,
        2, // generation advanced after pack step
        true,
        true,
        Some("would look like pack not ready".into()),
    );
    assert_eq!(decision, PromoteFlightDecision::AbortLifecycleChanged);

    // Same gen, pack not ready → WaitNotReady (transient).
    let wait = classify_post_pack_promote_decision(5, 5, false, true, Some("pack cold".into()));
    assert_eq!(
        wait,
        PromoteFlightDecision::WaitNotReady {
            reason: Some("pack cold".into())
        }
    );

    // Same gen, ready → ReadyToPromote.
    assert_eq!(
        classify_post_pack_promote_decision(5, 5, true, true, None),
        PromoteFlightDecision::ReadyToPromote
    );
}

/// Commit-boundary wait port must track config.server_port when set.
#[test]
fn promote_port_release_target_uses_configured_server_port() {
    assert_eq!(promote_port_release_target(Some(9999), 8765), 9999);
    assert_eq!(promote_port_release_target(None, 8765), 8765);
    assert_eq!(promote_port_release_target(Some(18080), 8765), 18080);
}

/// Post-stop lifecycle abort recovery: pack still enabled → fresh governed, not pin Direct.
#[test]
fn after_stop_lifecycle_recovery_prefers_governed_when_pack_enabled() {
    assert_eq!(
        promote_after_stop_lifecycle_recovery(true),
        AfterStopLifecycleRecovery::AttemptGovernedFresh
    );
    assert_eq!(
        promote_after_stop_lifecycle_recovery(false),
        AfterStopLifecycleRecovery::RestoreDirect
    );
}

/// Disable/Stop/Uninstall during the stop/wait window clears enablement; recovery
/// must use the **current** flag (false → RestoreDirect), not the attempt-start
/// snapshot that was true when ReadyToPromote was decided (Bugbot triage 2).
#[test]
fn disable_during_stop_window_yields_restore_direct() {
    // Attempt-start persisted was true (else we never commit). Concurrent
    // disable rewrites via_gateway_default=false; shell re-reads that value.
    let attempt_start_enabled = true;
    let current_enabled_after_concurrent_disable = false;
    assert_ne!(
        attempt_start_enabled, current_enabled_after_concurrent_disable,
        "scenario: enablement flipped during stop/wait"
    );
    assert_eq!(
        promote_after_stop_lifecycle_recovery(current_enabled_after_concurrent_disable),
        AfterStopLifecycleRecovery::RestoreDirect,
        "current disablement must RestoreDirect, not AttemptGovernedFresh"
    );
    // Contrast: if shell wrongly used attempt-start true, recovery would be wrong.
    assert_eq!(
        promote_after_stop_lifecycle_recovery(attempt_start_enabled),
        AfterStopLifecycleRecovery::AttemptGovernedFresh,
        "stale true would incorrectly prefer governed — production must not pass this"
    );
}

/// Pre-stop vs post-stop lifecycle errors are distinct (Bugbot: shell must know
/// whether Council was already torn down).
#[test]
fn lifecycle_changed_error_distinguishes_pre_and_post_stop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _g = lifecycle_gen_test_lock();
    let at = pack_lifecycle_generation();

    // Pre-stop: bump before stop runs.
    bump_pack_lifecycle_generation();
    let pre = promote_commit_after_stop_wait_detailed(
        at,
        || panic!("stop must not run on pre-stop lifecycle abort"),
        || panic!("wait must not run on pre-stop lifecycle abort"),
        || panic!("spawn must not run"),
    );
    assert_eq!(pre, Err(PromoteCommitError::LifecycleChangedBeforeStop));

    // Post-stop: bump during wait after stop.
    let at2 = pack_lifecycle_generation();
    let stops = AtomicUsize::new(0);
    let post = promote_commit_after_stop_wait_detailed(
        at2,
        || {
            stops.fetch_add(1, Ordering::SeqCst);
        },
        || {
            bump_pack_lifecycle_generation();
        },
        || panic!("spawn must not run on post-stop lifecycle abort"),
    );
    assert_eq!(post, Err(PromoteCommitError::LifecycleChangedAfterStop));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

/// Early abort: generation already stale before stop — no stop/wait/spawn.
#[test]
fn promote_commit_early_aborts_before_stop_when_generation_stale() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _g = lifecycle_gen_test_lock();

    let at = pack_lifecycle_generation();
    bump_pack_lifecycle_generation();
    let stop_calls = AtomicUsize::new(0);
    let started = AtomicBool::new(false);
    let result = promote_commit_after_stop_wait_detailed(
        at,
        || {
            stop_calls.fetch_add(1, Ordering::SeqCst);
        },
        || {},
        || {
            started.store(true, Ordering::SeqCst);
            Ok("no".into())
        },
    );
    assert_eq!(result, Err(PromoteCommitError::LifecycleChangedBeforeStop));
    assert_eq!(stop_calls.load(Ordering::SeqCst), 0);
    assert!(!started.load(Ordering::SeqCst));
}
