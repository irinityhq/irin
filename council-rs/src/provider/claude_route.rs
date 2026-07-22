//! Claude CLI + API model routing — driven by `claude_routing.yaml`.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeSeatConfig {
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default = "default_true")]
    pub no_session_persistence: bool,
}

impl Default for ClaudeSeatConfig {
    fn default() -> Self {
        Self {
            permission_mode: default_permission_mode(),
            output_format: default_output_format(),
            no_session_persistence: true,
        }
    }
}

fn default_permission_mode() -> String {
    "dontAsk".into()
}

fn default_output_format() -> String {
    "json".into()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiAdaptiveEntry {
    pub effort: String,
    #[serde(default = "default_api_max_tokens")]
    pub max_tokens: u32,
}

fn default_api_max_tokens() -> u32 {
    16_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeRoutingFile {
    #[serde(default = "default_cli_models")]
    pub cli_models: HashMap<String, String>,
    #[serde(default = "default_cli_default_model")]
    pub cli_default_model: String,
    #[serde(default = "default_cli_label_prefix")]
    pub cli_label_prefix: String,
    #[serde(default = "default_min_cli_version")]
    pub min_cli_version: String,
    #[serde(default)]
    pub seat: ClaudeSeatConfig,
    #[serde(default)]
    pub api_adaptive: HashMap<String, ApiAdaptiveEntry>,
}

fn default_cli_default_model() -> String {
    "claude-opus-4-8".into()
}

fn default_cli_label_prefix() -> String {
    "claude-cli-".into()
}

fn default_min_cli_version() -> String {
    "2.1.0".into()
}

/// `(major, minor, patch)` parsed from `claude --version` stdout.
pub fn parse_cli_version(raw: &str) -> Option<(u32, u32, u32)> {
    for token in raw.split_whitespace() {
        let parts: Vec<u32> = token
            .split('.')
            .take(3)
            .filter_map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .collect();
        if parts.len() >= 2 {
            return Some((parts[0], parts[1], parts.get(2).copied().unwrap_or(0)));
        }
    }
    None
}

pub fn version_at_least(have: (u32, u32, u32), min: (u32, u32, u32)) -> bool {
    have >= min
}

pub fn parse_min_cli_version(min: &str) -> Option<(u32, u32, u32)> {
    parse_cli_version(min)
}

fn default_cli_models() -> HashMap<String, String> {
    [
        ("claude-opus-4-8".into(), "claude-opus-4-8".into()),
        ("claude-opus-4-6".into(), "claude-opus-4-6".into()),
    ]
    .into_iter()
    .collect()
}

impl Default for ClaudeRoutingFile {
    fn default() -> Self {
        Self {
            cli_models: default_cli_models(),
            cli_default_model: default_cli_default_model(),
            cli_label_prefix: default_cli_label_prefix(),
            min_cli_version: default_min_cli_version(),
            seat: ClaudeSeatConfig {
                permission_mode: default_permission_mode(),
                output_format: default_output_format(),
                no_session_persistence: true,
            },
            api_adaptive: default_api_adaptive(),
        }
    }
}

fn default_api_adaptive() -> HashMap<String, ApiAdaptiveEntry> {
    [
        (
            "claude-opus-4-8".into(),
            ApiAdaptiveEntry {
                effort: "high".into(),
                max_tokens: 16_000,
            },
        ),
        (
            "claude-opus-4-6".into(),
            ApiAdaptiveEntry {
                effort: "high".into(),
                max_tokens: 16_000,
            },
        ),
    ]
    .into_iter()
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCliResolution {
    pub cli_model_arg: String,
    pub response_label: String,
    pub cabinet_model: String,
}

static ROUTING: OnceLock<RwLock<ClaudeRoutingFile>> = OnceLock::new();
static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn routing_store() -> &'static RwLock<ClaudeRoutingFile> {
    ROUTING.get_or_init(|| RwLock::new(load_routing_from_disk(&base_dir())))
}

fn base_dir() -> PathBuf {
    BASE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

pub fn set_base_dir(base_dir: &Path) {
    let _ = BASE_DIR.set(base_dir.to_path_buf());
    if let Some(lock) = ROUTING.get() {
        *lock.write().expect("claude routing lock") = load_routing_from_disk(base_dir);
    }
}

fn load_routing_from_disk(base_dir: &Path) -> ClaudeRoutingFile {
    let path = base_dir.join("claude_routing.yaml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return ClaudeRoutingFile::default(),
    };
    match serde_yaml::from_str::<ClaudeRoutingFile>(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("⚠️  claude_routing.yaml parse error ({e}); using built-in defaults");
            ClaudeRoutingFile::default()
        }
    }
}

pub fn routing_snapshot() -> ClaudeRoutingFile {
    routing_store().read().expect("claude routing lock").clone()
}

pub fn resolve_cli_model(cabinet_model: &str) -> ClaudeCliResolution {
    resolve_cli_model_with(&routing_snapshot(), cabinet_model)
}

pub fn resolve_cli_model_with(
    routing: &ClaudeRoutingFile,
    cabinet_model: &str,
) -> ClaudeCliResolution {
    let cabinet_model = cabinet_model.trim();
    if cabinet_model.is_empty() {
        let cli = routing.cli_default_model.clone();
        return ClaudeCliResolution {
            cli_model_arg: cli.clone(),
            response_label: format!("{}{}", routing.cli_label_prefix, cli),
            cabinet_model: String::new(),
        };
    }

    if let Some(cli_m) = routing.cli_models.get(cabinet_model) {
        let cli_m = cli_m.trim();
        return ClaudeCliResolution {
            cli_model_arg: cli_m.to_string(),
            response_label: format!("{}{}", routing.cli_label_prefix, cli_m),
            cabinet_model: cabinet_model.to_string(),
        };
    }

    // Legacy substring fallback (prefer explicit yaml entries).
    let cli_m = if cabinet_model.starts_with("claude-") {
        cabinet_model.to_string()
    } else {
        routing.cli_default_model.clone()
    };

    ClaudeCliResolution {
        cli_model_arg: cli_m.clone(),
        response_label: format!("{}{}", routing.cli_label_prefix, cli_m),
        cabinet_model: cabinet_model.to_string(),
    }
}

pub fn resolve_api_adaptive(wire_model: &str) -> Option<ApiAdaptiveEntry> {
    let routing = routing_snapshot();
    if let Some(entry) = routing.api_adaptive.get(wire_model.trim()) {
        return Some(entry.clone());
    }
    let m = wire_model.trim();
    if m.contains("opus-4-8") || m == "claude-opus-4-8" {
        return routing.api_adaptive.get("claude-opus-4-8").cloned();
    }
    if m.contains("opus-4-6") || m == "claude-opus-4-6" {
        return routing.api_adaptive.get("claude-opus-4-6").cloned();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_46_pins_cli_model_not_48() {
        let r = ClaudeRoutingFile::default();
        let res = resolve_cli_model_with(&r, "claude-opus-4-6");
        assert_eq!(res.cli_model_arg, "claude-opus-4-6");
        assert_eq!(res.response_label, "claude-cli-claude-opus-4-6");
    }

    #[test]
    fn api_adaptive_for_opus_46() {
        let entry = resolve_api_adaptive("claude-opus-4-6").expect("adaptive");
        assert_eq!(entry.effort, "high");
    }

    #[test]
    fn parse_cli_version_from_stdout() {
        assert_eq!(parse_cli_version("2.1.195 (abc123)"), Some((2, 1, 195)));
        assert_eq!(parse_cli_version("claude version 2.0.5"), Some((2, 0, 5)));
    }

    #[test]
    fn version_at_least_compare() {
        assert!(version_at_least((2, 1, 195), (2, 1, 0)));
        assert!(!version_at_least((2, 0, 99), (2, 1, 0)));
    }
}
