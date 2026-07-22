//! Live eval harness for utility roles (convergence judge + frame-check).
//!
//! Invoked by `council --judge-eval` and `scripts/judge_eval.py --live`.
//! Pins models via `COUNCIL_JUDGE_*` / `COUNCIL_FRAME_CHECK_*` env vars.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sovereign_protocol::types::SeatResponse;

use crate::config::Config;
use crate::engine::context::RequestContext;
use crate::engine::deliberate::{
    CascadeCandidate, convergence_judge_candidates, frame_check_candidates, parse_judge_json,
    provider_auth_ready,
};
use crate::provider;
use crate::types::JudgeAssessment;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeSeatFixture {
    pub seat: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeFixture {
    pub id: String,
    pub topic: String,
    pub seats: Vec<JudgeSeatFixture>,
    pub human_convergence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameFixture {
    pub id: String,
    pub prompt: String,
    pub expect: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeEvalFixtures {
    #[serde(default)]
    pub judge: Vec<JudgeFixture>,
    #[serde(default)]
    pub frame: Vec<FrameFixture>,
}

impl JudgeEvalFixtures {
    pub fn built_in() -> Self {
        let raw = include_str!("../../eval/fixtures/judge_eval.json");
        serde_json::from_str(raw).expect("embedded judge_eval fixtures must parse")
    }
}

pub fn load_fixtures(base_dir: &Path) -> Result<JudgeEvalFixtures> {
    let path = base_dir.join("eval/fixtures/judge_eval.json");
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Reading {}", path.display()))?;
        return serde_json::from_str(&content)
            .with_context(|| format!("Parsing {}", path.display()));
    }
    Ok(JudgeEvalFixtures::built_in())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalRole {
    Judge,
    Frame,
    Both,
}

impl EvalRole {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "judge" | "convergence" | "convergence_judge" => Ok(Self::Judge),
            "frame" | "frame_check" => Ok(Self::Frame),
            "both" | "all" => Ok(Self::Both),
            other => {
                anyhow::bail!("unknown --judge-eval-role {other:?} (use judge, frame, or both)")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParseMode {
    Json,
    Float,
    Unparseable,
    ProviderError,
    NoAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleEvalResult {
    pub fixture_id: String,
    pub role: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub parse_mode: ParseMode,
    pub ok: bool,
    pub latency_ms: u64,
    pub cost_usd: f64,
    pub human_convergence: Option<f64>,
    pub predicted_convergence: Option<f64>,
    pub calibration_error: Option<f64>,
    pub frame_expect: Option<String>,
    pub frame_got: Option<String>,
    pub assessment: Option<JudgeAssessment>,
    pub raw_text: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub results: Vec<RoleEvalResult>,
    pub summary: EvalSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSummary {
    pub total: usize,
    pub json_parse_ok: usize,
    pub frame_format_ok: usize,
    pub frame_expect_match: usize,
    pub provider_errors: usize,
    pub mean_calibration_error: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct EvalOpts {
    pub role: EvalRole,
    pub fixture_id: Option<String>,
    pub judge_provider: Option<String>,
    pub judge_model: Option<String>,
    pub frame_provider: Option<String>,
    pub frame_model: Option<String>,
}

fn pin_candidate(
    provider: Option<&str>,
    model: Option<&str>,
    fallback: &CascadeCandidate,
) -> CascadeCandidate {
    CascadeCandidate {
        provider: provider
            .map(str::to_string)
            .unwrap_or_else(|| fallback.provider.clone()),
        model: model
            .map(str::to_string)
            .unwrap_or_else(|| fallback.model.clone()),
        max_tok: fallback.max_tok,
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_valid_seat_response(resp: &SeatResponse) -> bool {
    !resp.text.trim().is_empty() && resp.error.is_none()
}

fn build_judge_prompt(responses: &[SeatResponse], topic: &str) -> (usize, usize, String) {
    let total_count = responses.len();
    let valid: Vec<&SeatResponse> = responses
        .iter()
        .filter(|r| is_valid_seat_response(r))
        .collect();
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
    let prompt = format!(
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
    );
    (total_count, valid_count, prompt)
}

fn classify_judge_text(text: &str) -> (ParseMode, Option<JudgeAssessment>, Option<f64>) {
    let judge_text = if let Some(pos) = text.rfind("</reasoning>") {
        text[pos + "</reasoning>".len()..].trim()
    } else {
        text.trim()
    };

    if let Some(assessment) = parse_judge_json(judge_text) {
        return (
            ParseMode::Json,
            Some(assessment.clone()),
            Some(assessment.convergence),
        );
    }

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
        return (ParseMode::Float, None, Some(score));
    }

    (ParseMode::Unparseable, None, None)
}

async fn eval_judge_candidate(
    fixture: &JudgeFixture,
    candidate: &CascadeCandidate,
    req_ctx: &RequestContext,
) -> RoleEvalResult {
    let responses: Vec<SeatResponse> = fixture
        .seats
        .iter()
        .map(|s| SeatResponse {
            seat_name: s.seat.clone(),
            text: s.text.clone(),
            ..Default::default()
        })
        .collect();

    if !provider_auth_ready(&candidate.provider) {
        return RoleEvalResult {
            fixture_id: fixture.id.clone(),
            role: "convergence_judge".into(),
            provider: Some(candidate.provider.clone()),
            model: Some(candidate.model.clone()),
            parse_mode: ParseMode::NoAuth,
            ok: false,
            latency_ms: 0,
            cost_usd: 0.0,
            human_convergence: Some(fixture.human_convergence),
            predicted_convergence: None,
            calibration_error: None,
            frame_expect: None,
            frame_got: None,
            assessment: None,
            raw_text: None,
            error: Some(format!("provider {} not authenticated", candidate.provider)),
        };
    }

    let (_, _, prompt) = build_judge_prompt(&responses, &fixture.topic);
    let resp = provider::ask_with_opts_and_context(
        &candidate.provider,
        &prompt,
        "",
        &candidate.model,
        candidate.max_tok,
        req_ctx,
    )
    .await;

    if let Some(err) = resp.error.clone() {
        return RoleEvalResult {
            fixture_id: fixture.id.clone(),
            role: "convergence_judge".into(),
            provider: Some(candidate.provider.clone()),
            model: Some(resp.model.clone()),
            parse_mode: ParseMode::ProviderError,
            ok: false,
            latency_ms: resp.latency_ms,
            cost_usd: resp.cost_usd,
            human_convergence: Some(fixture.human_convergence),
            predicted_convergence: None,
            calibration_error: None,
            frame_expect: None,
            frame_got: None,
            assessment: None,
            raw_text: Some(resp.text),
            error: Some(err),
        };
    }

    let (parse_mode, assessment, predicted) = classify_judge_text(&resp.text);
    let calibration_error = predicted.map(|p| (p - fixture.human_convergence).abs());
    let ok = parse_mode == ParseMode::Json;

    RoleEvalResult {
        fixture_id: fixture.id.clone(),
        role: "convergence_judge".into(),
        provider: Some(candidate.provider.clone()),
        model: Some(resp.model.clone()),
        parse_mode,
        ok,
        latency_ms: resp.latency_ms,
        cost_usd: resp.cost_usd,
        human_convergence: Some(fixture.human_convergence),
        predicted_convergence: predicted,
        calibration_error,
        frame_expect: None,
        frame_got: None,
        assessment,
        raw_text: Some(resp.text.chars().take(2000).collect()),
        error: None,
    }
}

fn build_frame_scan_prompt(prompt: &str) -> String {
    let truncated = if prompt.len() > 3000 {
        truncate_utf8(prompt, 3000)
    } else {
        prompt
    };
    format!(
        "You are a constraint auditor. Read the following deliberation \
         prompt and list every stated constraint, negation, or assumption \
         presented as fact. Focus on phrases like:\n\
         - 'we don't have X' / 'without X' / 'X is not available'\n\
         - 'given only Y' / 'limited to Y'\n\
         - 'there is no Z' / 'Z doesn't exist'\n\
         - 'the only option is W'\n\n\
         For each, output ONE LINE in this exact format:\n\
         ASSUMPTION: <quoted phrase> | VERIFY: <what to check>\n\n\
         If no embedded assumptions are found, respond with exactly: CLEAN\n\n\
         ---\n{}\n---",
        truncated
    )
}

fn classify_frame_output(text: &str) -> (String, bool) {
    let trimmed = text.trim();
    let upper = trimmed.to_uppercase();
    if upper.starts_with("CLEAN") {
        return ("CLEAN".into(), true);
    }
    let has_assumption = trimmed
        .lines()
        .any(|l| l.trim().to_uppercase().starts_with("ASSUMPTION:"));
    if has_assumption {
        return ("ASSUMPTION".into(), true);
    }
    ("UNPARSEABLE".into(), false)
}

async fn eval_frame_candidate(
    fixture: &FrameFixture,
    candidate: &CascadeCandidate,
    req_ctx: &RequestContext,
) -> RoleEvalResult {
    if !provider_auth_ready(&candidate.provider) {
        return RoleEvalResult {
            fixture_id: fixture.id.clone(),
            role: "frame_check".into(),
            provider: Some(candidate.provider.clone()),
            model: Some(candidate.model.clone()),
            parse_mode: ParseMode::NoAuth,
            ok: false,
            latency_ms: 0,
            cost_usd: 0.0,
            human_convergence: None,
            predicted_convergence: None,
            calibration_error: None,
            frame_expect: Some(fixture.expect.clone()),
            frame_got: None,
            assessment: None,
            raw_text: None,
            error: Some(format!("provider {} not authenticated", candidate.provider)),
        };
    }

    let scan_prompt = build_frame_scan_prompt(&fixture.prompt);
    let resp = provider::ask_with_opts_and_context(
        &candidate.provider,
        &scan_prompt,
        "",
        &candidate.model,
        candidate.max_tok,
        req_ctx,
    )
    .await;

    if let Some(err) = resp.error.clone() {
        return RoleEvalResult {
            fixture_id: fixture.id.clone(),
            role: "frame_check".into(),
            provider: Some(candidate.provider.clone()),
            model: Some(resp.model.clone()),
            parse_mode: ParseMode::ProviderError,
            ok: false,
            latency_ms: resp.latency_ms,
            cost_usd: resp.cost_usd,
            human_convergence: None,
            predicted_convergence: None,
            calibration_error: None,
            frame_expect: Some(fixture.expect.clone()),
            frame_got: None,
            assessment: None,
            raw_text: Some(resp.text),
            error: Some(err),
        };
    }

    let (got, format_ok) = classify_frame_output(&resp.text);
    let expect_match = fixture.expect.eq_ignore_ascii_case(&got);
    let ok = format_ok && expect_match;

    RoleEvalResult {
        fixture_id: fixture.id.clone(),
        role: "frame_check".into(),
        provider: Some(candidate.provider.clone()),
        model: Some(resp.model.clone()),
        parse_mode: if format_ok {
            ParseMode::Json
        } else {
            ParseMode::Unparseable
        },
        ok,
        latency_ms: resp.latency_ms,
        cost_usd: resp.cost_usd,
        human_convergence: None,
        predicted_convergence: None,
        calibration_error: None,
        frame_expect: Some(fixture.expect.clone()),
        frame_got: Some(got),
        assessment: None,
        raw_text: Some(resp.text.chars().take(2000).collect()),
        error: None,
    }
}

fn first_candidate(candidates: &[CascadeCandidate]) -> Option<&CascadeCandidate> {
    candidates
        .iter()
        .find(|c| provider_auth_ready(&c.provider))
        .or(candidates.first())
}

fn summarize(results: &[RoleEvalResult]) -> EvalSummary {
    let total = results.len();
    let json_parse_ok = results
        .iter()
        .filter(|r| r.role == "convergence_judge" && r.parse_mode == ParseMode::Json)
        .count();
    let frame_format_ok = results
        .iter()
        .filter(|r| r.role == "frame_check" && r.parse_mode == ParseMode::Json)
        .count();
    let frame_expect_match = results
        .iter()
        .filter(|r| r.role == "frame_check" && r.ok)
        .count();
    let provider_errors = results
        .iter()
        .filter(|r| r.parse_mode == ParseMode::ProviderError)
        .count();
    let cal_errors: Vec<f64> = results.iter().filter_map(|r| r.calibration_error).collect();
    let mean_calibration_error = if cal_errors.is_empty() {
        None
    } else {
        Some(cal_errors.iter().sum::<f64>() / cal_errors.len() as f64)
    };

    EvalSummary {
        total,
        json_parse_ok,
        frame_format_ok,
        frame_expect_match,
        provider_errors,
        mean_calibration_error,
    }
}

pub async fn run_eval(
    config: &Config,
    opts: EvalOpts,
    req_ctx: &RequestContext,
) -> Result<EvalReport> {
    let fixtures = load_fixtures(&config.base_dir)?;
    let roles = &config.roles;
    let models = &config.models;
    let mut results = Vec::new();

    let judge_candidates = convergence_judge_candidates(roles, models);
    let frame_candidates = frame_check_candidates(roles, models);

    if matches!(opts.role, EvalRole::Judge | EvalRole::Both) {
        let judge_fallback = first_candidate(&judge_candidates)
            .context("no convergence_judge candidates in roles config")?;
        let judge_candidate = pin_candidate(
            opts.judge_provider.as_deref(),
            opts.judge_model.as_deref(),
            judge_fallback,
        );
        for fixture in &fixtures.judge {
            if let Some(ref id) = opts.fixture_id
                && id != &fixture.id
            {
                continue;
            }
            results.push(eval_judge_candidate(fixture, &judge_candidate, req_ctx).await);
        }
    }

    if matches!(opts.role, EvalRole::Frame | EvalRole::Both) {
        let frame_fallback = first_candidate(&frame_candidates)
            .context("no frame_check candidates in roles config")?;
        let frame_candidate = pin_candidate(
            opts.frame_provider.as_deref(),
            opts.frame_model.as_deref(),
            frame_fallback,
        );
        for fixture in &fixtures.frame {
            if let Some(ref id) = opts.fixture_id
                && id != &fixture.id
            {
                continue;
            }
            results.push(eval_frame_candidate(fixture, &frame_candidate, req_ctx).await);
        }
    }

    let summary = summarize(&results);
    Ok(EvalReport { results, summary })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_fixtures_parse() {
        let f = JudgeEvalFixtures::built_in();
        assert_eq!(f.judge.len(), 3);
        assert_eq!(f.frame.len(), 2);
    }

    #[test]
    fn classify_judge_json_response() {
        let text = r#"{"convergence":0.82,"intent_aligned":true,"drift":null,"quality_flag":null,"recommendation":"converged","confidence":0.9}"#;
        let (mode, _, score) = classify_judge_text(text);
        assert_eq!(mode, ParseMode::Json);
        assert!((score.unwrap() - 0.82).abs() < f64::EPSILON);
    }

    #[test]
    fn classify_frame_clean_and_assumption() {
        let (got, ok) = classify_frame_output("CLEAN");
        assert_eq!(got, "CLEAN");
        assert!(ok);
        let (got, ok) =
            classify_frame_output("ASSUMPTION: no database | VERIFY: check persistence layer");
        assert_eq!(got, "ASSUMPTION");
        assert!(ok);
    }
}
