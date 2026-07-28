// Validation report parse, citation override, context format, and gate redaction.

use super::{CitationOverrideMode, truncate};
use crate::types::{ClaimImpact, ClaimVerdict, ClaimVerdictEntry, SeatResponse};

pub(super) fn parse_json_report(text: &str) -> Option<Vec<ClaimVerdictEntry>> {
    let text = text.trim();

    // Strip markdown code fences
    let text = if text.starts_with("```") {
        let lines: Vec<&str> = text.lines().collect();
        let start = 1;
        let end = if lines.last().is_some_and(|l| l.trim() == "```") {
            lines.len() - 1
        } else {
            lines.len()
        };
        lines[start..end].join("\n")
    } else {
        text.to_string()
    };
    let text = text.trim();

    // Try direct parse
    if let Ok(report) = serde_json::from_str::<Vec<ClaimVerdictEntry>>(text) {
        return Some(report);
    }

    // Try extracting JSON array from response
    if let Some(start) = text.find('[')
        && let Some(end) = text.rfind(']')
        && end > start
        && let Ok(report) = serde_json::from_str::<Vec<ClaimVerdictEntry>>(&text[start..=end])
    {
        return Some(report);
    }

    None
}

/// Structural citation override (v9.13.2).
/// If the model says SUPPORTED or CONTRADICTED but has no evidence citations,
/// it used its training data — override to NO_EVIDENCE (mode-dependent).
pub(super) fn apply_citation_override(
    report: &mut [ClaimVerdictEntry],
    mode: CitationOverrideMode,
) -> usize {
    if mode == CitationOverrideMode::Off {
        return 0;
    }
    let mut overrides = 0;
    let empty_citations = &[
        "none",
        "n/a",
        "no evidence",
        "no evidence found",
        "no supporting evidence",
        "not found",
    ];

    for item in report.iter_mut() {
        // Remap old taxonomy names if they sneak through
        // (handled by serde #[serde(other)] → Unknown, but just in case)

        let has_real_citation = item.evidence_citations.iter().any(|c| {
            let trimmed = c.trim().to_lowercase();
            !trimmed.is_empty() && !empty_citations.contains(&trimmed.as_str())
        });

        let applies = match mode {
            CitationOverrideMode::Off => false,
            CitationOverrideMode::ContradictedOnly => item.verdict == ClaimVerdict::Contradicted,
            CitationOverrideMode::All => {
                matches!(
                    item.verdict,
                    ClaimVerdict::Supported | ClaimVerdict::Contradicted
                )
            }
        };

        if applies && !has_real_citation {
            item._overridden_from = Some(format!("{:?}", item.verdict));
            item.verdict = ClaimVerdict::NoEvidence;
            overrides += 1;
        }
    }
    overrides
}

/// Format validation report for injection into cross-pollination context.
/// Splits into validated_findings (act on these) and unresolved_claims
/// (don't waste rounds on these).
pub fn format_validation_context(report: &[ClaimVerdictEntry]) -> String {
    if report.is_empty() {
        return String::new();
    }

    let mut validated = Vec::new();
    let mut unresolved = Vec::new();

    // Sort by impact (High first) so high-stakes contradictions aren't dropped by the cap.
    let mut items: Vec<_> = report.iter().collect();
    items.sort_by_key(|item| match item.impact {
        ClaimImpact::High => 0u8,
        ClaimImpact::Medium => 1,
        ClaimImpact::Low => 2,
        ClaimImpact::Unknown => 3,
    });

    for item in items.into_iter().take(8) {
        let icon = match item.verdict {
            ClaimVerdict::Supported => "✅",
            ClaimVerdict::Consistent => "🟡",
            ClaimVerdict::NoEvidence => "⚪",
            ClaimVerdict::Contradicted => "❌",
        };

        let mut entry = format!(
            "{} [{:?}] ({}): {}",
            icon, item.verdict, item.seat, item.claim
        );
        if !item.evidence_citations.is_empty() {
            for c in item.evidence_citations.iter().take(2) {
                entry.push_str(&format!("\n   📎 {}", c));
            }
        } else if !item.reasoning.is_empty() {
            let short = truncate(&item.reasoning, 200);
            entry.push_str(&format!("\n   Note: {}", short));
        }

        match item.verdict {
            ClaimVerdict::Supported | ClaimVerdict::Consistent | ClaimVerdict::Contradicted => {
                validated.push(entry);
            }
            _ => {
                unresolved.push(entry);
            }
        }
    }

    let mut lines = vec![
        String::new(),
        "--- VALIDATOR REPORT (Sheldon v2 — evidence-grounded) ---".into(),
    ];

    if !validated.is_empty() {
        lines.push(String::new());
        lines.push("## Validated Findings (evidence-backed — address these):".into());
        lines.extend(validated);
        lines.push(String::new());
        lines.push(
            "Claims marked CONTRADICTED must be revised or withdrawn. \
             Claims marked SUPPORTED can be built upon with confidence."
                .into(),
        );
    }

    if !unresolved.is_empty() {
        lines.push(String::new());
        lines.push("## Unresolved Claims (no evidence available — do NOT spiral):".into());
        lines.extend(unresolved);
        lines.push(String::new());
        lines.push(
            "These claims could not be validated with current evidence sources. \
             This does NOT mean they are false — treat as open assumptions. \
             Do NOT spend deliberation time re-arguing these. Move forward \
             on architecture and decisions; flag for out-of-band verification."
                .into(),
        );
    }

    lines.extend(
        ["", "--- END VALIDATOR REPORT ---", ""]
            .iter()
            .map(|s| s.to_string()),
    );
    lines.join("\n")
}

/// Sheldon gate mode (v9.13.4): redact only high-impact CONTRADICTED claims
/// (exact strings from report) from seat responses before R2+ cross-pollination.
/// Low/medium/unknown-impact contradicted claims stay in text (flagged in report).
/// No fuzzy matching (prevents mangling). High-impact gate limits poisoning
/// from single claims. Works with claim_validator cascade failover.
pub fn gate_responses(
    responses: &[SeatResponse],
    report: &[ClaimVerdictEntry],
) -> Vec<SeatResponse> {
    let mut contradicted_by_seat: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for item in report {
        if item.verdict == ClaimVerdict::Contradicted
            && item.claim.len() > 10
            && item.impact == ClaimImpact::High
        {
            contradicted_by_seat
                .entry(item.seat.clone())
                .or_default()
                .push(item.claim.clone());
        }
    }

    if contradicted_by_seat.is_empty() {
        return responses.to_vec();
    }

    let mut gated = Vec::new();
    for r in responses {
        if let Some(claims) = contradicted_by_seat.get(&r.seat_name) {
            let mut text = r.text.clone();
            for claim in claims {
                // Precise exact claim string only (from validator report). No fuzzy.
                if text.contains(claim.as_str()) {
                    let short = truncate(claim, 80);
                    text = text.replace(
                        claim.as_str(),
                        &format!("[REDACTED — CONTRADICTED (HIGH) by evidence: {}...]", short),
                    );
                }
            }
            let mut gated_r = r.clone();
            gated_r.text = text;
            gated.push(gated_r);
        } else {
            gated.push(r.clone());
        }
    }
    gated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClaimImpact, ClaimVerdict, ClaimVerdictEntry, SeatResponse};

    #[test]

    fn citation_override_contradicted_only_leaves_supported() {
        let mut report = vec![
            ClaimVerdictEntry {
                claim: "A".into(),

                seat: "s".into(),

                verdict: ClaimVerdict::Supported,

                evidence_citations: vec![],

                reasoning: String::new(),

                confidence: 0.0,

                impact: ClaimImpact::Unknown,

                _overridden_from: None,
            },
            ClaimVerdictEntry {
                claim: "B".into(),

                seat: "s".into(),

                verdict: ClaimVerdict::Contradicted,

                evidence_citations: vec![],

                reasoning: String::new(),

                confidence: 0.0,

                impact: ClaimImpact::Unknown,

                _overridden_from: None,
            },
        ];

        let n = apply_citation_override(&mut report, CitationOverrideMode::ContradictedOnly);

        assert_eq!(n, 1);

        assert_eq!(report[0].verdict, ClaimVerdict::Supported);

        assert_eq!(report[1].verdict, ClaimVerdict::NoEvidence);
    }

    #[test]

    fn citation_override_all_legacy_behavior() {
        let mut report = vec![ClaimVerdictEntry {
            claim: "A".into(),

            seat: "s".into(),

            verdict: ClaimVerdict::Supported,

            evidence_citations: vec![],

            reasoning: String::new(),

            confidence: 0.0,

            impact: ClaimImpact::Unknown,

            _overridden_from: None,
        }];

        let n = apply_citation_override(&mut report, CitationOverrideMode::All);

        assert_eq!(n, 1);

        assert_eq!(report[0].verdict, ClaimVerdict::NoEvidence);
    }

    #[test]

    fn gate_responses_redacts_only_high_impact_exact_claims() {
        let responses = vec![SeatResponse {
            seat_name: "Alice".into(),

            provider: "grok".into(),

            model: "grok-1".into(),

            text: "The sky is blue today. Foo bar is unrelated. Evidence shows the claim holds."
                .into(),

            round_num: 1,

            latency_ms: 123,

            tokens_in: 10,

            tokens_out: 30,

            cached_in: 0,

            cost_usd: 0.001,

            error: None,

            gateway: None,

            provider_provenance: None,
        }];

        let report = vec![
            ClaimVerdictEntry {
                claim: "The sky is blue today".into(),

                seat: "Alice".into(),

                verdict: ClaimVerdict::Contradicted,

                evidence_citations: vec!["https://weather.example.com".into()],

                reasoning: "direct contradiction from live data".into(),

                confidence: 0.95,

                impact: ClaimImpact::High,

                _overridden_from: None,
            },
            ClaimVerdictEntry {
                claim: "Foo bar is unrelated.".into(),

                seat: "Alice".into(),

                verdict: ClaimVerdict::Contradicted,

                evidence_citations: vec![],

                reasoning: "minor".into(),

                confidence: 0.2,

                impact: ClaimImpact::Low,

                _overridden_from: None,
            },
        ];

        let gated = gate_responses(&responses, &report);

        assert_eq!(gated.len(), 1);

        let out = &gated[0].text;

        assert!(
            out.contains("[REDACTED — CONTRADICTED (HIGH) by evidence:"),
            "high should be redacted: {}",
            out
        );

        assert!(
            !out.contains("The sky is blue today. Foo bar is unrelated."),
            "original high claim context broken"
        );

        assert!(
            out.contains("Foo bar is unrelated."),
            "low impact claim must remain"
        );
    }

    #[test]

    fn gate_responses_no_redact_when_no_high_contradicted_or_empty() {
        let responses = vec![SeatResponse {
            seat_name: "Bob".into(),

            provider: "claude".into(),

            model: "claude-3".into(),

            text: "All is well. Sky is green per some view.".into(),

            round_num: 2,

            latency_ms: 50,

            tokens_in: 5,

            tokens_out: 10,

            cached_in: 0,

            cost_usd: 0.0,

            error: None,

            gateway: None,

            provider_provenance: None,
        }];

        let report_med = vec![ClaimVerdictEntry {
            claim: "Sky is green per some view.".into(),

            seat: "Bob".into(),

            verdict: ClaimVerdict::Contradicted,

            evidence_citations: vec![],

            reasoning: "".into(),

            confidence: 0.1,

            impact: ClaimImpact::Medium,

            _overridden_from: None,
        }];

        let gated = gate_responses(&responses, &report_med);

        assert_eq!(gated[0].text, responses[0].text);

        let gated2 = gate_responses(&responses, &[]);

        assert_eq!(gated2[0].text, responses[0].text);
    }

    #[test]

    fn gate_responses_preserves_non_contradicted_and_other_seats() {
        let responses = vec![
            SeatResponse {
                seat_name: "A".into(),

                provider: "x".into(),

                model: "".into(),

                text: "claim-X here".into(),

                round_num: 1,

                latency_ms: 0,

                tokens_in: 0,

                tokens_out: 0,

                cached_in: 0,

                cost_usd: 0.0,

                error: None,

                gateway: None,

                provider_provenance: None,
            },
            SeatResponse {
                seat_name: "B".into(),

                provider: "x".into(),

                model: "".into(),

                text: "claim-Y here".into(),

                round_num: 1,

                latency_ms: 0,

                tokens_in: 0,

                tokens_out: 0,

                cached_in: 0,

                cost_usd: 0.0,

                error: None,

                gateway: None,

                provider_provenance: None,
            },
        ];

        let report = vec![ClaimVerdictEntry {
            claim: "claim-X here".into(),

            seat: "A".into(),

            verdict: ClaimVerdict::Supported,

            evidence_citations: vec![],

            reasoning: "".into(),

            confidence: 0.0,

            impact: ClaimImpact::High,

            _overridden_from: None,
        }];

        let gated = gate_responses(&responses, &report);

        assert_eq!(gated[0].text, "claim-X here");

        assert_eq!(gated[1].text, "claim-Y here");
    }

    // --- classify_claim unit tests (new pre-classification logic) ---
}
