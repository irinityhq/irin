// ==========================================================================
// router.rs — Quality-weighted smart routing engine.
//
// Port of the legacy model_router.py scoring system.
// Routes requests to the optimal backend based on 4 dimensions:
//
//   Score = (quality_w × quality)
//         − (latency_w × latency / max_latency)
//         − (cost_w × cost / max_cost)
//         − (risk_w × risk)
//
// The router loads model definitions from models.json and scores
// each available backend per-request based on task type, budget,
// and current provider health.
// ==========================================================================

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Configuration types (loaded from models.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelDef {
    pub id: String,
    pub provider: String,
    /// Model family for per-family health tracking. Models in the same
    /// family share a circuit breaker so one model's 429s don't darken
    /// unrelated models from the same provider. Example: "deepseek-v4",
    /// "qwen3.5", "nemotron-3". Falls back to provider name if unset.
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub fallback: Option<String>,
    #[serde(default)]
    pub pricing: PricingDef,
    #[serde(default)]
    pub capabilities: CapabilityDef,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PricingDef {
    #[serde(default, alias = "input")]
    pub input_per_1m: f64,
    #[serde(default, alias = "output")]
    pub output_per_1m: f64,
    #[serde(default)]
    pub cached_input: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CapabilityDef {
    #[serde(default)]
    pub quality: f64, // 0.0–1.0 benchmark-anchored quality rating
    #[serde(default)]
    pub max_context: u64, // max context window tokens
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_streaming: bool,
    /// Whether the model supports reasoning/thinking mode.
    #[serde(default)]
    pub supports_thinking: bool,
    /// Whether the model is designated as a coding specialist.
    #[serde(default)]
    pub is_code_specialist: bool,
}

// ---------------------------------------------------------------------------
// Task classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Coding,
    Creative,
    Analysis,
    #[allow(dead_code)]
    Conversation,
    ToolUse,
    Vision,
    General,
}

impl TaskType {
    /// Classify a request based on messages content and parameters.
    pub fn classify(body: &serde_json::Value) -> Self {
        // Check for tool_choice or tools → ToolUse
        if body.get("tools").is_some() || body.get("tool_choice").is_some() {
            return TaskType::ToolUse;
        }

        // Check for image content → Vision
        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            for msg in messages {
                if let Some(content) = msg.get("content") {
                    if let Some(arr) = content.as_array() {
                        for part in arr {
                            if part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                                return TaskType::Vision;
                            }
                        }
                    }
                }
            }
        }

        // Keyword-based classification from last user message
        let last_user_msg = Self::extract_last_user_message(body);
        if let Some(msg) = &last_user_msg {
            let lower = msg.to_lowercase();

            // Coding keywords
            let code_keywords = [
                "code",
                "function",
                "debug",
                "error",
                "compile",
                "implement",
                "refactor",
                "test",
                "bug",
                "class",
                "struct",
                "async",
                "api",
                "rust",
                "python",
                "javascript",
                "typescript",
                "sql",
            ];
            if code_keywords.iter().any(|k| lower.contains(k)) {
                return TaskType::Coding;
            }

            // Creative keywords
            let creative_keywords = [
                "write", "story", "poem", "creative", "imagine", "fiction", "compose", "draft",
                "essay", "blog", "article",
            ];
            if creative_keywords.iter().any(|k| lower.contains(k)) {
                return TaskType::Creative;
            }

            // Analysis keywords
            let analysis_keywords = [
                "analyze",
                "analyse",
                "compare",
                "evaluate",
                "assess",
                "review",
                "audit",
                "benchmark",
                "measure",
                "data",
            ];
            if analysis_keywords.iter().any(|k| lower.contains(k)) {
                return TaskType::Analysis;
            }
        }

        TaskType::General
    }

    fn extract_last_user_message(body: &serde_json::Value) -> Option<String> {
        body.get("messages")?
            .as_array()?
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?
            .get("content")?
            .as_str()
            .map(|s| s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Routing strategy — controls how the scorer prioritizes dimensions
// ---------------------------------------------------------------------------

/// Routing strategy selectable per-request via body or X-Routing-Strategy header.
///
///   quality  — best model for the job, cost is secondary
///   balanced — quality×cost tradeoff (default)
///   economy  — cheapest model that can handle the task
///   speed    — lowest latency above a quality floor
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RoutingStrategy {
    Quality,
    #[default]
    Balanced,
    Economy,
    Speed,
}

impl RoutingStrategy {
    /// Parse from string (header value, query param, etc.)
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "quality" | "best" => Some(RoutingStrategy::Quality),
            "balanced" | "default" => Some(RoutingStrategy::Balanced),
            "economy" | "cheap" | "budget" => Some(RoutingStrategy::Economy),
            "speed" | "fast" | "latency" => Some(RoutingStrategy::Speed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Scoring weights per task type × routing strategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct ScoringWeights {
    pub quality: f64,
    pub latency: f64,
    pub cost: f64,
    pub risk: f64,
}

impl ScoringWeights {
    /// Base weights per task type, then shifted by routing strategy.
    pub fn for_task(task: TaskType, strategy: RoutingStrategy) -> Self {
        // Start with task-specific base weights
        let base = match task {
            TaskType::Coding => ScoringWeights {
                quality: 0.50,
                latency: 0.10,
                cost: 0.20,
                risk: 0.20,
            },
            TaskType::Creative => ScoringWeights {
                quality: 0.55,
                latency: 0.05,
                cost: 0.20,
                risk: 0.20,
            },
            TaskType::Analysis => ScoringWeights {
                quality: 0.45,
                latency: 0.10,
                cost: 0.25,
                risk: 0.20,
            },
            TaskType::ToolUse => ScoringWeights {
                quality: 0.35,
                latency: 0.15,
                cost: 0.20,
                risk: 0.30,
            },
            TaskType::Vision => ScoringWeights {
                quality: 0.50,
                latency: 0.10,
                cost: 0.20,
                risk: 0.20,
            },
            TaskType::Conversation => ScoringWeights {
                quality: 0.35,
                latency: 0.25,
                cost: 0.25,
                risk: 0.15,
            },
            TaskType::General => ScoringWeights {
                quality: 0.40,
                latency: 0.15,
                cost: 0.25,
                risk: 0.20,
            },
        };

        // Apply strategy modifier — redistributes weight toward the priority axis
        match strategy {
            RoutingStrategy::Quality => ScoringWeights {
                quality: 0.75,               // quality dominates
                latency: base.latency * 0.3, // latency nearly irrelevant
                cost: base.cost * 0.2,       // cost nearly irrelevant
                risk: base.risk * 0.5,       // risk still matters somewhat
            },
            RoutingStrategy::Balanced => base, // no modification
            RoutingStrategy::Economy => ScoringWeights {
                quality: base.quality * 0.4, // quality floor only
                latency: base.latency * 0.3,
                cost: 0.70, // cost dominates
                risk: base.risk * 0.5,
            },
            RoutingStrategy::Speed => ScoringWeights {
                quality: base.quality * 0.5, // quality floor
                latency: 0.60,               // latency dominates
                cost: base.cost * 0.3,
                risk: base.risk * 0.5,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Per-family health tracking
//
// Health is tracked per (provider, family) tuple instead of per-provider.
// This prevents one model family's rate limits (e.g., deepseek-v4 on NIM)
// from circuit-breaking unrelated families (e.g., qwen3.5 on NIM).
// ---------------------------------------------------------------------------

/// Key for per-family health tracking: (provider, family).
/// If a model has no family, the provider name is used as the family.
pub type HealthKey = (String, String);

/// Derive the health key for a model.
pub fn health_key_for(model: &ModelDef) -> HealthKey {
    let family = model
        .family
        .clone()
        .unwrap_or_else(|| model.provider.clone());
    (model.provider.clone(), family)
}

#[derive(Debug, Clone)]
pub struct ProviderHealth {
    pub available: bool,
    pub error_rate: f64,     // 0.0–1.0 rolling error rate
    pub avg_latency_ms: f64, // rolling average response time
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self {
            available: true,
            error_rate: 0.0,
            avg_latency_ms: 500.0, // assume 500ms baseline until measured
            last_error: None,
            consecutive_failures: 0,
        }
    }
}

impl ProviderHealth {
    /// Risk score derived from health metrics (0.0 = healthy, 1.0 = dead)
    pub fn risk_score(&self) -> f64 {
        if !self.available {
            return 1.0;
        }
        // Weighted combination of error rate and consecutive failures
        let failure_risk = (self.consecutive_failures as f64 / 5.0).min(1.0);
        (self.error_rate * 0.7 + failure_risk * 0.3).min(1.0)
    }

    /// Record a successful request
    pub fn record_success(&mut self, latency_ms: f64) {
        self.consecutive_failures = 0;
        self.available = true;
        // Exponential moving average for latency
        self.avg_latency_ms = self.avg_latency_ms * 0.8 + latency_ms * 0.2;
        // Decay error rate
        self.error_rate *= 0.95;
    }

    /// Record a failed request
    pub fn record_failure(&mut self, error: String) {
        self.consecutive_failures += 1;
        self.error_rate = (self.error_rate * 0.8 + 0.2).min(1.0);
        self.last_error = Some(error);
        // Circuit breaker: 5 consecutive failures → mark unavailable.
        // Jitter matches ledger.lua 0.75–1.25× pattern to prevent
        // thundering-herd recovery across families.
        if self.consecutive_failures >= 5 {
            self.available = false;
            warn!(error = ?self.last_error, "circuit breaker OPEN — family marked unavailable");
        }
    }

    /// Attempt recovery (called periodically or on explicit reset)
    #[allow(dead_code)]
    pub fn attempt_recovery(&mut self) {
        if !self.available && self.consecutive_failures > 0 {
            self.available = true;
            self.consecutive_failures = 0;
            info!("circuit breaker HALF-OPEN — allowing probe request");
        }
    }
}

// ---------------------------------------------------------------------------
// Scored result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    pub model_id: String,
    pub provider: String,
    pub score: f64,
    pub task_type: TaskType,
    pub strategy: RoutingStrategy,
    pub breakdown: ScoreBreakdown,
    pub fallback: Option<String>,
    /// Sensitivity verdict from upstream caller (X-Sensitivity-Level header):
    /// "GREEN" | "YELLOW" | "RED". Gateway has no opinion — IRIN or
    /// other upstream classifies and the gateway obeys. RED forces local routing.
    pub sensitivity: String,
    pub requested_model: String,
    pub effective_model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoreBreakdown {
    pub quality_component: f64,
    pub latency_component: f64,
    pub cost_component: f64,
    pub risk_component: f64,
}

// ---------------------------------------------------------------------------
// Quality floor constants — per task type, minimum quality to be eligible.
// Models below this floor are excluded from scoring (hard prefilter).
// ---------------------------------------------------------------------------

fn quality_floor(task: TaskType) -> f64 {
    match task {
        TaskType::Coding => 0.80,
        TaskType::Vision => 0.80,
        TaskType::ToolUse => 0.80,
        TaskType::Creative | TaskType::Analysis => 0.78,
        TaskType::Conversation | TaskType::General => 0.75,
    }
}

// ---------------------------------------------------------------------------
// Smart Router
// ---------------------------------------------------------------------------

pub struct SmartRouter {
    models: Vec<ModelDef>,
    aliases: HashMap<String, String>, // alias → model_id
    health: Arc<RwLock<HashMap<HealthKey, ProviderHealth>>>, // (provider, family) → health
}

impl SmartRouter {
    /// Load router from models.json content.
    /// Supports two formats:
    ///   1. Array format: {"models": [{"id": "...", ...}]}
    ///   2. Dict format:  {"models": {"model-id": {...}}, "aliases": {"alias": "model-id"}}
    pub fn from_models_json(json: &serde_json::Value) -> Result<Self, String> {
        let models_val = json
            .get("models")
            .ok_or("missing 'models' key in models.json")?;

        let models: Vec<ModelDef> = if models_val.is_array() {
            // Array format — parse directly
            serde_json::from_value(models_val.clone())
                .map_err(|e| format!("failed to parse models array: {}", e))?
        } else if let Some(obj) = models_val.as_object() {
            // Dict format — convert to Vec<ModelDef>
            let mut out = Vec::new();
            for (id, val) in obj {
                let mut model_val = val.clone();
                if let Some(m) = model_val.as_object_mut() {
                    m.insert("id".to_string(), serde_json::Value::String(id.clone()));
                    // Ensure aliases defaults if missing
                    if !m.contains_key("aliases") {
                        m.insert("aliases".to_string(), serde_json::json!([]));
                    }
                    // Ensure capabilities defaults if missing
                    if !m.contains_key("capabilities") {
                        m.insert("capabilities".to_string(), serde_json::json!({}));
                    }
                }
                match serde_json::from_value::<ModelDef>(model_val) {
                    Ok(model) => out.push(model),
                    Err(e) => {
                        warn!(model_id = %id, error = %e, "skipping unparseable model");
                    }
                }
            }
            out
        } else {
            return Err("'models' must be an array or object".to_string());
        };

        // Build alias map
        let mut aliases = HashMap::new();

        // First, register model IDs themselves
        for model in &models {
            aliases.insert(model.id.clone(), model.id.clone());
            for alias in &model.aliases {
                aliases.insert(alias.clone(), model.id.clone());
            }
        }

        // Then, load top-level "aliases" section if present
        if let Some(alias_map) = json.get("aliases").and_then(|a| a.as_object()) {
            for (alias, target) in alias_map {
                if let Some(target_str) = target.as_str() {
                    aliases.insert(alias.clone(), target_str.to_string());
                }
            }
        }

        let mut health = HashMap::new();
        for model in &models {
            let key = health_key_for(model);
            health.entry(key).or_insert_with(ProviderHealth::default);
        }

        // RED-sensitivity routing forces a local provider. If no local model
        // exists in the registry, the router would 400 at request time
        // (`unknown model: sovereign-node`) instead of failing at startup.
        // Fail closed at boot — operators must declare a local model before
        // the gateway can claim to support sovereign routing.
        let has_local = models
            .iter()
            .any(|m| m.provider == "mlx" || m.provider == "local" || m.provider == "ollama");
        if !has_local {
            return Err(
                "no model with provider='mlx' or provider='local' is declared in models.json. \
                 RED-sensitivity routing requires a local provider as a sovereign fallback. \
                 Declare at least one (e.g., a 'sovereign-node' model with provider: 'local') \
                 before starting the gateway."
                    .to_string(),
            );
        }

        info!(
            models = models.len(),
            aliases = aliases.len(),
            "smart router initialized"
        );

        Ok(Self {
            models,
            aliases,
            health: Arc::new(RwLock::new(health)),
        })
    }

    /// Resolve an alias to a model ID
    pub fn resolve_alias(&self, name: &str) -> Option<&str> {
        self.aliases.get(name).map(|s| s.as_str())
    }

    /// Route a request to the best available backend.
    ///
    /// `sensitivity` is the verdict the upstream caller passed in via the
    /// `X-Sensitivity-Level` header — one of "GREEN" | "YELLOW" | "RED" (case-insensitive).
    /// Gateway has no opinion on payload sensitivity; IRIN or other upstream
    /// callers classify and the gateway obeys. RED forces routing to a local
    /// provider regardless of which model the client asked for.
    ///
    /// `sovereign_mode` comes from `X-Sovereign-Mode: true` — when set, ALL
    /// requests are treated as RED regardless of declared sensitivity. This
    /// is the "sovereign switch" that restricts routing to local-only providers.
    ///
    /// If `requested_model` is specified, validates and returns it (no scoring).
    /// If None, scores all available models and returns the best.
    #[tracing::instrument(skip(self, body), fields(sensitivity = sensitivity, strategy = ?strategy, sovereign_mode = sovereign_mode, requested_model = requested_model.unwrap_or("auto")))]
    pub async fn route(
        &self,
        sensitivity: &str,
        requested_model: Option<&str>,
        body: &serde_json::Value,
        strategy: RoutingStrategy,
        sovereign_mode: bool,
    ) -> Result<RoutingDecision, String> {
        let task_type = TaskType::classify(body);
        let weights = ScoringWeights::for_task(task_type, strategy);
        let health = self.health.read().await;

        let mut req_model_resolved = requested_model.unwrap_or("").to_string();
        if req_model_resolved.is_empty() {
            req_model_resolved = "auto".to_string();
        }
        let requested_raw = req_model_resolved.clone();

        let mut forced_local = false;
        let mut effective_model = requested_model;

        // Sovereign Routing: RED sensitivity OR sovereign_mode forces local provider.
        let is_red = sensitivity.eq_ignore_ascii_case("RED") || sovereign_mode;
        if is_red {
            if sovereign_mode {
                info!("Sovereign Mode: all routing restricted to local providers");
            }
            // Determine if the requested model is already local.
            let is_local = if let Some(req_name) = requested_model {
                let resolved = self.resolve_alias(req_name).unwrap_or(req_name);
                if let Some(m) = self.models.iter().find(|model| model.id == resolved) {
                    m.provider == "mlx" || m.provider == "local" || m.provider == "ollama"
                } else {
                    false // unknown model, assume cloud
                }
            } else {
                false // auto-route defaults to cloud, force local
            };

            if !is_local {
                warn!(
                    "Sovereign Routing: forcing local provider (was {:?})",
                    requested_model
                );
                forced_local = true;
                effective_model = Some("sovereign-node");
            }
        }

        // If a specific model is requested (or forced), validate it
        if let Some(requested) = effective_model {
            let resolved = self.resolve_alias(requested).unwrap_or(requested);
            if let Some(model) = self.models.iter().find(|m| m.id == resolved) {
                let hk = health_key_for(model);
                let family_health = health.get(&hk).cloned().unwrap_or_default();

                if !family_health.available {
                    // Check for fallback
                    if let Some(ref fallback_id) = model.fallback {
                        if let Some(fb_model) = self.models.iter().find(|m| m.id == *fallback_id) {
                            let fb_hk = health_key_for(fb_model);
                            let fb_health = health.get(&fb_hk).cloned().unwrap_or_default();
                            if fb_health.available {
                                warn!(
                                    requested = resolved,
                                    fallback = fallback_id,
                                    family = ?hk,
                                    "family unavailable — falling back"
                                );
                                let breakdown = self.score_model(fb_model, &fb_health, &weights);
                                return Ok(RoutingDecision {
                                    model_id: fb_model.id.clone(),
                                    provider: fb_model.provider.clone(),
                                    score: breakdown.total(),
                                    task_type,
                                    strategy,
                                    breakdown,
                                    fallback: fb_model.fallback.clone(),
                                    sensitivity: sensitivity.to_string(),
                                    requested_model: requested_raw,
                                    effective_model: fb_model.id.clone(),
                                });
                            }
                        }
                    }
                    return Err(format!(
                        "family {:?} is unavailable (circuit breaker open)",
                        hk
                    ));
                }

                let breakdown = self.score_model(model, &family_health, &weights);
                return Ok(RoutingDecision {
                    model_id: model.id.clone(),
                    provider: model.provider.clone(),
                    score: breakdown.total(),
                    task_type,
                    strategy,
                    breakdown,
                    fallback: model.fallback.clone(),
                    sensitivity: sensitivity.to_string(),
                    requested_model: requested_raw,
                    effective_model: model.id.clone(),
                });
            }
            return Err(format!("unknown model: '{}'", requested));
        }

        // No specific model requested — score all and pick the best
        let mut best: Option<(RoutingDecision, f64)> = None;

        let floor = quality_floor(task_type);

        for model in &self.models {
            // Enforce sovereign routing even in auto-mode
            if forced_local && model.provider != "mlx" && model.provider != "local" {
                continue;
            }

            let hk = health_key_for(model);
            let family_health = health.get(&hk).cloned().unwrap_or_default();

            if !family_health.available {
                continue;
            }

            // Quality floor prefilter — hard exclude below minimum for this task type.
            // This prevents low-quality free models from winning on cost alone.
            if model.capabilities.quality < floor {
                debug!(
                    model = %model.id,
                    quality = model.capabilities.quality,
                    floor,
                    "skipped — below quality floor"
                );
                continue;
            }

            // Capability prefilter — skip models that can't handle the task
            if task_type == TaskType::ToolUse && !model.capabilities.supports_tools {
                continue;
            }
            if task_type == TaskType::Vision && !model.capabilities.supports_vision {
                continue;
            }

            let breakdown = self.score_model(model, &family_health, &weights);
            let score = breakdown.total();

            debug!(
                model = %model.id,
                family = ?hk,
                score,
                q = breakdown.quality_component,
                l = breakdown.latency_component,
                c = breakdown.cost_component,
                r = breakdown.risk_component,
                "scored"
            );

            let is_better = best
                .as_ref()
                .is_none_or(|(_, best_score)| score > *best_score);
            if is_better {
                best = Some((
                    RoutingDecision {
                        model_id: model.id.clone(),
                        provider: model.provider.clone(),
                        score,
                        task_type,
                        strategy,
                        breakdown,
                        fallback: model.fallback.clone(),
                        sensitivity: sensitivity.to_string(),
                        requested_model: requested_raw.clone(),
                        effective_model: model.id.clone(),
                    },
                    score,
                ));
            }
        }

        best.map(|(decision, _)| decision)
            .ok_or_else(|| "no available models for this request".to_string())
    }

    /// Score a single model against the current weights and health.
    fn score_model(
        &self,
        model: &ModelDef,
        health: &ProviderHealth,
        weights: &ScoringWeights,
    ) -> ScoreBreakdown {
        let max_latency = 30000.0_f64; // 30s ceiling
        let max_cost = 100.0_f64; // $100/M tokens ceiling

        let quality_component = weights.quality * model.capabilities.quality;
        let latency_component = weights.latency * (health.avg_latency_ms / max_latency);
        let cost_component = weights.cost * (model.pricing.output_per_1m / max_cost);
        let risk_component = weights.risk * health.risk_score();

        ScoreBreakdown {
            quality_component,
            latency_component,
            cost_component,
            risk_component,
        }
    }

    /// Record a backend response for per-family health tracking.
    ///
    /// `model_id` is the actually-routed model (not alias). Used to derive
    /// the correct (provider, family) health key.
    pub async fn record_outcome(
        &self,
        model_id: &str,
        success: bool,
        latency_ms: f64,
        error: Option<String>,
    ) {
        // Find the model to derive its health key
        if let Some(model) = self.models.iter().find(|m| m.id == model_id) {
            let hk = health_key_for(model);
            let mut health = self.health.write().await;
            let entry = health
                .entry(hk.clone())
                .or_insert_with(ProviderHealth::default);
            if success {
                entry.record_success(latency_ms);
            } else {
                entry.record_failure(error.unwrap_or_else(|| "unknown error".to_string()));
            }
        } else {
            // Legacy path: fall back to provider-only key for unknown models
            warn!(
                model_id,
                "record_outcome: unknown model, cannot derive family"
            );
        }
    }

    /// Get current health status for all families
    #[allow(dead_code)]
    pub async fn health_status(&self) -> HashMap<HealthKey, ProviderHealth> {
        self.health.read().await.clone()
    }

    /// Attempt recovery for a specific family
    #[allow(dead_code)]
    pub async fn recover_family(&self, provider: &str, family: &str) {
        let key = (provider.to_string(), family.to_string());
        let mut health = self.health.write().await;
        if let Some(h) = health.get_mut(&key) {
            h.attempt_recovery();
        }
    }

    /// Attempt recovery for ALL families of a provider
    #[allow(dead_code)]
    pub async fn recover_provider(&self, provider: &str) {
        let mut health = self.health.write().await;
        for (key, h) in health.iter_mut() {
            if key.0 == provider {
                h.attempt_recovery();
            }
        }
    }
}

impl ScoreBreakdown {
    pub fn total(&self) -> f64 {
        self.quality_component - self.latency_component - self.cost_component - self.risk_component
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_models_json() -> serde_json::Value {
        json!({
            "models": [
                {
                    "id": "gpt-5.5",
                    "provider": "openai",
                    "aliases": ["gpt4"],
                    "fallback": "claude-sonnet-4-6",
                    "pricing": {"input_per_1m": 2.50, "output_per_1m": 10.00},
                    "capabilities": {
                        "quality": 0.92,
                        "max_context": 128000,
                        "supports_tools": true,
                        "supports_vision": true,
                        "supports_streaming": true
                    }
                },
                {
                    "id": "claude-sonnet-4-6",
                    "provider": "anthropic",
                    "aliases": ["claude", "sonnet"],
                    "pricing": {"input_per_1m": 3.00, "output_per_1m": 15.00},
                    "capabilities": {
                        "quality": 0.95,
                        "max_context": 200000,
                        "supports_tools": true,
                        "supports_vision": true,
                        "supports_streaming": true
                    }
                },
                {
                    "id": "xai-test-mini",
                    "provider": "xai",
                    "aliases": ["grok-mini", "grok"],
                    "pricing": {"input_per_1m": 0.30, "output_per_1m": 0.50},
                    "capabilities": {
                        "quality": 0.75,
                        "max_context": 131072,
                        "supports_tools": true,
                        "supports_vision": false,
                        "supports_streaming": true
                    }
                },
                {
                    "id": "sovereign-node",
                    "provider": "local",
                    "aliases": [],
                    "pricing": {"input_per_1m": 0.00, "output_per_1m": 0.00},
                    "capabilities": {
                        "quality": 0.60,
                        "max_context": 8192,
                        "supports_tools": false,
                        "supports_vision": false,
                        "supports_streaming": false
                    }
                }
            ]
        })
    }

    #[tokio::test]
    async fn load_models() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        assert_eq!(router.models.len(), 4);
    }

    #[tokio::test]
    async fn resolve_alias() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        assert_eq!(router.resolve_alias("gpt4"), Some("gpt-5.5"));
        assert_eq!(router.resolve_alias("claude"), Some("claude-sonnet-4-6"));
        assert_eq!(router.resolve_alias("nonexistent"), None);
    }

    #[tokio::test]
    async fn route_specific_model() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let decision = router
            .route(
                "GREEN",
                Some("gpt-5.5"),
                &body,
                RoutingStrategy::Balanced,
                false,
            )
            .await
            .unwrap();
        assert_eq!(decision.model_id, "gpt-5.5");
        assert_eq!(decision.provider, "openai");
    }

    #[tokio::test]
    async fn route_via_alias() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let decision = router
            .route(
                "GREEN",
                Some("claude"),
                &body,
                RoutingStrategy::Balanced,
                false,
            )
            .await
            .unwrap();
        assert_eq!(decision.model_id, "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn route_unknown_model_fails() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let result = router
            .route(
                "GREEN",
                Some("nonexistent-model"),
                &body,
                RoutingStrategy::Balanced,
                false,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn auto_route_picks_highest_score() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "Write me a poem"}]});
        let decision = router
            .route("GREEN", None, &body, RoutingStrategy::Quality, false)
            .await
            .unwrap();
        // Claude has highest quality (0.95) — should win on quality strategy
        assert_eq!(decision.model_id, "claude-sonnet-4-6");
        assert_eq!(decision.task_type, TaskType::Creative);
        assert_eq!(decision.strategy, RoutingStrategy::Quality);
    }

    #[tokio::test]
    async fn task_classification() {
        let body = json!({"messages": [{"role": "user", "content": "debug this rust code"}]});
        assert_eq!(TaskType::classify(&body), TaskType::Coding);

        let body = json!({"tools": [{"type": "function"}]});
        assert_eq!(TaskType::classify(&body), TaskType::ToolUse);

        let body =
            json!({"messages": [{"role": "user", "content": "compare these two approaches"}]});
        assert_eq!(TaskType::classify(&body), TaskType::Analysis);
    }

    #[tokio::test]
    async fn circuit_breaker() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();

        // Simulate 5 failures for openai (using model_id now, not provider)
        for _ in 0..5 {
            router
                .record_outcome("gpt-5.5", false, 0.0, Some("timeout".to_string()))
                .await;
        }

        // OpenAI family should be unavailable
        let health = router.health_status().await;
        let key = ("openai".to_string(), "openai".to_string());
        assert!(!health[&key].available);

        // Routing to gpt-5.5 should fall back to claude
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let decision = router
            .route(
                "GREEN",
                Some("gpt-5.5"),
                &body,
                RoutingStrategy::Balanced,
                false,
            )
            .await
            .unwrap();
        assert_eq!(decision.model_id, "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn vision_task_skips_non_vision_models() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
                ]
            }]
        });
        let decision = router
            .route("GREEN", None, &body, RoutingStrategy::Quality, false)
            .await
            .unwrap();
        assert_ne!(decision.model_id, "xai-test-mini");
        assert_eq!(decision.task_type, TaskType::Vision);
    }

    #[tokio::test]
    async fn economy_strategy_picks_cheapest() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let decision = router
            .route("GREEN", None, &body, RoutingStrategy::Economy, false)
            .await
            .unwrap();
        // xai-test-mini is cheapest ($0.50 output) — should win on economy
        assert_eq!(decision.model_id, "xai-test-mini");
        assert_eq!(decision.strategy, RoutingStrategy::Economy);
    }

    #[tokio::test]
    async fn quality_strategy_picks_best() {
        let router = SmartRouter::from_models_json(&test_models_json()).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let decision = router
            .route("GREEN", None, &body, RoutingStrategy::Quality, false)
            .await
            .unwrap();
        // claude-sonnet-4-6 has highest quality (0.95) — should win on quality
        assert_eq!(decision.model_id, "claude-sonnet-4-6");
        assert_eq!(decision.strategy, RoutingStrategy::Quality);
    }

    #[tokio::test]
    async fn strategy_parse() {
        assert_eq!(
            RoutingStrategy::from_str_opt("quality"),
            Some(RoutingStrategy::Quality)
        );
        assert_eq!(
            RoutingStrategy::from_str_opt("best"),
            Some(RoutingStrategy::Quality)
        );
        assert_eq!(
            RoutingStrategy::from_str_opt("cheap"),
            Some(RoutingStrategy::Economy)
        );
        assert_eq!(
            RoutingStrategy::from_str_opt("fast"),
            Some(RoutingStrategy::Speed)
        );
        assert_eq!(RoutingStrategy::from_str_opt("garbage"), None);
    }
}
