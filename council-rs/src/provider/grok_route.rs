//! Grok CLI vs xAI API model routing — driven by `grok_routing.yaml`.
//!
//! Local `grok` (Grok Build TUI) accepts `grok-build`, `grok-composer-2.5-fast`, etc.
//! API ids like `grok-4.3` are xAI Responses API only (`XAI_API_KEY`).

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Deserialize)]
pub struct HermesSeatEntry {
    #[serde(default = "default_hermes_provider")]
    pub provider: String,
    /// Omit to use the cabinet/role model id on the wire.
    pub model: Option<String>,
}

fn default_hermes_provider() -> String {
    "xai".into()
}

/// How council talks to Hermes — explicit in grok_routing.yaml (not inferred from filenames).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum HermesAdapterProtocol {
    /// Spawn `{default_adapter}` with `--model` / `--provider`; prompt on stdin.
    #[default]
    Script,
    /// Invoke `hermes -z` directly (flags in council; prefer script for operator control).
    Direct,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HermesTransportConfig {
    #[serde(default)]
    pub adapter_protocol: HermesAdapterProtocol,
    /// Relative to `--base-dir` when `COUNCIL_HERMES_SEAT_BIN` is unset.
    #[serde(default = "default_hermes_adapter_path")]
    pub default_adapter: String,
}

fn default_hermes_adapter_path() -> String {
    "scripts/hermes-seat-adapter.sh".into()
}

impl Default for HermesTransportConfig {
    fn default() -> Self {
        Self {
            adapter_protocol: HermesAdapterProtocol::default(),
            default_adapter: default_hermes_adapter_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GrokRoutingFile {
    #[serde(default = "default_cli_models")]
    pub cli_models: std::collections::HashMap<String, String>,
    #[serde(default = "default_api_only_ids")]
    pub api_only_ids: Vec<String>,
    #[serde(default = "default_api_only_prefixes")]
    pub api_only_prefixes: Vec<String>,
    #[serde(default = "default_cli_default_label")]
    pub cli_default_label: String,
    #[serde(default = "default_cli_pinned_label_prefix")]
    pub cli_pinned_label_prefix: String,
    /// Explicit cabinet model → Hermes xAI (or other) wire ids.
    #[serde(default)]
    pub hermes_seats: std::collections::HashMap<String, HermesSeatEntry>,
    /// When true, any `api_only_ids` / prefix match uses Hermes if available.
    #[serde(default = "default_true")]
    pub use_hermes_for_api_only: bool,
    #[serde(default = "default_hermes_label_prefix")]
    pub hermes_label_prefix: String,
    /// Hermes seat transport contract (adapter script vs direct CLI).
    #[serde(default)]
    pub hermes: HermesTransportConfig,
}

fn default_true() -> bool {
    true
}

fn default_hermes_label_prefix() -> String {
    "hermes-cli-".into()
}

fn default_cli_default_label() -> String {
    "grok-cli-default".into()
}

fn default_cli_pinned_label_prefix() -> String {
    "grok-cli-".into()
}

fn default_cli_models() -> std::collections::HashMap<String, String> {
    [
        ("grok-build".into(), "grok-build".into()),
        ("grok-build-0.1".into(), "grok-build".into()),
        ("grok-composer-2.5".into(), "grok-composer-2.5-fast".into()),
        (
            "grok-composer-2.5-fast".into(),
            "grok-composer-2.5-fast".into(),
        ),
        ("grok-multi-agent".into(), "grok-multi-agent".into()),
    ]
    .into_iter()
    .collect()
}

fn default_api_only_ids() -> Vec<String> {
    vec![
        "grok-4.3".into(),
        "grok-4.20-0309-reasoning".into(),
        "grok-4.20-0309-non-reasoning".into(),
        "grok-4.20-multi-agent-0309".into(),
        "grok-4-1-fast-non-reasoning".into(),
    ]
}

fn default_api_only_prefixes() -> Vec<String> {
    vec!["grok-4.".into()]
}

impl Default for GrokRoutingFile {
    fn default() -> Self {
        Self {
            cli_models: default_cli_models(),
            api_only_ids: default_api_only_ids(),
            api_only_prefixes: default_api_only_prefixes(),
            cli_default_label: default_cli_default_label(),
            cli_pinned_label_prefix: default_cli_pinned_label_prefix(),
            hermes_seats: default_hermes_seats(),
            use_hermes_for_api_only: true,
            hermes_label_prefix: default_hermes_label_prefix(),
            hermes: HermesTransportConfig::default(),
        }
    }
}

fn default_hermes_seats() -> std::collections::HashMap<String, HermesSeatEntry> {
    [
        (
            "grok-4.3".into(),
            HermesSeatEntry {
                provider: "xai".into(),
                model: None,
            },
        ),
        (
            "grok-4.20-0309-reasoning".into(),
            HermesSeatEntry {
                provider: "xai".into(),
                model: None,
            },
        ),
    ]
    .into_iter()
    .collect()
}

/// Hermes `-z` seat wire target (operator adapter owns flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HermesSeatResolution {
    pub wire_model: String,
    pub wire_provider: String,
    pub response_label: String,
    pub cabinet_model: String,
}

/// How a cabinet/role model string maps onto the local `grok` CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokCliResolution {
    /// `None` = omit `-m` (CLI default, currently grok-build).
    pub cli_model_arg: Option<String>,
    /// Stored on `ProviderResponse.model` for pricing/session honesty.
    pub response_label: String,
    pub cabinet_model: String,
    /// Cabinet asked for an API-only id; we run local CLI default instead.
    pub api_id_substituted: bool,
}

static ROUTING: OnceLock<RwLock<GrokRoutingFile>> = OnceLock::new();
static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn routing_store() -> &'static RwLock<GrokRoutingFile> {
    ROUTING.get_or_init(|| RwLock::new(load_routing_from_disk(&base_dir())))
}

fn base_dir() -> PathBuf {
    BASE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Called from `Config::load` so routing tracks `--base-dir`.
pub fn set_base_dir(base_dir: &Path) {
    let _ = BASE_DIR.set(base_dir.to_path_buf());
    if let Some(lock) = ROUTING.get() {
        *lock.write().expect("grok routing lock") = load_routing_from_disk(base_dir);
    }
}

fn load_routing_from_disk(base_dir: &Path) -> GrokRoutingFile {
    let path = base_dir.join("grok_routing.yaml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return GrokRoutingFile::default(),
    };
    match serde_yaml::from_str::<GrokRoutingFile>(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("⚠️  grok_routing.yaml parse error ({e}); using built-in defaults");
            GrokRoutingFile::default()
        }
    }
}

pub fn routing_snapshot() -> GrokRoutingFile {
    routing_store().read().expect("grok routing lock").clone()
}

pub fn hermes_transport_config() -> HermesTransportConfig {
    routing_snapshot().hermes
}

pub fn is_api_only_model(model: &str, routing: &GrokRoutingFile) -> bool {
    let m = model.trim();
    if m.is_empty() {
        return false;
    }
    if routing.api_only_ids.iter().any(|id| id == m) {
        return true;
    }
    routing
        .api_only_prefixes
        .iter()
        .any(|pfx| m.starts_with(pfx))
}

/// Resolve a cabinet/role model id for `grok_cli` / local `grok` subprocess seats.
pub fn resolve_cli_model(cabinet_model: &str) -> GrokCliResolution {
    let routing = routing_snapshot();
    resolve_cli_model_with(&routing, cabinet_model)
}

pub fn resolve_cli_model_with(routing: &GrokRoutingFile, cabinet_model: &str) -> GrokCliResolution {
    let cabinet_model = cabinet_model.trim();
    if cabinet_model.is_empty() {
        return GrokCliResolution {
            cli_model_arg: None,
            response_label: routing.cli_default_label.clone(),
            cabinet_model: String::new(),
            api_id_substituted: false,
        };
    }

    if let Some(cli_m) = routing.cli_models.get(cabinet_model) {
        let cli_m = cli_m.trim();
        let response_label = if cli_m.is_empty() {
            routing.cli_default_label.clone()
        } else {
            format!("{}{}", routing.cli_pinned_label_prefix, cli_m)
        };
        return GrokCliResolution {
            cli_model_arg: if cli_m.is_empty() {
                None
            } else {
                Some(cli_m.to_string())
            },
            response_label,
            cabinet_model: cabinet_model.to_string(),
            api_id_substituted: false,
        };
    }

    if is_api_only_model(cabinet_model, routing) {
        return GrokCliResolution {
            cli_model_arg: None,
            response_label: routing.cli_default_label.clone(),
            cabinet_model: cabinet_model.to_string(),
            api_id_substituted: true,
        };
    }

    // Unknown id: omit -m (CLI default) rather than pass a bad -m and fail the seat.
    GrokCliResolution {
        cli_model_arg: None,
        response_label: routing.cli_default_label.clone(),
        cabinet_model: cabinet_model.to_string(),
        api_id_substituted: false,
    }
}

/// Resolve Hermes seat transport for API-tier grok models (4.3, 4.20, …).
pub fn resolve_hermes_seat(cabinet_model: &str) -> Option<HermesSeatResolution> {
    let routing = routing_snapshot();
    resolve_hermes_seat_with(&routing, cabinet_model)
}

pub fn resolve_hermes_seat_with(
    routing: &GrokRoutingFile,
    cabinet_model: &str,
) -> Option<HermesSeatResolution> {
    let cabinet_model = cabinet_model.trim();
    if cabinet_model.is_empty() {
        return None;
    }

    let entry = routing.hermes_seats.get(cabinet_model);
    let via_api_only = routing.use_hermes_for_api_only && is_api_only_model(cabinet_model, routing);
    if entry.is_none() && !via_api_only {
        return None;
    }

    let (wire_provider, wire_model) = if let Some(e) = entry {
        (
            e.provider.clone(),
            e.model.clone().unwrap_or_else(|| cabinet_model.to_string()),
        )
    } else {
        ("xai".into(), cabinet_model.to_string())
    };

    Some(HermesSeatResolution {
        wire_model: wire_model.clone(),
        wire_provider,
        response_label: format!("{}{}", routing.hermes_label_prefix, wire_model),
        cabinet_model: cabinet_model.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_only_grok_43_uses_cli_default() {
        let r = GrokRoutingFile::default();
        let res = resolve_cli_model_with(&r, "grok-4.3");
        assert!(res.api_id_substituted);
        assert!(res.cli_model_arg.is_none());
        assert_eq!(res.response_label, "grok-cli-default");
    }

    #[test]
    fn cli_build_pins_m_flag() {
        let r = GrokRoutingFile::default();
        let res = resolve_cli_model_with(&r, "grok-build");
        assert_eq!(res.cli_model_arg.as_deref(), Some("grok-build"));
        assert_eq!(res.response_label, "grok-cli-grok-build");
    }

    #[test]
    fn composer_alias_maps_to_fast() {
        let r = GrokRoutingFile::default();
        let res = resolve_cli_model_with(&r, "grok-composer-2.5");
        assert_eq!(res.cli_model_arg.as_deref(), Some("grok-composer-2.5-fast"));
    }

    #[test]
    fn is_api_only_prefix() {
        let r = GrokRoutingFile::default();
        assert!(is_api_only_model("grok-4.20-0309-reasoning", &r));
        assert!(!is_api_only_model("grok-build", &r));
    }

    #[test]
    fn hermes_resolves_grok_43() {
        let r = GrokRoutingFile::default();
        let h = resolve_hermes_seat_with(&r, "grok-4.3").expect("hermes route");
        assert_eq!(h.wire_model, "grok-4.3");
        assert_eq!(h.wire_provider, "xai");
        assert_eq!(h.response_label, "hermes-cli-grok-4.3");
    }

    #[test]
    fn hermes_skips_grok_build() {
        let r = GrokRoutingFile::default();
        assert!(resolve_hermes_seat_with(&r, "grok-build").is_none());
    }

    #[test]
    fn hermes_default_protocol_is_script() {
        let r = GrokRoutingFile::default();
        assert_eq!(r.hermes.adapter_protocol, HermesAdapterProtocol::Script);
        assert_eq!(r.hermes.default_adapter, "scripts/hermes-seat-adapter.sh");
    }
}
