use super::*;
use super::super::cli_adapters::{
    ensure_cli_adapters, ensure_cli_adapters_with_tokens, ensure_proxy_tokens,
};
use crate::keychain::{
    load_gw_api_key, store_arm_principal_token, store_auth_pepper, store_claude_proxy_token,
    store_codex_proxy_token, store_gw_api_key, SecretStore, ARM_PRINCIPAL_TOKEN_ACCOUNT,
    AUTH_PEPPER_ACCOUNT, CLAUDE_PROXY_TOKEN_ACCOUNT, CODEX_PROXY_TOKEN_ACCOUNT, GW_API_KEY_ACCOUNT,
    KEYCHAIN_SERVICE,
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
            .insert((service.to_string(), account.to_string()), password.to_string());
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
/// adapters + compose secret env, plus pepper + arm principal.
///
/// The pre-fix cold-launch path double-got GW + Claude + Codex (8 gets for
/// 5 distinct accounts) and produced eight sequential authorization dialogs.
fn full_start_resume_keychain_sequence(store: &dyn SecretStore) -> Result<(), String> {
    // Resume flight: single GW get for decide + later auth proof.
    let key = load_gw_api_key(store)?
        .ok_or_else(|| "GW_API_KEY missing".to_string())?;
    let _ = pack_auth_revalidated_with_key(&key); // no Keychain re-entry
    // Single proxy-token load shared by adapters and compose env.
    let proxy_tokens = ensure_proxy_tokens(store)?;
    let _ = ensure_cli_adapters_with_tokens(&proxy_tokens.0, &proxy_tokens.1);
    let _env = super::super::env::build_compose_secret_env(store, None, Some(proxy_tokens))?;
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
    // Exactly the five expected distinct accounts, one get each.
    assert_eq!(
        gets.len(),
        5,
        "expected 5 Keychain gets (one per account), got {gets:?}"
    );
    for account in [
        GW_API_KEY_ACCOUNT,
        CLAUDE_PROXY_TOKEN_ACCOUNT,
        CODEX_PROXY_TOKEN_ACCOUNT,
        AUTH_PEPPER_ACCOUNT,
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
    // Regression anchor: prove the pre-fix call pattern exceeds 5 gets.
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
fn promote_early_window_bounds_resume_calls() {
    // Attempts 0..3 may resume; 4+ revalidate-only.
    assert!(promote_may_call_resume(0, 4, false));
    assert!(promote_may_call_resume(3, 4, false));
    assert!(!promote_may_call_resume(4, 4, false));
    assert!(!promote_may_call_resume(11, 4, false));
    // Ready pack never re-enters resume.
    assert!(!promote_may_call_resume(0, 4, true));
}
