//! Configuration loading — YAML cabinets, model registry, Tera prompts
//!
//! This is the "easy tweak" layer. Everything hot-reloadable:
//! - cabinets/*.yaml → Cabinet definitions
//! - models.yaml → Model IDs and pricing
//! - prompts/*.tera → System prompts with Jinja2-like templating

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tera::Tera;

use crate::types::{Cabinet, ModelRegistry, RolesConfig, RolesFile};

/// Problem Restate Gate — injected into every seat prompt via Tera.
const RESTATE_GATE: &str = "MANDATORY FIRST STEP: Before any analysis, restate the problem in \
your own words in 1-2 sentences. This proves comprehension and frames your unique perspective. \
Then proceed with your analysis.";

/// Frame Check Gate (v9.10.0 — anti-prompt-poisoning)
///
/// Appended to seat system prompts. Forces every seat to independently challenge
/// assumptions embedded in the prompt rather than accepting them as constraints.
/// Trinity learning: when one model frames the question with a false constraint,
/// all downstream models inherit the blind spot.
const FRAME_CHECK_GATE: &str = "\n\nFRAME CHECK: Before accepting any stated constraint in the \
prompt (e.g. 'we don't have X', 'without Y', 'given only Z'), ask yourself: IS THIS ACTUALLY \
TRUE? If a constraint is marked [UNVERIFIED], challenge it explicitly — state what you would \
check to verify it, and note if your analysis would change if the constraint is false. Do not \
let an unverified assumption become your ceiling.";

/// Resolved configuration — everything needed to run a deliberation.
pub struct Config {
    pub cabinets: HashMap<String, Cabinet>,
    pub models: ModelRegistry,
    pub roles: RolesConfig,
    pub tera: Tera,
    pub base_dir: PathBuf,
}

impl Config {
    /// Load all configuration from the given base directory.
    /// Expects: cabinets/*.yaml, models.yaml, roles.yaml, prompts/*.tera
    pub fn load(base_dir: &Path) -> Result<Self> {
        crate::provider::grok_route::set_base_dir(base_dir);
        crate::provider::claude_route::set_base_dir(base_dir);
        crate::provider::gemini_route::set_base_dir(base_dir);
        crate::provider::agy_route::set_base_dir(base_dir);
        crate::provider::hermes_cli::set_base_dir(base_dir);
        // Load models first so cabinet validation can reference the registry.
        let models = Self::load_models(base_dir)?;
        let mut roles = Self::load_roles(base_dir)?;
        roles.normalize_provider_slugs();
        // Warn on role models not in registry (catches "edit roles.yaml and forget models.yaml").
        for role_name in [
            "convergence_judge",
            "frame_check",
            "claim_validator",
            "scope_auditor",
        ] {
            if let Some(role) = match role_name {
                "convergence_judge" => Some(&roles.convergence_judge),
                "frame_check" => Some(&roles.frame_check),
                "claim_validator" => Some(&roles.claim_validator),
                "scope_auditor" => Some(&roles.scope_auditor),
                _ => None,
            } {
                for cand in &role.cascade {
                    if !models
                        .models
                        .values()
                        .any(|m| m.id == cand.model || cand.model.starts_with(&m.id))
                    {
                        eprintln!(
                            "⚠️  roles.yaml {} uses model '{}' not found in models.yaml (cost tracking / vault may miss it)",
                            role_name, cand.model
                        );
                    }
                }
            }
        }
        let cabinets = Self::load_cabinets(base_dir)?;
        let tera = Self::load_prompts(base_dir)?;

        // Phase 0.5 §4.3 + P1 #15: structural validation per cabinet; one batched
        // xmcp vault check for all unique model IDs (cabinets + roles) so startup
        // does not spam N warnings when xmcp is offline.
        for (name, cabinet) in &cabinets {
            validate_cabinet_structure(name, cabinet, &models)
                .map_err(|e| anyhow::anyhow!("Cabinet validation failed: {}", e))?;
        }

        let vault_model_ids = collect_config_vault_model_ids(&cabinets, &roles);
        validate_vault_models(&vault_model_ids).map_err(|e| anyhow::anyhow!("{}", e))?;

        Ok(Self {
            cabinets,
            models,
            roles,
            tera,
            base_dir: base_dir.to_path_buf(),
        })
    }

    fn load_roles(base_dir: &Path) -> Result<RolesConfig> {
        let path = base_dir.join("roles.yaml");
        if !path.exists() {
            eprintln!(
                "⚠️  roles.yaml not found at {} — using built-in utility-role defaults",
                path.display()
            );
            return Ok(RolesConfig::built_in_defaults());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Reading roles: {}", path.display()))?;
        let file: RolesFile =
            serde_yaml::from_str(&content).with_context(|| "Parsing roles.yaml")?;
        let convergence_judge = file
            .roles
            .get("convergence_judge")
            .cloned()
            .filter(|r| !r.cascade.is_empty())
            .unwrap_or_else(|| RolesConfig::built_in_defaults().convergence_judge);
        let frame_check = file
            .roles
            .get("frame_check")
            .cloned()
            .filter(|r| !r.cascade.is_empty())
            .unwrap_or_else(|| RolesConfig::built_in_defaults().frame_check);
        let claim_validator = file
            .roles
            .get("claim_validator")
            .cloned()
            .filter(|r| !r.cascade.is_empty())
            .unwrap_or_else(|| RolesConfig::built_in_defaults().claim_validator);
        let scope_auditor = file
            .roles
            .get("scope_auditor")
            .cloned()
            .filter(|r| !r.cascade.is_empty())
            .unwrap_or_else(|| RolesConfig::built_in_defaults().scope_auditor);
        Ok(RolesConfig {
            convergence_judge,
            frame_check,
            claim_validator,
            scope_auditor,
        })
    }

    fn load_cabinets(base_dir: &Path) -> Result<HashMap<String, Cabinet>> {
        let cabinet_dir = base_dir.join("cabinets");
        let mut cabinets = HashMap::new();

        if !cabinet_dir.exists() {
            anyhow::bail!("Cabinet directory not found: {}", cabinet_dir.display());
        }

        for entry in std::fs::read_dir(&cabinet_dir)
            .with_context(|| format!("Reading cabinet dir: {}", cabinet_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
            {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                if stem.contains("canary") {
                    // Internal canary variants (e.g. triage.canary-novertex) are
                    // not shown in default picker — they are host-specific overlays.
                    continue;
                }
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("Reading cabinet: {}", path.display()))?;
                let cabinet = parse_cabinet_yaml(&content)
                    .with_context(|| format!("Parsing cabinet: {}", path.display()))?;

                let key = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                cabinets.insert(key, cabinet);
            }
        }

        Ok(cabinets)
    }

    fn load_models(base_dir: &Path) -> Result<ModelRegistry> {
        let models_path = base_dir.join("models.yaml");
        let content = std::fs::read_to_string(&models_path)
            .with_context(|| format!("Reading models: {}", models_path.display()))?;
        let registry: ModelRegistry =
            serde_yaml::from_str(&content).with_context(|| "Parsing models.yaml")?;
        Ok(registry)
    }

    fn load_prompts(base_dir: &Path) -> Result<Tera> {
        let prompts_dir = base_dir.join("prompts");
        let glob_pattern = format!("{}/**/*.tera", prompts_dir.display());
        let mut tera = Tera::new(&glob_pattern)
            .with_context(|| format!("Loading prompt templates from: {}", prompts_dir.display()))?;

        // Register the restate_gate as a global variable
        tera.register_function("restate_gate", |_args: &HashMap<String, tera::Value>| {
            Ok(tera::Value::String(RESTATE_GATE.to_string()))
        });

        Ok(tera)
    }

    /// Render a system prompt — resolves template name or returns inline text.
    pub fn render_system_prompt(&self, system: &str) -> Result<String> {
        // Check if it's a template name (no spaces, no newlines)
        let template_name = format!("{}.tera", system);
        if self
            .tera
            .get_template_names()
            .any(|n| n.ends_with(&template_name))
        {
            let mut ctx = tera::Context::new();
            ctx.insert("restate_gate", RESTATE_GATE);
            ctx.insert("frame_check_gate", FRAME_CHECK_GATE);
            // Find the full template name
            let full_name = self
                .tera
                .get_template_names()
                .find(|n| n.ends_with(&template_name))
                .unwrap()
                .to_string();
            let rendered = self
                .tera
                .render(&full_name, &ctx)
                .with_context(|| format!("Rendering prompt template: {}", system))?;
            Ok(rendered.trim().to_string())
        } else {
            // Inline system prompt — append restate gate + frame check gate
            Ok(format!(
                "{}\n\n{}{}",
                system, RESTATE_GATE, FRAME_CHECK_GATE
            ))
        }
    }

    /// Get a cabinet by name.
    pub fn get_cabinet(&self, name: &str) -> Result<&Cabinet> {
        self.cabinets.get(name).with_context(|| {
            let available: Vec<_> = self.cabinets.keys().collect();
            format!("Unknown cabinet: '{}'. Available: {:?}", name, available)
        })
    }

    /// Resolve a cabinet by name to an owned `Cabinet`, falling back to disk for
    /// cabinets saved after startup (feature contract / feature contract).
    ///
    /// The startup `Arc<Config>` is immutable, so a cabinet saved via
    /// `POST /api/cabinets/save` (or imported, which POSTs to the same route)
    /// shows up in `GET /api/cabinets` (per-request disk re-scan) but is absent
    /// from `self.cabinets`. Without this fallback the frontend Run flow — which
    /// launches a saved cabinet by registry name over WS — fails with
    /// "Unknown cabinet". On a registry miss we load
    /// `<base_dir>/cabinets/<name>.yaml`, stamp the canonical hash
    /// (`parse_cabinet_yaml`), and run the same per-run execution gate the
    /// `custom_cabinet` path uses (`validate_cabinet_for_execution`). The name
    /// is treated as a file stem only — never a path — to forbid traversal.
    pub fn resolve_cabinet_owned(&self, name: &str) -> Result<Cabinet> {
        if let Some(cab) = self.cabinets.get(name) {
            return Ok(cab.clone());
        }
        // Registry miss: only a syntactically valid stem can name a saved file;
        // this also rejects traversal ("../x"), separators, and dotfiles.
        if !crate::warroom::cabinets_save::is_valid_cabinet_name(name) {
            let available: Vec<_> = self.cabinets.keys().collect();
            anyhow::bail!("Unknown cabinet: '{}'. Available: {:?}", name, available);
        }
        let path = self.base_dir.join("cabinets").join(format!("{name}.yaml"));
        let content = std::fs::read_to_string(&path).with_context(|| {
            let available: Vec<_> = self.cabinets.keys().collect();
            format!("Unknown cabinet: '{}'. Available: {:?}", name, available)
        })?;
        let cabinet = parse_cabinet_yaml(&content)
            .with_context(|| format!("Parsing saved cabinet: {}", path.display()))?;
        self.validate_cabinet_for_execution(name, &cabinet)?;
        Ok(cabinet)
    }

    /// Validate a cabinet supplied outside the checked-in registry before
    /// execution, such as a War Room custom cabinet (structural rules only;
    /// vault is batched once at `Config::load`).
    pub fn validate_cabinet_for_execution(&self, name: &str, cabinet: &Cabinet) -> Result<()> {
        validate_cabinet_structure(name, cabinet, &self.models)
            .map_err(|e| anyhow::anyhow!("Cabinet validation failed: {}", e))
    }

    /// Structural + per-cabinet vault check (e.g. War Room save before write).
    pub fn validate_cabinet_for_save(&self, name: &str, cabinet: &Cabinet) -> Result<()> {
        self.validate_cabinet_for_execution(name, cabinet)?;
        let model_ids: Vec<String> = cabinet_model_id_strings(cabinet);
        validate_vault_models(&model_ids).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Load an external cabinet YAML file and insert it into the registry.
    /// Returns the stem name used as the key.
    pub fn load_external_cabinet(&mut self, path: &Path) -> Result<String> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Reading external cabinet: {}", path.display()))?;
        let cabinet: Cabinet = serde_yaml::from_str(&content)
            .with_context(|| format!("Parsing external cabinet: {}", path.display()))?;
        let key = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        self.validate_cabinet_for_save(&key, &cabinet)?;
        self.cabinets.insert(key.clone(), cabinet);
        Ok(key)
    }

    /// List available cabinets.
    pub fn list_cabinets(&self) -> Vec<(&str, &str)> {
        let mut list: Vec<_> = self
            .cabinets
            .iter()
            .map(|(k, v)| (k.as_str(), v.description.as_str()))
            .collect();
        list.sort_by_key(|(k, _)| *k);
        list
    }
}

/// Canonical cabinet hash — SHA256 over the serde_json representation as
/// parsed (the `hash` field itself participates exactly as `load_cabinets`
/// always computed it). Used for `/api/deliberate` @hash pinning and fork
/// wire shapes; any path that parses cabinet YAML outside `Config::load`
/// must stamp this or pinning breaks on saved cabinets.
pub fn compute_cabinet_hash(cabinet: &Cabinet) -> String {
    let canonical_bytes = serde_json::to_vec(cabinet).unwrap_or_default();
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&canonical_bytes);
    hex::encode(hasher.finalize())
}

/// Parse cabinet YAML and stamp the canonical hash — the single parse path
/// shared by `Config::load_cabinets`, the live `/api/cabinets` re-scan, and
/// `POST /api/cabinets/save` (feature contract).
pub fn parse_cabinet_yaml(content: &str) -> Result<Cabinet> {
    let mut cabinet: Cabinet = serde_yaml::from_str(content).context("Parsing cabinet YAML")?;
    cabinet.hash = compute_cabinet_hash(&cabinet);
    Ok(cabinet)
}

/// Tolerant re-scan of `<base_dir>/cabinets` for the live `GET /api/cabinets`
/// listing (feature contract) — parse + hash only; malformed files are skipped with a
/// warning instead of failing the whole listing. No structural/vault
/// validation here: saved cabinets are validated at save time and again
/// per-run via `validate_cabinet_for_execution`, and re-running the xmcp
/// vault check would be a network call per GET.
pub fn scan_cabinets_dir(base_dir: &Path) -> HashMap<String, Cabinet> {
    let cabinet_dir = base_dir.join("cabinets");
    let mut cabinets = HashMap::new();
    let entries = match std::fs::read_dir(&cabinet_dir) {
        Ok(e) => e,
        Err(_) => return cabinets,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml")
        {
            continue;
        }
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        if stem.contains("canary") {
            // Internal canary variants (e.g. triage.canary-novertex) are
            // not shown in default picker — they are host-specific overlays.
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse_cabinet_yaml(&content) {
            Ok(cabinet) => {
                let key = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                cabinets.insert(key, cabinet);
            }
            Err(e) => {
                eprintln!(
                    "⚠️  scan_cabinets_dir: skipping {}: {:#}",
                    path.display(),
                    e
                );
            }
        }
    }
    cabinets
}

/// Phase 0.5 §4.3: structural validation for a single cabinet (no vault).
///
/// Structural rules (hard-fail, always):
///   - chair.provider and chair.model must not start with `council` —
///     forbids cabinets recursing back into the council endpoint.
///   - same applies to every seat.
fn validate_cabinet_structure(
    name: &str,
    cabinet: &Cabinet,
    _models: &ModelRegistry,
) -> Result<(), String> {
    fn is_council(s: &str) -> bool {
        s.starts_with("council")
    }
    if is_council(&cabinet.chair.provider) || is_council(&cabinet.chair.model) {
        return Err(format!(
            "Cabinet '{}': chair cannot use a council provider or model \
             (chair.provider='{}', chair.model='{}')",
            name, cabinet.chair.provider, cabinet.chair.model
        ));
    }
    for seat in &cabinet.seats {
        if is_council(&seat.provider) || is_council(&seat.model) {
            return Err(format!(
                "Cabinet '{}': seat '{}' cannot use a council provider or model \
                 (provider='{}', model='{}')",
                name, seat.name, seat.provider, seat.model
            ));
        }
    }

    if cabinet.local_code_only {
        if !crate::provider::is_readonly_cli_agent_provider(&cabinet.chair.provider) {
            return Err(format!(
                "Cabinet '{}': local_code_only chair must use a read-only CLI-agent provider \
                 (chair.provider='{}')",
                name, cabinet.chair.provider
            ));
        }
        for seat in &cabinet.seats {
            if !crate::provider::is_readonly_cli_agent_provider(&seat.provider) {
                return Err(format!(
                    "Cabinet '{}': local_code_only seat '{}' must use a read-only CLI-agent provider \
                     (provider='{}')",
                    name, seat.name, seat.provider
                ));
            }
        }
    }

    Ok(())
}

fn cabinet_model_id_strings(cabinet: &Cabinet) -> Vec<String> {
    let mut ids: Vec<String> = cabinet.seats.iter().map(|s| s.model.clone()).collect();
    ids.push(cabinet.chair.model.clone());
    ids
}

/// Unique model IDs from all cabinets plus utility roles (one vault batch at load).
fn collect_config_vault_model_ids(
    cabinets: &HashMap<String, Cabinet>,
    roles: &RolesConfig,
) -> Vec<String> {
    use std::collections::HashSet;

    let mut set = HashSet::new();
    for cabinet in cabinets.values() {
        for id in cabinet_model_id_strings(cabinet) {
            set.insert(id);
        }
    }
    for id in roles.all_model_ids() {
        set.insert(id.to_string());
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort();
    out
}

/// Vault rule (P1 #15): passed to `xmcp::model_check_blocking`, which owns the
/// offline/dev tolerance contract (`COUNCIL_SKIP_VAULT_CHECK`, unreachable soft-pass).
fn validate_vault_models(model_ids: &[String]) -> Result<(), String> {
    if model_ids.is_empty() {
        return Ok(());
    }
    let refs: Vec<&str> = model_ids.iter().map(String::as_str).collect();
    crate::xmcp::model_check_blocking(&refs).map_err(|e| format!("xmcp model_check failed: {e}"))
}
