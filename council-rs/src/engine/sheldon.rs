//! Sheldon — between-round claim validator (v9.13).
//!
//! Extracts factual claims from seat outputs, gathers evidence from web
//! tools and live X posts (via xmcp), sends to a validator model, and
//! returns structured verdicts. Three anti-hallucination guardrails:
//!
//! 1. v9.13.2 taxonomy: SUPPORTED/CONSISTENT/NO_EVIDENCE/CONTRADICTED
//!    (not VERIFIED/PLAUSIBLE/UNVERIFIED — those allowed the model to
//!    use stale training data as ground truth)
//! 2. Structural citation override: CONTRADICTED (default) or SUPPORTED+CONTRADICTED
//!    → NO_EVIDENCE when no real citations (`COUNCIL_SHELDON_CITATION_OVERRIDE`)
//! 3. Gate mode (v9.13.4): redact *only high-impact* CONTRADICTED claims (exact)
//!    from responses before R2+ cross-pollination; low-impact left (in report)

use crate::engine::context::RequestContext;
use crate::evidence;
use crate::provider;
use crate::types::{ClaimImpact, ClaimVerdict, ClaimVerdictEntry, RoleDefinition, SeatResponse};
use crate::xmcp;
use reqwest::Url;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::{Ipv4Addr, Ipv6Addr};

const REPO_CONTEXT_MAX_BYTES: usize = 20_000;

/// Citation override policy (v9.13.2 guardrail). Default: `contradicted` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CitationOverrideMode {
    Off,
    ContradictedOnly,
    All,
}

pub(crate) fn citation_override_mode() -> CitationOverrideMode {
    match std::env::var("COUNCIL_SHELDON_CITATION_OVERRIDE")
        .unwrap_or_else(|_| "contradicted".into())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "false" | "0" | "none" => CitationOverrideMode::Off,
        "all" | "true" | "1" | "both" | "supported" => CitationOverrideMode::All,
        _ => CitationOverrideMode::ContradictedOnly,
    }
}

/// Whether to apply the round-level "pure opinion" pre-filter skip.
/// Deprecated for round-level use: opinion filtering is per-claim in the validator LLM.
/// Kept for env compat; no longer consulted by `validate_round` (claim-validation path / council-rs product).
#[allow(dead_code)]
pub(crate) fn skip_opinion_round() -> bool {
    match std::env::var("COUNCIL_SHELDON_SKIP_OPINION_ROUND") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "0" && v != "false"
        }
        Err(_) => true,
    }
}

/// Session-scoped evidence cache for Sheldon validator (one per deliberation/phase).
/// Deduplicates web (exa/tavily/news/scholar/firecrawl) and xmcp (live X only) fetches
/// across rounds when the normalized query/topic is the same. Stores formatted section strings.
///
/// Hit: skip the HTTP/MCP roundtrip entirely.
/// Miss: perform fetch, format, store.
///
/// Opt out for debugging: COUNCIL_SHELDON_EVIDENCE_CACHE=0
#[derive(Default)]
pub struct EvidenceCache {
    store: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl EvidenceCache {
    /// Returns a clone of the cached formatted evidence block, if present.
    pub fn get(&self, key: &str) -> Option<String> {
        self.store.lock().ok().and_then(|m| m.get(key).cloned())
    }

    /// Store a formatted evidence block for a source+query key.
    pub fn insert(&self, key: String, value: String) {
        if let Ok(mut m) = self.store.lock() {
            m.insert(key, value);
        }
    }
}

fn sheldon_evidence_cache_enabled() -> bool {
    std::env::var("COUNCIL_SHELDON_EVIDENCE_CACHE")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v != "0" && v != "false"
        })
        .unwrap_or(true)
}

/// Classification of a claim (or round-summary text treated as claim-like content)
/// for pre-LLM scoping decisions in validate_round.
///
/// - Pure-heuristic (regex-free, LLM-free): string matching + signals.
/// - Used before the validator LLM call to skip entirely for Opinion,
///   and for LocalCode without repo context (auto-skip rather than guess).
/// - Integrates with existing would_skip_local_without_context guard and
///   evidence_context / --context repo signals.
/// - Priority: Opinion first (even if code words present), then LocalCode,
///   then PublicFact signals (URLs, specs, numbers), else Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ClaimClass {
    /// Local / repo code references: file paths (src/, foo.rs), function
    /// signatures ("fn foo(", "the function bar", "impl X"), Cargo manifests,
    /// "in src/", tests, etc. Without context these cannot be validated.
    LocalCode,
    /// Publicly checkable facts: web URLs, numeric specs/benchmarks,
    /// versions, RFCs, standards, timelines, costs, market data, etc.
    PublicFact,
    /// Normative/subjective/opinion content: "should", "we should", "better to",
    /// recommendations, value judgments. Per Sheldon rules these are ignored;
    /// skip LLM entirely to avoid waste.
    Opinion,
    /// No dominant signals matching the above classes.
    #[default]
    Unknown,
}

/// Lightweight pre-classifier. No additional crates/LLM calls.
/// Implements the requested improvements over the original 5-contains heuristic.
pub(crate) fn classify_claim(claim_text: &str) -> ClaimClass {
    let t = claim_text.to_lowercase();

    // Opinion signals (checked first): normative language, suggestions, "should" etc.
    // Task-specified: "should", "we should", "better to" + common variants.
    // Guard lightly against code identifiers like "should_foo" in rare mixed text.
    const OPINION_PHRASES: &[&str] = &[
        "should ",
        " we should",
        "we should ",
        " better to",
        " better if",
        "ought to",
        " i think",
        " in my opinion",
        "in my view",
        " recommend",
        " prefer ",
        "ideally ",
        " would be better",
        "must be",
        " needs to ",
        "have to ",
        " suggestion",
        " propose",
        " i suggest",
        "we ought",
        "should probably",
        "is better",
        "are better",
        "would rather",
    ];
    if OPINION_PHRASES.iter().any(|p| t.contains(p))
        && !t.contains("fn should")
        && !t.contains("should_")
    {
        return ClaimClass::Opinion;
    }

    // URL/public signals first (BEFORE local) so that docs.rs, raw github .rs files
    // in https://... contexts classify as PublicFact, not LocalCode.
    if t.contains("http://") || t.contains("https://") || t.contains("://") || t.contains("www.") {
        return ClaimClass::PublicFact;
    }

    // LocalCode signals (robust over original):
    // file paths, extensions, "in src/", "the function foo", function signatures,
    // impls, structs, Cargo, tests.
    const LOCAL_SIGNALS: &[&str] = &[
        "src/",
        " lib.rs",
        " main.rs",
        " mod.rs",
        ".rs ",
        ".rs)",
        ".rs:",
        ".rs.",
        "cargo.toml",
        "cargo.lock",
        "cargo.",
        " fn ",
        "pub fn ",
        "async fn ",
        " fn(",
        "impl ",
        " impl<",
        "struct ",
        "mod ",
        "enum ",
        "trait ",
        "the function ",
        "the method ",
        "this function",
        "this fn ",
        "in src/",
        "in the file ",
        "the test ",
        "unit test",
        "integration test",
        "the code in",
        "checkout",
        "repository behavior",
    ];
    let looks_like_path = t.contains('/')
        && (t.contains(".rs") || t.contains("src/") || t.contains("/lib") || t.contains("/src"));
    let has_fn_sig =
        (t.contains("fn ") || t.contains("pub fn") || t.contains("async fn")) && t.contains('(');
    if LOCAL_SIGNALS.iter().any(|s| t.contains(s)) || looks_like_path || has_fn_sig {
        return ClaimClass::LocalCode;
    }

    // PublicFact (non-URL specs etc after local): numbers that look like measurable facts/versions.
    if t.contains("rfc ")
        || t.contains(" rfc")
        || t.contains("spec ")
        || t.contains(" standard ")
        || t.contains(" api ")
        || t.contains("official docs")
    {
        return ClaimClass::PublicFact;
    }
    // Numbers that look like specs/benchmarks (%, timings, sizes, versions, $).
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    if has_digit
        && (t.contains('%')
            || t.contains("ms")
            || t.contains("kb")
            || t.contains("mb")
            || t.contains(" v")
            || t.contains("version")
            || t.contains("$")
            || t.contains("cost")
            || t.contains("benchmark")
            || t.contains("achieves")
            || t.contains("achieved"))
    {
        return ClaimClass::PublicFact;
    }

    ClaimClass::Unknown
}

/// True when operator supplied `--context` / `--map` text, or gathered evidence includes
/// bounded repo excerpts (not substring guesses on arbitrary evidence blobs).
pub(crate) fn has_repo_signal(context: &str, evidence_context: &str) -> bool {
    !context.trim().is_empty() || evidence_context.contains("<repo_context>")
}

/// Pre-LLM skip for rounds that cite local code without any repo signal.
/// Does not skip on Opinion class — the validator LLM drops normative claims per system rules.
pub(crate) fn should_skip_validator_llm(
    position_summary: &str,
    context: &str,
    evidence_context: &str,
) -> Option<&'static str> {
    if has_repo_signal(context, evidence_context) {
        return None;
    }
    if classify_claim(position_summary) == ClaimClass::LocalCode {
        return Some("local code (classify_claim) without --context/--map");
    }
    let looks_local = position_summary.contains("src/")
        || position_summary.contains(".rs")
        || position_summary.contains(" fn ")
        || position_summary.contains("impl ")
        || position_summary.contains("Cargo.");
    if looks_local {
        return Some("local code signals without --context/--map");
    }
    None
}

/// True when seat text looks like local code but no repo context was supplied.
pub(crate) fn would_skip_local_without_context(
    position_summary: &str,
    context: &str,
    evidence_context: &str,
) -> bool {
    should_skip_validator_llm(position_summary, context, evidence_context).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateSkipReason {
    InsufficientResponses,
    LocalCodeNoContext,
}

#[derive(Debug)]
pub enum ValidateRoundOutcome {
    /// Intentional no-op (do not cascade failover).
    Skipped(ValidateSkipReason),
    /// Provider error / empty response (try next cascade step).
    ProviderFailed,
    Ok(Vec<ClaimVerdictEntry>, f64),
}

pub(crate) fn build_position_summary(responses: &[SeatResponse]) -> Option<String> {
    let valid: Vec<&SeatResponse> = responses
        .iter()
        .filter(|r| !r.text.is_empty() && r.error.is_none())
        .collect();
    if valid.len() < 2 {
        return None;
    }
    Some(
        valid
            .iter()
            .map(|r| {
                format!(
                    "### {} ({}):\n{}",
                    r.seat_name,
                    r.provider,
                    truncate(&r.text, 3000)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn search_safe_query(topic: &str) -> &str {
    if topic.len() > 200 {
        let trimmed = truncate(topic, 200);
        trimmed.rfind(' ').map_or(trimmed, |i| &trimmed[..i])
    } else {
        topic
    }
}

/// Cache key for evidence gather — fingerprint full topic so truncated search queries
/// cannot collide across distinct validation prompts.
fn evidence_cache_key(source: &str, topic: &str) -> String {
    let mut hasher = DefaultHasher::new();
    topic.hash(&mut hasher);
    format!("{source}:{:016x}", hasher.finish())
}

/// Returns false when `--validate` cannot run (empty claim_validator cascade).
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

async fn gather_evidence(
    topic: &str,
    valid: &[&SeatResponse],
    context: &str,
    verbose: bool,
    cache: Option<&EvidenceCache>,
) -> String {
    let has_xmcp = xmcp::is_available().await;
    let has_web = evidence::is_available();
    let repo_context = repo_context_evidence(context);

    if !has_xmcp && !has_web && repo_context.is_none() {
        if verbose {
            eprintln!("🔍 Validator: no evidence sources available — claim extraction only");
        }
        return String::new();
    }

    if verbose {
        let mut sources = Vec::new();
        if has_xmcp {
            sources.push("xmcp");
        }
        if has_web {
            sources.push("native web");
        }
        if repo_context.is_some() {
            sources.push("repo context");
        }
        eprintln!("🔍 Validator: {} detected", sources.join(" + "));
    }

    let combined_text: String = valid
        .iter()
        .take(3)
        .map(|r| truncate(&r.text, 500))
        .collect::<Vec<_>>()
        .join(" ");
    let keywords = extract_keywords(&combined_text, topic);
    let queries = build_query_pairs(&keywords);

    // Run all evidence sources in parallel.
    // xmcp is used *only* for live/recent X posts (raw intel via searchPostsRecent).
    // Personal bookmark/intel corpus is never consulted from Sheldon.
    let xmcp_fut = gather_xmcp_evidence(topic, &queries, has_xmcp, cache);
    let web_fut = gather_web_evidence(topic, has_web, verbose, cache);
    let (xmcp_parts, web_parts) = tokio::join!(xmcp_fut, web_fut);

    let mut parts = Vec::new();
    if let Some(repo_context) = repo_context {
        parts.push(repo_context);
    }
    parts.extend(xmcp_parts);
    parts.extend(web_parts);

    if parts.is_empty() {
        return String::new();
    }

    if verbose {
        eprintln!("      📡 {} evidence sections gathered", parts.len());
    }

    format!(
        "\n\n<evidence source=\"multi-source intelligence pipeline\">\n\
         Use this evidence to VERIFY or CONTRADICT claims. You MUST \
         cite specific items when they are relevant to a claim.\n\n\
         {}\n</evidence>\n",
        parts.join("\n")
    )
}

fn repo_context_evidence(context: &str) -> Option<String> {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return None;
    }

    let clipped = truncate(trimmed, REPO_CONTEXT_MAX_BYTES);
    let truncated = clipped.len() < trimmed.len();
    let suffix = if truncated {
        "\n\n[repo context truncated]"
    } else {
        ""
    };

    Some(format!(
        "## Local Repo Context (operator-provided)\n\
         Source: the same --context/--map text supplied to deliberation seats. \
         Use this as the ONLY evidence for local source files, symbols, tests, \
         build scripts, and repository runtime behavior.\n\n\
         <repo_context>\n{}{}\n</repo_context>",
        clipped, suffix
    ))
}

async fn gather_xmcp_evidence(
    _topic: &str,
    queries: &[String],
    available: bool,
    _cache: Option<&EvidenceCache>,
) -> Vec<String> {
    if !available {
        return vec![];
    }

    let mut parts = Vec::new();

    // Sheldon uses xmcp *strictly* as a bridge to live/recent X posts (raw intel).
    // The personal bookmark / intel corpus is **never** queried from here.
    // Bookmarks can be sparse/stale and would bias validation toward the
    // operator's existing collection rather than fresh public signals.
    //
    // We use xmcp::search_posts (searchPostsRecent) for live X only.
    let mut seen_ids = std::collections::HashSet::new();
    let mut post_results = Vec::new();
    for q in queries.iter().take(3) {
        let hits = xmcp::search_posts(q, 3).await;
        for p in hits {
            let pid = p
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !pid.is_empty() && !seen_ids.insert(pid) {
                continue;
            }
            post_results.push(p);
        }
    }

    if !post_results.is_empty() {
        parts.push("## Live X Posts (via xmcp)".into());
        for post in post_results.iter().take(8) {
            let text = post.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let text_short = truncate(text, 300);
            let likes = post
                .get("public_metrics")
                .and_then(|m| m.get("like_count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mut entry = format!("- {}", text_short);
            if likes > 0 {
                entry.push_str(&format!(" [{} likes]", likes));
            }
            // If the live search payload ever carries enrichment, we can surface
            // it here the same way (author, why, etc.). For now this is raw live.
            parts.push(entry);
        }
    }

    parts
}

async fn gather_web_evidence(
    topic: &str,
    available: bool,
    verbose: bool,
    cache: Option<&EvidenceCache>,
) -> Vec<String> {
    if !available {
        return vec![];
    }

    let mut parts = Vec::new();
    let evidence_run = evidence::EvidenceRun::new();

    // Extract URLs from topic for Firecrawl verification. Blocked targets are
    // logged before dispatch, so SSRF regression checks do not depend on model
    // prose or third-party scraper behavior.
    let (urls, blocked_urls) = extract_scrape_targets(topic);
    if verbose && !blocked_urls.is_empty() {
        let examples = blocked_urls
            .iter()
            .take(3)
            .map(|blocked| format!("{} ({})", truncate(&blocked.raw, 80), blocked.reason))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "      🛡️  URL sanitizer: blocked {} scrape target(s) before dispatch: {}",
            blocked_urls.len(),
            examples
        );
    }

    // Truncate topic to a search-safe query length (Tavily max ~400 chars,
    // but shorter queries produce better results across all engines).
    let search_query = search_safe_query(topic);

    // Session cache check (P0 evidence dedup). Key per source + stable query.
    // On hit we skip the (paid) network call and reuse the previously formatted block.
    let use_cache = cache.is_some() && sheldon_evidence_cache_enabled();

    // Provider failures are local to each native source; one auth or outage
    // problem should not suppress the rest of the evidence gather.
    let exa_key = evidence_cache_key("exa", topic);
    let exa_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&exa_key).is_some() {
                // We will push the cached formatted block after fetch phase.
                // For now signal empty so join shape preserved; post-process below.
                vec![]
            } else {
                evidence::exa_search_with_run(search_query, 5, Some(&evidence_run)).await
            }
        } else {
            evidence::exa_search_with_run(search_query, 5, Some(&evidence_run)).await
        }
    } else {
        evidence::exa_search_with_run(search_query, 5, Some(&evidence_run)).await
    };

    let tavily_key = evidence_cache_key("tavily", topic);
    let tavily_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&tavily_key).is_some() {
                vec![]
            } else {
                evidence::tavily_search_with_run(search_query, 5, Some(&evidence_run)).await
            }
        } else {
            evidence::tavily_search_with_run(search_query, 5, Some(&evidence_run)).await
        }
    } else {
        evidence::tavily_search_with_run(search_query, 5, Some(&evidence_run)).await
    };

    let news_key = evidence_cache_key("news", topic);
    let news_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&news_key).is_some() {
                vec![]
            } else {
                evidence::news_search_with_run(search_query, Some(&evidence_run)).await
            }
        } else {
            evidence::news_search_with_run(search_query, Some(&evidence_run)).await
        }
    } else {
        evidence::news_search_with_run(search_query, Some(&evidence_run)).await
    };

    let scholar_key = evidence_cache_key("scholar", topic);
    let scholar_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&scholar_key).is_some() {
                vec![]
            } else {
                evidence::scholar_search_with_run(search_query, Some(&evidence_run)).await
            }
        } else {
            evidence::scholar_search_with_run(search_query, Some(&evidence_run)).await
        }
    } else {
        evidence::scholar_search_with_run(search_query, Some(&evidence_run)).await
    };

    let scrape_fut = scrape_topic_urls(&urls, verbose, &evidence_run);
    let scraped = scrape_fut.await;

    // Note: the join shape for the four searches was replaced by individual awaits on miss
    // to allow per-source cache decisions. Scrapes remain after url extract.

    // Source: Exa semantic web search
    let exa_key = evidence_cache_key("exa", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&exa_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      🌐 Exa: cache hit");
        }
    }
    if !exa_results.is_empty() {
        let mut block = "\n## Web Intelligence (Exa semantic search)".to_string();
        for item in exa_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let score = item.get("score").and_then(|v| v.as_f64());
            let text_short = truncate(text, 300);
            let mut entry = if !title.is_empty() {
                format!("- [{}]({}) — {}", title, url, text_short)
            } else {
                format!("- {} — {}", url, text_short)
            };
            if let Some(s) = score {
                entry.push_str(&format!(" [relevance={:.2}]", s));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(exa_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!("      🌐 Exa: {} web results", exa_results.len().min(5));
        }
    }

    // Source: Tavily recency-tuned web search (last 7 days)
    let tavily_key = evidence_cache_key("tavily", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&tavily_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      🔎 Tavily: cache hit");
        }
    }
    if !tavily_results.is_empty() {
        let mut block = "\n## Recency-Biased Web (Tavily)".to_string();
        for item in tavily_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let score = item.get("score").and_then(|v| v.as_f64());
            let content_short = truncate(content, 300);
            let mut entry = if !title.is_empty() {
                format!("- [{}]({}) — {}", title, url, content_short)
            } else {
                format!("- {} — {}", url, content_short)
            };
            if let Some(s) = score {
                entry.push_str(&format!(" [relevance={:.2}]", s));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(tavily_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!(
                "      🔎 Tavily: {} recent web results",
                tavily_results.len().min(5)
            );
        }
    }

    // Source: Real-time news (timestamped, attributed)
    let news_key = evidence_cache_key("news", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&news_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      📰 News: cache hit");
        }
    }
    if !news_results.is_empty() {
        let mut block = "\n## Breaking News (Tavily News, last 7 days)".to_string();
        for item in news_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let source = item.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let date = item.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let published = item
                .get("published_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let snippet_short = truncate(snippet, 300);
            let timestamp = if !published.is_empty() {
                published
            } else {
                date
            };
            let mut entry = format!("- [{}]({}) — {}", title, url, snippet_short);
            if !source.is_empty() {
                entry.push_str(&format!(" [{}]", source));
            }
            if !timestamp.is_empty() {
                entry.push_str(&format!(" ({})", timestamp));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(news_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!("      📰 News: {} articles", news_results.len().min(5));
        }
    }

    // Source: Academic papers (citation-weighted)
    let scholar_key = evidence_cache_key("scholar", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&scholar_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      🎓 Scholar: cache hit");
        }
    }
    if !scholar_results.is_empty() {
        let mut block = "\n## Academic Papers (Semantic Scholar)".to_string();
        for item in scholar_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let source = item.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let citations = item
                .get("citation_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let year = item.get("year").and_then(|v| v.as_str()).unwrap_or("");
            let snippet_short = truncate(snippet, 300);
            let mut entry = if !url.is_empty() {
                format!("- [{}]({}) — {}", title, url, snippet_short)
            } else {
                format!("- {} — {}", title, snippet_short)
            };
            if !source.is_empty() {
                entry.push_str(&format!(" [{}]", source));
            }
            if !year.is_empty() {
                entry.push_str(&format!(" ({})", year));
            }
            if citations > 0 {
                entry.push_str(&format!(" [{} citations]", citations));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(scholar_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!("      🎓 Scholar: {} papers", scholar_results.len().min(5));
        }
    }

    // Source: Firecrawl URL scraping (for URLs cited in the topic)
    if !scraped.is_empty() {
        parts.push("\n## Cited URL Content (Firecrawl)".into());
        parts.extend(scraped);
    }

    parts
}

#[cfg(test)]
fn extract_urls(text: &str) -> Vec<String> {
    extract_scrape_targets(text).0
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockedScrapeUrl {
    raw: String,
    reason: &'static str,
}

fn extract_scrape_targets(text: &str) -> (Vec<String>, Vec<BlockedScrapeUrl>) {
    let mut allowed = Vec::new();
    let mut blocked = Vec::new();

    text.split_whitespace()
        .filter_map(classify_scrape_candidate)
        .for_each(|decision| match decision {
            Ok(url) => {
                if allowed.len() < 3 {
                    allowed.push(url);
                }
            }
            Err(blocked_url) => blocked.push(blocked_url),
        });

    (allowed, blocked)
}

fn classify_scrape_candidate(raw: &str) -> Option<Result<String, BlockedScrapeUrl>> {
    // Strip trailing punctuation that gets swept in from prose before parsing.
    let clean = raw.trim_end_matches([')', '.', ',', ']', ';', '>', '"', '\'']);
    if !(clean.starts_with("http://") || clean.starts_with("https://")) {
        return None;
    }

    let blocked = |reason| {
        Some(Err(BlockedScrapeUrl {
            raw: clean.to_string(),
            reason,
        }))
    };

    let parsed = match Url::parse(clean) {
        Ok(parsed) => parsed,
        Err(_) => return blocked("invalid_url"),
    };
    if parsed.scheme() != "https" {
        return blocked("non_https");
    }
    let host = match parsed.host_str() {
        Some(host) => host,
        None => return blocked("missing_host"),
    };
    if let Some(reason) = private_host_block_reason(host) {
        return blocked(reason);
    }
    Some(Ok(parsed.to_string()))
}

fn private_host_block_reason(host: &str) -> Option<&'static str> {
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if host == "localhost" || host == "metadata.google.internal" {
        return Some("local_name");
    }

    if host.ends_with(".local") || host.ends_with(".internal") {
        return Some("internal_tld");
    }

    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return is_private_ipv4(ip).then_some("private_ipv4");
    }

    if is_wildcard_local_dns_host(&host) || embeds_private_ipv4_labels(&host) {
        return Some("wildcard_or_embedded_private_ip");
    }

    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            return Some("private_ipv6");
        }
        if let Some(v4) = ip.to_ipv4_mapped() {
            return is_private_ipv4(v4).then_some("private_ipv4_mapped_ipv6");
        }
        let first = ip.segments()[0];
        if (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80 {
            return Some("private_ipv6");
        }
    }

    None
}

fn is_wildcard_local_dns_host(host: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "nip.io",
        "sslip.io",
        "xip.io",
        "localtest.me",
        "lvh.me",
        "vcap.me",
    ];

    SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{}", suffix)))
}

fn embeds_private_ipv4_labels(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.windows(4).any(|window| {
        let octets = window
            .iter()
            .map(|label| label.parse::<u8>())
            .collect::<Result<Vec<_>, _>>();
        if let Ok(octets) = octets
            && let [a, b, c, d] = octets.as_slice()
        {
            return is_private_ipv4(Ipv4Addr::new(*a, *b, *c, *d));
        }
        false
    })
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 0
        || octets[0] == 10
        || octets[0] == 127
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
        || (octets[0] == 169 && octets[1] == 254)
}

async fn scrape_topic_urls(
    urls: &[String],
    verbose: bool,
    evidence_run: &evidence::EvidenceRun,
) -> Vec<String> {
    if urls.is_empty() {
        return vec![];
    }

    let mut parts = Vec::new();
    let futs: Vec<_> = urls
        .iter()
        .map(|url| evidence::scrape_url_with_run(url.as_str(), Some(evidence_run)))
        .collect();
    let results = futures_util::future::join_all(futs).await;

    for (url, content) in urls.iter().zip(results) {
        if let Some(md) = content {
            parts.push(format!("### {}", url));
            parts.push(truncate(&md, 1500).to_string());
            if verbose {
                eprintln!(
                    "      🔥 Firecrawl: scraped {} ({} chars)",
                    url,
                    md.len().min(1500)
                );
            }
        }
    }

    parts
}

fn extract_keywords(text: &str, topic: &str) -> Vec<String> {
    static STOP: &[&str] = &[
        "this",
        "that",
        "with",
        "from",
        "have",
        "what",
        "should",
        "would",
        "could",
        "which",
        "their",
        "about",
        "into",
        "they",
        "them",
        "than",
        "then",
        "also",
        "been",
        "more",
        "need",
        "very",
        "will",
        "just",
        "each",
        "make",
        "like",
        "only",
        "when",
        "some",
        "here",
        "there",
        "these",
        "those",
        "does",
        "done",
        "your",
        "such",
        "before",
        "after",
        "problem",
        "restatement",
        "analysis",
        "round",
        "seat",
    ];

    let mut keywords: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.len() > 3
            && word.chars().next().is_some_and(|c| c.is_uppercase())
            && word.chars().all(|c| c.is_alphanumeric())
        {
            let wl = word.to_lowercase();
            if !STOP.contains(&wl.as_str()) && !keywords.iter().any(|k| k == word) {
                keywords.push(word.to_string());
                if keywords.len() >= 8 {
                    break;
                }
            }
        }
    }
    if keywords.len() < 3 {
        for w in topic.split_whitespace().take(8) {
            let clean = w.trim_matches(|c: char| !c.is_alphanumeric());
            if clean.len() > 3
                && !STOP.contains(&clean.to_lowercase().as_str())
                && !keywords.contains(&clean.to_string())
            {
                keywords.push(clean.to_string());
                if keywords.len() >= 5 {
                    break;
                }
            }
        }
    }
    keywords
}

fn build_query_pairs(keywords: &[String]) -> Vec<String> {
    let mut queries = Vec::new();
    if keywords.len() >= 2 {
        queries.push(format!("{} {}", keywords[0], keywords[1]));
    }
    if keywords.len() >= 4 {
        queries.push(format!("{} {}", keywords[2], keywords[3]));
    }
    if keywords.len() >= 6 {
        queries.push(format!("{} {}", keywords[4], keywords[5]));
    }
    queries
}

fn parse_json_report(text: &str) -> Option<Vec<ClaimVerdictEntry>> {
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
fn apply_citation_override(report: &mut [ClaimVerdictEntry], mode: CitationOverrideMode) -> usize {
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

    #[test]
    fn extract_urls_strips_trailing_punctuation_before_scrape() {
        let urls = extract_urls("Check https://example.com/path?q=1), then https://ok.example/a.");

        assert_eq!(
            urls,
            vec![
                "https://example.com/path?q=1".to_string(),
                "https://ok.example/a".to_string()
            ]
        );
    }

    #[test]
    fn extract_urls_blocks_private_hosts() {
        let urls = extract_urls(
            "Skip https://127.0.0.1/health and https://metadata.google.internal/latest.",
        );

        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_blocks_http_localhost_and_internal_hosts() {
        let (allowed, blocked) = extract_scrape_targets(
            "Skip http://example.com/plain and https://localhost/admin and https://api.service.internal/path.",
        );

        assert!(allowed.is_empty());
        assert_eq!(blocked.len(), 3);
        assert!(blocked.iter().any(|b| b.reason == "non_https"));
        assert!(blocked.iter().any(|b| b.reason == "local_name"));
        assert!(blocked.iter().any(|b| b.reason == "internal_tld"));
    }

    #[test]
    fn extract_urls_blocks_userinfo_authority_confusion() {
        let urls = extract_urls(
            "Skip https://example.com@169.254.169.254/latest and https://user:pass@metadata.google.internal/latest.",
        );

        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_blocks_private_ipv6_ranges() {
        let urls =
            extract_urls("Skip https://[fe80::1]/ and https://[fc00::1]/ and https://[::1]/.");

        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_blocks_ipv4_mapped_ipv6_loopback() {
        let urls = extract_urls("Skip https://[::ffff:127.0.0.1]/.");

        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_blocks_encoded_private_ip_hosts() {
        let urls = extract_urls(
            "Skip https://2130706433/ and https://0x7f000001/ and https://017700000001/.",
        );

        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_blocks_wildcard_dns_private_targets() {
        let urls = extract_urls(
            "Skip https://169.254.169.254.nip.io/latest and https://127.0.0.1.sslip.io/.",
        );

        assert!(urls.is_empty());
    }

    #[test]
    fn extract_scrape_targets_reports_local_block_reasons() {
        let (allowed, blocked) = extract_scrape_targets(
            "Scrape https://example.com and skip https://2130706433/ plus https://127.0.0.1.sslip.io/.",
        );

        assert_eq!(allowed, vec!["https://example.com/".to_string()]);
        assert_eq!(blocked.len(), 2);
        assert!(blocked.iter().any(|b| b.reason == "private_ipv4"));
        assert!(
            blocked
                .iter()
                .any(|b| b.reason == "wildcard_or_embedded_private_ip")
        );
    }

    #[test]
    fn extract_urls_allows_public_https_hosts() {
        let urls = extract_urls("Keep https://example.com/ok and https://docs.rs/url/latest/url/.");

        assert_eq!(
            urls,
            vec![
                "https://example.com/ok".to_string(),
                "https://docs.rs/url/latest/url/".to_string()
            ]
        );
    }

    #[test]
    fn extract_scrape_targets_caps_allowed_urls_at_three() {
        let (allowed, blocked) = extract_scrape_targets(
            "Keep https://one.example/a https://two.example/b https://three.example/c https://four.example/d.",
        );

        assert!(blocked.is_empty());
        assert_eq!(
            allowed,
            vec![
                "https://one.example/a".to_string(),
                "https://two.example/b".to_string(),
                "https://three.example/c".to_string()
            ]
        );
    }

    #[test]
    fn evidence_cache_key_differs_for_same_truncated_prefix() {
        let prefix = "a".repeat(190);
        let topic_a = format!("{prefix} {}", "b".repeat(40));
        let topic_b = format!("{prefix} {}", "c".repeat(40));
        assert_eq!(search_safe_query(&topic_a), search_safe_query(&topic_b));
        assert_ne!(
            evidence_cache_key("exa", &topic_a),
            evidence_cache_key("exa", &topic_b)
        );
    }

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
    fn search_safe_query_truncates_at_word_boundary_under_200_bytes() {
        let topic = format!("{} {}", "a".repeat(190), "b".repeat(40));
        let query = search_safe_query(&topic);

        assert_eq!(query, "a".repeat(190));
        assert!(query.len() <= 200);
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

    #[test]
    fn repo_context_evidence_labels_local_code_source() {
        let evidence =
            repo_context_evidence("src/types.rs\nfn from_provider() { /* trims empty text */ }")
                .expect("repo context evidence");

        assert!(evidence.contains("## Local Repo Context"));
        assert!(evidence.contains("<repo_context>"));
        assert!(evidence.contains("src/types.rs"));
        assert!(evidence.contains("ONLY evidence for local source files"));
    }

    #[test]
    fn repo_context_evidence_caps_context_without_splitting_characters() {
        let context = format!("{}é", "a".repeat(REPO_CONTEXT_MAX_BYTES));
        let evidence = repo_context_evidence(&context).expect("repo context evidence");

        assert!(evidence.contains("[repo context truncated]"));
        assert!(evidence.is_char_boundary(evidence.len()));
    }

    #[test]
    fn would_skip_local_without_context_when_no_map() {
        let summary = "### Munger (grok):\nRefactor src/engine/sheldon.rs validate_round fn";
        assert!(would_skip_local_without_context(summary, "", ""));
        assert!(!would_skip_local_without_context(
            summary,
            "src/engine/sheldon.rs\npub async fn validate_round",
            ""
        ));
    }

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

    #[test]
    fn classify_claim_detects_opinion_should_better() {
        assert_eq!(
            classify_claim("We should use a different approach here"),
            ClaimClass::Opinion
        );
        assert_eq!(
            classify_claim("It would be better to refactor the module"),
            ClaimClass::Opinion
        );
        assert_eq!(
            classify_claim("The team ought to prefer async for this"),
            ClaimClass::Opinion
        );
        // "should" inside fn identifier does not make it Opinion (guard), but fn sig -> LocalCode
        assert_eq!(
            classify_claim("fn should_handle() { }"),
            ClaimClass::LocalCode
        );
    }

    #[test]
    fn classify_claim_detects_local_code_paths_and_fns() {
        assert_eq!(
            classify_claim("Refactor src/engine/sheldon.rs in the validate_round fn"),
            ClaimClass::LocalCode
        );
        assert_eq!(
            classify_claim("Update the function foo_bar in lib.rs"),
            ClaimClass::LocalCode
        );
        assert_eq!(
            classify_claim("impl Foo for Bar in src/types.rs"),
            ClaimClass::LocalCode
        );
        assert_eq!(
            classify_claim("See Cargo.toml for the version"),
            ClaimClass::LocalCode
        );
        assert_eq!(
            classify_claim("the function gather_evidence does X"),
            ClaimClass::LocalCode
        );
        assert_eq!(
            classify_claim("in src/ the test fails"),
            ClaimClass::LocalCode
        );
        assert_eq!(
            classify_claim("pub async fn validate_round(...)"),
            ClaimClass::LocalCode
        );
    }

    #[test]
    fn classify_claim_detects_public_facts_and_urls() {
        assert_eq!(
            classify_claim("The API is documented at https://docs.rs/foo"),
            ClaimClass::PublicFact
        );
        assert_eq!(
            classify_claim("Per RFC 1234 the timeout is 42ms"),
            ClaimClass::PublicFact
        );
        assert_eq!(
            classify_claim("Version v2.3 achieves 95% on the benchmark"),
            ClaimClass::PublicFact
        );
        assert_eq!(
            classify_claim("According to the official spec it costs $10"),
            ClaimClass::PublicFact
        );
        assert_eq!(
            classify_claim("It uses 128kb of memory per the study"),
            ClaimClass::PublicFact
        );
    }

    #[test]
    fn classify_claim_unknown_for_plain_statements() {
        assert_eq!(
            classify_claim("The server started successfully yesterday."),
            ClaimClass::Unknown
        );
        assert_eq!(
            classify_claim("Error rates remained low across runs."),
            ClaimClass::Unknown
        );
    }

    #[test]
    fn classify_claim_prioritizes_opinion_over_local() {
        // opinion markers still classify as Opinion for per-claim heuristics
        assert_eq!(
            classify_claim("We should change fn foo() in src/bar.rs"),
            ClaimClass::Opinion
        );
        assert_eq!(
            classify_claim("We should refactor but the fn in src/ uses 10ms and costs $5"),
            ClaimClass::Opinion
        );
    }

    #[test]
    fn has_repo_signal_uses_context_and_repo_tag() {
        assert!(!has_repo_signal("", ""));
        assert!(has_repo_signal("map excerpt", ""));
        assert!(has_repo_signal(
            "",
            "<repo_context>\nfn foo\n</repo_context>"
        ));
    }

    #[test]
    fn should_skip_validator_llm_local_only_without_context() {
        let summary = "### Seat:\nRefactor src/engine/sheldon.rs validate_round";
        assert!(should_skip_validator_llm(summary, "", "").is_some());
        assert!(should_skip_validator_llm(summary, "", "").is_some());
        assert!(should_skip_validator_llm(summary, "src/engine/sheldon.rs content", "").is_none());
    }

    #[test]
    fn should_not_skip_opinion_heavy_round_without_context() {
        let summary = "### Seat:\nWe should recommend a better path forward for the architecture";
        assert!(should_skip_validator_llm(summary, "", "").is_none());
    }

    #[test]
    fn would_skip_local_without_context_uses_classifier() {
        let summary =
            "### Seat (grok):\nChange the function in src/foo.rs: pub fn bar() { impl X {} }";
        assert!(would_skip_local_without_context(summary, "", ""));
        // with context, do not skip
        assert!(!would_skip_local_without_context(
            summary,
            "-- some repo map for src/foo.rs",
            "",
        ));
        // non-local should not trigger
        assert!(!would_skip_local_without_context(
            "plain fact here 42ms",
            "",
            ""
        ));
    }

    #[test]
    fn evidence_cache_hit_miss_basic() {
        let cache = EvidenceCache::default();
        assert!(cache.get("exa:foo").is_none());
        cache.insert("exa:foo".into(), "## Exa hit".into());
        assert_eq!(cache.get("exa:foo").as_deref(), Some("## Exa hit"));
        // different key miss
        assert!(cache.get("tavily:foo").is_none());
    }
}
