//! Sheldon — between-round claim validator (v9.13).
//!
//! Extracts factual claims from seat outputs, gathers evidence from web
//! tools and operator-provided repo context, sends to a validator model, and
//! returns structured verdicts. Three anti-hallucination guardrails:
//!
//! 1. v9.13.2 taxonomy: SUPPORTED/CONSISTENT/NO_EVIDENCE/CONTRADICTED
//!    (not VERIFIED/PLAUSIBLE/UNVERIFIED — those allowed the model to
//!    use stale training data as ground truth)
//! 2. Structural citation override: CONTRADICTED (default) or SUPPORTED+CONTRADICTED
//!    → NO_EVIDENCE when no real citations (`COUNCIL_SHELDON_CITATION_OVERRIDE`)
//! 3. Gate mode (v9.13.4): redact *only high-impact* CONTRADICTED claims (exact)
//!    from responses before R2+ cross-pollination; low-impact left (in report)
//!
//! Module layout (pure split of former `sheldon.rs`):
//! - `validate` — claim_validator_ready, system prompts, validate_round
//! - `evidence` — gather_evidence, repo_context
//! - `web_evidence` — gather_web_evidence + URL/SSRF pipeline
//! - `report` — parse/override/format/gate

mod evidence;
mod report;
mod validate;
mod web_evidence;

pub use report::{format_validation_context, gate_responses};
pub use validate::{ValidatorConfig, claim_validator_ready, validate_round};

use crate::types::{ClaimVerdictEntry, SeatResponse};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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

/// Session-scoped evidence cache for Sheldon validator (one per deliberation/phase).
/// Deduplicates web (exa/tavily/news/scholar/firecrawl) fetches
/// across rounds when the normalized query/topic is the same. Stores formatted section strings.
///
/// Hit: skip the HTTP roundtrip entirely.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn search_safe_query_truncates_at_word_boundary_under_200_bytes() {
        let topic = format!("{} {}", "a".repeat(190), "b".repeat(40));

        let query = search_safe_query(&topic);

        assert_eq!(query, "a".repeat(190));

        assert!(query.len() <= 200);
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
