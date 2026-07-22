// ==========================================================================
// policy.rs — Sensitivity-based policy firewall.
//
// Three routing modes based on data sensitivity:
//
//   SOVEREIGN  — local inference only (Librarian node)
//   INTERNAL   — trusted providers only (self-hosted, contractual)
//   PUBLIC     — any provider allowed
//
// The firewall classifies requests by:
//   1. Explicit header (X-Sensitivity-Level)
//   2. Content analysis (PII patterns, code signatures)
//   3. Default policy (configurable)
//
// In DRY-RUN mode, the firewall logs decisions but does not block.
// This enables shadow-mode validation before enforcing.
// ==========================================================================

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;
use tracing::{debug, info, warn};

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
    /// Whether to aggressively block on PII detection
    #[serde(default)]
    pub block_on_pii: bool,
    /// Whether to block if jailbreak signals are detected
    #[serde(default)]
    pub block_jailbreaks: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default_level: SensitivityLevel::Public,
            dry_run: true, // Start in dry-run for safety
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
            block_on_pii: false,
            block_jailbreaks: true,
        }
    }
}

// ---------------------------------------------------------------------------
// PII / sensitivity signal detection
// ---------------------------------------------------------------------------

static PII_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(), "ssn_pattern"),
        (
            Regex::new(r"\b\d{4}[\s-]?\d{4}[\s-]?\d{4}[\s-]?\d{4}\b").unwrap(),
            "credit_card_pattern",
        ),
        (
            Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").unwrap(),
            "email_address",
        ),
        (
            Regex::new(r"(?i)(password|passwd|secret|token|api[_-]?key)\s*[:=]\s*\S+").unwrap(),
            "credential_leak",
        ),
        (
            Regex::new(r"(?i)(ssn|social\s+security|date\s+of\s+birth|dob)\s*[:=]").unwrap(),
            "pii_field_label",
        ),
        (
            Regex::new(r"-----BEGIN\s+(RSA\s+)?PRIVATE\s+KEY-----").unwrap(),
            "private_key",
        ),
        (
            Regex::new(r"(?i)(HIPAA|PHI|protected\s+health\s+information)").unwrap(),
            "health_data_signal",
        ),
    ]
});

static JAILBREAK_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(r"(?i)(jailbreak|DAN\s*mode|developer\s*mode|god\s*mode|unrestricted\s*mode|ignore\s+all\s+previous\s+instructions|bypassing|unfiltered|no\s+moderation|without\s+rules)").unwrap(),
            "jailbreak_signal",
        ),
    ]
});

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

    /// Evaluate whether a request to `provider` is allowed given the
    /// sensitivity level (from header or auto-detected).
    #[tracing::instrument(skip(self, content), fields(provider = provider))]
    pub fn evaluate(
        &self,
        provider: &str,
        explicit_level: Option<SensitivityLevel>,
        content: Option<&str>,
    ) -> PolicyDecision {
        let mut signals = Vec::new();

        // 1. Determine sensitivity level
        let level = if let Some(explicit) = explicit_level {
            signals.push(format!("explicit_header:{:?}", explicit));
            explicit
        } else if let Some(text) = content {
            self.detect_sensitivity(text, &mut signals)
        } else {
            self.config.default_level
        };

        if let Some(text) = content {
            for (pattern, name) in JAILBREAK_PATTERNS.iter() {
                if pattern.is_match(text) && !signals.contains(&name.to_string()) {
                    signals.push(name.to_string());
                }
            }
        }

        // 2. Check provider against level
        let allowed_providers = match level {
            SensitivityLevel::Sovereign => &self.config.sovereign_providers,
            SensitivityLevel::Restricted => &self.config.restricted_providers,
            SensitivityLevel::Internal => &self.config.internal_providers,
            SensitivityLevel::Public => &self.config.public_providers,
        };

        let mut provider_allowed = allowed_providers.contains(provider);
        let mut block_reason = String::new();

        if self.config.block_on_pii
            && !signals.is_empty()
            && signals
                .iter()
                .any(|s| s != "jailbreak_signal" && !s.starts_with("explicit_header"))
        {
            provider_allowed = false;
            block_reason = "PII detected and block_on_pii is enabled".to_string();
        }

        if self.config.block_jailbreaks && signals.contains(&"jailbreak_signal".to_string()) {
            provider_allowed = false;
            block_reason = "Jailbreak pattern detected and block_jailbreaks is enabled".to_string();
        }

        // 3. Construct decision
        let reason = if provider_allowed {
            String::new()
        } else if !block_reason.is_empty() {
            block_reason
        } else {
            format!(
                "provider '{}' not allowed at {:?} sensitivity level",
                provider, level
            )
        };

        // In dry-run, log but allow
        let effective_allowed = if self.config.dry_run && !provider_allowed {
            warn!(
                provider,
                level = ?level,
                signals = ?signals,
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

    /// Auto-detect sensitivity level from content.
    /// Any PII signal → INTERNAL. Private keys / health data → SOVEREIGN.
    fn detect_sensitivity(&self, content: &str, signals: &mut Vec<String>) -> SensitivityLevel {
        let mut max_level = self.config.default_level;

        for (pattern, name) in PII_PATTERNS.iter() {
            if pattern.is_match(content) {
                signals.push(name.to_string());

                match *name {
                    "private_key" | "health_data_signal" => {
                        max_level = SensitivityLevel::Sovereign;
                    }
                    "ssn_pattern" | "credit_card_pattern" | "credential_leak" => {
                        if !matches!(max_level, SensitivityLevel::Sovereign) {
                            max_level = SensitivityLevel::Restricted;
                        }
                    }
                    _ => {
                        if matches!(max_level, SensitivityLevel::Public) {
                            max_level = SensitivityLevel::Internal;
                        }
                    }
                }
            }
        }

        if !signals.is_empty() {
            debug!(
                signals = ?signals,
                level = ?max_level,
                "auto-detected sensitivity"
            );
        }

        max_level
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
    fn public_provider_allowed_at_public() {
        let fw = firewall(false);
        let decision = fw.evaluate("xai", Some(SensitivityLevel::Public), None);
        assert!(decision.allowed);
    }

    #[test]
    fn public_provider_blocked_at_sovereign() {
        let fw = firewall(false);
        let decision = fw.evaluate("openai", Some(SensitivityLevel::Sovereign), None);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("not allowed"));
    }

    #[test]
    fn local_provider_allowed_at_all_levels() {
        let fw = firewall(false);

        let d1 = fw.evaluate("local", Some(SensitivityLevel::Public), None);
        let d2 = fw.evaluate("local", Some(SensitivityLevel::Internal), None);
        let d3 = fw.evaluate("local", Some(SensitivityLevel::Sovereign), None);

        assert!(d1.allowed);
        assert!(d2.allowed);
        assert!(d3.allowed);
    }

    #[test]
    fn dry_run_allows_but_flags() {
        let fw = firewall(true);
        let decision = fw.evaluate("nvidia", Some(SensitivityLevel::Sovereign), None);
        // Dry-run should allow even though nvidia isn't in sovereign_providers
        assert!(decision.allowed);
        assert!(decision.dry_run);
    }

    #[test]
    fn auto_detect_ssn() {
        let fw = firewall(false);
        let content = "My SSN is 123-45-6789 please process this";
        let decision = fw.evaluate("xai", None, Some(content));
        // SSN → RESTRICTED, xai not in restricted_providers → blocked
        assert!(!decision.allowed);
        assert_eq!(decision.level, SensitivityLevel::Restricted);
        assert!(decision
            .detected_signals
            .contains(&"ssn_pattern".to_string()));
    }

    #[test]
    fn auto_detect_private_key() {
        let fw = firewall(false);
        let content = "-----BEGIN PRIVATE KEY-----\nMIIEvg...";
        let decision = fw.evaluate("openai", None, Some(content));
        // Private key → SOVEREIGN, openai not in sovereign_providers → blocked
        assert!(!decision.allowed);
        assert_eq!(decision.level, SensitivityLevel::Sovereign);
    }

    #[test]
    fn auto_detect_credential() {
        let fw = firewall(false);
        let content = "Here's my api_key: sk-abc123def456 for the service";
        let decision = fw.evaluate("openai-dedicated", None, Some(content));
        // Credential → RESTRICTED, openai-dedicated IS in restricted_providers → allowed
        assert!(decision.allowed);
        assert_eq!(decision.level, SensitivityLevel::Restricted);
    }

    #[test]
    fn clean_content_uses_default() {
        let fw = firewall(false);
        let content = "What is the weather like today?";
        let decision = fw.evaluate("nvidia", None, Some(content));
        // No signals → default (PUBLIC), nvidia is in public_providers → allowed
        assert!(decision.allowed);
        assert_eq!(decision.level, SensitivityLevel::Public);
    }

    #[test]
    fn explicit_header_overrides_detection() {
        let fw = firewall(false);
        // Content has PII but header says PUBLIC
        let content = "My SSN is 123-45-6789";
        let decision = fw.evaluate("xai", Some(SensitivityLevel::Public), Some(content));
        // Explicit header wins
        assert!(decision.allowed);
        assert_eq!(decision.level, SensitivityLevel::Public);
    }
}
