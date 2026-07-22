use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
pub use sovereign_protocol::types::{
    CapabilityToken, GatewayProvenance, ProblemDetails, ProviderProvenance, ProviderResponse,
    SeatResponse, WorkerProvenanceGuard,
};

/// Matches schemas/judge.v2.schema.json from the Python council.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeAssessment {
    pub convergence: f64,
    #[serde(default = "default_true")]
    pub intent_aligned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_flag: Option<String>,
    /// Homogeneity / quick-agreement metric for diversity countermeasures (Bet 1).
    /// 0.0 = high lexical/semantic diversity across seat responses.
    /// 1.0 = high homogenization (seats converged on near-identical phrasing/ideas).
    /// Computed from valid seat texts (avg pairwise Jaccard on word sets). Extended
    /// JudgeAssessment + threshold paths. Pairs with Sheldon external grounding.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub homogeneity_score: Option<f64>,
    /// Quick-agreement flag: true when high homogeneity detected (typically >=0.75).
    /// Forces 'continue' / raises effective threshold to break attractor states
    /// even on high raw convergence_score. Signals potential model slop risk.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub quick_agreement: Option<bool>,
    pub recommendation: String,
    #[serde(default = "default_one")]
    pub confidence: f64,
}

fn default_true() -> bool {
    true
}
fn default_one() -> f64 {
    1.0
}

// ---------------------------------------------------------------------------
// Sheldon Validator (v9.13)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ClaimVerdict {
    Supported,
    Consistent,
    #[default]
    NoEvidence,
    Contradicted,
}

impl serde::Serialize for ClaimVerdict {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            Self::Supported => "SUPPORTED",
            Self::Consistent => "CONSISTENT",
            Self::NoEvidence => "NO_EVIDENCE",
            Self::Contradicted => "CONTRADICTED",
        })
    }
}

impl<'de> serde::Deserialize<'de> for ClaimVerdict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.to_uppercase().as_str() {
            "SUPPORTED" | "VERIFIED" => Self::Supported,
            "CONSISTENT" | "PLAUSIBLE" => Self::Consistent,
            "NO_EVIDENCE" | "UNVERIFIED" => Self::NoEvidence,
            "CONTRADICTED" => Self::Contradicted,
            _ => Self::NoEvidence,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClaimImpact {
    High,
    Medium,
    Low,
    #[serde(other)]
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimVerdictEntry {
    pub claim: String,
    pub seat: String,
    pub verdict: ClaimVerdict,
    #[serde(default)]
    pub evidence_citations: Vec<String>,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub impact: ClaimImpact,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub _overridden_from: Option<String>,
}

/// Results from one round of deliberation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundResult {
    pub round_num: u32,
    pub responses: Vec<SeatResponse>,
    pub convergence_score: f64,
    pub converged: bool,
    #[serde(default)]
    pub judge_provider: Option<String>,
    #[serde(default)]
    pub judge_assessment: Option<JudgeAssessment>,
    /// Gateway-owned ledger correlation handles for every provider candidate
    /// attempted by the convergence-judge cascade in this round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judge_gateway_attempts: Vec<GatewayProvenance>,
    #[serde(default)]
    pub flip_flop_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub validation_report: Option<Vec<ClaimVerdictEntry>>,
}

/// Budget tracking for cost-pause (v9.12.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetRecord {
    pub max_usd: f64,
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_taken: Option<String>,
}

/// Tag identifying where a session was initiated (§4.4).
///
/// `Cli` — local CLI run via `./council "Topic"` (default for legacy sessions).
/// `Warroom` — interactive warroom WebSocket session.
/// `Api` — synchronous `/api/deliberate` request (Phase 0.5 council endpoint).
/// `ApiCancelled` — partial-result diagnostic from a cancelled API session
///                  (§4.5, §12.5). NEVER returned in the API response; private
///                  session file only. Excluded from precedent by default.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    #[default]
    Cli,
    Warroom,
    Api,
    ApiCancelled,
    #[serde(other)]
    Unknown,
}

/// Effective transport for a persisted proceeding. `Unknown` is reserved for
/// legacy sessions written before the route became part of the record.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRoute {
    Direct,
    Governed,
    #[serde(other)]
    #[default]
    Unknown,
}

/// Full council session — the complete transcript.
/// Defaults on numeric fields keep older Python sessions readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilSession {
    pub session_id: String,
    pub topic: String,
    pub cabinet_name: String,
    pub rounds: Vec<RoundResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthesis_model: Option<String>,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub total_latency_ms: u64,
    #[serde(default)]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub specops_triggered: bool,
    #[serde(default)]
    pub specops_cost_usd: f64,
    #[serde(
        default = "default_legacy_session_mode",
        deserialize_with = "deserialize_session_mode_lenient"
    )]
    pub mode: SessionMode,
    #[serde(default)]
    pub precedent_ids: Vec<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub budget: Option<BudgetRecord>,
    #[serde(default)]
    pub context_sources: Vec<String>,

    // Phase 0.5 (§4.4): provenance tag for filtering precedent + audit trails.
    // 260+ legacy sessions deserialize as SessionOrigin::Cli via #[serde(default)].
    #[serde(default)]
    pub origin: SessionOrigin,

    /// Effective, backend-enforced transport. Governed means every model leg
    /// was required to pass through Gateway; failures do not fall back direct.
    #[serde(default)]
    pub execution_route: ExecutionRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_sensitivity: Option<String>,

    // Phase 0.5 P0 #1 (§4.7): chair token + cost plumbed through synthesize().
    // Pre-v2.2 sessions deserialize as 0/0/0.0.
    #[serde(default)]
    pub chair_tokens_in: u32,
    #[serde(default)]
    pub chair_tokens_out: u32,
    #[serde(default)]
    pub chair_cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chair_provider_provenance: Option<ProviderProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chair_gateway_provenance: Option<GatewayProvenance>,

    // Phase 5 provenance: track the originating gateway request / escalation ID
    // that triggered this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<String>,

    // Phase 6 (v0.3): Worker execution provenance and metrics to correlate
    // War Room UI with outbox task outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_provenance: Option<WorkerProvenanceGuard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_metrics: Option<serde_json::Value>,
}

fn default_schema_version() -> u32 {
    2
}
fn default_tier() -> String {
    "best".to_string()
}

/// Tolerate `null` and non-string values for `mode`. Old Python sessions used
/// `"normal"` and a couple have `null` — both should round-trip without
/// dropping the session from the catalog. Sessions predating the mode flag
/// are Python-era "normal" deliberations, so `null` / unparseable values map
/// to SessionMode::Normal (matching the sessions_list default). Unknown
/// strings still land on SessionMode::Unknown via `#[serde(other)]`.
fn deserialize_session_mode_lenient<'de, D>(d: D) -> Result<SessionMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let opt: Option<SessionMode> = Option::deserialize(d).unwrap_or(None);
    Ok(opt.unwrap_or(SessionMode::Normal))
}

/// Absent-key default for `CouncilSession::mode` — legacy session files
/// without the key are Python-era "normal" sessions, NOT TearDown. Rust-era
/// writers always serialize the mode key, so this only fires for legacy JSON.
fn default_legacy_session_mode() -> SessionMode {
    SessionMode::Normal
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// Legacy Python-era sessions (no explicit mode flag). Also the lenient
    /// deserialization fallback for absent / null `mode` keys.
    Normal,
    /// Default for NEW sessions constructed in Rust (`SessionMode::default()`).
    #[default]
    TearDown,
    Pathfind,
    /// Stress + paired-replacement constraint. See Mode::Harden in mode.rs.
    Harden,
    Blind,
    Recall,
    Wargame,
    Premortem,
    /// Direct-fire one-shots persisted by the WS path (feature contract). The CLI
    /// direct-fire handlers print-and-exit without saving; only warroom
    /// sessions carry these tags. Slugs match `engine::direct_fire`.
    Contrarian,
    Munger,
    Kiss,
    Specops,
    /// Catch-all for unknown / null modes — keeps old session JSON parseable.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Cabinet Configuration (loaded from YAML)
// ---------------------------------------------------------------------------

/// A deliberation seat — provider + model + system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seat {
    pub name: String,
    pub provider: String,
    pub model: String,
    /// System prompt template name (resolved via Tera)
    /// OR inline system prompt text.
    pub system: String,
}

/// Chair configuration — synthesizes the final ruling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chair {
    pub name: String,
    pub provider: String,
    pub model: String,
    /// Optional inline system prompt for specialized chair behavior.
    #[serde(default)]
    pub system: Option<String>,
    /// Thinking effort for adaptive thinking providers (claude, gpt).
    /// Values: "low", "medium", "high", "max"
    #[serde(default)]
    pub thinking_effort: Option<String>,
}

/// Controls the Chair synthesis output contract.
/// "generic" (default): the standard numbered 1-7 synthesis scaffold used by
/// warroom/standard/etc.
/// "directive_proposal_v1": strict machine-output mode for council-triage.
///   The Chair must emit exactly one ```json ... irin.directive.proposal.v1
///   fence and nothing else. The generic scaffold is suppressed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisMode {
    #[default]
    Generic,
    DirectiveProposalV1,
}

/// A cabinet — the complete deliberation configuration.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cabinet {
    #[serde(default)]
    pub hash: String,
    pub name: String,

    #[serde(default)]
    pub description: String,
    pub rounds: u32,
    pub seats: Vec<Seat>,
    pub chair: Chair,
    /// When true, this cabinet is restricted to local CLI-agent providers and
    /// skips global provider preflights such as frame-check.
    #[serde(default)]
    pub local_code_only: bool,
    /// Machine-output contract for the Chair synthesis (Phase 3 council-triage).
    /// When set to DirectiveProposalV1, the generic "Structure your synthesis 1..7"
    /// scaffold is omitted and a strict fence-only prompt is used instead.
    #[serde(default)]
    pub synthesis_mode: SynthesisMode,
}

// ---------------------------------------------------------------------------
// Utility roles (convergence judge, frame check) — loaded from roles.yaml
// ---------------------------------------------------------------------------

/// One step in a role cascade (provider + model + token budget).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleCascadeStep {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_role_max_tokens")]
    pub max_tokens: u32,
}

fn default_role_max_tokens() -> u32 {
    512
}

/// A named utility role with an ordered provider/model cascade.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleDefinition {
    #[serde(default)]
    pub description: String,
    pub cascade: Vec<RoleCascadeStep>,
}

/// Top-level roles.yaml shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolesFile {
    pub roles: std::collections::HashMap<String, RoleDefinition>,
}

/// Resolved utility roles used at runtime.
#[derive(Debug, Clone)]
pub struct RolesConfig {
    pub convergence_judge: RoleDefinition,
    pub frame_check: RoleDefinition,
    pub claim_validator: RoleDefinition,
    pub scope_auditor: RoleDefinition,
}

impl RolesConfig {
    /// Built-in defaults when roles.yaml is absent (matches shipped roles.yaml).
    pub fn built_in_defaults() -> Self {
        Self {
            convergence_judge: RoleDefinition {
                description: "Scores seat agreement each round (judge.v2 JSON)".into(),
                cascade: vec![
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.20-0309-reasoning".into(),
                        max_tokens: 1024,
                    },
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.3".into(),
                        max_tokens: 512,
                    },
                    RoleCascadeStep {
                        provider: "nvidia".into(),
                        model: "mistralai/mistral-large-3-675b-instruct-2512".into(),
                        max_tokens: 512,
                    },
                ],
            },
            frame_check: RoleDefinition {
                description: "R1 embedded-assumption scanner (anti-prompt-poisoning)".into(),
                cascade: vec![
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.3".into(),
                        max_tokens: 512,
                    },
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.20-0309-reasoning".into(),
                        max_tokens: 512,
                    },
                    RoleCascadeStep {
                        provider: "nvidia".into(),
                        model: "mistralai/mistral-large-3-675b-instruct-2512".into(),
                        max_tokens: 512,
                    },
                    RoleCascadeStep {
                        provider: "gemini_agy".into(),
                        model: "gemini-3.5-flash".into(),
                        max_tokens: 512,
                    },
                ],
            },
            claim_validator: RoleDefinition {
                description: "Claim verification (Sheldon) between rounds".into(),
                cascade: vec![
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.20-0309-reasoning".into(),
                        max_tokens: 1024,
                    },
                    RoleCascadeStep {
                        provider: "grok_build".into(),
                        model: "grok-cli-default".into(),
                        max_tokens: 1024,
                    },
                    RoleCascadeStep {
                        provider: "claude_code".into(),
                        model: "claude-opus-4-6".into(),
                        max_tokens: 1024,
                    },
                    RoleCascadeStep {
                        provider: "codex_cli".into(),
                        model: "gpt-5.6-sol".into(),
                        max_tokens: 1024,
                    },
                ],
            },
            scope_auditor: RoleDefinition {
                description: "Scoped investigator for steering detection and boundary review (anti-framing, scope discipline)".into(),
                cascade: vec![
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.3".into(),
                        max_tokens: 1024,
                    },
                    RoleCascadeStep {
                        provider: "grok_hermes".into(),
                        model: "grok-4.20-0309-reasoning".into(),
                        max_tokens: 1024,
                    },
                    RoleCascadeStep {
                        provider: "nvidia".into(),
                        model: "mistralai/mistral-large-3-675b-instruct-2512".into(),
                        max_tokens: 1024,
                    },
                ],
            },
        }
    }

    /// Normalize legacy `nim` slug → `nvidia` on every cascade step.
    pub fn normalize_provider_slugs(&mut self) {
        for role in [
            &mut self.convergence_judge,
            &mut self.frame_check,
            &mut self.claim_validator,
            &mut self.scope_auditor,
        ] {
            for step in &mut role.cascade {
                if step.provider == "nim" {
                    step.provider = "nvidia".into();
                }
            }
        }
    }

    /// All model IDs referenced by utility roles (for vault validation).
    pub fn all_model_ids(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for role in [
            &self.convergence_judge,
            &self.frame_check,
            &self.claim_validator,
            &self.scope_auditor,
        ] {
            for step in &role.cascade {
                out.push(step.model.as_str());
            }
        }
        out
    }
}

impl Default for RolesConfig {
    fn default() -> Self {
        Self::built_in_defaults()
    }
}

#[cfg(test)]
mod roles_config_tests {
    use super::RolesConfig;

    #[test]
    fn built_in_judge_and_validator_keep_calibrated_grok_pins() {
        let roles = RolesConfig::built_in_defaults();

        let judge = &roles.convergence_judge.cascade[0];
        assert_eq!(judge.provider, "grok_hermes");
        assert_eq!(judge.model, "grok-4.20-0309-reasoning");

        let validator = &roles.claim_validator.cascade[0];
        assert_eq!(validator.provider, "grok_hermes");
        assert_eq!(validator.model, "grok-4.20-0309-reasoning");

        let no_evidence_fallback = &roles.claim_validator.cascade[1];
        assert_eq!(no_evidence_fallback.provider, "grok_build");
        assert_eq!(no_evidence_fallback.model, "grok-cli-default");
    }
}

// ---------------------------------------------------------------------------
// Model Registry & Pricing (loaded from YAML)
// ---------------------------------------------------------------------------

/// Model pricing per MTok (million tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

/// Model registry entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    #[serde(default)]
    pub description: String,
    pub pricing: ModelPricing,
}

/// Full model registry — loaded from models.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistry {
    pub models: std::collections::HashMap<String, ModelEntry>,
}

impl ModelRegistry {
    /// Resolve a runtime model string to a registry entry (exact, then longest-prefix).
    pub fn lookup_entry(&self, model: &str) -> Option<&ModelEntry> {
        for entry in self.models.values() {
            if entry.id == model {
                return Some(entry);
            }
        }
        let mut best_entry: Option<&ModelEntry> = None;
        let mut best_len = 0;
        for entry in self.models.values() {
            if model.starts_with(&entry.id) && entry.id.len() > best_len {
                best_entry = Some(entry);
                best_len = entry.id.len();
            }
        }
        if best_entry.is_none() {
            for entry in self.models.values() {
                if entry.id.starts_with(model) && entry.id.len() > best_len {
                    best_entry = Some(entry);
                    best_len = entry.id.len();
                }
            }
        }
        best_entry
    }

    /// Provider slug for a runtime model ID (from models.yaml `provider` field).
    pub fn provider_for_model(&self, model: &str) -> Option<String> {
        self.lookup_entry(model).map(|e| e.provider.clone())
    }

    /// Estimate cost in USD from token counts.
    /// Matches on model entry `id` fields (what the API returns), not registry keys.
    /// Uses exact match first, then longest-prefix match.
    pub fn estimate_cost(
        &self,
        model: &str,
        tokens_in: u32,
        tokens_out: u32,
        cached_in: u32,
    ) -> f64 {
        if let Some(entry) = self.lookup_entry(model) {
            Self::calc_cost(&entry.pricing, tokens_in, tokens_out, cached_in)
        } else {
            if tokens_in > 0 || tokens_out > 0 {
                eprintln!(
                    "WARNING: no pricing entry for model '{}' (exact or prefix match); returning $0.00 cost. Register it in models.yaml to fix tracking.",
                    model
                );
            }
            0.0
        }
    }

    fn calc_cost(pricing: &ModelPricing, tokens_in: u32, tokens_out: u32, cached_in: u32) -> f64 {
        let uncached_in = tokens_in.saturating_sub(cached_in);
        let cost = (uncached_in as f64 * pricing.input
            + cached_in as f64 * pricing.cached_input
            + tokens_out as f64 * pricing.output)
            / 1_000_000.0;
        Self::char_floor_cost(cost) // P1-5/6: char-floor (4 decimal granularity) for cost
    }

    /// Char-floor cost to 4 decimal places (e.g. $0.0001 granularity) for billing consistency.
    pub fn char_floor_cost(cost: f64) -> f64 {
        (cost * 10_000.0).floor() / 10_000.0
    }
}

// ---------------------------------------------------------------------------
// Precedent Index Entry
// ---------------------------------------------------------------------------

/// One line in the precedent JSONL index (v2 schema).
/// Field names match Python's _build_index_entry output.
/// Aliases keep older Rust-era entries parseable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecedentEntry {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(rename = "id", alias = "session_id")]
    pub session_id: String,
    #[serde(rename = "ts", alias = "timestamp")]
    pub timestamp: String,
    pub topic: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(rename = "ruling_digest", alias = "digest")]
    pub digest: String,
    #[serde(default)]
    pub confidence: String,
    pub cabinet: String,
    #[serde(default)]
    pub convergence: f64,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub seat_count: usize,
    #[serde(default)]
    pub rounds: usize,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub synthesis_model: Option<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub judge_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failure_status: Option<String>,
    #[serde(default)]
    pub cited_by: Vec<String>,
    #[serde(default)]
    pub challenged_by: Vec<String>,
    // Phase 0.5 §4.4: provenance — defaults to Cli for the 260+ legacy entries.
    // Used by `precedent::retrieve(include_api=false)` to exclude Api / ApiCancelled
    // sessions from precedent injection unless the caller opts in.
    #[serde(default)]
    pub origin: SessionOrigin,
    #[serde(default)]
    pub execution_route: ExecutionRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_sensitivity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub worker_provenance: Option<WorkerProvenanceGuard>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_request_id: Option<String>,
}
