//! Hermes seat transport — spawns an operator-controlled adapter, not Hermes logic.
//!
//! Protocol from `grok_routing.yaml` → `hermes.adapter_protocol` (default: script).
//! Override binary: `COUNCIL_HERMES_SEAT_BIN`. Council passes model id + prompt; flags live in the script.

use crate::provider::agent_cli;
use crate::provider::grok_route::{self, HermesAdapterProtocol, HermesSeatResolution};
use crate::types::{ProviderProvenance, ProviderResponse};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Tracks `--base-dir` so the default adapter script resolves correctly.
pub fn set_base_dir(base_dir: &Path) {
    let _ = BASE_DIR.set(base_dir.to_path_buf());
}

fn base_dir() -> PathBuf {
    BASE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

pub fn is_hermes_seat_available() -> bool {
    resolve_hermes_seat_bin().is_some()
}

pub fn prefer_hermes_seat() -> bool {
    match std::env::var("COUNCIL_HERMES_SEAT") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "0" && v != "false"
        }
        Err(_) => true,
    }
}

fn effective_protocol() -> HermesAdapterProtocol {
    if std::env::var("COUNCIL_HERMES_SEAT_BIN").is_ok() {
        // Operator override always uses the script contract.
        return HermesAdapterProtocol::Script;
    }
    grok_route::hermes_transport_config().adapter_protocol
}

fn resolve_hermes_seat_bin() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("COUNCIL_HERMES_SEAT_BIN") {
        let p = PathBuf::from(bin.trim());
        if p.as_os_str().is_empty() {
            return None;
        }
        return is_executable(&p).then_some(p);
    }

    match effective_protocol() {
        HermesAdapterProtocol::Script => {
            let rel = grok_route::hermes_transport_config().default_adapter;
            let adapter = base_dir().join(rel.trim());
            // The bundled adapter delegates to the Hermes CLI. Shipping the
            // script makes the transport discoverable only when that
            // dependency is usable; otherwise default cabinets must filter the
            // seat instead of failing their first call with exit 127.
            bundled_script_if_usable(adapter, which_hermes().is_some())
        }
        HermesAdapterProtocol::Direct => which_hermes(),
    }
}

fn bundled_script_if_usable(adapter: PathBuf, hermes_available: bool) -> Option<PathBuf> {
    (is_executable(&adapter) && hermes_available).then_some(adapter)
}

fn which_hermes() -> Option<PathBuf> {
    hermes_on_path(std::env::var_os("PATH").as_deref())
}

fn hermes_on_path(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("hermes"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn uses_script_protocol() -> bool {
    matches!(effective_protocol(), HermesAdapterProtocol::Script)
}

fn full_prompt(prompt: &str, system: &str) -> String {
    if system.trim().is_empty() {
        prompt.to_string()
    } else {
        format!("[SYSTEM]\n{system}\n\n[USER]\n{prompt}")
    }
}

pub async fn ask_hermes(
    prompt: &str,
    system: &str,
    route: &HermesSeatResolution,
) -> ProviderResponse {
    let Some(bin) = resolve_hermes_seat_bin() else {
        let transport = grok_route::hermes_transport_config();
        return ProviderResponse {
            error: Some(format!(
                "hermes_cli: no seat surface (protocol={:?}, adapter={})",
                effective_protocol(),
                transport.default_adapter
            )),
            ..Default::default()
        };
    };

    let combined = full_prompt(prompt, system);
    let mut cmd = tokio::process::Command::new(&bin);
    let provenance = ProviderProvenance::cli_readonly("hermes_cli", "usage_unavailable");

    if uses_script_protocol() {
        cmd.args([
            "--model",
            route.wire_model.as_str(),
            "--provider",
            route.wire_provider.as_str(),
        ]);
        return agent_cli::run_stdout(
            cmd,
            Some(combined.as_str()),
            "hermes_cli",
            route.response_label.clone(),
            provenance,
        )
        .await;
    }

    cmd.args([
        "-z",
        combined.as_str(),
        "--provider",
        route.wire_provider.as_str(),
        "-m",
        route.wire_model.as_str(),
        "--safe-mode",
        "--ignore-user-config",
        "--ignore-rules",
    ]);
    agent_cli::run_stdout(
        cmd,
        None,
        "hermes_cli",
        route.response_label.clone(),
        provenance,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::grok_route::HermesAdapterProtocol;

    #[test]
    fn prefer_hermes_seat_defaults_on() {
        assert!(prefer_hermes_seat());
    }

    #[test]
    fn env_override_forces_script_protocol() {
        unsafe {
            std::env::set_var("COUNCIL_HERMES_SEAT_BIN", "/tmp/my-adapter.sh");
        }
        assert!(uses_script_protocol());
        unsafe {
            std::env::remove_var("COUNCIL_HERMES_SEAT_BIN");
        }
    }

    #[test]
    fn yaml_default_protocol_is_script() {
        assert_eq!(
            grok_route::hermes_transport_config().adapter_protocol,
            HermesAdapterProtocol::Script
        );
    }

    #[cfg(unix)]
    #[test]
    fn hermes_discovery_resolves_path_without_executing_the_binary() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "irin-hermes-path-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let hermes = dir.join("hermes");
        // If discovery executed this file the test would hang. PATH resolution
        // must only inspect file type and executable mode.
        std::fs::write(&hermes, "#!/bin/sh\nsleep 300\n").expect("write fixture");
        std::fs::set_permissions(&hermes, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture executable");

        assert_eq!(hermes_on_path(Some(dir.as_os_str())), Some(hermes.clone()));
        assert!(hermes_on_path(None).is_none());

        std::fs::remove_dir_all(dir).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn bundled_script_requires_hermes_dependency() {
        use std::os::unix::fs::PermissionsExt;

        let adapter =
            std::env::temp_dir().join(format!("irin-hermes-adapter-test-{}", std::process::id()));
        std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").expect("write adapter fixture");
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755))
            .expect("make adapter executable");

        assert!(bundled_script_if_usable(adapter.clone(), false).is_none());
        assert_eq!(
            bundled_script_if_usable(adapter.clone(), true),
            Some(adapter.clone())
        );

        std::fs::remove_file(adapter).expect("remove adapter fixture");
    }
}
