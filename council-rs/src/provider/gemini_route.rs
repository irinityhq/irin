//! Gemini / Vertex model routing — driven by `gemini_routing.yaml`.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Deserialize)]
pub struct GeminiGenerationEntry {
    #[serde(default = "default_thinking_level")]
    pub thinking_level: String,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
}

fn default_thinking_level() -> String {
    "medium".into()
}

fn default_max_output_tokens() -> u32 {
    4096
}

fn default_temperature() -> f64 {
    0.7
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeminiRoutingFile {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub models: HashMap<String, GeminiGenerationEntry>,
    #[serde(default = "default_cli_model")]
    pub cli_default_model: String,
}

fn default_model() -> String {
    "gemini-3.1-pro-preview".into()
}

fn default_cli_model() -> String {
    "gemini-3.1-pro-preview".into()
}

impl Default for GeminiRoutingFile {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            models: default_models(),
            cli_default_model: default_cli_model(),
        }
    }
}

fn default_models() -> HashMap<String, GeminiGenerationEntry> {
    [
        (
            "gemini-3.1-pro-preview".into(),
            GeminiGenerationEntry {
                thinking_level: "medium".into(),
                // Mirrors gemini_routing.yaml: thoughtsTokenCount spends from
                // maxOutputTokens on Vertex Gemini 3; medium thinking burns
                // ~3-4k, so 4096 yielded MAX_TOKENS with zero visible text.
                max_output_tokens: 16384,
                temperature: 0.7,
            },
        ),
        (
            "gemini-3.5-flash".into(),
            GeminiGenerationEntry {
                thinking_level: "low".into(),
                // Mirrors gemini_routing.yaml: low thinking still spends from
                // maxOutputTokens; 512 risks the same zero-text MAX_TOKENS.
                max_output_tokens: 2048,
                temperature: 0.7,
            },
        ),
        (
            "gemini-3.1-flash-lite".into(),
            GeminiGenerationEntry {
                thinking_level: "low".into(),
                max_output_tokens: 1024,
                temperature: 0.7,
            },
        ),
    ]
    .into_iter()
    .collect()
}

static ROUTING: OnceLock<RwLock<GeminiRoutingFile>> = OnceLock::new();
static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn routing_store() -> &'static RwLock<GeminiRoutingFile> {
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
        *lock.write().expect("gemini routing lock") = load_routing_from_disk(base_dir);
    }
}

fn load_routing_from_disk(base_dir: &Path) -> GeminiRoutingFile {
    let path = base_dir.join("gemini_routing.yaml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return GeminiRoutingFile::default(),
    };
    match serde_yaml::from_str::<GeminiRoutingFile>(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("⚠️  gemini_routing.yaml parse error ({e}); using built-in defaults");
            GeminiRoutingFile::default()
        }
    }
}

pub fn routing_snapshot() -> GeminiRoutingFile {
    routing_store().read().expect("gemini routing lock").clone()
}

pub fn resolve_wire_model(cabinet_model: &str) -> String {
    let routing = routing_snapshot();
    let m = cabinet_model.trim();
    if m.is_empty() {
        return routing.default_model;
    }
    m.to_string()
}

pub fn resolve_generation(wire_model: &str) -> GeminiGenerationEntry {
    let routing = routing_snapshot();
    let m = wire_model.trim();
    if let Some(entry) = routing.models.get(m) {
        return entry.clone();
    }
    // Longest-prefix match for dated snapshots (e.g. gemini-3.1-pro-preview-…).
    let mut best: Option<(&String, &GeminiGenerationEntry)> = None;
    for (key, entry) in &routing.models {
        if m.starts_with(key.as_str()) && best.map(|(k, _)| key.len() > k.len()).unwrap_or(true) {
            best = Some((key, entry));
        }
    }
    if let Some((_, entry)) = best {
        return entry.clone();
    }
    GeminiGenerationEntry {
        thinking_level: default_thinking_level(),
        max_output_tokens: default_max_output_tokens(),
        temperature: default_temperature(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_gets_low_thinking_from_defaults() {
        let gen_cfg = resolve_generation("gemini-3.5-flash");
        assert_eq!(gen_cfg.thinking_level, "low");
        assert_eq!(gen_cfg.max_output_tokens, 2048);
    }

    #[test]
    fn empty_model_uses_default_wire_id() {
        let routing = GeminiRoutingFile::default();
        assert_eq!(
            resolve_wire_model_with(&routing, ""),
            "gemini-3.1-pro-preview"
        );
    }

    fn resolve_wire_model_with(routing: &GeminiRoutingFile, model: &str) -> String {
        let m = model.trim();
        if m.is_empty() {
            return routing.default_model.clone();
        }
        m.to_string()
    }
}
