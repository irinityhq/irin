//! Sovereign Protocol Types
//!
//! The shared contract between Council, Gateway, and Librarian.
//! Every type derives Serialize/Deserialize for wire compatibility.
//! These types are the "sovereign-protocol crate" the Invariant called for.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Gateway Provenance (Phase 0: Council → Gateway)
// ---------------------------------------------------------------------------

/// Provenance data from Gateway routing. Captured from response headers
/// when Council routes calls through Gateway (--via-gateway).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayProvenance {
    #[serde(default)]
    pub routed_model: String,
    #[serde(default)]
    pub routed_provider: String,
    #[serde(default)]
    pub fallback_used: bool,
    #[serde(default)]
    pub gateway_request_id: String,
}

/// Status of the execution provenance on the worker leg.
/// Used to prevent fabricating provenance when only correlation exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerProvenanceStatus {
    OpaqueHandleOnly,
    VerifiedExact,
    Unavailable,
}

/// The fabrication guard containing the explicit status and safe handle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerProvenanceGuard {
    pub status: WorkerProvenanceStatus,
    /// Must explicitly acknowledge fabrication guard is active
    pub fabrication_guard: bool,
    /// The unverified/opaque correlation handle, if any
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opaque_handle: Option<String>,
}

impl WorkerProvenanceGuard {
    pub fn new_opaque(handle: Option<String>) -> Self {
        Self {
            status: WorkerProvenanceStatus::OpaqueHandleOnly,
            fabrication_guard: true,
            opaque_handle: handle,
        }
    }

    pub fn new_unavailable() -> Self {
        Self {
            status: WorkerProvenanceStatus::Unavailable,
            fabrication_guard: true,
            opaque_handle: None,
        }
    }
}

/// Provider/run provenance for a seat response. This is deliberately additive:
/// historical sessions omit it, while CLI-agent seats can distinguish
/// read-only local-code access and unavailable usage accounting from normal API
/// calls with reported token usage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderProvenance {
    #[serde(default)]
    pub runner: String,
    #[serde(default)]
    pub access_mode: String,
    #[serde(default)]
    pub accounting: String,
    #[serde(default)]
    pub filesystem: String,
}

impl ProviderProvenance {
    pub fn new(
        runner: impl Into<String>,
        access_mode: impl Into<String>,
        accounting: impl Into<String>,
        filesystem: impl Into<String>,
    ) -> Self {
        Self {
            runner: runner.into(),
            access_mode: access_mode.into(),
            accounting: accounting.into(),
            filesystem: filesystem.into(),
        }
    }

    pub fn api(runner: impl Into<String>) -> Self {
        Self::new(
            runner,
            "api_text_only",
            "reported_tokens_estimated_cost",
            "none",
        )
    }

    pub fn api_web(runner: impl Into<String>) -> Self {
        Self::new(
            runner,
            "api_web_tool",
            "reported_tokens_estimated_cost",
            "none",
        )
    }

    pub fn cli_readonly(runner: impl Into<String>, accounting: impl Into<String>) -> Self {
        Self::new(runner, "cli_agent_readonly", accounting, "read_only")
    }

    pub fn cli_tools(runner: impl Into<String>, accounting: impl Into<String>) -> Self {
        Self::new(runner, "cli_agent_tools", accounting, "tools_unspecified")
    }

    pub fn gateway() -> Self {
        Self::new("gateway", "gateway", "gateway_reported", "gateway")
    }
}

// ---------------------------------------------------------------------------
// Provider Response (unified shape from all 4 providers)
// ---------------------------------------------------------------------------

/// Response from any LLM provider. Every provider client must return this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub text: String,
    pub model: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cached_in: u32,
    pub latency_ms: u64,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_provenance: Option<GatewayProvenance>,
    /// Every Gateway request made while producing this provider response.
    /// Normally one entry; a rate-limit retry preserves both correlation IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gateway_attempts: Vec<GatewayProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_provenance: Option<ProviderProvenance>,
}

impl Default for ProviderResponse {
    fn default() -> Self {
        Self {
            text: String::new(),
            model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cached_in: 0,
            latency_ms: 0,
            cost_usd: 0.0,
            error: None,
            gateway_provenance: None,
            gateway_attempts: Vec::new(),
            provider_provenance: None,
        }
    }
}

impl ProviderResponse {
    pub fn with_provider_provenance(mut self, provenance: ProviderProvenance) -> Self {
        self.provider_provenance = Some(provenance);
        self
    }
}

// ---------------------------------------------------------------------------
// Deliberation Types
// ---------------------------------------------------------------------------

/// A single seat's response in a round.
/// Pre-Gen-9.6 (Python) sessions don't carry `cached_in` / `cost_usd`; default
/// to zero so historical transcripts still parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SeatResponse {
    pub seat_name: String,
    pub provider: String,
    pub model: String,
    pub text: String,
    pub round_num: u32,
    pub latency_ms: u64,
    pub tokens_in: u32,
    pub tokens_out: u32,
    #[serde(default)]
    pub cached_in: u32,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_provenance: Option<ProviderProvenance>,
}

impl SeatResponse {
    pub fn from_provider(
        seat_name: &str,
        provider: &str,
        round_num: u32,
        resp: ProviderResponse,
        scrubber: impl Fn(&str) -> String,
    ) -> Self {
        let text = scrubber(&resp.text);
        let error = resp.error.map(|e| scrubber(&e)).or_else(|| {
            if text.trim().is_empty() {
                Some(format!(
                    "empty provider response (model: {}, tokens_in: {}, tokens_out: {})",
                    resp.model, resp.tokens_in, resp.tokens_out
                ))
            } else {
                None
            }
        });

        Self {
            seat_name: seat_name.to_string(),
            provider: provider.to_string(),
            model: resp.model,
            text,
            round_num,
            latency_ms: resp.latency_ms,
            tokens_in: resp.tokens_in,
            tokens_out: resp.tokens_out,
            cached_in: resp.cached_in,
            cost_usd: resp.cost_usd,
            error,
            gateway: resp.gateway_provenance,
            provider_provenance: resp.provider_provenance,
        }
    }
}

/// RFC 9457 Problem Details for HTTP APIs
/// Provides a uniform error payload across Gateway, Sentinel, and Council.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProblemDetails {
    /// A URI reference that identifies the problem type.
    #[serde(
        rename = "type",
        default = "default_type",
        skip_serializing_if = "is_default_type"
    )]
    pub type_uri: String,

    /// A short, human-readable summary of the problem type.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,

    /// The HTTP status code generated by the origin server for this occurrence of the problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,

    /// A human-readable explanation specific to this occurrence of the problem.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,

    /// A URI reference that identifies the specific occurrence of the problem.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instance: String,

    /// Additional extensions
    #[serde(flatten)]
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
}

fn default_type() -> String {
    "about:blank".to_string()
}

fn is_default_type(t: &String) -> bool {
    t == "about:blank"
}

impl ProblemDetails {
    pub fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            type_uri: default_type(),
            title: title.into(),
            status: None,
            detail: detail.into(),
            instance: String::new(),
            extensions: std::collections::HashMap::new(),
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_type(mut self, type_uri: impl Into<String>) -> Self {
        self.type_uri = type_uri.into();
        self
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    pub fn with_extension(mut self, key: impl Into<String>, value: impl serde::Serialize) -> Self {
        if let Ok(val) = serde_json::to_value(value) {
            self.extensions.insert(key.into(), val);
        }
        self
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CapabilityToken {
    pub actor: String,
    pub subject: String,
    pub tenant: String,
    pub allowed_actions: Vec<String>,
    pub approval_required: bool,
    pub expires_at: u64,
    pub max_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}
