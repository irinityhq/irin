//! Launch / resume decisions and Council child env for the shell.

use super::cli_adapters::{
    current_status as current_adapter_status, ensure_cli_adapters_with_tokens, ensure_proxy_tokens,
    CliAdaptersStatus,
};
use super::enable::{
    compose_up, lifecycle_stage, port_busy_by_foreign_gateway, wait_control_plane,
};
use super::env::{
    build_full_compose_env, build_full_compose_env_with_launch_secrets,
    load_or_create_watch_admin_token, write_public_compose_env, LaunchSecrets,
    PACK_WATCH_CANARY_TENANT,
};
use super::health::{
    desktop_project_running, gateway_health_ok, http_get_status, models_authenticated,
};
use super::install::{
    compose_file, installed_pack_root, load_validated_manifest, verify_images_present,
};
use super::keys::{ensure_arm_keys_file, ensure_ledger_key};
use super::status::gateway_pack_status_fresh;
use super::types::{GatewayPackState, GatewayPackStatus};
use crate::docker_cli::{probe_docker_daemon, DockerDaemonState, DESKTOP_GATEWAY_URL};
use crate::keychain::{load_gw_api_key, KeychainSecretStore, SecretStore};
use crate::private_config::load_or_create_private_config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static WATCH_TOKEN_RECONCILIATION_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static ADAPTER_RECONCILE_PENDING: AtomicBool = AtomicBool::new(false);

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
    // Fresh authority sample: gateway_pack_status_fresh loads the key once and
    // runs /v1/models live (never the presentation cache). If the pack is
    // authenticated-ready, load the key again for the env — this authority path
    // is one-shot per spawn, not on the 5s background tick, so the extra get
    // does not produce repeated prompts.
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

/// A running pre-token pack is reconciled at most once per app process. Both
/// authenticated read surfaces must accept the held token; model auth alone
/// proves only the older Gateway contract.
pub fn decide_watch_token_reconciliation(
    pack_auth_ok: bool,
    watch_status: Option<u16>,
    outbox_status: Option<u16>,
    already_attempted: bool,
) -> bool {
    pack_auth_ok && (watch_status == Some(401) || outbox_status == Some(401)) && !already_attempted
}

pub fn governed_launch_after_watch_reconciliation(
    pack_auth_ok: bool,
    reconciliation_ok: bool,
) -> bool {
    pack_auth_ok && reconciliation_ok
}

pub fn watch_admin_surfaces_ready(watch_status: Option<u16>, outbox_status: Option<u16>) -> bool {
    watch_status == Some(200) && outbox_status == Some(200)
}

pub fn watch_admin_surfaces_authenticated(token: &str) -> bool {
    let (watch_status, outbox_status) = watch_admin_surface_statuses(token);
    watch_admin_surfaces_ready(watch_status, outbox_status)
}

fn watch_admin_surface_statuses(token: &str) -> (Option<u16>, Option<u16>) {
    let watch = http_get_status(
        &format!("{DESKTOP_GATEWAY_URL}/watch/ui-snapshot/{PACK_WATCH_CANARY_TENANT}"),
        Some(token),
    )
    .ok()
    .map(|(status, _)| status);
    let outbox = http_get_status(
        &format!("{DESKTOP_GATEWAY_URL}/watch/outbox/{PACK_WATCH_CANARY_TENANT}"),
        Some(token),
    )
    .ok()
    .map(|(status, _)| status);
    (watch, outbox)
}

/// Pure promote policy: early attempts may enter [`resume_installed_pack`]
/// (which itself chooses wait-only vs full-start). After the early window,
/// only revalidation polls remain — no stacked compose/Keychain storms.
pub fn promote_may_call_resume(
    attempt: u32,
    max_early_resume_attempts: u32,
    pack_auth_ok: bool,
) -> bool {
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
    resume_installed_pack_locked(store, None, None)
}

/// Resume with the GW key and compose secrets already loaded by cold launch.
/// This variant performs no Keychain reads for those six launch accounts.
pub fn resume_installed_pack_with_key(
    store: &dyn SecretStore,
    key: &str,
    launch_secrets: &LaunchSecrets,
) -> Result<(), String> {
    let _flight = resume_flight_lock()
        .lock()
        .map_err(|_| "gateway pack resume lock poisoned".to_string())?;
    resume_installed_pack_locked(store, Some(key.to_string()), Some(launch_secrets))
}

fn resume_installed_pack_locked(
    store: &dyn SecretStore,
    held_key: Option<String>,
    launch_secrets: Option<&LaunchSecrets>,
) -> Result<(), String> {
    let project_running = desktop_project_running();
    // One GW_API_KEY Keychain get for the whole resume flight. Prior path
    // re-loaded the same account on revalidate, compose, and final proof —
    // each get can surface a separate macOS authorization dialog.
    let key_opt = held_key.or_else(|| load_gw_api_key(store).ok().flatten());
    let pack_auth_ok = key_opt
        .as_ref()
        .is_some_and(|k| pack_auth_revalidated_with_key(k));
    let watch_token = match launch_secrets {
        Some(secrets) => Some(secrets.watch_admin_token.clone()),
        None if pack_auth_ok => Some(load_or_create_watch_admin_token(store)?),
        None => None,
    };
    let (watch_status, outbox_status) = watch_token
        .as_deref()
        .map(watch_admin_surface_statuses)
        .unwrap_or((None, None));
    let reconcile_watch_token = watch_token.is_some()
        && decide_watch_token_reconciliation(
            pack_auth_ok,
            watch_status,
            outbox_status,
            WATCH_TOKEN_RECONCILIATION_ATTEMPTED.load(Ordering::SeqCst),
        );
    if reconcile_watch_token {
        WATCH_TOKEN_RECONCILIATION_ATTEMPTED.store(true, Ordering::SeqCst);
        lifecycle_stage("resume_watch_token_reconcile", "begin");
    }
    if pack_auth_ok
        && !watch_admin_surfaces_ready(watch_status, outbox_status)
        && !reconcile_watch_token
    {
        return Err(
            "Gateway Pack models auth passed but Watch/Outbox admin auth is not ready".to_string(),
        );
    }
    match decide_resume_pack_action(pack_auth_ok, project_running) {
        ResumePackAction::AlreadyReady if !reconcile_watch_token => {
            // Pack is governed-ready; ensure host adapters after app relaunch.
            // A NotReady→Ready transition also force-recreates the running
            // containers so their proxy URL/token environment becomes current.
            ensure_and_reconcile_launch_adapters(store, launch_secrets)?;
            return Ok(());
        }
        ResumePackAction::WaitOnly => {
            resume_wait_only(store, key_opt.clone(), launch_secrets)?;
            return resume_installed_pack_locked(store, key_opt, launch_secrets);
        }
        ResumePackAction::AlreadyReady | ResumePackAction::FullStart => {}
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
    let proxy_tokens = match launch_secrets {
        Some(secrets) => secrets.proxy_tokens.clone(),
        None => ensure_proxy_tokens(store)?,
    };
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
    let spawn_env = match launch_secrets {
        Some(secrets) => build_full_compose_env_with_launch_secrets(
            None,
            &pack_root,
            &ledger,
            &validated,
            existing_key_id.as_deref(),
            secrets,
        )?,
        None => build_full_compose_env(
            store,
            None,
            &pack_root,
            &ledger,
            &validated,
            existing_key_id.as_deref(),
            Some(proxy_tokens),
        )?,
    };
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
    let final_watch_token = match watch_token {
        Some(token) => token,
        None => load_or_create_watch_admin_token(store)?,
    };
    let (watch_status, outbox_status) = watch_admin_surface_statuses(&final_watch_token);
    if !watch_admin_surfaces_ready(watch_status, outbox_status) {
        if reconcile_watch_token {
            lifecycle_stage("resume_watch_token_reconcile", "error");
        }
        return Err(
            "Gateway Pack resume completed but Watch/Outbox admin auth is not ready".to_string(),
        );
    }
    if reconcile_watch_token {
        lifecycle_stage("resume_watch_token_reconcile", "ok");
    }
    Ok(())
}

fn adapter_became_ready(before: CliAdaptersStatus, after: CliAdaptersStatus) -> bool {
    (!before.claude.is_ready() && after.claude.is_ready())
        || (!before.codex.is_ready() && after.codex.is_ready())
}

fn adapter_reconcile_required(
    before: CliAdaptersStatus,
    after: CliAdaptersStatus,
    retry_pending: bool,
) -> bool {
    retry_pending || adapter_became_ready(before, after)
}

fn ensure_and_reconcile_launch_adapters(
    store: &dyn SecretStore,
    launch_secrets: Option<&LaunchSecrets>,
) -> Result<(), String> {
    let before = current_adapter_status();
    let proxy_tokens = match launch_secrets {
        Some(secrets) => secrets.proxy_tokens.clone(),
        // Adapter availability is optional to the governed pack. Preserve the
        // existing best-effort launch contract if Keychain token access fails;
        // status refresh will project TokenMissing separately.
        None => match ensure_proxy_tokens(store) {
            Ok(tokens) => tokens,
            Err(_) => return Ok(()),
        },
    };
    let after = ensure_cli_adapters_with_tokens(&proxy_tokens.0, &proxy_tokens.1);
    let became_ready = adapter_became_ready(before, after);
    if became_ready {
        // Set before any fallible reconciliation work. A failure leaves this
        // sticky so a later resume retries even though adapter status is now
        // Ready→Ready.
        ADAPTER_RECONCILE_PENDING.store(true, Ordering::SeqCst);
    }
    if !adapter_reconcile_required(
        before,
        after,
        ADAPTER_RECONCILE_PENDING.load(Ordering::SeqCst),
    ) {
        return Ok(());
    }

    let pack_root = installed_pack_root().ok_or_else(|| {
        "Gateway Pack is not installed; cannot reconcile CLI adapters".to_string()
    })?;
    let validated = load_validated_manifest(&pack_root)?;
    verify_images_present(&validated)?;
    let ledger = ensure_ledger_key()?;
    let existing_key_id = load_or_create_private_config()?.gateway_key_id;
    let env_path = write_public_compose_env(
        &pack_root,
        &ledger,
        &validated.gateway,
        &validated.sidecar,
        existing_key_id.as_deref(),
    )?;
    let spawn_env = match launch_secrets {
        Some(secrets) => build_full_compose_env_with_launch_secrets(
            None,
            &pack_root,
            &ledger,
            &validated,
            existing_key_id.as_deref(),
            secrets,
        )?,
        None => build_full_compose_env(
            store,
            None,
            &pack_root,
            &ledger,
            &validated,
            existing_key_id.as_deref(),
            Some(proxy_tokens),
        )?,
    };
    lifecycle_stage("resume_adapter_reconcile", "begin");
    compose_up(&compose_file(&pack_root), &env_path, &spawn_env).inspect_err(|_| {
        lifecycle_stage("resume_adapter_reconcile", "error");
    })?;
    wait_control_plane()?;
    ADAPTER_RECONCILE_PENDING.store(false, Ordering::SeqCst);
    lifecycle_stage("resume_adapter_reconcile", "ok");
    Ok(())
}

/// Project already running: wait rather than unconditionally rebuilding. A CLI
/// adapter readiness transition performs one bounded env reconcile/recreate.
/// Then wait for control plane and re-check models auth.
/// `key_opt` is the GW_API_KEY already loaded for this resume flight (no re-get).
fn resume_wait_only(
    store: &dyn SecretStore,
    key_opt: Option<String>,
    launch_secrets: Option<&LaunchSecrets>,
) -> Result<(), String> {
    match probe_docker_daemon() {
        DockerDaemonState::Ready => {}
        DockerDaemonState::CliMissing => {
            return Err("Docker CLI missing; cannot resume Gateway Pack".to_string());
        }
        DockerDaemonState::DaemonDown => {
            return Err("Docker daemon not ready; cannot resume Gateway Pack".to_string());
        }
    }
    ensure_and_reconcile_launch_adapters(store, launch_secrets)?;
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
    // The fresh sample at the top already resolved key presence/authentication;
    // no second Keychain get is needed here.
    // Post-lifecycle: pack is expected running when we reach here after enable;
    // do not treat as stopped hard-down solely because child route changed.
    st.refresh_predicates(false);
    st
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
