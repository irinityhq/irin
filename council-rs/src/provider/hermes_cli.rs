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
            is_executable(&adapter).then_some(adapter)
        }
        HermesAdapterProtocol::Direct => which_hermes(),
    }
}

fn which_hermes() -> Option<PathBuf> {
    std::process::Command::new("hermes")
        .arg("--version")
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| PathBuf::from("hermes"))
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
}
