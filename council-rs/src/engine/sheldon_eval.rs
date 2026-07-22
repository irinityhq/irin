//! Live eval harness for Sheldon claim validator (claim_validator role).
//!
//! Invoked by `council --sheldon-eval` and `scripts/sheldon_eval.py --live`.
//! Scoped fixtures (`skip_scoped`) run without API calls; others require `--sheldon-eval-live`.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sovereign_protocol::types::SeatResponse;

use crate::config::Config;
use crate::engine::context::RequestContext;
use crate::engine::deliberate::provider_auth_ready;
use crate::engine::sheldon::{self, ValidatorConfig};
use crate::types::ClaimVerdict;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheldonSeatFixture {
    pub seat: String,
    pub text: String,
    #[serde(default = "default_seat_provider")]
    pub provider: String,
}

fn default_seat_provider() -> String {
    "grok".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheldonFixture {
    pub id: String,
    pub topic: String,
    #[serde(default)]
    pub context: String,
    pub seats: Vec<SheldonSeatFixture>,
    /// skip_scoped | report | no_report
    pub expect: String,
    #[serde(default)]
    pub min_claims: Option<u32>,
    #[serde(default)]
    pub max_contradicted: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheldonEvalFixtures {
    pub fixtures: Vec<SheldonFixture>,
}

impl SheldonEvalFixtures {
    pub fn built_in() -> Self {
        let raw = include_str!("../../eval/fixtures/sheldon_eval.json");
        serde_json::from_str(raw).expect("embedded sheldon_eval fixtures must parse")
    }
}

pub fn load_fixtures(base_dir: &Path) -> Result<SheldonEvalFixtures> {
    let path = base_dir.join("eval/fixtures/sheldon_eval.json");
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Reading {}", path.display()))?;
        return serde_json::from_str(&content)
            .with_context(|| format!("Parsing {}", path.display()));
    }
    Ok(SheldonEvalFixtures::built_in())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SheldonOutcome {
    SkipScoped,
    NoReport,
    Report,
    SkippedNotLive,
    NoAuth,
    ProviderError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheldonEvalResult {
    pub fixture_id: String,
    pub provider: String,
    pub model: Option<String>,
    pub outcome: SheldonOutcome,
    pub ok: bool,
    pub claim_count: u32,
    pub contradicted_count: u32,
    pub override_count: u32,
    pub latency_ms: u64,
    pub cost_usd: f64,
    pub error: Option<String>,
    /// For cascade failover support: the provider/step that actually produced the validation result (or the attempted one).
    #[serde(default)]
    pub succeeded_provider: Option<String>,
    #[serde(default)]
    pub cascade_step: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheldonEvalSummary {
    pub total: usize,
    pub ok: usize,
    pub skip_scoped_ok: usize,
    pub live_report_ok: usize,
    pub provider_errors: usize,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheldonEvalReport {
    pub results: Vec<SheldonEvalResult>,
    pub summary: SheldonEvalSummary,
}

#[derive(Debug, Clone)]
pub struct SheldonEvalOpts {
    pub fixture_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub live: bool,
    pub scoped_only: bool,
}

fn fixture_responses(fixture: &SheldonFixture) -> Vec<SeatResponse> {
    fixture
        .seats
        .iter()
        .map(|s| SeatResponse {
            seat_name: s.seat.clone(),
            provider: s.provider.clone(),
            text: s.text.clone(),
            ..Default::default()
        })
        .collect()
}

fn count_overrides(report: &[crate::types::ClaimVerdictEntry]) -> u32 {
    report
        .iter()
        .filter(|e| e._overridden_from.is_some())
        .count() as u32
}

fn count_contradicted(report: &[crate::types::ClaimVerdictEntry]) -> u32 {
    report
        .iter()
        .filter(|e| e.verdict == ClaimVerdict::Contradicted)
        .count() as u32
}

fn check_expect(
    fixture: &SheldonFixture,
    outcome: &SheldonOutcome,
    claim_count: u32,
    contradicted_count: u32,
) -> bool {
    match fixture.expect.as_str() {
        "skip_scoped" => *outcome == SheldonOutcome::SkipScoped,
        "report" => {
            *outcome == SheldonOutcome::Report
                && claim_count >= fixture.min_claims.unwrap_or(1)
                && fixture
                    .max_contradicted
                    .is_none_or(|max| contradicted_count <= max)
        }
        "no_report" => *outcome == SheldonOutcome::NoReport,
        other => {
            eprintln!("⚠️  unknown fixture expect {other:?} for {}", fixture.id);
            false
        }
    }
}

async fn eval_fixture(
    fixture: &SheldonFixture,
    provider: &str,
    model: Option<String>,
    live: bool,
    req_ctx: &RequestContext,
) -> SheldonEvalResult {
    let responses = fixture_responses(fixture);

    if fixture.expect == "skip_scoped" {
        let position_summary = sheldon::build_position_summary(&responses);
        let skipped = position_summary.is_some_and(|summary| {
            sheldon::would_skip_local_without_context(&summary, &fixture.context, "")
        });
        let outcome = if skipped {
            SheldonOutcome::SkipScoped
        } else {
            SheldonOutcome::NoReport
        };
        let ok = check_expect(fixture, &outcome, 0, 0);
        return SheldonEvalResult {
            fixture_id: fixture.id.clone(),
            provider: provider.to_string(),
            model: model.clone(),
            outcome,
            ok,
            claim_count: 0,
            contradicted_count: 0,
            override_count: 0,
            latency_ms: 0,
            cost_usd: 0.0,
            error: if ok {
                None
            } else {
                Some("expected skip_scoped but guard did not fire".into())
            },
            succeeded_provider: Some(provider.to_string()),
            cascade_step: None,
        };
    }

    if !live {
        return SheldonEvalResult {
            fixture_id: fixture.id.clone(),
            provider: provider.to_string(),
            model: model.clone(),
            outcome: SheldonOutcome::SkippedNotLive,
            ok: false,
            claim_count: 0,
            contradicted_count: 0,
            override_count: 0,
            latency_ms: 0,
            cost_usd: 0.0,
            error: Some("pass --sheldon-eval-live for non-scoped fixtures".into()),
            succeeded_provider: Some(provider.to_string()),
            cascade_step: None,
        };
    }

    if !provider_auth_ready(provider) {
        return SheldonEvalResult {
            fixture_id: fixture.id.clone(),
            provider: provider.to_string(),
            model: model.clone(),
            outcome: SheldonOutcome::NoAuth,
            ok: false,
            claim_count: 0,
            contradicted_count: 0,
            override_count: 0,
            latency_ms: 0,
            cost_usd: 0.0,
            error: Some(format!("provider {provider} not authenticated")),
            succeeded_provider: Some(provider.to_string()),
            cascade_step: None,
        };
    }

    let vcfg = ValidatorConfig {
        provider: provider.to_string(),
        model: model.clone(),
        gate: false,
        verbose: false,
    };

    let started = Instant::now();
    let val_result = sheldon::validate_round(
        &responses,
        &fixture.topic,
        &fixture.context,
        1,
        &vcfg,
        req_ctx,
        None,
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match val_result {
        sheldon::ValidateRoundOutcome::Skipped(_) => {
            let outcome = SheldonOutcome::SkipScoped;
            let ok = check_expect(fixture, &outcome, 0, 0);
            SheldonEvalResult {
                fixture_id: fixture.id.clone(),
                provider: provider.to_string(),
                model: model.clone(),
                outcome,
                ok,
                claim_count: 0,
                contradicted_count: 0,
                override_count: 0,
                latency_ms,
                cost_usd: 0.0,
                error: if ok {
                    None
                } else {
                    Some("unexpected skip".into())
                },
                succeeded_provider: Some(provider.to_string()),
                cascade_step: None,
            }
        }
        sheldon::ValidateRoundOutcome::ProviderFailed => {
            let outcome = SheldonOutcome::ProviderError;
            let ok = check_expect(fixture, &outcome, 0, 0);
            SheldonEvalResult {
                fixture_id: fixture.id.clone(),
                provider: provider.to_string(),
                model: model.clone(),
                outcome,
                ok,
                claim_count: 0,
                contradicted_count: 0,
                override_count: 0,
                latency_ms,
                cost_usd: 0.0,
                error: if ok {
                    None
                } else {
                    Some("expected report but validator failed".into())
                },
                succeeded_provider: Some(provider.to_string()),
                cascade_step: None,
            }
        }
        sheldon::ValidateRoundOutcome::Ok(report, cost) => {
            let claim_count = report.len() as u32;
            let contradicted_count = count_contradicted(&report);
            let override_count = count_overrides(&report);
            let outcome = SheldonOutcome::Report;
            let ok = check_expect(fixture, &outcome, claim_count, contradicted_count);
            SheldonEvalResult {
                fixture_id: fixture.id.clone(),
                provider: provider.to_string(),
                model: model.clone(),
                outcome,
                ok,
                claim_count,
                contradicted_count,
                override_count,
                latency_ms,
                cost_usd: cost,
                error: None,
                succeeded_provider: Some(provider.to_string()),
                cascade_step: None,
            }
        }
    }
}

fn summarize(results: &[SheldonEvalResult]) -> SheldonEvalSummary {
    let total = results.len();
    let ok = results.iter().filter(|r| r.ok).count();
    let skip_scoped_ok = results
        .iter()
        .filter(|r| r.outcome == SheldonOutcome::SkipScoped && r.ok)
        .count();
    let live_report_ok = results
        .iter()
        .filter(|r| r.outcome == SheldonOutcome::Report && r.ok)
        .count();
    let provider_errors = results
        .iter()
        .filter(|r| {
            matches!(
                r.outcome,
                SheldonOutcome::ProviderError | SheldonOutcome::NoAuth
            )
        })
        .count();
    let total_cost_usd = results.iter().map(|r| r.cost_usd).sum();

    SheldonEvalSummary {
        total,
        ok,
        skip_scoped_ok,
        live_report_ok,
        provider_errors,
        total_cost_usd,
    }
}

pub async fn run_eval(
    config: &Config,
    opts: SheldonEvalOpts,
    req_ctx: &RequestContext,
) -> Result<SheldonEvalReport> {
    let fixtures = load_fixtures(&config.base_dir)?;
    let claim_role = &config.roles.claim_validator;

    let use_cascade = opts.provider.is_none();
    let default_step = claim_role
        .cascade
        .first()
        .context("no claim_validator cascade in roles.yaml")?;
    let pinned_provider = opts
        .provider
        .clone()
        .unwrap_or_else(|| default_step.provider.clone());
    let pinned_model = opts
        .model
        .clone()
        .or_else(|| Some(default_step.model.clone()));

    let mut results = Vec::new();
    for fixture in &fixtures.fixtures {
        if let Some(ref id) = opts.fixture_id
            && id != &fixture.id
        {
            continue;
        }
        if opts.scoped_only && fixture.expect != "skip_scoped" {
            continue;
        }

        if use_cascade && opts.live && fixture.expect != "skip_scoped" {
            // Cascade failover support in eval: try claim_validator steps until one
            // produces Some(report), like the production path in deliberate.rs.
            // This lets --sheldon-eval (no --eval-provider) gracefully use grok_cli -> grok etc.
            let mut chosen: Option<SheldonEvalResult> = None;
            for (step_idx, step) in claim_role.cascade.iter().enumerate() {
                let p = step.provider.clone();
                let m = Some(step.model.clone());
                if !provider_auth_ready(&p) {
                    continue;
                }
                let responses = fixture_responses(fixture);
                let vcfg = ValidatorConfig {
                    provider: p.clone(),
                    model: m.clone(),
                    gate: false,
                    verbose: false,
                };
                let started = Instant::now();
                let val_res = sheldon::validate_round(
                    &responses,
                    &fixture.topic,
                    &fixture.context,
                    1,
                    &vcfg,
                    req_ctx,
                    None,
                )
                .await;
                let lat = started.elapsed().as_millis() as u64;
                let res = match val_res {
                    sheldon::ValidateRoundOutcome::Skipped(_) => {
                        let outcome = SheldonOutcome::SkipScoped;
                        let ok = check_expect(fixture, &outcome, 0, 0);
                        SheldonEvalResult {
                            fixture_id: fixture.id.clone(),
                            provider: p.clone(),
                            model: m.clone(),
                            outcome,
                            ok,
                            claim_count: 0,
                            contradicted_count: 0,
                            override_count: 0,
                            latency_ms: lat,
                            cost_usd: 0.0,
                            error: None,
                            succeeded_provider: Some(p.clone()),
                            cascade_step: Some(step_idx),
                        }
                    }
                    sheldon::ValidateRoundOutcome::ProviderFailed => {
                        let outcome = SheldonOutcome::NoReport;
                        let ok = check_expect(fixture, &outcome, 0, 0);
                        SheldonEvalResult {
                            fixture_id: fixture.id.clone(),
                            provider: p.clone(),
                            model: m.clone(),
                            outcome,
                            ok,
                            claim_count: 0,
                            contradicted_count: 0,
                            override_count: 0,
                            latency_ms: lat,
                            cost_usd: 0.0,
                            error: Some("no report from step (cascade continued)".into()),
                            succeeded_provider: Some(p.clone()),
                            cascade_step: Some(step_idx),
                        }
                    }
                    sheldon::ValidateRoundOutcome::Ok(report, cost) => {
                        let claim_count = report.len() as u32;
                        let contradicted_count = count_contradicted(&report);
                        let override_count = count_overrides(&report);
                        let outcome = SheldonOutcome::Report;
                        let ok = check_expect(fixture, &outcome, claim_count, contradicted_count);
                        SheldonEvalResult {
                            fixture_id: fixture.id.clone(),
                            provider: p.clone(),
                            model: m.clone(),
                            outcome,
                            ok,
                            claim_count,
                            contradicted_count,
                            override_count,
                            latency_ms: lat,
                            cost_usd: cost,
                            error: None,
                            succeeded_provider: Some(p.clone()),
                            cascade_step: Some(step_idx),
                        }
                    }
                };
                if res.outcome == SheldonOutcome::Report {
                    chosen = Some(res);
                    break;
                }
                chosen = Some(res);
            }
            if let Some(r) = chosen {
                results.push(r);
                continue;
            }
        }

        // Pinned single-provider path (used for --provider / matrix compares of grok_cli vs grok)
        let mut r = eval_fixture(
            fixture,
            &pinned_provider,
            pinned_model.clone(),
            opts.live,
            req_ctx,
        )
        .await;
        if r.succeeded_provider.is_none() {
            r.succeeded_provider = Some(pinned_provider.clone());
        }
        if r.cascade_step.is_none() {
            r.cascade_step = Some(0);
        }
        results.push(r);
    }

    Ok(SheldonEvalReport {
        summary: summarize(&results),
        results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_fixtures_parse() {
        let f = SheldonEvalFixtures::built_in();
        assert!(f.fixtures.len() >= 5);
    }

    #[test]
    fn skip_scoped_fixture_is_deterministic() {
        let fixtures = SheldonEvalFixtures::built_in();
        let fixture = fixtures
            .fixtures
            .iter()
            .find(|f| f.id == "local_no_map")
            .expect("local_no_map fixture");
        let responses = fixture_responses(fixture);
        let summary = sheldon::build_position_summary(&responses).unwrap();
        assert!(sheldon::would_skip_local_without_context(
            &summary,
            &fixture.context,
            ""
        ));
    }

    #[test]
    fn harness_covers_scoped_guard_interaction() {
        let fixtures = SheldonEvalFixtures::built_in();
        // scoping guard interaction exercised by skip_scoped path in eval_fixture
        let has_scoped = fixtures.fixtures.iter().any(|f| f.expect == "skip_scoped");
        assert!(has_scoped);
    }
}
