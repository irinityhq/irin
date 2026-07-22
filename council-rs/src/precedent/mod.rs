//! Precedent engine — institutional memory
//!
//! JSONL index for keyword search across prior deliberations.
//! Port of Python's precedent_search / _load_index / _precedent_index_append.
//!
//! Format: sessions/index.jsonl — one JSON object per line
//! Schema: { session_id, topic, cabinet, keywords: [], digest, timestamp }

use crate::types::{CouncilSession, PrecedentEntry, SessionOrigin};
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::PathBuf;

// ─── Paths ───────────────────────────────────────────────────────────

fn sessions_dir() -> PathBuf {
    std::env::var("COUNCIL_SESSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("sessions"))
}

fn index_path() -> PathBuf {
    sessions_dir().join("index.jsonl")
}

// ─── Index loading ───────────────────────────────────────────────────

/// Load the full precedent index from disk.
pub fn load_index() -> Vec<PrecedentEntry> {
    let path = index_path();
    if !path.exists() {
        return vec![];
    }

    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<PrecedentEntry>(&line).ok())
        .collect()
}

// ─── Unified retrieval ───────────────────────────────────────────────
//
// One ranker for every surface: War Room preview (`GET /api/precedent`),
// CLI `--recall`, engine injection (CLI + stream), stream `precedent_loaded`,
// and the persisted `session.precedent_ids`. Within a deliberation run, the
// injected text, the streamed matches, and the saved ids all come from one
// `RetrievalReceipt` — identical by construction. The typing-time preview
// runs the same ranker with the same defaults but re-queries per keystroke,
// so it can drift from the convene-time set if the topic or index changes.

/// Default result cap on every retrieval surface.
pub const RETRIEVE_LIMIT: usize = 5;
/// Default minimum fused score. Entries below it are dropped — fewer than
/// `RETRIEVE_LIMIT` results is normal, "always five" was junk fill.
pub const RETRIEVE_THRESHOLD: f64 = 0.15;

/// One ranked precedent with its fused score and a human-readable reason.
#[derive(Debug, Clone)]
pub struct RankedHit {
    pub entry: PrecedentEntry,
    /// Fused relevance in [0, 1]-ish space (see `rank` for the formula).
    pub score: f64,
    /// Why it matched, e.g. "semantic 0.62 · keyword 0.25".
    pub why: String,
}

/// Frozen result of one retrieval. Thread this single object to every surface.
#[derive(Debug, Clone)]
pub struct RetrievalReceipt {
    /// Ranker identity: "hybrid-v1" (dense + keyword) or "keyword-v1".
    pub engine: &'static str,
    pub query: String,
    pub threshold: f64,
    pub hits: Vec<RankedHit>,
}

impl RetrievalReceipt {
    /// Session ids of the hits, in rank order — the value persisted to
    /// `session.precedent_ids`.
    pub fn ids(&self) -> Vec<String> {
        self.hits
            .iter()
            .map(|h| h.entry.session_id.clone())
            .collect()
    }
}

/// Retrieve precedents for `query` with the default engine selection
/// (hybrid when an embedding index exists, keyword otherwise).
///
/// Phase 0.5 §4.4: `include_api=false` excludes Api / ApiCancelled sessions
/// from CLI/warroom precedent injection.
pub fn retrieve(query: &str, limit: usize, threshold: f64, include_api: bool) -> RetrievalReceipt {
    retrieve_with_mode(query, limit, threshold, include_api, false)
}

/// As `retrieve`, but `force_keyword=true` skips the dense layer entirely
/// (used by the preview API's explicit `mode=keyword`).
pub fn retrieve_with_mode(
    query: &str,
    limit: usize,
    threshold: f64,
    include_api: bool,
    force_keyword: bool,
) -> RetrievalReceipt {
    let index = load_index();
    let semantic = if force_keyword || !crate::warroom::embeddings::is_present() {
        None
    } else {
        // May block on model init/download — callers already run retrieval
        // inside spawn_blocking. None (no vectors / embed failure) degrades
        // to keyword-only.
        crate::warroom::embeddings::similarity_scores(query)
    };
    rank(
        &index,
        semantic.as_ref(),
        query,
        limit,
        threshold,
        include_api,
    )
}

/// Tokenize for keyword overlap: lowercase words, len > 2, punctuation trimmed.
fn query_tokens(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Pure ranking core — no filesystem, no model. `semantic` maps session_id →
/// cosine similarity when a dense index is available.
///
/// Scoring:
/// - keyword: overlap of query tokens with entry keywords (1×) and topic (2×),
///   normalized by the max possible raw score (3 × query token count) → [0, 1].
/// - hybrid (entry has a vector): 0.7 × cosine + 0.3 × keyword.
/// - hybrid (entry missing from the vector index, e.g. stale embeddings):
///   keyword score at full weight, so new sessions aren't starved.
/// - entries below `threshold` (or at exactly 0) are dropped. Note the fused
///   threshold is stricter than the old pure-cosine preview: a hit with zero
///   keyword overlap needs cosine ≥ threshold/0.7 (≈0.21 at the default).
///   Intentional — that band was mostly noise.
///
/// Order: score desc, then timestamp desc, then session_id — deterministic ties.
pub fn rank(
    index: &[PrecedentEntry],
    semantic: Option<&std::collections::HashMap<String, f64>>,
    query: &str,
    limit: usize,
    threshold: f64,
    include_api: bool,
) -> RetrievalReceipt {
    let engine: &'static str = if semantic.is_some() {
        "hybrid-v1"
    } else {
        "keyword-v1"
    };
    let mut receipt = RetrievalReceipt {
        engine,
        query: query.to_string(),
        threshold,
        hits: vec![],
    };

    let query_words = query_tokens(query);
    if index.is_empty() || query_words.is_empty() {
        return receipt;
    }

    let allow_origin = |o: SessionOrigin| {
        include_api || !matches!(o, SessionOrigin::Api | SessionOrigin::ApiCancelled)
    };
    // Cap the normalization denominator: a long matter (40–80 words) with a
    // few genuinely overlapping terms must not score near zero in
    // keyword-only mode. 12 tokens ≈ a full one-line topic.
    let max_raw = (query_words.len().min(12) * 3) as f64;

    let mut hits: Vec<RankedHit> = index
        .iter()
        .filter(|entry| allow_origin(entry.origin))
        .filter_map(|entry| {
            let entry_words: std::collections::HashSet<&str> =
                entry.keywords.iter().map(|w| w.as_str()).collect();
            let overlap = query_words
                .iter()
                .filter(|w| entry_words.contains(w.as_str()))
                .count();
            let topic_words = query_tokens(&entry.topic);
            let topic_overlap = query_words
                .iter()
                .filter(|w| topic_words.contains(w.as_str()))
                .count();
            let keyword = ((overlap + topic_overlap * 2) as f64 / max_raw).min(1.0);

            let (score, why) = match semantic.and_then(|m| m.get(&entry.session_id)) {
                Some(cos) => {
                    let cos = cos.max(0.0);
                    (
                        0.7 * cos + 0.3 * keyword,
                        format!("semantic {:.2} · keyword {:.2}", cos, keyword),
                    )
                }
                None if semantic.is_some() => {
                    (keyword, format!("keyword {:.2} (no embedding)", keyword))
                }
                None => (keyword, format!("keyword {:.2}", keyword)),
            };

            (score > 0.0 && score >= threshold).then(|| RankedHit {
                entry: entry.clone(),
                score,
                why,
            })
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.entry.timestamp.cmp(&a.entry.timestamp))
            .then_with(|| a.entry.session_id.cmp(&b.entry.session_id))
    });
    hits.truncate(limit);
    receipt.hits = hits;
    receipt
}

// ─── Index append ────────────────────────────────────────────────────

/// Append a new entry to the precedent index.
pub fn index_append(entry: &PrecedentEntry) -> Result<()> {
    let path = index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Opening index: {}", path.display()))?;

    let json = serde_json::to_string(entry)?;
    writeln!(file, "{}", json)?;
    Ok(())
}

/// Build a precedent entry from a completed session (v2 schema).
pub fn entry_from_session(session: &CouncilSession) -> PrecedentEntry {
    let keywords = extract_keywords(&session.topic, session.synthesis.as_deref().unwrap_or(""));
    let digest = build_digest(session);
    let last_round = session.rounds.last();

    let confidence = session
        .synthesis
        .as_deref()
        .and_then(|s| {
            for pat in &["HIGH", "MEDIUM-HIGH", "MEDIUM", "LOW"] {
                if s.contains(pat) {
                    return Some(pat.to_string());
                }
            }
            None
        })
        .unwrap_or_else(|| "UNKNOWN".to_string());

    let convergence = last_round
        .map(|r| (r.convergence_score * 100.0).round() / 100.0)
        .unwrap_or(0.0);

    let mode = format!("{:?}", session.mode).to_lowercase();

    let seat_count = session
        .rounds
        .first()
        .map(|r| r.responses.len())
        .unwrap_or(0);

    PrecedentEntry {
        schema_version: 2,
        session_id: session.session_id.clone(),
        timestamp: session.timestamp.to_rfc3339()[..19].to_string(),
        topic: truncate_at_char_boundary(&session.topic, 200),
        keywords,
        digest,
        confidence,
        cabinet: session.cabinet_name.clone(),
        convergence,
        mode,
        seat_count,
        rounds: session.rounds.len(),
        synthesis_model: session.synthesis_model.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        tier: session.tier.clone(),
        judge_provider: last_round.and_then(|r| r.judge_provider.clone()),
        failure_status: None,
        cited_by: vec![],
        challenged_by: vec![],
        origin: session.origin,
        execution_route: session.execution_route,
        gateway_sensitivity: session.gateway_sensitivity.clone(),
        worker_provenance: session.worker_provenance.clone(),
        parent_request_id: session.parent_request_id.clone(),
    }
}

/// Index a completed session (build entry + append).
pub fn index_session(session: &CouncilSession) -> Result<()> {
    let entry = entry_from_session(session);
    index_append(&entry)
}

// ─── Reindex ─────────────────────────────────────────────────────────

/// Rebuild the entire index from session JSON files.
/// Equivalent to Python's reindex_sessions.
pub fn reindex() -> Result<usize> {
    let dir = sessions_dir();
    if !dir.exists() {
        anyhow::bail!("Sessions directory not found: {}", dir.display());
    }

    let mut entries: Vec<PrecedentEntry> = Vec::new();

    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("Reading sessions dir: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<CouncilSession>(&content)
        {
            entries.push(entry_from_session(&session));
        }
    }

    // Sort by timestamp
    entries.sort_by_key(|e| e.timestamp.clone());

    // Write fresh index
    let path = index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("Creating index: {}", path.display()))?;

    for e in &entries {
        writeln!(file, "{}", serde_json::to_string(e)?)?;
    }

    Ok(entries.len())
}

// ─── Flight recorder ─────────────────────────────────────────────────

/// Write a flight-recorder markdown summary to runs/.
pub fn write_flight_record(session: &CouncilSession) -> Result<String> {
    let runs_dir = std::env::var("COUNCIL_RUNS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("runs"));

    std::fs::create_dir_all(&runs_dir)?;

    let filename = format!(
        "{}_{}_status.md",
        session.timestamp.format("%Y%m%d_%H%M%S"),
        session.session_id
    );
    let path = runs_dir.join(&filename);

    let md = flight_record_markdown(session);
    std::fs::write(&path, &md)?;
    Ok(path.to_string_lossy().to_string())
}

pub(crate) fn flight_record_markdown(session: &CouncilSession) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Council Session: {}\n\n", session.session_id));
    md.push_str(&format!("**Topic:** {}\n\n", session.topic));
    md.push_str(&format!(
        "**Cabinet:** {} | **Mode:** {:?}\n\n",
        session.cabinet_name, session.mode
    ));
    md.push_str(&format!(
        "**Rounds:** {} | **Tokens:** {} | **Cost:** ${:.4}\n\n",
        session.rounds.len(),
        session.total_tokens,
        session.total_cost_usd
    ));
    md.push_str(&format!("**Timestamp:** {}\n\n", session.timestamp));

    // Flight records are previews; session JSON retains full provider output.
    // Canonical directive envelopes contain only the parsed, fenced proposal.
    // Seats section below is intentionally preview-only, metadata-only (no full .text).
    // Full raw provider+chair text (incl. finish_reason=length metadata) lives 100% in the sessions/*.json
    // written by save_session serde. Human preview paths MUST label "preview-only".
    md.push_str("## Seats (preview-only; full responses and provider metadata are stored in session JSON)\n\n");
    for rnd in &session.rounds {
        md.push_str(&format!("### Round {}\n\n", rnd.round_num));
        md.push_str(&format!(
            "Convergence: {:.0}%\n\n",
            rnd.convergence_score * 100.0
        ));
        for resp in &rnd.responses {
            let status = if resp.error.is_some() { "❌" } else { "✅" };
            md.push_str(&format!(
                "- {} **{}** ({}/{}): {}ms, {} tok out\n",
                status, resp.seat_name, resp.provider, resp.model, resp.latency_ms, resp.tokens_out
            ));
        }
        md.push('\n');
    }

    if let Some(synth) = &session.synthesis {
        md.push_str("## Synthesis\n\n");
        md.push_str(synth);
        md.push('\n');
    }

    md
}

// ─── Precedent injection ─────────────────────────────────────────────

/// Format a retrieval receipt for injection into round prompts (Cold Eyes:
/// injected R2+, not R1 — see engine/deliberate.rs). Citator style: every
/// hit carries its session id, score, and why-matched so seats can affirm,
/// distinguish, or overrule by id, and the prompt text is auditable against
/// the persisted `precedent_ids`.
pub fn format_for_injection(receipt: &RetrievalReceipt) -> String {
    if receipt.hits.is_empty() {
        return String::new();
    }

    let mut text = format!(
        "INSTITUTIONAL MEMORY — Prior Rulings (engine={}, threshold={:.2}):\n",
        receipt.engine, receipt.threshold
    );
    text.push_str(&"─".repeat(40));
    text.push('\n');

    for (i, hit) in receipt.hits.iter().enumerate() {
        let entry = &hit.entry;
        text.push_str(&format!(
            "\n{}. [{}] {} ({}) — score {:.2} ({})\n   ID: {}\n   {}\n",
            i + 1,
            entry
                .timestamp
                .split('T')
                .next()
                .unwrap_or(&entry.timestamp),
            entry.topic,
            entry.cabinet,
            hit.score,
            hit.why,
            entry.session_id,
            entry.digest,
        ));
    }

    text.push_str(&"─".repeat(40));
    text.push_str(
        "\n\nConsider these prior deliberations. You may affirm, build on, \
                    distinguish, or explicitly overrule a prior ruling, but acknowledge \
                    it and cite it by ID.\n\n",
    );
    text
}

// ─── Internal helpers ────────────────────────────────────────────────

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Extract keywords from topic + synthesis for indexing.
fn extract_keywords(topic: &str, synthesis: &str) -> Vec<String> {
    let stopwords: std::collections::HashSet<&str> = [
        "the", "and", "for", "that", "this", "with", "from", "are", "was", "have", "has", "had",
        "been", "will", "would", "could", "should", "not", "but", "they", "their", "them", "what",
        "when", "where", "which", "while", "about", "into", "through", "during", "before", "after",
        "above", "below", "between", "each", "every", "some", "more", "most", "other", "than",
        "then", "just", "also", "very", "can", "does", "did", "its", "our", "your", "all", "any",
    ]
    .into_iter()
    .collect();

    let combined = format!("{} {}", topic, synthesis);
    let mut word_freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for word in combined.to_lowercase().split_whitespace() {
        let clean = word
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if clean.len() > 3 && !stopwords.contains(clean.as_str()) {
            *word_freq.entry(clean).or_insert(0) += 1;
        }
    }

    let mut words: Vec<(String, usize)> = word_freq.into_iter().collect();
    words.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    words.into_iter().take(20).map(|(w, _)| w).collect()
}

/// Build a one-line digest from the synthesis.
fn build_digest(session: &CouncilSession) -> String {
    let synth = session.synthesis.as_deref().unwrap_or("");
    if synth.is_empty() {
        return format!("{} round(s), no synthesis", session.rounds.len());
    }

    // Find the "Ruling" section if present
    for line in synth.lines() {
        let lower = line.to_lowercase();
        if lower.contains("ruling") && line.len() > 15 {
            let clean = line.trim_start_matches('#').trim_start_matches('*').trim();
            if clean.len() > 20 {
                return truncate_at_char_boundary(clean, 500);
            }
        }
    }

    // Fallback: first 500 chars of synthesis (matches Python's [:500])
    truncate_at_char_boundary(&synth.replace('\n', " "), 500)
        .trim()
        .to_string()
}

/// List all session JSON files.
pub fn list_sessions(limit: usize) -> Vec<(String, CouncilSession)> {
    let dir = sessions_dir();
    if !dir.exists() {
        return vec![];
    }

    let mut sessions: Vec<(String, CouncilSession)> = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let path = e.path();
            let content = std::fs::read_to_string(&path).ok()?;
            let session: CouncilSession = serde_json::from_str(&content).ok()?;
            Some((path.to_string_lossy().to_string(), session))
        })
        .collect();

    // Sort newest first
    sessions.sort_by_key(|(_, s)| std::cmp::Reverse(s.timestamp));
    sessions.truncate(limit);
    sessions
}

/// JSON objects for War Room `PrecedentMatch` surfaces, from a retrieval
/// receipt — same shape as `entries_to_match_values` plus `score` and `why`.
pub fn receipt_to_match_values(receipt: &RetrievalReceipt) -> Vec<serde_json::Value> {
    receipt
        .hits
        .iter()
        .map(|hit| {
            let mut v = entries_to_match_values(std::slice::from_ref(&hit.entry))
                .pop()
                .unwrap_or_else(|| serde_json::json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "score".into(),
                    serde_json::json!((hit.score * 10000.0).round() / 10000.0),
                );
                o.insert("why".into(), serde_json::json!(hit.why));
            }
            v
        })
        .collect()
}

/// JSON objects for War Room `PrecedentMatch` / `precedent_loaded` WS events.
pub fn entries_to_match_values(entries: &[PrecedentEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.session_id,
                "ts": e.timestamp,
                "topic": e.topic,
                "cabinet": e.cabinet,
                "keywords": e.keywords,
                "ruling_digest": e.digest,
                "confidence": e.confidence,
                "convergence": e.convergence,
                "mode": e.mode,
            })
        })
        .collect()
}

/// True when a session filename belongs to session `id`.
///
/// Session files are `council_<date>_<time>_<session_id>.json`, so the id is the
/// trailing underscore-delimited token. Match it exactly — anchored by the
/// leading `_` and the `.json` extension — instead of a loose `name.contains(id)`,
/// which let one id match an unrelated session whose name merely embedded it (a
/// short id landing inside another session's UUID, or inside the date/time).
fn session_file_matches(name: &str, id: &str) -> bool {
    !id.is_empty() && name.ends_with(&format!("_{}.json", id))
}

/// Load a single session by its exact session id.
pub fn load_session(id: &str) -> Option<CouncilSession> {
    let dir = sessions_dir();
    if !dir.exists() {
        return None;
    }

    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| session_file_matches(&e.file_name().to_string_lossy(), id))
        .find_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str(&content).ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        topic: &str,
        keywords: &[&str],
        ts: &str,
        origin: SessionOrigin,
    ) -> PrecedentEntry {
        PrecedentEntry {
            schema_version: 2,
            session_id: id.into(),
            timestamp: ts.into(),
            topic: topic.into(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            digest: format!("digest for {}", id),
            confidence: "high".into(),
            cabinet: "standard".into(),
            convergence: 0.85,
            mode: "teardown".into(),
            seat_count: 4,
            rounds: 2,
            synthesis_model: None,
            version: String::new(),
            tier: "best".into(),
            judge_provider: None,
            failure_status: None,
            cited_by: vec![],
            challenged_by: vec![],
            origin,
            execution_route: Default::default(),
            gateway_sensitivity: None,
            worker_provenance: None,
            parent_request_id: None,
        }
    }

    #[test]
    fn entries_to_match_values_maps_precedent_entry_fields() {
        let entries = vec![entry(
            "council_20260101_abc",
            "Market expansion",
            &["market"],
            "2026-01-01T00:00:00Z",
            Default::default(),
        )];
        let matches = entries_to_match_values(&entries);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["id"], "council_20260101_abc");
        assert_eq!(
            matches[0]["ruling_digest"],
            "digest for council_20260101_abc"
        );
    }

    #[test]
    fn rank_keyword_thresholds_junk_and_excludes_api_origin() {
        let query = "gateway sensitivity routing for governed council traffic";
        let index = vec![
            // Strong: several topic-word overlaps.
            entry(
                "strong",
                "Gateway sensitivity routing defaults",
                &["gateway", "routing"],
                "2026-01-02T00:00:00Z",
                SessionOrigin::Cli,
            ),
            // Junk: shares exactly one generic token — the old engine let this in.
            entry(
                "junk",
                "Trading desk council rituals",
                &[],
                "2026-01-03T00:00:00Z",
                SessionOrigin::Cli,
            ),
            // Api-origin must be excluded even if relevant.
            entry(
                "api",
                "Gateway sensitivity routing defaults",
                &["gateway", "routing"],
                "2026-01-04T00:00:00Z",
                SessionOrigin::Api,
            ),
            entry(
                "nomatch",
                "Muse voice pipeline",
                &[],
                "2026-01-05T00:00:00Z",
                SessionOrigin::Cli,
            ),
        ];

        let receipt = rank(
            &index,
            None,
            query,
            RETRIEVE_LIMIT,
            RETRIEVE_THRESHOLD,
            false,
        );
        assert_eq!(receipt.engine, "keyword-v1");
        assert_eq!(receipt.ids(), vec!["strong".to_string()]);
        assert!(receipt.hits[0].score >= RETRIEVE_THRESHOLD);
        assert!(receipt.hits[0].why.starts_with("keyword"));
    }

    #[test]
    fn rank_hybrid_fuses_semantic_and_covers_missing_vectors() {
        let query = "precedent retrieval integrity";
        let index = vec![
            // No token overlap at all — only the dense layer can find it.
            entry(
                "semantic_only",
                "Institutional memory audit trail",
                &["memory"],
                "2026-01-02T00:00:00Z",
                SessionOrigin::Cli,
            ),
            // Keyword match but missing from the vector index (stale embeddings):
            // must fall back to full-weight keyword, not starve at 0.3×.
            entry(
                "no_vector",
                "Precedent retrieval integrity seam",
                &["precedent", "retrieval", "integrity"],
                "2026-01-03T00:00:00Z",
                SessionOrigin::Cli,
            ),
            entry(
                "cold",
                "Muse voice pipeline",
                &[],
                "2026-01-04T00:00:00Z",
                SessionOrigin::Cli,
            ),
        ];
        let mut semantic = std::collections::HashMap::new();
        semantic.insert("semantic_only".to_string(), 0.62);
        semantic.insert("cold".to_string(), 0.03);

        let receipt = rank(
            &index,
            Some(&semantic),
            query,
            RETRIEVE_LIMIT,
            RETRIEVE_THRESHOLD,
            false,
        );
        assert_eq!(receipt.engine, "hybrid-v1");
        let ids = receipt.ids();
        assert!(ids.contains(&"semantic_only".to_string()));
        assert!(ids.contains(&"no_vector".to_string()));
        assert!(
            !ids.contains(&"cold".to_string()),
            "0.7×0.03 is below threshold"
        );

        let no_vector = receipt
            .hits
            .iter()
            .find(|h| h.entry.session_id == "no_vector")
            .unwrap();
        assert!(no_vector.why.contains("no embedding"));
        // Full query coverage in topic+keywords → full-weight keyword score.
        assert!(no_vector.score > 0.5, "got {}", no_vector.score);
    }

    #[test]
    fn rank_empty_query_or_index_is_empty_and_limit_holds() {
        assert!(rank(&[], None, "anything", 5, 0.15, false).hits.is_empty());
        let index = vec![entry(
            "a",
            "Topic",
            &[],
            "2026-01-01T00:00:00Z",
            SessionOrigin::Cli,
        )];
        assert!(rank(&index, None, "", 5, 0.15, false).hits.is_empty());

        let many: Vec<PrecedentEntry> = (0..10)
            .map(|i| {
                entry(
                    &format!("s{}", i),
                    "gateway routing sensitivity",
                    &["gateway"],
                    &format!("2026-01-0{}T00:00:00Z", (i % 9) + 1),
                    SessionOrigin::Cli,
                )
            })
            .collect();
        let receipt = rank(&many, None, "gateway routing sensitivity", 5, 0.15, false);
        assert_eq!(
            receipt.hits.len(),
            5,
            "limit respected, not junk-filled to 10"
        );
    }

    #[test]
    fn rank_long_query_does_not_starve_keyword_scores() {
        // 40+ word matter, keyword-only mode: a strong short topic with a few
        // real overlaps must survive the threshold (capped denominator).
        let query = "we need to decide whether the gateway sensitivity routing \
                     defaults for governed council traffic should change before \
                     launch given the recent war room incidents and the desire \
                     to keep local first behavior intact across every deployment \
                     surface we currently operate including the desktop app";
        let index = vec![entry(
            "strong",
            "Gateway sensitivity routing defaults for governed traffic",
            &["gateway", "routing", "sensitivity", "governed"],
            "2026-01-02T00:00:00Z",
            SessionOrigin::Cli,
        )];
        let receipt = rank(
            &index,
            None,
            query,
            RETRIEVE_LIMIT,
            RETRIEVE_THRESHOLD,
            false,
        );
        assert_eq!(receipt.ids(), vec!["strong".to_string()]);
        assert!(
            receipt.hits[0].score >= RETRIEVE_THRESHOLD,
            "got {}",
            receipt.hits[0].score
        );
    }

    #[test]
    fn rank_ties_break_deterministically_newest_first() {
        let index = vec![
            entry(
                "older",
                "gateway routing",
                &[],
                "2026-01-01T00:00:00Z",
                SessionOrigin::Cli,
            ),
            entry(
                "newer",
                "gateway routing",
                &[],
                "2026-01-05T00:00:00Z",
                SessionOrigin::Cli,
            ),
        ];
        let a = rank(&index, None, "gateway routing", 5, 0.15, false);
        let b = rank(&index, None, "gateway routing", 5, 0.15, false);
        assert_eq!(a.ids(), b.ids());
        assert_eq!(a.ids(), vec!["newer".to_string(), "older".to_string()]);
    }

    /// The integrity gate: the receipt's ids ARE what gets injected, streamed,
    /// and persisted. Prompt text cites every id; match values mirror the ids
    /// in order; `ids()` is what session save writes to `precedent_ids`.
    #[test]
    fn receipt_ids_equal_injection_and_match_values() {
        let index = vec![
            entry(
                "hit_a",
                "gateway routing sensitivity",
                &["gateway"],
                "2026-01-02T00:00:00Z",
                SessionOrigin::Cli,
            ),
            entry(
                "hit_b",
                "governed gateway traffic",
                &["gateway", "governed"],
                "2026-01-03T00:00:00Z",
                SessionOrigin::Cli,
            ),
        ];
        let receipt = rank(
            &index,
            None,
            "governed gateway routing sensitivity traffic",
            5,
            0.15,
            false,
        );
        assert!(!receipt.hits.is_empty());

        let prompt = format_for_injection(&receipt);
        for id in receipt.ids() {
            assert!(
                prompt.contains(&format!("ID: {}", id)),
                "prompt missing {}",
                id
            );
        }
        assert!(prompt.contains(receipt.engine));

        let matches = receipt_to_match_values(&receipt);
        let match_ids: Vec<String> = matches
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(match_ids, receipt.ids());
        assert!(
            matches
                .iter()
                .all(|m| m["score"].is_number() && m["why"].is_string())
        );
    }

    #[test]
    fn format_for_injection_empty_receipt_is_empty() {
        let receipt = RetrievalReceipt {
            engine: "keyword-v1",
            query: "q".into(),
            threshold: 0.15,
            hits: vec![],
        };
        assert!(format_for_injection(&receipt).is_empty());
    }

    #[test]
    fn session_file_matches_requires_exact_id_token() {
        let real = "council_20260101_120000_a1b2c3d4e5f6.json";

        // Exact session-id token matches.
        assert!(session_file_matches(real, "a1b2c3d4e5f6"));

        // Regression: the old `name.contains(id)` matched all of these; the
        // anchored exact-token match must reject them.
        assert!(!session_file_matches(real, "a1b2c3")); // leading fragment of the id
        assert!(!session_file_matches(real, "b2c3d4e5f6")); // interior fragment of the id
        assert!(!session_file_matches(real, "120000")); // fragment of the timestamp
        // Unrelated session whose UUID merely embeds another id.
        assert!(!session_file_matches(
            "council_20260101_120000_00a1b2c3d4e5.json",
            "a1b2c3d4e5"
        ));

        // Empty id must never match a real session.
        assert!(!session_file_matches(real, ""));
        // Wrong extension does not match.
        assert!(!session_file_matches(
            "council_20260101_120000_a1b2c3d4e5f6.txt",
            "a1b2c3d4e5f6"
        ));
    }
}
