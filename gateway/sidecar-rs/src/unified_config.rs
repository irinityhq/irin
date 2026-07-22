// ==========================================================================
// unified_config.rs — Optional single-file YAML config (Phase 5 item 4).
//
// When `GATEWAY_CONFIG_PATH` is set and points at a readable YAML file,
// this module parses it into a `UnifiedConfig` whose sections override
// the per-component JSON files (`models.json`, `auth_keys.json`,
// `decon_config.json`, `shape_limits.json`, and a forward-compat
// `ip_policy` stub).
//
// Backward-compatibility contract:
//   - If `GATEWAY_CONFIG_PATH` is unset → all per-component JSON loaders
//     run unchanged.
//   - If it IS set, each section is OPTIONAL inside the YAML. Missing
//     sections fall back to the existing JSON-file path for that
//     component. (i.e. you can move just `auth` into the YAML and leave
//     models/decon/shape on disk.)
//
// Lua side-channel: the OpenResty image has no YAML parser, so when a
// YAML file is present, this module also writes derived JSON files for
// the Lua-consumed sections (`models`, `shape_limits`) into the
// directory pointed to by `GATEWAY_DERIVED_DIR` (default
// `/var/lib/sidecar/derived`). Lua reads those derived files via the
// `MODELS_JSON_PATH` / `SHAPE_LIMITS_PATH` env vars in compose. This
// keeps the Lua hot path JSON-only.
// ==========================================================================

use serde::Deserialize;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Top-level shape of `gateway.yaml`. Every field is optional so a partial
/// YAML simply "shadows" the corresponding JSON file. Untyped
/// `serde_json::Value` is used for sections that already have rich typed
/// loaders elsewhere — we hand the value off rather than re-modeling.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UnifiedConfig {
    /// Model registry (provider/model/alias tree). Same shape as
    /// `conf/models.json`.
    #[serde(default)]
    pub models: Option<serde_json::Value>,

    /// Auth config — keys, global/IP rate limits.
    #[serde(default)]
    pub auth: Option<serde_json::Value>,

    /// Forward-compatible IP allow/deny stub. No production consumer in
    /// the current sidecar; preserved here so a YAML written today is
    /// valid against tomorrow's binary.
    #[serde(default)]
    pub ip_policy: Option<serde_json::Value>,

    /// 7-stage decontaminator config. Same shape as `conf/decon_config.json`.
    #[serde(default)]
    pub decon: Option<serde_json::Value>,

    /// ASM structural input gates (Lua-consumed). Same shape as
    /// `conf/shape_limits.json`.
    #[serde(default)]
    pub shape_limits: Option<serde_json::Value>,
}

impl UnifiedConfig {
    /// Read + parse a YAML config file. Returns an error string on IO or
    /// parse failure so the caller can decide whether to panic or warn.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        let cfg: UnifiedConfig = serde_yaml::from_str(&raw)
            .map_err(|e| format!("cannot parse YAML at {}: {}", path.display(), e))?;
        Ok(cfg)
    }

    /// Resolve the configured path from env. `None` means use individual
    /// JSON files (backward-compatible default).
    pub fn configured_path() -> Option<PathBuf> {
        std::env::var("GATEWAY_CONFIG_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }

    /// Write the Lua-consumed sections (models, shape_limits) to derived
    /// JSON files, so the OpenResty side (which can't parse YAML) reads
    /// JSON. Always writes BOTH files when invoked — when a section is
    /// absent from the YAML, copies the legacy on-disk JSON so Lua's
    /// auto-routing (`/var/lib/sidecar/derived/...`) still finds a file.
    /// This is the invariant that keeps Lua and Rust from diverging.
    ///
    /// Returns the directory the files were written into.
    pub fn materialize_lua_derived(&self) -> Result<PathBuf, String> {
        let dir = std::env::var("GATEWAY_DERIVED_DIR")
            .unwrap_or_else(|_| "/var/lib/sidecar/derived".to_string());
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create derived dir {}: {}", dir.display(), e))?;

        // models — YAML wins; else copy from MODELS_JSON_PATH if set.
        let models_dst = dir.join("models.json");
        if let Some(v) = &self.models {
            let body =
                serde_json::to_string_pretty(v).map_err(|e| format!("serialize models: {}", e))?;
            std::fs::write(&models_dst, body)
                .map_err(|e| format!("write {}: {}", models_dst.display(), e))?;
            info!(path = %models_dst.display(), "unified_config: wrote derived models.json (from YAML)");
        } else if let Ok(src) = std::env::var("MODELS_JSON_PATH") {
            if !src.is_empty() && std::path::Path::new(&src).exists() {
                std::fs::copy(&src, &models_dst)
                    .map_err(|e| format!("copy {} → {}: {}", src, models_dst.display(), e))?;
                info!(src, dst = %models_dst.display(), "unified_config: copied models.json (YAML lacks section)");
            } else {
                warn!("unified_config: no `models` in YAML and MODELS_JSON_PATH unset/missing — Lua may fail to load registry");
            }
        }

        // shape_limits — YAML wins; else copy from a conventional path
        // (the same /conf mount the Lua side historically uses, but
        // resolved relative to the sidecar's filesystem view).
        let shape_dst = dir.join("shape_limits.json");
        if let Some(v) = &self.shape_limits {
            let body = serde_json::to_string_pretty(v)
                .map_err(|e| format!("serialize shape_limits: {}", e))?;
            std::fs::write(&shape_dst, body)
                .map_err(|e| format!("write {}: {}", shape_dst.display(), e))?;
            info!(path = %shape_dst.display(), "unified_config: wrote derived shape_limits.json (from YAML)");
        } else {
            let candidates = ["/conf/shape_limits.json", "conf/shape_limits.json"];
            let mut copied = false;
            for src in candidates.iter() {
                if std::path::Path::new(src).exists() {
                    std::fs::copy(src, &shape_dst)
                        .map_err(|e| format!("copy {} → {}: {}", src, shape_dst.display(), e))?;
                    info!(src = *src, dst = %shape_dst.display(), "unified_config: copied shape_limits.json (YAML lacks section)");
                    copied = true;
                    break;
                }
            }
            if !copied {
                warn!("unified_config: no `shape_limits` in YAML and no source JSON found — Lua will use empty defaults");
            }
        }

        Ok(dir)
    }
}

/// Helper used by main.rs to emit a one-line summary at boot describing
/// which sections were sourced from the YAML and which fell back.
pub fn log_section_sources(cfg: &Option<UnifiedConfig>) {
    let Some(c) = cfg else {
        info!("unified_config: GATEWAY_CONFIG_PATH not set — using per-component JSON files");
        return;
    };
    info!(
        models = c.models.is_some(),
        auth = c.auth.is_some(),
        ip_policy = c.ip_policy.is_some(),
        decon = c.decon.is_some(),
        shape_limits = c.shape_limits.is_some(),
        "unified_config: loaded YAML — sections present (false = fallback to JSON)"
    );
    if c.ip_policy.is_some() {
        warn!(
            "unified_config: ip_policy section is a forward-compat stub — \
             the current sidecar has no IP allow/deny consumer"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_yaml() {
        let yaml = r#"
models:
  providers:
    xai:
      base_url: "https://api.x.ai"
  models: {}
auth:
  global_rpm: 500
  ip_rpm: 60
  keys: []
ip_policy:
  trusted_internal_cidrs: ["10.0.0.0/8"]
  deny_cidrs: []
  mode: "allow_internal_deny_explicit"
decon:
  block_severity: 0.7
  stages: {}
shape_limits:
  default:
    max_messages: 256
"#;
        let cfg: UnifiedConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.models.is_some());
        assert!(cfg.auth.is_some());
        assert!(cfg.ip_policy.is_some());
        assert!(cfg.decon.is_some());
        assert!(cfg.shape_limits.is_some());
    }

    #[test]
    fn parses_partial_yaml() {
        let yaml = r#"
auth:
  global_rpm: 1000
"#;
        let cfg: UnifiedConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.auth.is_some());
        assert!(cfg.models.is_none());
        assert!(cfg.decon.is_none());
        assert!(cfg.shape_limits.is_none());
    }

    #[test]
    fn parses_empty_yaml() {
        let yaml = "";
        let cfg: UnifiedConfig = serde_yaml::from_str(yaml).unwrap_or_default();
        assert!(cfg.models.is_none());
    }

    #[test]
    fn rejects_bad_yaml() {
        let yaml = "models: [unclosed";
        let res: Result<UnifiedConfig, _> = serde_yaml::from_str(yaml);
        assert!(res.is_err());
    }
}
