//! Compose env construction: public pins, secret process env, teardown placeholders.

use super::cli_adapters::{
    apply_proxy_compose_env, current_status as cli_adapters_current_status, empty_proxy_compose_pairs,
    ensure_proxy_tokens,
};
use super::install::load_validated_manifest;
use super::keys::{random_hex, serialize_public_env, validate_env_value, write_atomic_0600};
use super::manifest::{ImageRef, ValidatedManifest};
use super::paths::{
    arm_keys_path, ensure_gateway_dir, ledger_key_path, public_env_path, runtime_env_path,
    ARM_KEYS_CONTAINER_PATH,
};
use crate::docker_cli::{path_is_safe_argv, ComposeEnv};
use crate::keychain::{
    load_arm_principal_token, load_auth_pepper, load_watch_admin_token, store_auth_pepper,
    store_watch_admin_token, SecretStore, ARM_PRINCIPAL_NAME,
};
use crate::private_config::gui_login_environment;
use std::fs;
use std::path::{Path, PathBuf};

/// Canary tenant the desktop pack sidecar serves. The app-owned Council child
/// must send the same tenant on governed spawns or every Watch/Outbox admin
/// read 403s (source development keeps the `sovereign` default in Council).
pub(crate) const PACK_WATCH_CANARY_TENANT: &str = "canary";

/// Non-secret compose pins from validated sources only (manifest image refs,
/// app-owned paths, fixed pack-contract values). Single source for both the
/// public env file and the per-spawn forced env: Compose variable precedence
/// ranks the process environment above `--env-file`, so the env file alone
/// cannot stop an ambient parent value from swapping an image or pack path.
pub(crate) fn pack_pin_pairs(
    pack_root: &Path,
    ledger: &Path,
    gateway_image: &ImageRef,
    sidecar_image: &ImageRef,
    key_id: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    if !path_is_safe_argv(pack_root) || !path_is_safe_argv(ledger) {
        return Err("pack root or ledger path rejected".to_string());
    }
    // Touch ID bridge: the app-owned enrollment registry, mounted read-only.
    // Same validation class as the ledger key path.
    let arm_keys = arm_keys_path();
    if !path_is_safe_argv(&arm_keys) {
        return Err("arm attest keys path rejected".to_string());
    }
    let mut pairs = vec![
        (
            "IRIN_DESKTOP_ARM_KEYS".into(),
            arm_keys.display().to_string(),
        ),
        (
            "GW_ARM_ATTEST_KEYS_PATH".into(),
            ARM_KEYS_CONTAINER_PATH.to_string(),
        ),
        (
            "IRIN_GATEWAY_IMAGE".into(),
            gateway_image.as_str().to_string(),
        ),
        (
            "IRIN_SIDECAR_IMAGE".into(),
            sidecar_image.as_str().to_string(),
        ),
        (
            "IRIN_DESKTOP_PACK_ROOT".into(),
            pack_root.display().to_string(),
        ),
        (
            "IRIN_DESKTOP_LEDGER_KEY".into(),
            ledger.display().to_string(),
        ),
        ("GATEWAY_DURABLE".into(), "1".into()),
        ("GATEWAY_AUTH_FAIL_CLOSED".into(), "true".into()),
        ("SIDECAR_SOCKET_MODE".into(), "0660".into()),
        ("SIDECAR_SOCKET_GID".into(), "9999".into()),
        ("GW_ENABLE_COUNCIL_ENDPOINT".into(), "0".into()),
        (
            "COUNCIL_BASE_URL".into(),
            "http://host.docker.internal:8765".into(),
        ),
        ("GW_ENABLE_STREAMING".into(), "0".into()),
        ("GW_ENABLE_BATCH".into(), "0".into()),
        ("GATEWAY_BASE_URL".into(), "http://gateway:8080".into()),
        ("WATCH_PRODUCER_ENABLED".into(), "false".into()),
        ("WATCH_DISPATCHER_ENABLED".into(), "false".into()),
        (
            "WATCH_CANARY_TENANT".into(),
            PACK_WATCH_CANARY_TENANT.into(),
        ),
        ("DAILY_SPEND_CAP_USD".into(), "25".into()),
        ("WATCH_MAX_FANOUT_COST_USD".into(), "2.50".into()),
        // Council-spend route disabled — never generate COUNCIL_GATEWAY_TOKEN.
        // WATCH_ADMIN_TOKEN is minted into the secret env (Keychain-held),
        // never a public pin.
        ("BOOTSTRAP_TOKEN".into(), "".into()),
    ];
    if let Some(kid) = key_id {
        if !kid.is_empty() {
            validate_env_value("COUNCIL_GATEWAY_KEY_ID", kid)?;
            pairs.push(("COUNCIL_GATEWAY_KEY_ID".into(), kid.to_string()));
        }
    }
    Ok(pairs)
}

pub(crate) fn write_public_compose_env(
    pack_root: &Path,
    ledger: &Path,
    gateway_image: &ImageRef,
    sidecar_image: &ImageRef,
    key_id: Option<&str>,
) -> Result<PathBuf, String> {
    let pairs = pack_pin_pairs(pack_root, ledger, gateway_image, sidecar_image, key_id)?;
    ensure_gateway_dir()?;
    let body = serialize_public_env(&pairs)?;
    let path = public_env_path();
    write_atomic_0600(&path, body.as_bytes())?;
    // Scrub any legacy secret-bearing runtime.env if present.
    let legacy = runtime_env_path();
    if legacy.is_file() {
        let _ = fs::remove_file(&legacy);
    }
    Ok(path)
}

/// Compose process env forcing every non-secret interpolated key from
/// validated sources. Explicit per-spawn `cmd.env` values beat both the
/// ambient parent environment and the `--env-file` pins under Compose
/// variable precedence.
pub(crate) fn build_pack_pin_env(
    pack_root: &Path,
    ledger: &Path,
    gateway_image: &ImageRef,
    sidecar_image: &ImageRef,
    key_id: Option<&str>,
) -> Result<ComposeEnv, String> {
    Ok(
        pack_pin_pairs(pack_root, ledger, gateway_image, sidecar_image, key_id)?
            .into_iter()
            .collect(),
    )
}

/// Build process env for compose: secrets + providers. Never written to disk.
///
/// `proxy_tokens`: when `Some`, use the already-loaded Claude/Codex tokens and
/// do **not** re-enter Keychain for those accounts. Cold-launch FullStart resume
/// loads tokens once for adapters then passes them here so each account is
/// authorized at most once per flight (macOS can prompt on every get).
pub(crate) fn build_compose_secret_env(
    store: &dyn SecretStore,
    bootstrap: Option<&str>,
    proxy_tokens: Option<(String, String)>,
) -> Result<ComposeEnv, String> {
    let mut env = ComposeEnv::new();
    let pepper = match load_auth_pepper(store).map_err(|e| format!("keychain load pepper: {e}"))? {
        Some(p) => p,
        None => {
            let p = random_hex(32)?;
            store_auth_pepper(store, &p).map_err(|e| format!("keychain store pepper: {e}"))?;
            p
        }
    };
    validate_env_value("AUTH_PEPPER", &pepper)?;
    env.insert("AUTH_PEPPER".into(), pepper);

    // Watch/Outbox admin read surface: Keychain-held bearer, minted once at
    // Enable with the same load-or-mint pattern as AUTH_PEPPER. Secret
    // channel only — never the public env file, never the ambient parent env.
    let watch_admin = match load_watch_admin_token(store)
        .map_err(|e| format!("keychain load watch admin token: {e}"))?
    {
        Some(t) => t,
        None => {
            let t = random_hex(32)?;
            store_watch_admin_token(store, &t)
                .map_err(|e| format!("keychain store watch admin token: {e}"))?;
            t
        }
    };
    validate_env_value("WATCH_ADMIN_TOKEN", &watch_admin)?;
    env.insert("WATCH_ADMIN_TOKEN".into(), watch_admin);

    if let Some(bs) = bootstrap {
        validate_env_value("BOOTSTRAP_TOKEN", bs)?;
        env.insert("BOOTSTRAP_TOKEN".into(), bs.to_string());
    } else {
        env.insert("BOOTSTRAP_TOKEN".into(), String::new());
    }

    // Touch ID bridge — custody domain 1. The arm-principal registry string is
    // built ONLY from the Keychain-held token, never from the ambient
    // environment (`GW_ARM_PRINCIPALS` is in AMBIENT_SCRUB_ENV_KEYS) and never
    // written to the public env file. Absent/invalid token => empty registry =>
    // the sidecar parses zero principals and every arm route 401s: enabling the
    // pack cannot create arming capability on its own.
    let principals = match load_arm_principal_token(store)
        .map_err(|e| format!("keychain load arm principal: {e}"))?
    {
        Some(tok) => format!("{ARM_PRINCIPAL_NAME}:{tok}"),
        None => String::new(),
    };
    validate_env_value("GW_ARM_PRINCIPALS", &principals)?;
    env.insert("GW_ARM_PRINCIPALS".into(), principals);

    // Provider keys from login/process only — never persisted to app env file.
    // Skip gui_login_environment when IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV=1 (tests).
    let login = if std::env::var_os("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV").is_some() {
        Vec::new()
    } else {
        gui_login_environment()
    };
    for key in [
        "XAI_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "NVIDIA_API_KEY",
    ] {
        let val = std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                login
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_default();
        // Provider keys are optional for pack start. Skip any value that fails
        // injection validation (CR/LF/NUL) rather than aborting Enable — pack
        // auth still works; only that provider route is empty.
        let safe = if val.is_empty() {
            String::new()
        } else if validate_env_value(key, &val).is_ok() {
            val
        } else {
            String::new()
        };
        env.insert(key.to_string(), safe);
    }

    // Host CLI adapters (Claude/Codex): Keychain tokens + live health only.
    // Never write these to the public env file. Unready adapters inject empty
    // URL/token so Gateway readiness stays fail-closed — never a Direct fallthrough.
    let (claude_tok, codex_tok) = match proxy_tokens {
        Some(pair) => pair,
        None => ensure_proxy_tokens(store)
            .map_err(|e| format!("keychain proxy tokens: {e}"))?,
    };
    let adapter_status = cli_adapters_current_status();
    apply_proxy_compose_env(&mut env, &adapter_status, &claude_tok, &codex_tok)?;

    Ok(env)
}

/// Full per-spawn compose env for `up`: validated non-secret pins merged
/// under the Keychain/login secret env (secrets win on the BOOTSTRAP_TOKEN
/// overlap). This is the only legitimate channel for compose-interpolated
/// values; the docker_cli spawn path scrubs ambient copies first and forces
/// disarmed Watch/admin surfaces last.
///
/// `proxy_tokens`: pass `Some` when the caller already loaded Claude/Codex
/// tokens (single-pass resume/enable); `None` loads/mints inside.
pub(crate) fn build_full_compose_env(
    store: &dyn SecretStore,
    bootstrap: Option<&str>,
    pack_root: &Path,
    ledger: &Path,
    validated: &ValidatedManifest,
    key_id: Option<&str>,
    proxy_tokens: Option<(String, String)>,
) -> Result<ComposeEnv, String> {
    let mut env = build_pack_pin_env(
        pack_root,
        ledger,
        &validated.gateway,
        &validated.sidecar,
        key_id,
    )?;
    env.extend(build_compose_secret_env(store, bootstrap, proxy_tokens)?);
    Ok(env)
}

/// Spawn env for stop/uninstall only: validated non-secret pins when the
/// installed manifest still reads cleanly, plus **empty** secret placeholders
/// so Compose can interpolate the file without loading Keychain or login
/// provider keys. Teardown never starts services; real secrets must not ride
/// the Compose process env on this path. A corrupt manifest must not block
/// teardown — empty pins plus empty secret slots still scrub ambient secrets
/// via the docker_cli spawn path and force disarmed Watch/admin surfaces.
pub(crate) fn teardown_compose_env(pack_root: &Path, key_id: Option<&str>) -> ComposeEnv {
    let mut env = load_validated_manifest(pack_root)
        .and_then(|v| {
            build_pack_pin_env(
                pack_root,
                &ledger_key_path(),
                &v.gateway,
                &v.sidecar,
                key_id,
            )
        })
        .unwrap_or_default();
    // Empty secret slots win over any pin defaults and replace ambient values
    // after the spawn scrub — never Keychain or login-shell material.
    for key in [
        "AUTH_PEPPER",
        "BOOTSTRAP_TOKEN",
        "XAI_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "NVIDIA_API_KEY",
        // Touch ID bridge: teardown never carries the arm-principal registry.
        "GW_ARM_PRINCIPALS",
    ] {
        env.insert(key.to_string(), String::new());
    }
    // Host CLI adapters: teardown never loads Keychain proxy tokens.
    for (k, v) in empty_proxy_compose_pairs() {
        env.insert(k, v);
    }
    // The bind-mount source and the in-container registry path are non-secret
    // pins Compose must still interpolate for `down`; fall back to the fixed
    // app-owned values when the manifest was unreadable.
    env.entry("IRIN_DESKTOP_ARM_KEYS".to_string())
        .or_insert_with(|| arm_keys_path().display().to_string());
    env.entry("GW_ARM_ATTEST_KEYS_PATH".to_string())
        .or_insert_with(|| ARM_KEYS_CONTAINER_PATH.to_string());
    env
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
