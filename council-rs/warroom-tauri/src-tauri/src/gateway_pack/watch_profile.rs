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
    // Prefer the code-signed bundled asset; the installed pack copy is
    // user-writable and only a fallback for older installs without resources.
    if let Some(root) = bundled_pack_root() {
        let p = root.join(DEFAULT_TEMPLATE_NAME);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(root) = installed_pack_root() {
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

fn read_profile_bytes() -> Result<Option<Vec<u8>>, String> {
    let path = watch_profile_path();
    if !path.is_file() {
        return Ok(None);
    }
    fs::read(&path)
        .map(Some)
        .map_err(|e| format!("read watch profile: {e}"))
}

fn restore_profile_bytes(body: &[u8]) -> Result<(), String> {
    ensure_watch_dirs()?;
    let dest = watch_profile_path();
    let tmp = dest.with_extension("yaml.tmp");
    fs::write(&tmp, body).map_err(|e| format!("restore watch profile: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("restore watch profile: {e}"))?;
    Ok(())
}

/// Restore the snapshot taken before a switch, then reconcile so the pack
/// matches the restored file. Restore errors abort before a second reconcile.
fn rollback_watch_profile<F>(previous: Option<&[u8]>, mut reconcile: F) -> Result<(), String>
where
    F: FnMut() -> Result<(), String>,
{
    match previous {
        Some(body) => restore_profile_bytes(body)?,
        None => remove_profile_file()?,
    }
    reconcile()
}

/// Install or remove the durable profile, then reconcile. On reconcile
/// failure the prior file is restored and the pack is compensating-recreated
/// so `watch_sentinels_enabled()` matches the pack that actually came up.
fn commit_watch_profile_switch<F>(enabled: bool, mut reconcile: F) -> Result<bool, String>
where
    F: FnMut() -> Result<(), String>,
{
    let previous = read_profile_bytes()?;
    if enabled {
        install_default_profile_file()?;
    } else {
        remove_profile_file()?;
    }
    if let Err(e) = reconcile() {
        return match rollback_watch_profile(previous.as_deref(), reconcile) {
            Ok(()) => Err(format!(
                "pack recreate failed; watch profile and pack restored: {e}"
            )),
            Err(heal) => Err(format!(
                "pack recreate failed; watch profile restore or pack heal failed: {e}; {heal}"
            )),
        };
    }
    Ok(watch_sentinels_enabled())
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

    match commit_watch_profile_switch(enabled, || reconcile_pack_for_profile(store)) {
        Ok(state) => {
            lifecycle_stage(
                "watch_profile",
                if enabled {
                    "reconciled_on"
                } else {
                    "reconciled_off"
                },
            );
            Ok(state)
        }
        Err(e) => {
            lifecycle_stage("watch_profile", "error");
            Err(e)
        }
    }
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
    fn failed_enable_rolls_back_profile_file() {
        let _g = test_env_lock();
        let prev_support = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let support = std::env::temp_dir().join(format!(
            "gw-watch-profile-rollback-on-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&support);
        fs::create_dir_all(&support).unwrap();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &support);
        let _ = remove_profile_file();
        let mut calls = 0;
        let err = commit_watch_profile_switch(true, || {
            calls += 1;
            Err("docker down".into())
        })
        .unwrap_err();
        assert!(err.contains("restore or pack heal failed"), "{err}");
        assert_eq!(
            calls, 2,
            "failed On must compensating-reconcile after restore"
        );
        assert!(
            !watch_sentinels_enabled(),
            "failed On must not leave the profile file"
        );
        match prev_support {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&support);
    }

    #[test]
    fn failed_disable_restores_profile_file() {
        let _g = test_env_lock();
        let prev_support = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let support = std::env::temp_dir().join(format!(
            "gw-watch-profile-rollback-off-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&support);
        fs::create_dir_all(&support).unwrap();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &support);
        install_default_profile_file().expect("install template");
        let before = fs::read(watch_profile_path()).unwrap();
        let mut calls = 0;
        let err = commit_watch_profile_switch(false, || {
            calls += 1;
            Err("docker down".into())
        })
        .unwrap_err();
        assert!(
            err.contains("restored") || err.contains("heal failed"),
            "{err}"
        );
        assert_eq!(
            calls, 2,
            "failed Off must compensating-reconcile after restore"
        );
        assert!(
            watch_sentinels_enabled(),
            "failed Off must restore the profile file"
        );
        assert_eq!(fs::read(watch_profile_path()).unwrap(), before);
        let _ = remove_profile_file();
        match prev_support {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&support);
    }

    #[test]
    fn failed_enable_restores_existing_profile() {
        let _g = test_env_lock();
        let prev_support = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let support = std::env::temp_dir().join(format!(
            "gw-watch-profile-rollback-on-existing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&support);
        fs::create_dir_all(&support).unwrap();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &support);
        restore_profile_bytes(b"custom-prior-profile\n").expect("seed existing profile");
        let before = fs::read(watch_profile_path()).unwrap();
        let mut calls = 0;
        let err = commit_watch_profile_switch(true, || {
            calls += 1;
            Err("control plane not ready".into())
        })
        .unwrap_err();
        assert!(err.contains("restore or pack heal failed"), "{err}");
        assert_eq!(
            calls, 2,
            "failed On must compensating-reconcile after restore"
        );
        assert_eq!(
            fs::read(watch_profile_path()).unwrap(),
            before,
            "failed On must restore the prior profile, not delete it"
        );
        let _ = remove_profile_file();
        match prev_support {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&support);
    }

    #[test]
    fn failed_switch_heals_pack_when_second_reconcile_succeeds() {
        let _g = test_env_lock();
        let prev_support = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let support = std::env::temp_dir().join(format!(
            "gw-watch-profile-heal-after-recreate-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&support);
        fs::create_dir_all(&support).unwrap();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &support);
        let _ = remove_profile_file();
        let mut calls = 0;
        let err = commit_watch_profile_switch(true, || {
            calls += 1;
            if calls == 1 {
                Err("control plane not ready".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(err.contains("watch profile and pack restored"), "{err}");
        assert_eq!(calls, 2, "heal must run after the first recreate failure");
        assert!(
            !watch_sentinels_enabled(),
            "healed On-from-empty must leave no profile file"
        );
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
        let prev_support = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let support =
            std::env::temp_dir().join(format!("gw-watch-inbox-support-{}", std::process::id()));
        let _ = fs::remove_dir_all(&support);
        fs::create_dir_all(&support).unwrap();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &support);
        let p = watch_inbox_path_string().unwrap();
        assert!(Path::new(&p).is_dir());
        match prev_support {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&support);
    }
}
