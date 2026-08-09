//! Pack-native watch profile toggle: install/remove bundled template + bounded
//! sidecar recreate under the lifecycle lock.

use super::cli_adapters::{ensure_cli_adapters_with_tokens, ensure_proxy_tokens};
use super::enable::{compose_up, lifecycle_stage, wait_control_plane, LIFECYCLE_LOCK};
use super::env::{build_full_compose_env, write_public_compose_env, PACK_WATCH_CANARY_TENANT};
use super::install::{
    compose_file, installed_pack_root, load_validated_manifest, verify_images_present,
};
use super::keys::{ensure_arm_keys_file, ensure_ledger_key};
use super::paths::{bundled_pack_root, ensure_watch_dirs, watch_inbox_dir, watch_profile_path};
use super::status::{bump_pack_lifecycle_generation, invalidate_status_cache};
use crate::docker_cli::{probe_docker_daemon, DockerDaemonState};
use crate::keychain::SecretStore;
use crate::private_config::load_or_create_private_config;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_TEMPLATE_NAME: &str = "default-sentinels.yaml";

/// True when the durable profile file is present (enabled switch).
pub fn watch_sentinels_enabled() -> bool {
    watch_profile_path().is_file()
}

/// Host path of the watch inbox (for "Open inbox folder").
pub fn watch_inbox_path_string() -> Result<String, String> {
    ensure_watch_dirs()?;
    Ok(watch_inbox_dir().display().to_string())
}

/// Open the watch inbox in the host file manager (macOS `open`).
pub fn open_watch_inbox() -> Result<String, String> {
    let path = watch_inbox_path_string()?;
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg(&path)
            .status()
            .map_err(|e| format!("open inbox: {e}"))?;
        if !status.success() {
            return Err(format!("open inbox failed with {status}"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        return Err(format!(
            "open inbox is only supported on macOS (path={path})"
        ));
    }
    Ok(path)
}

/// Locate the bundled default template (installed pack first, then bundled root).
fn default_template_path() -> Result<PathBuf, String> {
    if let Some(root) = installed_pack_root() {
        let p = root.join(DEFAULT_TEMPLATE_NAME);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(root) = bundled_pack_root() {
        let p = root.join(DEFAULT_TEMPLATE_NAME);
        if p.is_file() {
            return Ok(p);
        }
    }
    // Repo layout when running unit tests / dev without staged resources.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packaging/gateway-pack")
        .join(DEFAULT_TEMPLATE_NAME);
    if repo.is_file() {
        return Ok(repo.canonicalize().unwrap_or(repo));
    }
    Err("bundled default watch profile template not found".into())
}

/// Install the known-good default profile into app-support (does not recreate).
pub(crate) fn install_default_profile_file() -> Result<(), String> {
    ensure_watch_dirs()?;
    let src = default_template_path()?;
    let body = fs::read(&src).map_err(|e| format!("read default watch profile: {e}"))?;
    // Fail closed: refuse a template that is not the canary tenant shape.
    let text = String::from_utf8_lossy(&body);
    let tenant_line = format!("tenant: {PACK_WATCH_CANARY_TENANT}");
    if !text.contains(&tenant_line) {
        return Err(format!(
            "default watch profile template must use tenant {PACK_WATCH_CANARY_TENANT}"
        ));
    }
    if !text.contains("file-inbox-watch") {
        return Err("default watch profile template must declare file-inbox-watch".into());
    }
    let dest = watch_profile_path();
    let tmp = dest.with_extension("yaml.tmp");
    fs::write(&tmp, &body).map_err(|e| format!("write watch profile: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("install watch profile: {e}"))?;
    Ok(())
}

pub(crate) fn remove_profile_file() -> Result<(), String> {
    let path = watch_profile_path();
    if path.is_file() {
        fs::remove_file(&path).map_err(|e| format!("remove watch profile: {e}"))?;
    }
    Ok(())
}

/// Bounded pack recreate so the sidecar reloads SENTINELS_CONFIG_PATH pins.
fn reconcile_pack_for_profile(store: &dyn SecretStore) -> Result<(), String> {
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
        .ok_or_else(|| "Gateway Pack is not installed; enable Gateway first".to_string())?;
    // Integrity: pack must still look installed.
    if !pack_root.join("docker-compose.yml").is_file() {
        return Err("Gateway Pack install is incomplete".into());
    }
    let validated = load_validated_manifest(&pack_root)?;
    verify_images_present(&validated)?;
    ensure_watch_dirs()?;
    let _ = ensure_arm_keys_file()?;
    let ledger = ensure_ledger_key()?;
    // Same as enable/resume: ensure adapters with live tokens before env so
    // dead adapters do not inject empty proxy URL/token from cache alone.
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
    let spawn_env = build_full_compose_env(
        store,
        None,
        &pack_root,
        &ledger,
        &validated,
        existing_key_id.as_deref(),
        Some(proxy_tokens),
    )?;
    let compose = compose_file(&pack_root);
    compose_up(&compose, &env_path, &spawn_env)?;
    wait_control_plane()?;
    Ok(())
}

/// Enable or disable the pack-native watch profile.
///
/// On: install bundled default template, then one `--force-recreate` reconcile
/// so the sidecar loads the profile (fail-fast if YAML invalid).
/// Off: remove profile file and recreate back to 0 sentinels.
///
/// Takes the same lifecycle lock as Enable/Disable so mutations never interleave.
pub fn set_watch_sentinels_enabled(store: &dyn SecretStore, enabled: bool) -> Result<bool, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;
    bump_pack_lifecycle_generation();
    invalidate_status_cache();

    if installed_pack_root().is_none() {
        lifecycle_stage("watch_profile", "error");
        return Err("Gateway Pack is not installed; enable Gateway first".into());
    }

    ensure_watch_dirs().map_err(|e| {
        lifecycle_stage("watch_profile", "error");
        e
    })?;

    if enabled {
        install_default_profile_file().map_err(|e| {
            lifecycle_stage("watch_profile", "error");
            e
        })?;
        lifecycle_stage("watch_profile", "installed");
    } else {
        remove_profile_file().map_err(|e| {
            lifecycle_stage("watch_profile", "error");
            e
        })?;
        lifecycle_stage("watch_profile", "removed");
    }

    // Always reconcile when pack is installed so pins + containers match the
    // durable switch. Failures surface to the operator (never silent 0 sentinels
    // while "enabled").
    if let Err(e) = reconcile_pack_for_profile(store) {
        lifecycle_stage("watch_profile", "error");
        // Leave file state as the durable switch; operator can re-toggle or fix
        // Docker and resume. Report the error clearly.
        return Err(format!(
            "watch profile file updated but pack recreate failed: {e}"
        ));
    }
    lifecycle_stage(
        "watch_profile",
        if enabled {
            "reconciled_on"
        } else {
            "reconciled_off"
        },
    );
    Ok(watch_sentinels_enabled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_config::{test_env_lock, APP_SUPPORT_ROOT_ENV};
    use std::fs;

    #[test]
    fn install_and_remove_profile_file() {
        let _g = test_env_lock();
        let prev_support = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let support =
            std::env::temp_dir().join(format!("gw-watch-profile-support-{}", std::process::id()));
        let _ = fs::remove_dir_all(&support);
        fs::create_dir_all(&support).unwrap();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &support);
        let _ = remove_profile_file();
        assert!(!watch_sentinels_enabled());
        install_default_profile_file().expect("install template");
        assert!(watch_sentinels_enabled());
        let body = fs::read_to_string(watch_profile_path()).unwrap();
        assert!(body.contains(&format!("tenant: {PACK_WATCH_CANARY_TENANT}")));
        assert!(body.contains("file-inbox-watch"));
        // Template source must not live under the pack tree identity — dest is app-support.
        assert!(watch_profile_path()
            .to_string_lossy()
            .contains("sentinels.yaml"));
        remove_profile_file().unwrap();
        assert!(!watch_sentinels_enabled());
        match prev_support {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&support);
    }

    #[test]
    fn default_template_is_canary_tenant() {
        let path = default_template_path().expect("template reachable from repo");
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains(&format!("tenant: {PACK_WATCH_CANARY_TENANT}")));
        assert!(body.contains("file-inbox-watch"));
        assert!(body.contains("/var/lib/gateway/inbox"));
    }

    #[test]
    fn watch_inbox_path_creates_dir() {
        let _g = test_env_lock();
        let p = watch_inbox_path_string().unwrap();
        assert!(Path::new(&p).is_dir());
    }
}
