// ==========================================================================
// policy.rs — Sensitivity-based policy firewall.
//
// Three routing modes based on data sensitivity:
//
//   SOVEREIGN  — local inference only (Librarian node)
//   INTERNAL   — trusted providers only (self-hosted, contractual)
//   PUBLIC     — any provider allowed
//
// The firewall checks the caller-declared X-Sensitivity-Level against the
// configured provider allowlist. Missing levels default to GREEN/PUBLIC.
//
// In DRY-RUN mode, the firewall logs decisions but does not block.
// This enables shadow-mode validation before enforcing.
// ==========================================================================

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SensitivityLevel {
    Sovereign,
    Restricted,
    Internal,
    Public,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub level: SensitivityLevel,
    pub provider: String,
    pub reason: String,
    pub dry_run: bool,
    pub detected_signals: Vec<String>,
}

// ---------------------------------------------------------------------------
// Provider classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyConfig {
    /// Default sensitivity when no header or signal detected
    pub default_level: SensitivityLevel,
    /// Whether to enforce or just log
    pub dry_run: bool,
    /// Providers allowed at SOVEREIGN level (typically local only)
    pub sovereign_providers: HashSet<String>,
    /// Providers allowed at RESTRICTED level
    pub restricted_providers: HashSet<String>,
    /// Providers allowed at INTERNAL level
    pub internal_providers: HashSet<String>,
    /// All providers (SOVEREIGN + RESTRICTED + INTERNAL + external)
    pub public_providers: HashSet<String>,
}

fn dry_run_from_env(v: Option<&str>) -> bool {
    !matches!(v, Some("1"))
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let dry_run = dry_run_from_env(std::env::var("GW_POLICY_ENFORCE").ok().as_deref());
        Self {
            default_level: SensitivityLevel::Public,
            dry_run,
            sovereign_providers: HashSet::from([
                "local".to_string(),
                "librarian".to_string(),
                "ollama".to_string(),
            ]),
            restricted_providers: HashSet::from([
                "local".to_string(),
                "librarian".to_string(),
                "ollama".to_string(),
                "openai-dedicated".to_string(),
            ]),
            internal_providers: HashSet::from([
                "local".to_string(),
                "librarian".to_string(),
                "ollama".to_string(),
                "openai-dedicated".to_string(),
                "openai".to_string(),
                "anthropic".to_string(),
                // CLI proxies reach the same vendors as their API twins —
                // same trust tier as openai/anthropic above.
                "claude-cli".to_string(),
                "gpt-cli".to_string(),
                // council (localhost council-rs) fans out to external seats,
                // so it sits at internal/public — NOT sovereign/restricted.
                "council".to_string(),
            ]),
            public_providers: HashSet::from([
                "local".to_string(),
                "librarian".to_string(),
                "ollama".to_string(),
                "openai-dedicated".to_string(),
                "openai".to_string(),
                "anthropic".to_string(),
                "xai".to_string(),
                "google".to_string(),
                "nvidia".to_string(),
                "deepseek".to_string(),
                "together".to_string(),
                // Providers actually registered in conf/models.json that were
                // missing here — every council/CLI/vertex call was tripping
                // the dry-run WOULD BLOCK log (and would hard-fail if dry_run
                // ever flips to enforce). "google" above is a phantom name;
                // the registry's Gemini providers are vertex/gemini-cli.
                "council".to_string(),
                "claude-cli".to_string(),
                "gpt-cli".to_string(),
                "gemini-cli".to_string(),
                "vertex".to_string(),
                "chaos".to_string(),
            ]),
        }
    }
}

// ---------------------------------------------------------------------------
// Policy Firewall
// ---------------------------------------------------------------------------

pub struct PolicyFirewall {
    config: PolicyConfig,
}

impl PolicyFirewall {
    pub fn new(config: PolicyConfig) -> Self {
        info!(
            dry_run = config.dry_run,
            default = ?config.default_level,
            "policy firewall initialized"
        );
        Self { config }
    }

    /// Evaluate whether `provider` is allowed at the caller-declared level.
    #[tracing::instrument(skip(self), fields(provider = provider))]
    pub fn evaluate(
        &self,
        provider: &str,
        sensitivity_level: Option<SensitivityLevel>,
    ) -> PolicyDecision {
        let mut signals = Vec::new();
        let level = sensitivity_level.unwrap_or(self.config.default_level);
        if let Some(explicit) = sensitivity_level {
            signals.push(format!("explicit_header:{:?}", explicit));
        }

        let allowed_providers = match level {
            SensitivityLevel::Sovereign => &self.config.sovereign_providers,
            SensitivityLevel::Restricted => &self.config.restricted_providers,
            SensitivityLevel::Internal => &self.config.internal_providers,
            SensitivityLevel::Public => &self.config.public_providers,
        };
        let provider_allowed = allowed_providers.contains(provider);
        let reason = if provider_allowed {
            String::new()
        } else {
            format!(
                "provider '{}' not allowed at {:?} sensitivity level",
                provider, level
            )
        };

        let effective_allowed = if self.config.dry_run && !provider_allowed {
            warn!(
                provider,
                level = ?level,
                "policy firewall: WOULD BLOCK (dry-run mode)"
            );
            true
        } else {
            if !provider_allowed {
                warn!(
                    provider,
                    level = ?level,
                    "policy firewall: BLOCKED"
                );
            }
            provider_allowed
        };

        PolicyDecision {
            allowed: effective_allowed,
            level,
            provider: provider.to_string(),
            reason,
            dry_run: self.config.dry_run && !provider_allowed,
            detected_signals: signals,
        }
    }

    /// Check if dry-run mode is active
    #[allow(dead_code)]
    pub fn is_dry_run(&self) -> bool {
        self.config.dry_run
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn firewall(dry_run: bool) -> PolicyFirewall {
        let config = PolicyConfig {
            dry_run,
            ..PolicyConfig::default()
        };
        PolicyFirewall::new(config)
    }

    #[test]
    fn dry_run_env_enforces_only_exact_one() {
        assert!(!dry_run_from_env(Some("1")));
        assert!(dry_run_from_env(None));
        assert!(dry_run_from_env(Some("0")));
        assert!(dry_run_from_env(Some("true")));
    }

    #[test]
    fn missing_level_defaults_to_public() {
        let decision = firewall(false).evaluate("xai", None);
        assert!(decision.allowed);
        assert_eq!(decision.level, SensitivityLevel::Public);
    }

    #[test]
    fn sovereign_level_enforces_provider_allowlist() {
        let fw = firewall(false);

        let cloud = fw.evaluate("openai", Some(SensitivityLevel::Sovereign));
        assert!(!cloud.allowed);
        assert!(cloud.reason.contains("not allowed"));

        let sovereign = fw.evaluate("local", Some(SensitivityLevel::Sovereign));
        assert!(sovereign.allowed);
    }

    #[test]
    fn local_provider_allowed_at_all_levels() {
        let fw = firewall(false);

        let public = fw.evaluate("local", Some(SensitivityLevel::Public));
        let internal = fw.evaluate("local", Some(SensitivityLevel::Internal));
        let sovereign = fw.evaluate("local", Some(SensitivityLevel::Sovereign));

        assert!(public.allowed);
        assert!(internal.allowed);
        assert!(sovereign.allowed);
    }

    #[test]
    fn dry_run_allows_but_flags() {
        let decision = firewall(true).evaluate("nvidia", Some(SensitivityLevel::Sovereign));
        assert!(decision.allowed);
        assert!(decision.dry_run);
    }
}
