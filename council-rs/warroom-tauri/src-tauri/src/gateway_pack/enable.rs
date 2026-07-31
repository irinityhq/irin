//! Pack lifecycle mutations: enable / disable / stop / uninstall.

use super::cli_adapters::{
    ensure_cli_adapters_with_tokens, ensure_proxy_tokens, stop_cli_adapters,
};
use super::env::{build_full_compose_env, teardown_compose_env, write_public_compose_env};
use super::health::{
    admin_surface_ready, desktop_project_running, gateway_health_ok, models_authenticated,
    models_fail_closed_without_key, provision_council_client,
};
use super::install::{
    compose_file, install_pack_files, installed_pack_root, load_validated_manifest,
    verify_images_present, verify_pack_asset_integrity,
};
use super::keys::{ensure_arm_keys_file, ensure_ledger_key, random_hex};
use super::paths::{gateway_data_dir, public_env_path, PACK_DIR_NAME};
use super::status::{
    bump_pack_lifecycle_generation, gateway_pack_status_fresh, gateway_pack_status_fresh_with_key,
    invalidate_auth_observation, invalidate_status_cache,
};
use super::types::{GatewayPackState, GatewayPackStatus};
use crate::docker_cli::{
    compose_command_with_env, format_cmd_failure, probe_docker_daemon, resolve_docker_cli,
    ComposeEnv, DockerDaemonState, DOCKER_CMD_TIMEOUT, DOCKER_COMPOSE_UP_TIMEOUT,
};
use crate::keychain::{
    delete_all_gateway_pack_secrets, is_valid_gw_raw_key, load_gw_api_key, SecretStore,
};
use crate::private_config::{load_or_create_private_config, write_private_config_at};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// Global lifecycle lock — enable/disable/stop/uninstall must not interleave.
/// Global lifecycle lock — enable/disable/stop/uninstall must not interleave.
static LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) const COMPOSE_UP_ARGS: &[&str] =
    &["up", "-d", "--remove-orphans", "--force-recreate", "--wait"];

pub(crate) fn compose_up(
    compose: &Path,
    env_path: &Path,
    spawn_env: &ComposeEnv,
) -> Result<(), String> {
    // The installed pack tree is user-writable; prove it still matches the
    // code-signed bundle (re-staging once on mismatch) before any spawn that
    // carries pack secrets.
    if let Some(pack_root) = compose.parent() {
        verify_pack_asset_integrity(pack_root)?;
    }
    // Every explicit Enable is a boot-time configuration reload: Keychain arm
    // principals, the attestation registry, provider env, and atomically staged
    // bind mounts must all reach fresh containers. A successful no-op `up`
    // leaves old allowlists and stale macOS file-share mounts in place.
    let up = compose_command_with_env(
        compose,
        Some(env_path),
        COMPOSE_UP_ARGS,
        Some(spawn_env),
        DOCKER_COMPOSE_UP_TIMEOUT,
    )?;
    if !up.status.success() {
        return Err(format_cmd_failure("gateway pack up", &up));
    }
    Ok(())
}

/// Append a single non-secret lifecycle stage line for operator/smoke diagnosis.
/// Never logs values, keys, paths that may contain credentials, or command output.
pub fn lifecycle_stage(stage: &str, detail: &str) {
    let dir = gateway_data_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("lifecycle.log");
    let line = format!(
        "{} stage={} detail={}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        stage,
        detail
    );
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Full enable workflow. Returns ready only when Gateway auth is proven.
/// Caller (lib.rs) must restart Council into governed mode and treat restart
/// failure as overall failure (not ready).
pub fn enable_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;
    bump_pack_lifecycle_generation();
    invalidate_status_cache();
    invalidate_auth_observation();
    // A pack identity change invalidates any prior rehearsal presentation.
    crate::touch_id::clear_rehearsal_passed();
    lifecycle_stage("enable_begin", "ok");

    match probe_docker_daemon() {
        DockerDaemonState::CliMissing => {
            lifecycle_stage("enable_abort", "docker_cli_missing");
            return Ok(gateway_pack_status_fresh(store));
        }
        DockerDaemonState::DaemonDown => {
            lifecycle_stage("enable_abort", "docker_daemon_down");
            return Ok(gateway_pack_status_fresh(store));
        }
        DockerDaemonState::Ready => {}
    }
    let _ = resolve_docker_cli()?;
    lifecycle_stage("docker_ready", "ok");

    let pack_root = install_pack_files().inspect_err(|_| {
        lifecycle_stage("install_pack", "error");
    })?;
    lifecycle_stage("install_pack", "ok");
    let validated = load_validated_manifest(&pack_root).inspect_err(|_| {
        lifecycle_stage("manifest", "error");
    })?;
    verify_images_present(&validated).inspect_err(|_| {
        lifecycle_stage("verify_images", "error");
    })?;
    lifecycle_stage("verify_images", "ok");

    let ledger = ensure_ledger_key().inspect_err(|_| {
        lifecycle_stage("ledger", "error");
    })?;
    lifecycle_stage("ledger", "ok");
    // Touch ID bridge: the registry bind source must exist as a FILE before
    // compose up. Default `[]` = fail-closed unloaded registry, so enabling
    // Gateway never arms anything.
    ensure_arm_keys_file().inspect_err(|_| {
        lifecycle_stage("arm_keys", "error");
    })?;
    lifecycle_stage("arm_keys", "ok");

    // Host CLI adapters before compose env: mint Keychain tokens once and start
    // app-owned Claude/Codex listeners when CLIs are present+authenticated.
    // Tokens are reused for every compose secret env on this enable flight so
    // each proxy account is read at most once (Keychain can prompt per get).
    // Missing CLI leaves that route empty (fail-closed) and does not abort Enable.
    let proxy_tokens = ensure_proxy_tokens(store).inspect_err(|_| {
        lifecycle_stage("cli_adapters", "token_error");
    })?;
    let adapter_status = ensure_cli_adapters_with_tokens(&proxy_tokens.0, &proxy_tokens.1);
    lifecycle_stage(
        "cli_adapters",
        &format!(
            "claude={} codex={}",
            if adapter_status.claude.is_ready() {
                "ready"
            } else {
                adapter_status.claude_reason.as_str()
            },
            if adapter_status.codex.is_ready() {
                "ready"
            } else {
                adapter_status.codex_reason.as_str()
            }
        ),
    );

    let existing_key_id = load_or_create_private_config()?.gateway_key_id;
    let env_path = write_public_compose_env(
        &pack_root,
        &ledger,
        &validated.gateway,
        &validated.sidecar,
        existing_key_id.as_deref(),
    )
    .inspect_err(|_| {
        lifecycle_stage("public_env", "error");
    })?;
    lifecycle_stage("public_env", "ok");

    if port_busy_by_foreign_gateway()? {
        lifecycle_stage("port_check", "foreign_busy");
        return Err(
            "port 18080 is in use by a process outside irin-desktop-gateway; \
             stop the foreign Gateway or free the port. The desktop pack will not replace it."
                .to_string(),
        );
    }
    lifecycle_stage("port_check", "ok");

    let compose = compose_file(&pack_root);

    // Reuse existing Keychain key if still valid after start; else provision with bootstrap.
    let existing = load_gw_api_key(store).inspect_err(|_| {
        lifecycle_stage("keychain_load", "error");
    })?;
    lifecycle_stage(
        "keychain_load",
        if existing.is_some() {
            "present"
        } else {
            "absent"
        },
    );
    let need_provision = match existing.as_ref() {
        Some(k) => {
            // Start without bootstrap first if we might already be provisioned.
            let spawn_env = build_full_compose_env(
                store,
                None,
                &pack_root,
                &ledger,
                &validated,
                existing_key_id.as_deref(),
                Some(proxy_tokens.clone()),
            )
            .inspect_err(|_| {
                lifecycle_stage("secret_env", "error");
            })?;
            lifecycle_stage("secret_env", "ok");
            compose_up(&compose, &env_path, &spawn_env).inspect_err(|_| {
                lifecycle_stage("compose_up_existing", "error");
            })?;
            lifecycle_stage("compose_up_existing", "ok");
            wait_control_plane().inspect_err(|_| {
                lifecycle_stage("wait_control_plane", "error");
            })?;
            lifecycle_stage("wait_control_plane", "ok");
            !models_authenticated(k)
        }
        None => true,
    };
    lifecycle_stage(
        "need_provision",
        if need_provision { "true" } else { "false" },
    );

    let key_id = if need_provision {
        // Generate bootstrap only for provisioning.
        let bootstrap = random_hex(32)?;
        let spawn_env = build_full_compose_env(
            store,
            Some(&bootstrap),
            &pack_root,
            &ledger,
            &validated,
            existing_key_id.as_deref(),
            Some(proxy_tokens.clone()),
        )
        .inspect_err(|e| {
            // Fixed non-secret categories only — never log the error body if it
            // could include env material. Classify known prefixes.
            let cat = if e.contains("keychain") {
                "keychain_error"
            } else if e.contains("env value") || e.contains("forbidden") {
                "env_validate_error"
            } else {
                "secret_env_error"
            };
            lifecycle_stage("secret_env_bootstrap", cat);
        })?;
        lifecycle_stage("secret_env_bootstrap", "ok");
        compose_up(&compose, &env_path, &spawn_env).inspect_err(|_| {
            lifecycle_stage("compose_up_bootstrap", "error");
        })?;
        lifecycle_stage("compose_up_bootstrap", "ok");
        wait_control_plane().inspect_err(|_| {
            lifecycle_stage("wait_control_plane_bootstrap", "error");
        })?;
        lifecycle_stage("wait_control_plane_bootstrap", "ok");
        if !models_fail_closed_without_key() {
            lifecycle_stage("models_fail_closed", "error");
            return Err("gateway /v1/models did not fail closed without a client key".to_string());
        }
        lifecycle_stage("models_fail_closed", "ok");
        let kid = provision_council_client(store, &bootstrap).inspect_err(|_| {
            lifecycle_stage("provision", "error");
        })?;
        lifecycle_stage("provision", "ok");
        // Blank bootstrap and recreate sidecar without it.
        let spawn_env_blank = build_full_compose_env(
            store,
            None,
            &pack_root,
            &ledger,
            &validated,
            Some(&kid),
            Some(proxy_tokens.clone()),
        )
        .inspect_err(|_| {
            lifecycle_stage("secret_env_blank", "error");
        })?;
        write_public_compose_env(
            &pack_root,
            &ledger,
            &validated.gateway,
            &validated.sidecar,
            Some(&kid),
        )?;
        compose_up(&compose, &env_path, &spawn_env_blank).inspect_err(|_| {
            lifecycle_stage("compose_up_blank", "error");
        })?;
        wait_control_plane().inspect_err(|_| {
            lifecycle_stage("wait_control_plane_blank", "error");
        })?;
        kid
    } else {
        if !models_fail_closed_without_key() {
            lifecycle_stage("models_fail_closed", "error");
            return Err("gateway /v1/models did not fail closed without a client key".to_string());
        }
        existing_key_id.unwrap_or_else(|| "existing".into())
    };

    // Confirm auth after provision path.
    let key = load_gw_api_key(store)?
        .ok_or_else(|| "GW_API_KEY missing from Keychain after enable".to_string())?;
    if !models_authenticated(&key) {
        lifecycle_stage("models_auth", "error");
        return Err("Gateway client key failed /v1/models after enable".to_string());
    }
    lifecycle_stage("models_auth", "ok");

    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = true;
    cfg.gateway_key_id = Some(key_id);
    cfg.gateway_pack_version = Some(validated.pack_version.clone());
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    assert_private_json_has_no_raw_key()?;
    lifecycle_stage("enable_complete", "authenticated");

    let mut st = gateway_pack_status_fresh_with_key(store, Some(&key));
    // Not fully ready until Council restart succeeds — lib marks the proven
    // governed child. Pack auth alone is spawn-capable but not governed-ready.
    if st.authenticated && st.enabled {
        st.state = GatewayPackState::Degraded;
        st.council_governed = false;
        st.message =
            "Gateway Pack authenticated. Council restart required for governed mode.".into();
    }
    st.refresh_predicates(false);
    Ok(st)
}

pub(crate) fn wait_control_plane() -> Result<(), String> {
    for _ in 0..60 {
        if gateway_health_ok() && admin_surface_ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(
        "gateway pack started but authenticated control plane is not ready \
         (/health + /admin/keys not accepting requests)"
            .to_string(),
    )
}

pub(crate) fn assert_private_json_has_no_raw_key() -> Result<(), String> {
    let path = crate::private_config::private_config_path();
    if !path.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    if raw.contains("gw_") {
        for part in raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if is_valid_gw_raw_key(part) {
                return Err("private.json must never contain the raw GW_API_KEY".to_string());
            }
        }
    }
    Ok(())
}

pub(crate) fn port_busy_by_foreign_gateway() -> Result<bool, String> {
    if desktop_project_running() {
        return Ok(false);
    }
    if gateway_health_ok() {
        return Ok(true);
    }
    use std::net::TcpStream;
    match TcpStream::connect_timeout(
        &"127.0.0.1:18080".parse().unwrap(),
        Duration::from_millis(200),
    ) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Disable governed mode: flip private config. Does not delete pack data/Keychain.
pub fn disable_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;
    bump_pack_lifecycle_generation();
    invalidate_status_cache();
    invalidate_auth_observation();
    crate::touch_id::clear_rehearsal_passed();
    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = false;
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    let _ = store;
    Ok(gateway_pack_status_fresh(store))
}

/// Stop desktop compose project only after Direct mode is recorded.
/// Refuses if still enabled (via_gateway_default) — caller must disable first,
/// or we auto-disable then stop so Council is not left governed against a dead Gateway.
pub fn stop_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    lifecycle_stage("stop_begin", "ok");
    let _guard = match LIFECYCLE_LOCK.lock() {
        Ok(guard) => guard,
        Err(_) => {
            lifecycle_stage("stop_lock", "error");
            return Err("gateway pack lifecycle lock poisoned".to_string());
        }
    };
    bump_pack_lifecycle_generation();
    invalidate_status_cache();
    invalidate_auth_observation();
    crate::touch_id::clear_rehearsal_passed();
    lifecycle_stage("stop_lock", "ok");

    let mut cfg = match load_or_create_private_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            lifecycle_stage("stop_config", "error");
            return Err(err);
        }
    };
    if cfg.via_gateway_default {
        // Switch to Direct first so we never leave enabled Council against stopped Gateway.
        cfg.via_gateway_default = false;
        if let Err(err) =
            write_private_config_at(&crate::private_config::private_config_path(), &cfg)
        {
            lifecycle_stage("stop_config", "error");
            return Err(err);
        }
        lifecycle_stage("stop_config", "updated_direct");
    } else {
        lifecycle_stage("stop_config", "already_direct");
    }

    // Stop app-owned host adapters with the pack (idempotent).
    stop_cli_adapters();
    lifecycle_stage("cli_adapters_stop", "ok");

    if let Some(pack_root) = installed_pack_root() {
        let compose = compose_file(&pack_root);
        if compose.is_file() {
            let env = public_env_path();
            let env_arg = env.is_file().then_some(env.as_path());
            let spawn_env = teardown_compose_env(&pack_root, cfg.gateway_key_id.as_deref());
            lifecycle_stage("stop_compose", "begin");
            let out = match compose_command_with_env(
                &compose,
                env_arg,
                &["stop"],
                Some(&spawn_env),
                DOCKER_CMD_TIMEOUT,
            ) {
                Ok(out) => out,
                Err(err) => {
                    lifecycle_stage("stop_compose", "error");
                    return Err(err);
                }
            };
            if !out.status.success() {
                lifecycle_stage("stop_compose", "nonzero");
                lifecycle_stage("stop_down", "begin");
                let out2 = match compose_command_with_env(
                    &compose,
                    env_arg,
                    &["down", "--remove-orphans"],
                    Some(&spawn_env),
                    DOCKER_CMD_TIMEOUT,
                ) {
                    Ok(out) => out,
                    Err(err) => {
                        lifecycle_stage("stop_down", "error");
                        return Err(err);
                    }
                };
                if !out2.status.success() {
                    lifecycle_stage("stop_down", "nonzero");
                    return Err(format_cmd_failure("gateway pack stop", &out2));
                }
                lifecycle_stage("stop_down", "ok");
            } else {
                lifecycle_stage("stop_compose", "ok");
            }
        }
    }
    let status = gateway_pack_status_fresh(store);
    lifecycle_stage("stop_complete", "ok");
    Ok(status)
}

/// Destructive uninstall: only irin-desktop-gateway project + app-owned gateway dir + Keychain items.
pub fn uninstall_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;
    bump_pack_lifecycle_generation();
    invalidate_status_cache();
    invalidate_auth_observation();
    crate::touch_id::clear_rehearsal_passed();

    // Host adapters first so uninstall never leaves listeners after Keychain wipe.
    stop_cli_adapters();

    let key_id = load_or_create_private_config()
        .ok()
        .and_then(|c| c.gateway_key_id);
    if let Some(pack_root) = installed_pack_root() {
        let compose = compose_file(&pack_root);
        if compose.is_file() {
            let env = public_env_path();
            let env_arg = env.is_file().then_some(env.as_path());
            let spawn_env = teardown_compose_env(&pack_root, key_id.as_deref());
            let out = compose_command_with_env(
                &compose,
                env_arg,
                &["down", "--volumes", "--remove-orphans"],
                Some(&spawn_env),
                DOCKER_CMD_TIMEOUT,
            )?;
            if !out.status.success() {
                return Err(format_cmd_failure("gateway pack uninstall down", &out));
            }
        }
    } else if let Some(pack_root) = {
        // Best-effort down even if marker missing but pack dir exists.
        let p = gateway_data_dir().join(PACK_DIR_NAME);
        p.join("docker-compose.yml").is_file().then_some(p)
    } {
        let compose = compose_file(&pack_root);
        // No env file here: the pinned spawn env is the only source for the
        // compose-interpolated image refs and pack paths.
        let spawn_env = teardown_compose_env(&pack_root, key_id.as_deref());
        let _ = compose_command_with_env(
            &compose,
            None,
            &["down", "--volumes", "--remove-orphans"],
            Some(&spawn_env),
            DOCKER_CMD_TIMEOUT,
        );
    }

    // Always attempt Keychain cleanup; never claim success if an item remains.
    // Compose is already down (best-effort above); continue removing app data
    // so a Keychain ACL failure does not leave a half-installed pack tree.
    let keychain_err = delete_all_gateway_pack_secrets(store).err();
    let dir = gateway_data_dir();
    if dir.is_dir() {
        fs::remove_dir_all(&dir).map_err(|e| format!("remove gateway data dir: {e}"))?;
    }
    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = false;
    cfg.gateway_key_id = None;
    cfg.gateway_pack_version = None;
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    if let Some(e) = keychain_err {
        return Err(format!(
            "Gateway Pack files removed, but Keychain cleanup failed ({e}).              GW_API_KEY and/or AUTH_PEPPER may still be present under the IRIN              Keychain service — re-run Uninstall or delete those items manually."
        ));
    }
    Ok(gateway_pack_status_fresh(store))
}

#[cfg(test)]
#[path = "enable_tests.rs"]
mod tests;
