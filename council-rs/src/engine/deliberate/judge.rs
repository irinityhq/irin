//! Convergence judging: participation gates, the structured judge round, and
//! judge usage accounting shared by the CLI engine and the streaming path.

use crate::engine::context::RequestContext;
use crate::provider;
use crate::text::truncate_utf8;
use crate::types::*;

use super::provider_auth_ready;
use super::utility_roles::convergence_judge_candidates;

const SUSPECT_QUALITY_CONVERGENCE_PENALTY: f64 = 0.15;
const MIN_VALID_SEAT_RESPONSES: usize = 2;
const MIN_VALID_PARTICIPATION_RATIO: f64 = 0.80;

fn suspect_quality_flag(flag: &str) -> bool {
    matches!(
        flag.trim().to_ascii_lowercase().as_str(),
        "thin" | "circular" | "off_topic"
    )
}

pub(crate) fn convergence_quality_penalty_enabled(validate: bool) -> bool {
    match std::env::var("COUNCIL_CONVERGENCE_QUALITY_PENALTY")
        .unwrap_or_else(|_| "validate".into())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "always" | "on" | "true" | "1" => true,
        "off" | "false" | "0" | "none" => false,
        _ => validate,
    }
}

pub(crate) fn effective_convergence_threshold(
    base_threshold: f64,
    assessment: Option<&JudgeAssessment>,
    apply_quality_penalty: bool,
) -> f64 {
    let mut thresh = base_threshold.clamp(0.0, 1.0);
    if apply_quality_penalty {
        let penalty = assessment
            .and_then(|a| a.quality_flag.as_deref())
            .filter(|flag| suspect_quality_flag(flag))
            .map(|_| SUSPECT_QUALITY_CONVERGENCE_PENALTY)
            .unwrap_or(0.0);
        thresh = (thresh + penalty).clamp(0.0, 1.0);
    }

    // Homogeneity / quick-agreement penalty (Bet 1 extension).
    // Raises effective threshold (makes "converged" harder) when seat outputs are
    // too similar (lexical Jaccard homogeneity >=0.75). Addresses model homogenization
    // / slop attractor without new primitives. Complements quality_flag path.
    if let Some(h) = assessment.and_then(|a| a.homogeneity_score)
        && h >= 0.75
    {
        let homo_p = 0.10 + ((h - 0.75) * 0.4).min(0.15);
        thresh = (thresh + homo_p).clamp(0.0, 1.0);
    }
    thresh
}

fn is_valid_seat_response(resp: &SeatResponse) -> bool {
    resp.error.is_none() && !resp.text.trim().is_empty()
}

fn participation_ratio(valid_count: usize, total_count: usize) -> f64 {
    if total_count == 0 {
        return 0.0;
    }
    valid_count as f64 / total_count as f64
}

fn participation_adjusted_score(raw_score: f64, valid_count: usize, total_count: usize) -> f64 {
    (raw_score.clamp(0.0, 1.0) * participation_ratio(valid_count, total_count)).clamp(0.0, 1.0)
}

fn compute_textual_homogeneity(texts: &[String]) -> f64 {
    if texts.len() < 2 {
        return 0.0;
    }
    let sets: Vec<std::collections::HashSet<String>> = texts
        .iter()
        .map(|t| {
            t.to_lowercase()
                .split_whitespace()
                .filter(|w| w.len() > 2)
                .map(|w| w.to_string())
                .collect()
        })
        .collect();
    let mut sims = vec![];
    for i in 0..sets.len() {
        for j in (i + 1)..sets.len() {
            let inter = sets[i].intersection(&sets[j]).count() as f64;
            let uni = sets[i].union(&sets[j]).count() as f64;
            if uni > 0.0 {
                sims.push(inter / uni);
            }
        }
    }
    if sims.is_empty() {
        0.0
    } else {
        sims.iter().sum::<f64>() / sims.len() as f64
    }
}

fn incomplete_participation_assessment(
    valid_count: usize,
    total_count: usize,
    convergence: f64,
) -> JudgeAssessment {
    JudgeAssessment {
        convergence,
        intent_aligned: false,
        drift: Some(format!(
            "incomplete seat participation: {valid_count}/{total_count} valid responses"
        )),
        quality_flag: Some("thin".into()),
        homogeneity_score: None,
        quick_agreement: None,
        recommendation: "continue".into(),
        confidence: 1.0,
    }
}

fn mark_incomplete_participation(
    mut assessment: JudgeAssessment,
    valid_count: usize,
    total_count: usize,
    convergence: f64,
) -> JudgeAssessment {
    assessment.convergence = convergence;
    if valid_count < total_count {
        assessment.quality_flag.get_or_insert_with(|| "thin".into());
        assessment.drift.get_or_insert_with(|| {
            format!("incomplete seat participation: {valid_count}/{total_count} valid responses")
        });
        if assessment.recommendation == "converged" {
            assessment.recommendation = "continue".into();
        }
    }
    assessment
}

/// Aggregate resource usage from every attempted convergence-judge candidate.
///
/// A cascade may fail over after a provider has already consumed time and
/// tokens. Those attempts are still real session usage and must be included in
/// the same totals and budget checks as the successful candidate.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JudgeUsage {
    pub tokens: u32,
    pub latency_ms: u64,
    pub cost_usd: f64,
}

impl JudgeUsage {
    fn record_response(
        &mut self,
        response: &ProviderResponse,
        candidate_model: &str,
        models: &ModelRegistry,
    ) {
        self.tokens = self
            .tokens
            .saturating_add(response.tokens_in.saturating_add(response.tokens_out));
        self.latency_ms = self.latency_ms.saturating_add(response.latency_ms);
        let billed_model = if response.model.trim().is_empty() {
            candidate_model
        } else {
            &response.model
        };
        self.cost_usd += models.estimate_cost(
            billed_model,
            response.tokens_in,
            response.tokens_out,
            response.cached_in,
        );
    }
}

fn record_judge_attempt(
    usage: &mut JudgeUsage,
    gateway_attempts: &mut Vec<crate::types::GatewayProvenance>,
    response: &ProviderResponse,
    candidate_model: &str,
    models: &ModelRegistry,
) {
    usage.record_response(response, candidate_model, models);
    if response.gateway_attempts.is_empty() {
        if let Some(provenance) = response
            .gateway_provenance
            .as_ref()
            .filter(|p| !p.gateway_request_id.is_empty())
        {
            gateway_attempts.push(provenance.clone());
        }
    } else {
        gateway_attempts.extend(
            response
                .gateway_attempts
                .iter()
                .filter(|p| !p.gateway_request_id.is_empty())
                .cloned(),
        );
    }
}

impl std::ops::AddAssign for JudgeUsage {
    fn add_assign(&mut self, other: Self) {
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.latency_ms = self.latency_ms.saturating_add(other.latency_ms);
        self.cost_usd += other.cost_usd;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JudgeRoundResult {
    pub score: f64,
    pub provider: Option<String>,
    pub assessment: Option<JudgeAssessment>,
    pub usage: JudgeUsage,
    pub gateway_attempts: Vec<crate::types::GatewayProvenance>,
}

impl JudgeRoundResult {
    pub(super) fn skipped() -> Self {
        Self {
            score: 1.0,
            provider: None,
            assessment: None,
            usage: JudgeUsage::default(),
            gateway_attempts: Vec::new(),
        }
    }
}

pub(crate) fn build_judge_prompt(
    valid: &[&SeatResponse],
    total_count: usize,
    topic: &str,
) -> String {
    let valid_count = valid.len();
    let mut summaries = String::new();
    for (i, resp) in valid.iter().enumerate() {
        let truncated = truncate_utf8(&resp.text, 500);
        summaries.push_str(&format!("- {}: {}\n", resp.seat_name, truncated));
        if i >= 4 {
            break;
        }
    }

    let topic_snippet = truncate_utf8(topic, 300);
    format!(
        "You are a convergence judge for a multi-model deliberation.\n\n\
         ORIGINAL TOPIC:\n{}\n\n\
         EXPECTED SEATS: {}\n\
         VALID RESPONSES: {}\n\
         FAILED OR EMPTY RESPONSES: {}\n\n\
         ANALYST POSITIONS ({} valid models):\n{}\n\n\
         Assess the deliberation and respond with ONLY this JSON object:\n\
         {{\"convergence\": <0.0-1.0>, \"intent_aligned\": <true/false>, \
         \"drift\": <null or \"description\">, \
         \"quality_flag\": <null or \"thin\" or \"circular\" or \"off_topic\">, \
         \"homogeneity_score\": <null or 0.0-1.0>, \
         \"quick_agreement\": <null or true/false>, \
         \"recommendation\": <\"continue\" or \"converged\" or \"escalate\" or \"reframe\">, \
         \"confidence\": <0.0-1.0>}}\n\n\
         Rules:\n\
         - convergence: 0.0 = total disagreement, 1.0 = perfect consensus\n\
         - Failed or empty responses count against convergence; do not ignore them.\n\
         - intent_aligned: did responses address the ORIGINAL topic?\n\
         - drift: null if no drift, otherwise describe what drifted\n\
         - quality_flag: null unless responses are thin/circular/off_topic\n\
         - recommendation: 'converged' if convergence >= 0.8\n\
         - confidence: your confidence in this assessment\n\n\
         Respond with ONLY the JSON. No explanation.",
        topic_snippet,
        total_count,
        valid_count,
        total_count.saturating_sub(valid_count),
        valid_count,
        summaries
    )
}

/// Structured convergence judge (v9.12.0).
///
/// Returns the score, provider, assessment, and accumulated usage from every
/// attempted cascade candidate.
/// Asks for JSON matching judge.v2 schema: {convergence, intent_aligned,
/// drift, quality_flag, recommendation, confidence}.
/// Falls back to naked float -> keyword heuristic on parse failure.
///
/// Shared with the streaming path. `req_ctx` carries the
/// per-session gateway override (feature contract) — the judge prompt contains round
/// content, so it must honor `via_gateway`/`sensitivity` like seat calls.
pub(crate) async fn judge_round(
    responses: &[SeatResponse],
    topic: &str,
    req_ctx: &RequestContext,
    roles: &crate::types::RolesConfig,
    models: &crate::types::ModelRegistry,
) -> JudgeRoundResult {
    let mut usage = JudgeUsage::default();
    let mut gateway_attempts = Vec::new();
    let total_count = responses.len();
    let valid: Vec<&SeatResponse> = responses
        .iter()
        .filter(|r| is_valid_seat_response(r))
        .collect();
    let valid_count = valid.len();

    let homo = if valid.len() >= 2 {
        let texts: Vec<String> = valid.iter().map(|r| r.text.clone()).collect();
        Some(compute_textual_homogeneity(&texts))
    } else {
        None
    };

    if total_count < MIN_VALID_SEAT_RESPONSES || valid_count < MIN_VALID_SEAT_RESPONSES {
        return JudgeRoundResult {
            score: 0.0,
            provider: None,
            assessment: Some(incomplete_participation_assessment(
                valid_count,
                total_count,
                0.0,
            )),
            usage,
            gateway_attempts,
        };
    }

    if participation_ratio(valid_count, total_count) < MIN_VALID_PARTICIPATION_RATIO {
        return JudgeRoundResult {
            score: 0.0,
            provider: None,
            assessment: Some(incomplete_participation_assessment(
                valid_count,
                total_count,
                0.0,
            )),
            usage,
            gateway_attempts,
        };
    }

    let prompt = build_judge_prompt(&valid, total_count, topic);

    let judge_configs = convergence_judge_candidates(roles, models);

    for candidate in judge_configs {
        if req_ctx.via_gateway != Some(true) && !provider_auth_ready(&candidate.provider) {
            continue;
        }

        let resp = provider::ask_with_opts_and_context(
            &candidate.provider,
            &prompt,
            "",
            &candidate.model,
            candidate.max_tok,
            req_ctx,
        )
        .await;
        record_judge_attempt(
            &mut usage,
            &mut gateway_attempts,
            &resp,
            &candidate.model,
            models,
        );
        if resp.error.is_some() {
            continue;
        }

        let text = resp.text.trim();
        let judge_text = if let Some(pos) = text.rfind("</reasoning>") {
            text[pos + "</reasoning>".len()..].trim()
        } else {
            text
        };

        // Attempt 1: Parse as structured JSON
        if let Some(assessment) = parse_judge_json(judge_text) {
            let score =
                participation_adjusted_score(assessment.convergence, valid_count, total_count);
            let mut assess =
                mark_incomplete_participation(assessment, valid_count, total_count, score);
            assess.homogeneity_score = homo;
            assess.quick_agreement = homo.map(|h| h >= 0.75);
            if assess.recommendation == "converged" && homo.is_some_and(|h| h >= 0.75) {
                assess.recommendation = "continue".into();
            }
            return JudgeRoundResult {
                score,
                provider: Some(candidate.provider.clone()),
                assessment: Some(assess),
                usage,
                gateway_attempts,
            };
        }

        // Attempt 2: Naked float extraction (last match in 0.0-1.0)
        let mut last_score: Option<f64> = None;
        for word in judge_text.split_whitespace() {
            let clean = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
            if let Ok(score) = clean.parse::<f64>()
                && (0.0..=1.0).contains(&score)
            {
                last_score = Some(score);
            }
        }
        if let Some(score) = last_score {
            let adj = participation_adjusted_score(score, valid_count, total_count);
            let assess = if valid_count < total_count {
                Some(incomplete_participation_assessment(
                    valid_count,
                    total_count,
                    adj,
                ))
            } else {
                Some(JudgeAssessment {
                    convergence: adj,
                    intent_aligned: true,
                    drift: None,
                    quality_flag: None,
                    homogeneity_score: homo,
                    quick_agreement: homo.map(|h| h >= 0.75),
                    recommendation: if adj >= 0.8 {
                        "converged".into()
                    } else {
                        "continue".into()
                    },
                    confidence: 0.6,
                })
            };
            return JudgeRoundResult {
                score: adj,
                provider: Some(candidate.provider.clone()),
                assessment: assess,
                usage,
                gateway_attempts,
            };
        }

        // Unparseable response from this candidate — try next provider.
    }

    // Fallback: keyword heuristic
    let agree_w = ["agree", "concur", "align", "support", "endorse"];
    let disagree_w = ["disagree", "oppose", "reject", "concern", "risk"];
    let mut positions = Vec::new();
    for r in &valid {
        let low = r.text.to_lowercase();
        let a: usize = agree_w.iter().filter(|w| low.contains(**w)).count();
        let d: usize = disagree_w.iter().filter(|w| low.contains(**w)).count();
        positions.push(if a > d {
            'a'
        } else if d > a {
            'd'
        } else {
            'n'
        });
    }
    let most_common = positions
        .iter()
        .fold(std::collections::HashMap::new(), |mut m, c| {
            *m.entry(c).or_insert(0) += 1;
            m
        })
        .into_values()
        .max()
        .unwrap_or(0);
    let raw_score = most_common as f64 / valid_count as f64;
    let adj = participation_adjusted_score(raw_score, valid_count, total_count);
    JudgeRoundResult {
        score: adj,
        provider: None,
        assessment: if valid_count < total_count {
            Some(incomplete_participation_assessment(
                valid_count,
                total_count,
                adj,
            ))
        } else {
            Some(JudgeAssessment {
                convergence: adj,
                intent_aligned: true,
                drift: None,
                quality_flag: None,
                homogeneity_score: homo,
                quick_agreement: homo.map(|h| h >= 0.75),
                recommendation: if adj >= 0.8 {
                    "converged".into()
                } else {
                    "continue".into()
                },
                confidence: 0.5,
            })
        },
        usage,
        gateway_attempts,
    }
}

/// Parse structured judge JSON. Returns None on failure.
pub(crate) fn parse_judge_json(text: &str) -> Option<JudgeAssessment> {
    let text = text.trim();
    // Strip markdown fences
    let text = if text.starts_with("```") {
        text.lines()
            .filter(|l| !l.starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let obj: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    // Validate required fields
    obj.get("convergence")?.as_f64()?;
    let rec = obj.get("recommendation")?.as_str()?;
    if !["continue", "converged", "escalate", "reframe"].contains(&rec) {
        return None;
    }
    serde_json::from_value(obj).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(text: &str) -> SeatResponse {
        SeatResponse {
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_or_whitespace_seats_are_not_valid_participation() {
        assert!(is_valid_seat_response(&seat("real answer")));
        assert!(!is_valid_seat_response(&seat("   \n\t")));

        let errored = SeatResponse {
            text: "real answer".into(),
            error: Some("provider failed".into()),
            ..Default::default()
        };
        assert!(!is_valid_seat_response(&errored));
    }

    #[test]
    fn participation_adjusted_score_keeps_empty_seats_in_denominator() {
        let score = participation_adjusted_score(1.0, 4, 5);
        assert!((score - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn incomplete_participation_marks_quality_and_blocks_converged_recommendation() {
        let assessment = mark_incomplete_participation(
            JudgeAssessment {
                convergence: 1.0,
                intent_aligned: true,
                drift: None,
                quality_flag: None,
                homogeneity_score: None,
                quick_agreement: None,
                recommendation: "converged".into(),
                confidence: 0.9,
            },
            4,
            5,
            0.8,
        );

        assert_eq!(assessment.convergence, 0.8);
        assert_eq!(assessment.quality_flag.as_deref(), Some("thin"));
        assert_eq!(assessment.recommendation, "continue");
        assert!(assessment.drift.unwrap().contains("4/5"));
    }

    #[test]
    fn judge_usage_counts_failed_and_successful_cascade_attempts() {
        let models = ModelRegistry {
            models: [
                (
                    "failed".to_string(),
                    ModelEntry {
                        id: "failed-model".into(),
                        provider: "test".into(),
                        description: String::new(),
                        pricing: ModelPricing {
                            input: 1.0,
                            cached_input: 0.5,
                            output: 2.0,
                        },
                    },
                ),
                (
                    "successful".to_string(),
                    ModelEntry {
                        id: "successful-model".into(),
                        provider: "test".into(),
                        description: String::new(),
                        pricing: ModelPricing {
                            input: 2.0,
                            cached_input: 1.0,
                            output: 4.0,
                        },
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let failed = ProviderResponse {
            model: String::new(),
            tokens_in: 100,
            tokens_out: 50,
            latency_ms: 125,
            error: Some("upstream rejected response".into()),
            gateway_provenance: Some(crate::types::GatewayProvenance {
                routed_model: "failed-model".into(),
                routed_provider: "first-provider".into(),
                fallback_used: false,
                gateway_request_id: "gw-failed-attempt".into(),
            }),
            gateway_attempts: vec![
                crate::types::GatewayProvenance {
                    routed_model: "failed-model".into(),
                    routed_provider: "first-provider".into(),
                    fallback_used: false,
                    gateway_request_id: "gw-rate-limited-attempt".into(),
                },
                crate::types::GatewayProvenance {
                    routed_model: "failed-model".into(),
                    routed_provider: "first-provider".into(),
                    fallback_used: false,
                    gateway_request_id: "gw-failed-attempt".into(),
                },
            ],
            ..Default::default()
        };
        let successful = ProviderResponse {
            model: "successful-model".into(),
            tokens_in: 100,
            tokens_out: 50,
            latency_ms: 275,
            gateway_provenance: Some(crate::types::GatewayProvenance {
                routed_model: "successful-model".into(),
                routed_provider: "second-provider".into(),
                fallback_used: false,
                gateway_request_id: "gw-successful-attempt".into(),
            }),
            ..Default::default()
        };

        let mut usage = JudgeUsage::default();
        let mut gateway_attempts = Vec::new();
        record_judge_attempt(
            &mut usage,
            &mut gateway_attempts,
            &failed,
            "failed-model",
            &models,
        );
        record_judge_attempt(
            &mut usage,
            &mut gateway_attempts,
            &successful,
            "ignored-candidate-model",
            &models,
        );

        assert_eq!(usage.tokens, 300);
        assert_eq!(usage.latency_ms, 400);
        assert!((usage.cost_usd - 0.0006).abs() < f64::EPSILON);
        assert_eq!(gateway_attempts.len(), 3);
        assert_eq!(
            gateway_attempts[0].gateway_request_id,
            "gw-rate-limited-attempt"
        );
        assert_eq!(gateway_attempts[1].gateway_request_id, "gw-failed-attempt");
        assert_eq!(
            gateway_attempts[2].gateway_request_id,
            "gw-successful-attempt"
        );
    }

    #[test]
    fn judge_usage_accumulates_once_across_rounds() {
        let mut session_usage = JudgeUsage {
            tokens: 30,
            latency_ms: 40,
            cost_usd: 0.001,
        };
        session_usage += JudgeUsage {
            tokens: 50,
            latency_ms: 60,
            cost_usd: 0.002,
        };

        assert_eq!(session_usage.tokens, 80);
        assert_eq!(session_usage.latency_ms, 100);
        assert!((session_usage.cost_usd - 0.003).abs() < f64::EPSILON);
    }

    #[test]
    fn homogeneity_threshold_raises_convergence() {
        let base = 0.8;
        let assess = JudgeAssessment {
            convergence: 0.9,
            intent_aligned: true,
            drift: None,
            quality_flag: None,
            homogeneity_score: Some(0.8),
            quick_agreement: Some(true),
            recommendation: "converged".into(),
            confidence: 0.9,
        };
        let eff = effective_convergence_threshold(base, Some(&assess), true);
        assert!(eff > base, "expected threshold bump on high homogeneity");
    }
}
