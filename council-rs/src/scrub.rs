use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

pub const PLACEHOLDER: &str = "[REDACTED:secret]";

fn patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            // Extended secret patterns: sk-, xai-, AIza, nvapi-, gsk_ (Groq)
            Regex::new(r"(?i)(sk-|xai-|AIza|nvapi-|gsk_)[A-Za-z0-9_\-]{20,}").unwrap(),
            // Telegram bot token
            Regex::new(r"\b\d+:AA[A-Za-z0-9_\-]{30,}\b").unwrap(),
            // Slack/Discord Webhooks
            Regex::new(r"https://hooks\.slack\.com/services/T[a-zA-Z0-9_]{8,}/B[a-zA-Z0-9_]{8,}/[a-zA-Z0-9_]{24}").unwrap(),
            Regex::new(r"https://(?:ptb\.|canary\.)?discord(?:app)?\.com/api/webhooks/[0-9]{15,20}/[A-Za-z0-9_\-]{60,}").unwrap(),
            // GCP PRIVATE KEY
            Regex::new(r"-----BEGIN PRIVATE KEY-----(?s).*?-----END PRIVATE KEY-----").unwrap(),
            // GitHub PAT
            Regex::new(r"\bghp_[A-Za-z0-9_]{36}\b").unwrap(),
            // GitHub fine-grained
            Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{60,}\b").unwrap(),
            // Bearer tokens
            Regex::new(r"\bBearer\s+[A-Za-z0-9._\-+/=]{16,}\b").unwrap(),
            // JWT (3-segment base64url)
            Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b").unwrap(),
            // AWS access key id
            Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            // AWS secret access key (heuristic)
            Regex::new(r#"(?i)(?:secret|aws_secret|secret_access_key)["'\s:=]+[A-Za-z0-9/+=]{40}"#).unwrap(),
            // Generic key=value
            Regex::new(r#"(?i)\b(?:api[_-]?key|password|passwd|secret|token)\s*[:=]\s*["']?([A-Za-z0-9_\-]{8,})["']?"#).unwrap(),
        ]
    })
}

fn shannon_entropy(s: &str) -> f32 {
    let mut counts = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let len = s.len() as f32;
    counts
        .values()
        .map(|&count| {
            let p = count as f32 / len;
            -p * p.log2()
        })
        .sum()
}

pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = text.to_string();
    for pat in patterns() {
        out = pat.replace_all(&out, PLACEHOLDER).into_owned();
    }

    // Shannon entropy fallback for bare tokens (20+ chars, high entropy >= 4.5)
    static WORD_RE: OnceLock<Regex> = OnceLock::new();
    let word_re = WORD_RE.get_or_init(|| Regex::new(r"\b[A-Za-z0-9_\-]{20,}\b").unwrap());

    out = word_re
        .replace_all(&out, |caps: &regex::Captures| {
            let matched = &caps[0];
            if shannon_entropy(matched) >= 4.5 {
                PLACEHOLDER.to_string()
            } else {
                matched.to_string()
            }
        })
        .into_owned();

    out
}

/// T24: Sheldon validation reports carry raw validator output (`claim` +
/// `reasoning`) that never passes through the per-seat `from_provider`
/// redaction closure. Scrub secret shapes out of both free-text fields before
/// the report is persisted into a `RoundResult`.
pub fn redact_validation_report(
    mut report: Vec<crate::types::ClaimVerdictEntry>,
) -> Vec<crate::types::ClaimVerdictEntry> {
    for entry in &mut report {
        entry.claim = redact(&entry.claim);
        entry.reasoning = redact(&entry.reasoning);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClaimImpact, ClaimVerdict, ClaimVerdictEntry};

    // Credential-shaped sentinel: matches the `sk-` pattern in `patterns()`.
    const SENTINEL: &str = "sk-SENTINELSECRETsk1234567890abcdef";

    #[test]
    fn redact_masks_credential_sentinel_keeps_content() {
        // Shared primitive behind the stream-topic scrub (fix 2), the
        // operator-facing topic prints (fix 6), and the mapmaker scrub (fix 7):
        // a credential shape in free text is masked; surrounding business
        // content survives.
        let input = format!("please review the {SENTINEL} deploy plan today");
        let out = redact(&input);
        assert!(!out.contains(SENTINEL), "sentinel leaked: {out}");
        assert!(out.contains(PLACEHOLDER), "placeholder missing: {out}");
        assert!(out.contains("review the"), "content mangled: {out}");
        assert!(out.contains("deploy plan today"), "content mangled: {out}");
    }

    #[test]
    fn redact_validation_report_masks_claim_and_reasoning() {
        // Fix 10: Sheldon claim/reasoning are raw validator output that bypasses
        // the per-seat from_provider redaction closure.
        let report = vec![ClaimVerdictEntry {
            claim: format!("the key {SENTINEL} is valid"),
            seat: "aristotle".to_string(),
            verdict: ClaimVerdict::Contradicted,
            evidence_citations: vec![],
            reasoning: format!("observed {SENTINEL} in the seat transcript"),
            confidence: 0.9,
            impact: ClaimImpact::High,
            _overridden_from: None,
        }];
        let out = redact_validation_report(report);
        assert!(
            !out[0].claim.contains(SENTINEL),
            "claim leaked: {}",
            out[0].claim
        );
        assert!(
            !out[0].reasoning.contains(SENTINEL),
            "reasoning leaked: {}",
            out[0].reasoning
        );
        assert!(out[0].claim.contains(PLACEHOLDER));
        assert!(out[0].reasoning.contains(PLACEHOLDER));
        // Non-content fields are untouched — redaction is content-preserving.
        assert_eq!(out[0].seat, "aristotle");
        assert_eq!(out[0].verdict, ClaimVerdict::Contradicted);
        assert_eq!(out[0].impact, ClaimImpact::High);
    }
}
