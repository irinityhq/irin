//! Launch / resume decisions and Council child env for the shell.

use super::cli_adapters::{
    ensure_cli_adapters, ensure_cli_adapters_with_tokens, ensure_proxy_tokens,
};
use super::enable::{
    compose_up, lifecycle_stage, port_busy_by_foreign_gateway, wait_control_plane,
};
use super::env::{build_full_compose_env, write_public_compose_env};
use super::health::{desktop_project_running, gateway_health_ok, models_authenticated};
use super::install::{
    compose_file, installed_pack_root, load_validated_manifest, verify_images_present,
};
use super::keys::{ensure_arm_keys_file, ensure_ledger_key};
use super::status::gateway_pack_status_fresh;
use super::types::{GatewayPackState, GatewayPackStatus};
use crate::docker_cli::{probe_docker_daemon, DockerDaemonState, DESKTOP_GATEWAY_URL};
use crate::keychain::{gw_api_key_present, load_gw_api_key, KeychainSecretStore, SecretStore};
use crate::private_config::load_or_create_private_config;
use std::sync::{Mutex, OnceLock};

#[allow(dead_code)]
pub fn default_secret_store() -> KeychainSecretStore {
    KeychainSecretStore
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GatewayChildEnv {
    pub api_key: String,
    pub gateway_url: String,
}

#[allow(dead_code)]
pub fn gateway_child_env_if_ready(
    store: &dyn SecretStore,
) -> Result<Option<GatewayChildEnv>, String> {
    let st = gateway_pack_status_fresh(store);
    if !st.enabled || st.state != GatewayPackState::AuthenticatedReady {
        return Ok(None);
    }
    let key =
        load_gw_api_key(store)?.ok_or_else(|| "GW_API_KEY missing from Keychain".to_string())?;
    Ok(Some(GatewayChildEnv {
        api_key: key,
        gateway_url: DESKTOP_GATEWAY_URL.to_string(),
    }))
}

/// Launch-time revalidation of a persisted governed route: Docker daemon up,
/// the owned pack project running, `/health` OK, and the Keychain-held client
/// key still passing `/v1/models`. Proves pack authentication only — at launch
/// no Council child exists yet, so this says nothing about any child route.
/// Callers spawn the child (governed or Direct) after this decision.
///
/// This function starts nothing. When containers are stopped or Docker is
/// still coming up, use [`resume_installed_pack`] for a bounded start+wait.
///
/// Loads the client key once from Keychain. When the caller already holds the
/// key, use [`pack_auth_revalidated_with_key`] to avoid a repeated get (each
/// get can surface a macOS Keychain authorization dialog).
pub fn pack_auth_revalidated(store: &dyn SecretStore) -> bool {
    match load_gw_api_key(store) {
        Ok(Some(key)) => pack_auth_revalidated_with_key(&key),
        _ => false,
    }
}

/// Same proof as [`pack_auth_revalidated`] using an already-loaded client key
/// (no Keychain re-entry for `GW_API_KEY` on this call).
pub fn pack_auth_revalidated_with_key(key: &str) -> bool {
    if !matches!(probe_docker_daemon(), DockerDaemonState::Ready) {
        return false;
    }
    if installed_pack_root().is_none() || !desktop_project_running() || !gateway_health_ok() {
        return false;
    }
    models_authenticated(key)
}

/// Pure resume work selection: avoid re-entering Keychain-heavy compose when
/// the pack project is already running (promote retries must poll/wait, not
/// force-recreate). Full start only when containers are down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePackAction {
    AlreadyReady,
    /// Project up: wait for control-plane + auth only (no compose secret rebuild).
    WaitOnly,
    /// Project down: compose up + secret env + adapters.
    FullStart,
}

/// Decide the resume path from revalidation + project liveness only.
pub fn decide_resume_pack_action(pack_auth_ok: bool, project_running: bool) -> ResumePackAction {
    if pack_auth_ok {
        ResumePackAction::AlreadyReady
    } else if project_running {
        ResumePackAction::WaitOnly
    } else {
        ResumePackAction::FullStart
    }
}

/// Pure promote policy: early attempts may enter [`resume_installed_pack`]
/// (which itself chooses wait-only vs full-start). After the early window,
/// only revalidation polls remain — no stacked compose/Keychain storms.
pub fn promote_may_call_resume(attempt: u32, max_early_resume_attempts: u32, pack_auth_ok: bool) -> bool {
    !pack_auth_ok && attempt < max_early_resume_attempts
}

fn resume_flight_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Cold-launch / recovery resume: start the already-installed app-owned pack
/// and wait bounded for control-plane health + Keychain key auth.
///
/// Does **not** provision new keys, does **not** flip `via_gateway_default`,
/// and does **not** claim Council is governed. Fail-closed: `Err` means the
/// pack is not authenticated-ready (caller starts Council Direct and may
/// schedule a later bounded promote).
///
/// Single-flight: concurrent promote + launch callers join one resume so
/// compose/Keychain work cannot overlap and supersede authorization prompts.
pub fn resume_installed_pack(store: &dyn SecretStore) -> Result<(), String> {
    let _flight = resume_flight_lock()
        .lock()
        .map_err(|_| "gateway pack resume lock poisoned".to_string())?;
    resume_installed_pack_locked(store)
}

fn resume_installed_pack_locked(store: &dyn SecretStore) -> Result<(), String> {
    let project_running = desktop_project_running();
    // One GW_API_KEY Keychain get for the whole resume flight. Prior path
    // re-loaded the same account on revalidate, compose, and final proof —
    // each get can surface a separate macOS authorization dialog.
    let key_opt = load_gw_api_key(store).ok().flatten();
    let pack_auth_ok = key_opt
        .as_ref()
        .is_some_and(|k| pack_auth_revalidated_with_key(k));
    match decide_resume_pack_action(pack_auth_ok, project_running) {
        ResumePackAction::AlreadyReady => {
            // Pack is governed-ready; still ensure host adapters after app relaunch.
            let _ = ensure_cli_adapters(store);
            return Ok(());
        }
        ResumePackAction::WaitOnly => {
            return resume_wait_only(store, key_opt);
        }
        ResumePackAction::FullStart => {}
    }
    match probe_docker_daemon() {
        DockerDaemonState::Ready => {}
        DockerDaemonState::CliMissing => {
            return Err("Docker CLI missing; cannot resume Gateway Pack".to_string());
        }
        DockerDaemonState::DaemonDown => {
            return Err("Docker daemon not ready; cannot resume Gateway Pack".to_string());
        }
    }
    let pack_root = installed_pack_root()
        .ok_or_else(|| "Gateway Pack is not installed; cannot resume governed route".to_string())?;
    let validated = load_validated_manifest(&pack_root)?;
    verify_images_present(&validated)?;
    let ledger = ensure_ledger_key()?;
    let _ = ensure_arm_keys_file();
    // Single-pass Keychain for Claude/Codex: load tokens once, start adapters
    // with those values, inject the same values into compose env (no second get).
    let proxy_tokens = ensure_proxy_tokens(store)?;
    let _ = ensure_cli_adapters_with_tokens(&proxy_tokens.0, &proxy_tokens.1);
    let existing_key_id = load_or_create_private_config()?.gateway_key_id;
    let env_path = write_public_compose_env(
        &pack_root,
        &ledger,
        &validated.gateway,
        &validated.sidecar,
        existing_key_id.as_deref(),
    )?;
    if port_busy_by_foreign_gateway()? {
        return Err(
            "port 18080 is in use by a process outside irin-desktop-gateway; cannot resume"
                .to_string(),
        );
    }
    let key = key_opt.ok_or_else(|| {
        "GW_API_KEY missing from Keychain; cannot resume governed route".to_string()
    })?;
    let spawn_env = build_full_compose_env(
        store,
        None,
        &pack_root,
        &ledger,
        &validated,
        existing_key_id.as_deref(),
        Some(proxy_tokens),
    )?;
    lifecycle_stage("resume_compose_up", "begin");
    compose_up(&compose_file(&pack_root), &env_path, &spawn_env).inspect_err(|_| {
        lifecycle_stage("resume_compose_up", "error");
    })?;
    lifecycle_stage("resume_compose_up", "ok");
    wait_control_plane().inspect_err(|_| {
        lifecycle_stage("resume_wait", "error");
    })?;
    lifecycle_stage("resume_wait", "ok");
    if !models_authenticated(&key) {
        lifecycle_stage("resume_auth", "error");
        return Err("Gateway client key failed /v1/models after pack resume".to_string());
    }
    lifecycle_stage("resume_auth", "ok");
    if !pack_auth_revalidated_with_key(&key) {
        return Err("pack resume completed but revalidation still false".to_string());
    }
    Ok(())
}

/// Project already running: do not rebuild compose secret env / force-recreate.
/// Ensure host adapters, wait for control plane, re-check models auth.
/// `key_opt` is the GW_API_KEY already loaded for this resume flight (no re-get).
fn resume_wait_only(store: &dyn SecretStore, key_opt: Option<String>) -> Result<(), String> {
    match probe_docker_daemon() {
        DockerDaemonState::Ready => {}
        DockerDaemonState::CliMissing => {
            return Err("Docker CLI missing; cannot resume Gateway Pack".to_string());
        }
        DockerDaemonState::DaemonDown => {
            return Err("Docker daemon not ready; cannot resume Gateway Pack".to_string());
        }
    }
    let _ = ensure_cli_adapters(store);
    let key = key_opt.ok_or_else(|| {
        "GW_API_KEY missing from Keychain; cannot resume governed route".to_string()
    })?;
    lifecycle_stage("resume_wait_only", "begin");
    wait_control_plane().inspect_err(|_| {
        lifecycle_stage("resume_wait_only", "error");
    })?;
    lifecycle_stage("resume_wait_only", "ok");
    if !models_authenticated(&key) {
        lifecycle_stage("resume_auth", "error");
        return Err("Gateway client key failed /v1/models after pack resume wait".to_string());
    }
    lifecycle_stage("resume_auth", "ok");
    if !pack_auth_revalidated_with_key(&key) {
        return Err("pack resume wait completed but revalidation still false".to_string());
    }
    Ok(())
}

/// Pure launch decision: spawn governed only when the operator left the pack
/// enabled and pack-side auth is proven (or was just resumed).
#[cfg(test)]
pub fn decide_launch_via_gateway(persisted_via_gateway: bool, pack_auth_ok: bool) -> bool {
    persisted_via_gateway && pack_auth_ok
}

/// Pure: packaged installs must not let the frontend call `start_council_server`.
/// Source-dev (unpackaged) keeps the existing frontend start path.
#[cfg(test)]
pub fn frontend_may_start_council(packaged_install: bool) -> bool {
    !packaged_install
}

/// Pure cold-launch race model: first starter owns the child lock; the loser
/// cannot correct ownership. Packaged policy forbids frontend start so native
/// is the sole owner.
///
/// Returns the owned child's `via_gateway` after both sides attempt, or `None`
/// if nobody starts.
#[cfg(test)]
pub fn cold_launch_owned_via_gateway(
    packaged: bool,
    persisted_via_gateway: bool,
    pack_auth_ok: bool,
    frontend_starts: bool,
    frontend_wins_race: bool,
) -> Option<bool> {
    let native_starts = packaged;
    let native_via = decide_launch_via_gateway(persisted_via_gateway, pack_auth_ok);
    // Packaged frontend start uses via_gateway=None → Direct.
    let frontend_via = false;
    match (native_starts, frontend_starts) {
        (false, false) => None,
        (true, false) => Some(native_via),
        (false, true) => Some(frontend_via),
        (true, true) => {
            if frontend_wins_race {
                Some(frontend_via)
            } else {
                Some(native_via)
            }
        }
    }
}

/// Pure: after a fail-closed Direct spawn with pack still enabled, a later
/// recovery may promote to governed without manual re-enable when pack auth
/// becomes ready and the owned child is still Direct.
pub fn may_promote_to_governed(
    persisted_via_gateway: bool,
    owned_route: Option<bool>,
    pack_auth_ok: bool,
) -> bool {
    persisted_via_gateway && owned_route == Some(false) && pack_auth_ok
}

/// Pure resume outcome for launch (success + fail-closed). Does not invent
/// governed readiness: governed only when pack is ready and spawn succeeds.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchResumeOutcome {
    Governed,
    DirectFailClosed,
}

#[cfg(test)]
pub fn decide_launch_resume_outcome(
    persisted_via_gateway: bool,
    pack_ready_immediately: bool,
    resume_ok: bool,
    governed_spawn_ok: bool,
) -> LaunchResumeOutcome {
    if !persisted_via_gateway {
        return LaunchResumeOutcome::DirectFailClosed;
    }
    let pack_ok = pack_ready_immediately || resume_ok;
    if pack_ok && governed_spawn_ok {
        LaunchResumeOutcome::Governed
    } else {
        LaunchResumeOutcome::DirectFailClosed
    }
}

/// Mark status after Council restart proof (called from lib.rs).
///
/// Always takes a fresh sample: this is a post-lifecycle authority surface.
pub fn status_with_council_route(
    store: &dyn SecretStore,
    council_governed: bool,
    council_direct: bool,
) -> GatewayPackStatus {
    let mut st = gateway_pack_status_fresh(store);
    if council_governed && st.authenticated && st.enabled && gateway_health_ok() {
        st.state = GatewayPackState::AuthenticatedReady;
        st.council_governed = true;
        st.message = "Gateway Pack is authenticated and Council is governed.".into();
    } else if council_direct && !st.enabled {
        st.council_governed = false;
        if st.state == GatewayPackState::AuthenticatedReady {
            st.state = GatewayPackState::Disabled;
        }
    } else if st.enabled && st.authenticated && !council_governed {
        st.state = GatewayPackState::Degraded;
        st.council_governed = false;
        st.message = "Gateway is authenticated but Council did not enter governed mode.".into();
    }
    let _ = gw_api_key_present(store);
    // Post-lifecycle: pack is expected running when we reach here after enable;
    // do not treat as stopped hard-down solely because child route changed.
    st.refresh_predicates(false);
    st
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
