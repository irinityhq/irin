//! Agy CLI model routing — driven by `agy_routing.yaml`.
//!
//! Maps cabinet legacy gemini-* IDs (still used across cabinets/roles.yaml/models.yaml)
//! to the exact model display names expected by `agy --model` (from `agy models`).
//! Direct agy slugs pass through.
//! Loaded at --base-dir; hot via set_base_dir like grok/claude/gemini routes.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Deserialize)]
pub struct AgyRoutingFile {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub models: HashMap<String, String>,
}

fn default_model() -> String {
    "Gemini 3.1 Pro (High)".into()
}

impl Default for AgyRoutingFile {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            models: default_models(),
        }
    }
}

fn default_models() -> HashMap<String, String> {
    [
        (
            "gemini-3.1-pro-preview".into(),
            "Gemini 3.1 Pro (High)".into(),
        ),
        (
            "gemini-3.5-flash".into(),
            "Gemini 3.5 Flash (Medium)".into(),
        ),
        (
            "gemini-3.1-flash-lite".into(),
            "Gemini 3.5 Flash (Low)".into(),
        ),
    ]
    .into_iter()
    .collect()
}

static ROUTING: OnceLock<RwLock<AgyRoutingFile>> = OnceLock::new();
static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn routing_store() -> &'static RwLock<AgyRoutingFile> {
    ROUTING.get_or_init(|| RwLock::new(load_routing_from_disk(&base_dir())))
}

fn base_dir() -> PathBuf {
    BASE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Called from `Config::load` (and equivalent) so routing tracks `--base-dir`.
pub fn set_base_dir(base_dir: &Path) {
    let _ = BASE_DIR.set(base_dir.to_path_buf());
    if let Some(lock) = ROUTING.get() {
        *lock.write().expect("agy routing lock") = load_routing_from_disk(base_dir);
    }
}

fn load_routing_from_disk(base_dir: &Path) -> AgyRoutingFile {
    let path = base_dir.join("agy_routing.yaml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return AgyRoutingFile::default(),
    };
    match serde_yaml::from_str::<AgyRoutingFile>(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("⚠️  agy_routing.yaml parse error ({e}); using built-in defaults");
            AgyRoutingFile::default()
        }
    }
}

pub fn routing_snapshot() -> AgyRoutingFile {
    routing_store().read().expect("agy routing lock").clone()
}

/// Resolve cabinet model (e.g. "gemini-3.1-pro-preview" or direct agy slug) to
/// the value to pass as `agy --model <value>`.
/// - exact match in yaml map wins
/// - empty -> default_model
/// - otherwise passthrough (supports direct use of agy display names with agy_cli)
pub fn resolve_agy_model(cabinet_model: &str) -> String {
    resolve_agy_model_with(&routing_snapshot(), cabinet_model)
}

pub fn resolve_agy_model_with(routing: &AgyRoutingFile, cabinet_model: &str) -> String {
    let m = cabinet_model.trim();
    if m.is_empty() {
        return routing.default_model.clone();
    }
    if let Some(mapped) = routing.models.get(m) {
        return mapped.clone();
    }
    // Passthrough for direct agy slugs ("Gemini 3.1 Pro (High)", etc.) and unknown.
    // Prefix match fallback for dated or variant legacy forms.
    let mut best: Option<(&String, &String)> = None;
    for (key, val) in &routing.models {
        if m.starts_with(key.as_str()) && best.map(|(k, _)| key.len() > k.len()).unwrap_or(true) {
            best = Some((key, val));
        }
    }
    if let Some((_, mapped)) = best {
        return mapped.clone();
    }
    m.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_pro_preview_maps_to_agy_high() {
        let res = resolve_agy_model("gemini-3.1-pro-preview");
        assert_eq!(res, "Gemini 3.1 Pro (High)");
    }

    #[test]
    fn legacy_flash_maps_to_agy_medium() {
        let res = resolve_agy_model("gemini-3.5-flash");
        assert_eq!(res, "Gemini 3.5 Flash (Medium)");
    }

    #[test]
    fn direct_agy_slug_passthrough() {
        let res = resolve_agy_model("Gemini 3.5 Flash (High)");
        assert_eq!(res, "Gemini 3.5 Flash (High)");
    }

    #[test]
    fn empty_uses_default() {
        let res = resolve_agy_model("");
        assert_eq!(res, "Gemini 3.1 Pro (High)");
    }

    #[test]
    fn unknown_legacy_passthrough_keeps_for_error_loud() {
        let res = resolve_agy_model("gemini-2.0-pro");
        assert_eq!(res, "gemini-2.0-pro");
    }

    #[test]
    fn prefix_match_for_snapshot_style() {
        let res = resolve_agy_model("gemini-3.1-pro-preview-foo");
        assert_eq!(res, "Gemini 3.1 Pro (High)");
    }
}
