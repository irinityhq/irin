// Claim-validator readiness, system prompts, and validate_round.

use super::evidence::gather_evidence;
use super::report::{apply_citation_override, parse_json_report};
use super::{
    EvidenceCache, ValidateRoundOutcome, ValidateSkipReason, build_position_summary,
    citation_override_mode, should_skip_validator_llm, truncate,
};
use crate::engine::context::RequestContext;
use crate::evidence;
use crate::provider;
use crate::types::{ClaimImpact, ClaimVerdict, RoleDefinition, SeatResponse};

pub fn claim_validator_ready(claim_role: &RoleDefinition, round_num: u32) -> bool {
    if !claim_role.cascade.is_empty() {
        return true;
    }
    if round_num == 1 {
        eprintln!(
            "   ⚠️  --validate: claim_validator cascade is empty in roles.yaml — validation skipped"
        );
    }
    false
}

const SHELDON_SYSTEM_WITH_WEB_SEARCH: &str = r#"<role>You are Sheldon — a pedantic, evidence-only claim validator. You assess factual claims using the provided evidence sources AND your web search tool. Search the web to verify specific numbers, API behaviors, benchmarks, and technical specs. You do NOT deliberate, strategize, or opine.</role>

<rules>
1. Extract the 5-8 most consequential FACTUAL claims: numbers, timelines, costs, technical specs, market assertions, regulatory statements, hardware capabilities, local code behavior, and deployment feasibility.
2. Report EVERY claim you find. Your goal is COVERAGE.
3. Ignore opinions, recommendations, and value judgments at the per-claim level.
4. For public-world claims, USE YOUR WEB SEARCH TOOL to verify them. Search for official documentation, benchmarks, or authoritative sources.
5. For LOCAL CODE claims (file paths, functions, tests, build scripts, repository behavior, or runtime behavior in this checkout), do NOT use web search or model memory. Verify only from <repo_context> or explicit local-code excerpts in the <evidence> section. If local repo evidence is absent, verdict MUST be NO_EVIDENCE.
6. For each claim, assign a verdict:
   - SUPPORTED: evidence from web search or the <evidence> section directly confirms the claim
   - CONSISTENT: evidence is directionally aligned but not definitive
   - NO_EVIDENCE: no relevant information found after searching (this is the default — it is NOT a negative signal)
   - CONTRADICTED: evidence directly conflicts with the claim (you MUST cite the specific conflicting source)
7. You MUST populate the evidence_citations field with specific quotes, URLs, file/symbol snippets, or paraphrases from your search results or the <evidence> section. If you cannot cite a specific source, your verdict MUST be NO_EVIDENCE.
8. Include your confidence as a float 0.0-1.0.
</rules>

<output_format>
Respond with ONLY a JSON array. Each element:
{"claim": "exact quoted text", "seat": "seat name", "verdict": "SUPPORTED|CONSISTENT|NO_EVIDENCE|CONTRADICTED", "evidence_citations": ["specific quote or paraphrase from evidence"], "reasoning": "max 2 sentences", "confidence": 0.0, "impact": "HIGH|MEDIUM|LOW"}
No preamble. No explanation. No markdown fencing. Just the JSON array.
</output_format>"#;

const SHELDON_SYSTEM_EVIDENCE_ONLY: &str = r#"<role>You are Sheldon — a pedantic, evidence-only claim validator. You assess factual claims using ONLY the provided evidence sources. In this provider path you do not have an external web-search tool. You do NOT deliberate, strategize, or opine.</role>

<rules>
1. Extract the 5-8 most consequential FACTUAL claims: numbers, timelines, costs, technical specs, market assertions, regulatory statements, hardware capabilities, local code behavior, and deployment feasibility.
2. Report EVERY claim you find. Your goal is COVERAGE.
3. Ignore opinions, recommendations, and value judgments at the per-claim level.
4. For each claim, use only the <evidence> section and the deliberation text provided in the prompt. Do not invent search results or cite unstated external sources.
5. For LOCAL CODE claims (file paths, functions, tests, build scripts, repository behavior, or runtime behavior in this checkout), verify only from <repo_context> or explicit local-code excerpts in the <evidence> section. If local repo evidence is absent, verdict MUST be NO_EVIDENCE.
6. For each claim, assign a verdict:
   - SUPPORTED: evidence from the <evidence> section directly confirms the claim
   - CONSISTENT: evidence is directionally aligned but not definitive
   - NO_EVIDENCE: no relevant information found in the provided evidence (this is the default — it is NOT a negative signal)
   - CONTRADICTED: provided evidence directly conflicts with the claim (you MUST cite the specific conflicting source)
7. You MUST populate the evidence_citations field with specific quotes, URLs, file/symbol snippets, or paraphrases from the <evidence> section. If you cannot cite a specific source, your verdict MUST be NO_EVIDENCE.
8. Include your confidence as a float 0.0-1.0.
</rules>

<output_format>
Respond with ONLY a JSON array. Each element:
{"claim": "exact quoted text", "seat": "seat name", "verdict": "SUPPORTED|CONSISTENT|NO_EVIDENCE|CONTRADICTED", "evidence_citations": ["specific quote or paraphrase from evidence"], "reasoning": "max 2 sentences", "confidence": 0.0, "impact": "HIGH|MEDIUM|LOW"}
No preamble. No explanation. No markdown fencing. Just the JSON array.
</output_format>"#;

const DEFAULT_GROK_MODEL: &str = "grok-4.3";
const DEFAULT_GROK_HERMES_MODEL: &str = "grok-4.20-0309-reasoning";
const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-4-6";
const DEFAULT_GPT_MODEL: &str = "gpt-5.6-sol";
// Note: primary models now come from roles.yaml claim_validator cascade when set.

fn sheldon_system_for_provider(provider: &str) -> &'static str {
    if matches!(provider, "grok_build" | "grok" | "grok_cli" | "grok_api") {
        // Grok Build exposes the native web/X tools used by Sheldon. The
        // explicit Hermes transport does not inherit that claim.
        SHELDON_SYSTEM_WITH_WEB_SEARCH
    } else {
        SHELDON_SYSTEM_EVIDENCE_ONLY
    }
}

pub struct ValidatorConfig {
    pub provider: String,
    pub model: Option<String>, // from roles if present
    pub gate: bool,
    pub verbose: bool,
}

/// Run Sheldon validation on a round's responses.
///
/// `req_ctx` carries the per-session gateway override (feature contract) — the
/// validator prompt contains full round content, so it must honor
/// `via_gateway`/`sensitivity` like seat calls.
pub async fn validate_round(
    responses: &[SeatResponse],
    topic: &str,
    context: &str,
    round_num: u32,
    config: &ValidatorConfig,
    req_ctx: &RequestContext,
    evidence_cache: Option<&EvidenceCache>,
) -> ValidateRoundOutcome {
    let valid: Vec<&SeatResponse> = responses
        .iter()
        .filter(|r| !r.text.is_empty() && r.error.is_none())
        .collect();
    if valid.len() < 2 {
        return ValidateRoundOutcome::Skipped(ValidateSkipReason::InsufficientResponses);
    }

    let position_summary = match build_position_summary(responses) {
        Some(s) => s,
        None => return ValidateRoundOutcome::Skipped(ValidateSkipReason::InsufficientResponses),
    };

    // Local-code guard BEFORE paid evidence gather (xmcp / web).
    if let Some(reason) = should_skip_validator_llm(&position_summary, context, "") {
        if config.verbose {
            eprintln!("   ⏭️  Sheldon: {reason} — skipping validator");
        }
        return ValidateRoundOutcome::Skipped(ValidateSkipReason::LocalCodeNoContext);
    }

    evidence::check_available(config.verbose).await;
    let evidence_context =
        gather_evidence(topic, &valid, context, config.verbose, evidence_cache).await;

    // Evidence-only transports cannot improve an empty gather. Treat that as
    // a failed cascade step so a later native-search validator can recover.
    if evidence_context.is_empty()
        && !provider::validator_has_native_search(&config.provider, req_ctx)
    {
        if config.verbose {
            eprintln!(
                "   ⚠️  Validator {} has no supplied evidence; trying native-search fallback",
                config.provider
            );
        }
        return ValidateRoundOutcome::ProviderFailed;
    }

    let prompt = format!(
        "<topic>{}</topic>\n\n<deliberation round=\"{}\">\n{}\n</deliberation>\n{}\n\nExtract and validate the factual claims. JSON array only.",
        truncate(topic, 500),
        round_num,
        position_summary,
        evidence_context,
    );

    let system_prompt = sheldon_system_for_provider(config.provider.as_str());

    let model = config
        .model
        .clone()
        .unwrap_or_else(|| match config.provider.as_str() {
            "grok_build" => "grok-4.5".to_string(),
            "grok_hermes" => DEFAULT_GROK_HERMES_MODEL.to_string(),
            "grok" | "grok_cli" | "grok_api" => DEFAULT_GROK_MODEL.to_string(),
            "claude" | "claude_code" | "claude_api" => DEFAULT_CLAUDE_MODEL.to_string(),
            "gpt" | "codex_cli" | "openai_api" => DEFAULT_GPT_MODEL.to_string(),
            _ => "".to_string(),
        });

    let resp = provider::ask_validator(
        config.provider.as_str(),
        &prompt,
        system_prompt,
        &model,
        req_ctx,
    )
    .await;

    if resp.error.is_some() || resp.text.is_empty() {
        if config.verbose {
            eprintln!(
                "   ⚠️  Validator error: {}",
                resp.error.as_deref().unwrap_or("empty response")
            );
        }
        return ValidateRoundOutcome::ProviderFailed;
    }

    let mut report = match parse_json_report(&resp.text) {
        Some(r) => r,
        None => return ValidateRoundOutcome::ProviderFailed,
    };

    // Structural citation override (v9.13.2)
    let overrides = apply_citation_override(&mut report, citation_override_mode());

    if config.verbose {
        eprintln!(
            "   🔬 Validator ({}) — {}ms{}",
            resp.model,
            resp.latency_ms,
            if !evidence_context.is_empty() {
                " + evidence"
            } else {
                ""
            }
        );
        if overrides > 0 {
            eprintln!(
                "      🛡️  {} verdict(s) overridden to NO_EVIDENCE (no citations)",
                overrides
            );
        }
        for item in report.iter().take(8) {
            let icon = match item.verdict {
                ClaimVerdict::Supported => "✅",
                ClaimVerdict::Consistent => "🟡",
                ClaimVerdict::NoEvidence => "⚪",
                ClaimVerdict::Contradicted => "❌",
            };
            let impact = match item.impact {
                ClaimImpact::High => " [HIGH]",
                ClaimImpact::Medium => " [MEDIUM]",
                ClaimImpact::Low => " [LOW]",
                ClaimImpact::Unknown => "",
            };
            let override_tag = if item._overridden_from.is_some() {
                " ← overridden"
            } else {
                ""
            };
            let claim_short = truncate(&item.claim, 80);
            eprintln!(
                "      {} {:?}{}: {}{}",
                icon, item.verdict, impact, claim_short, override_tag
            );
        }
    }

    ValidateRoundOutcome::Ok(report, resp.cost_usd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::RequestContext;
    use crate::provider;
    use crate::types::RoleDefinition;

    #[test]

    fn claim_validator_ready_false_when_cascade_empty() {
        let role = RoleDefinition {
            description: String::new(),

            cascade: vec![],
        };

        assert!(!claim_validator_ready(&role, 1));
    }

    #[test]

    fn claim_validator_ready_true_when_cascade_populated() {
        use crate::types::RoleCascadeStep;

        let role = RoleDefinition {
            description: String::new(),

            cascade: vec![RoleCascadeStep {
                provider: "grok_cli".into(),

                model: "grok-4.3".into(),

                max_tokens: 512,
            }],
        };

        assert!(claim_validator_ready(&role, 1));
    }

    #[test]

    fn non_grok_validator_prompt_does_not_claim_web_tool() {
        let prompt = sheldon_system_for_provider("gpt");

        assert!(prompt.contains("ONLY the provided evidence sources"));

        assert!(!prompt.contains("USE YOUR WEB SEARCH TOOL"));

        assert!(prompt.contains("LOCAL CODE claims"));

        assert!(prompt.contains("verdict MUST be NO_EVIDENCE"));
    }

    #[test]

    fn grok_validator_prompt_retains_web_search_instruction() {
        let prompt = sheldon_system_for_provider("grok");

        assert!(prompt.contains("USE YOUR WEB SEARCH TOOL"));

        assert!(prompt.contains("do NOT use web search or model memory"));
    }

    #[test]

    fn grok_hermes_validator_uses_only_supplied_evidence() {
        let prompt = sheldon_system_for_provider("grok_hermes");

        assert!(prompt.contains("ONLY the provided evidence sources"));

        assert!(!prompt.contains("USE YOUR WEB SEARCH TOOL"));
    }

    #[test]

    fn native_search_validator_routes_are_explicit() {
        let direct = RequestContext {
            via_gateway: Some(false),

            ..RequestContext::default()
        };

        let governed = RequestContext {
            via_gateway: Some(true),

            ..RequestContext::default()
        };

        assert!(provider::validator_has_native_search("grok_build", &direct));

        assert!(provider::validator_has_native_search("grok_api", &direct));

        assert!(!provider::validator_has_native_search(
            "grok_hermes",
            &direct
        ));

        assert!(!provider::validator_has_native_search(
            "grok_build",
            &governed
        ));
    }
}
