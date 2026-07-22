//! Drift self-audit — re-runs recent sessions in BLIND mode and scores how
//! much precedent injection moved the verdict.
//!
//! Two modes:
//!   - `run_drift_report` — daily/weekly drift_<date>.md
//!   - `run_weekly_summary` — aggregates the last N drift reports + posts webhooks

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use super::{runs_dir, sessions_dir};
use crate::config::Config;
use crate::engine::deliberate;
use crate::mode::Mode;
use crate::precedent;
use crate::types::SessionMode;

/// Deliberation modes eligible for precedent-anchoring drift checks.
/// Includes legacy `normal` plus the War Room modes in active use.
fn is_drift_eligible_mode(mode: &str) -> bool {
    matches!(mode, "normal" | "teardown" | "pathfind" | "harden")
}

fn session_mode_to_deliberate(mode: &SessionMode) -> Option<Mode> {
    match mode {
        SessionMode::Normal | SessionMode::TearDown => Some(Mode::TearDown),
        SessionMode::Pathfind => Some(Mode::Pathfind),
        SessionMode::Harden => Some(Mode::Harden),
        _ => None,
    }
}

fn lock_path() -> PathBuf {
    sessions_dir().join("drift.lock")
}

pub fn is_running() -> bool {
    lock_path().exists()
}

pub fn acquire_lock() -> bool {
    let path = lock_path();
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, Utc::now().to_rfc3339()).is_ok()
}

pub fn release_lock() {
    let _ = std::fs::remove_file(lock_path());
}

fn iso_mtime(p: &std::path::Path) -> String {
    p.metadata()
        .and_then(|m| m.modified())
        .ok()
        .map(|t| {
            let dt: DateTime<Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default()
}

pub fn list_reports() -> Vec<Value> {
    let dir = runs_dir();
    if !dir.exists() {
        return vec![];
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.starts_with("drift_") || !name.ends_with(".md") {
            continue;
        }
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(json!({
            "name": name,
            "size": size,
            "mtime": iso_mtime(&path),
        }));
    }
    out.sort_by(|a, b| {
        b.get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .cmp(a.get("name").and_then(|x| x.as_str()).unwrap_or(""))
    });
    out
}

pub fn get_report(name: &str) -> Option<Value> {
    if name.contains('/') || name.contains("..") {
        return None;
    }
    let path = runs_dir().join(name);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    Some(json!({
        "name": name,
        "content": content,
        "mtime": iso_mtime(&path),
    }))
}

// ─── Weekly summaries (read) ──────────────────────────────────────

fn collect_weekly_files() -> Vec<PathBuf> {
    let dir = runs_dir();
    if !dir.exists() {
        return vec![];
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("weekly_drift_") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    out.sort_by(|a, b| {
        b.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .cmp(a.file_name().and_then(|n| n.to_str()).unwrap_or(""))
    });
    out
}

pub fn latest_weekly_summary() -> Option<Value> {
    let files = collect_weekly_files();
    let path = files.first()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn weekly_history(limit: usize) -> Vec<Value> {
    collect_weekly_files()
        .into_iter()
        .take(limit)
        .filter_map(|p| std::fs::read_to_string(&p).ok())
        .filter_map(|c| serde_json::from_str::<Value>(&c).ok())
        .collect()
}

// ─── drift/run — replay sessions in blind mode ────────────────────

const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "to", "of", "and", "or", "but", "in", "on", "for", "with", "as",
    "by", "at", "this", "that",
];

fn words_set(text: &str) -> HashSet<String> {
    let stop: HashSet<&str> = STOP_WORDS.iter().copied().collect();
    let mut out = HashSet::new();
    for w in text.to_lowercase().split(|c: char| !c.is_alphabetic()) {
        if w.len() < 4 || stop.contains(w) {
            continue;
        }
        out.insert(w.to_string());
    }
    out
}

fn confidence_token(text: &str) -> Option<String> {
    for token in ["HIGH", "MEDIUM", "LOW"] {
        if text.contains(token) {
            return Some(token.to_string());
        }
    }
    None
}

fn jaccard_score(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count();
    let uni = a.union(b).count().max(1);
    inter as f64 / uni as f64
}

#[derive(Debug, Clone)]
struct DriftRow {
    session_id: String,
    topic: String,
    cabinet: String,
    ts: String,
    original_convergence: f64,
    blind_convergence: f64,
    drift_score: f64,
    similarity: Option<f64>,
    similarity_method: &'static str,
    jaccard: f64,
    confidence_normal: Option<String>,
    confidence_blind: Option<String>,
    confidence_drift: bool,
    blind_synthesis_preview: String,
}

/// Serialize one drift row for the per-session JSON consumed by weekly digests.
///
/// On the jaccard fallback (`similarity` is None / `similarity_method` ==
/// "jaccard") the jaccard value is emitted as `similarity` so the field is
/// always a number; the legacy `jaccard` field is retained for back-compat and
/// method disambiguation.
fn drift_row_json(r: &DriftRow) -> Value {
    json!({
        "session_id": r.session_id,
        "topic": r.topic,
        "cabinet": r.cabinet,
        "ts": r.ts,
        "original_convergence": r.original_convergence,
        "blind_convergence": r.blind_convergence,
        "drift_score": r.drift_score,
        "similarity": r.similarity.unwrap_or(r.jaccard),
        "similarity_method": r.similarity_method,
        "jaccard": r.jaccard,
        "confidence_normal": r.confidence_normal,
        "confidence_blind": r.confidence_blind,
        "confidence_drift": r.confidence_drift,
        "blind_synthesis_preview": r.blind_synthesis_preview,
    })
}

/// Scores computed by `diff` for one normal/blind synthesis pair.
struct DiffScores {
    drift_score: f64,
    similarity: Option<f64>,
    similarity_method: &'static str,
    jaccard: f64,
    confidence_normal: Option<String>,
    confidence_blind: Option<String>,
    confidence_drift: bool,
}

fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

/// feature contract: drift = `1 − cosine` when the embedding path is available, else
/// `1 − jaccard`. Returns the drift score plus which method produced it
/// ("cosine" | "jaccard") for the report's `similarity_method` field.
fn drift_from_similarity(jaccard: f64, similarity: Option<f64>) -> (f64, &'static str) {
    match similarity {
        Some(s) => (round4(1.0 - s), "cosine"),
        None => (round4(1.0 - jaccard), "jaccard"),
    }
}

fn diff(original: &str, blind: &str) -> DiffScores {
    let a = words_set(original);
    let b = words_set(blind);
    let jaccard = jaccard_score(&a, &b);
    // feature contract: semantic similarity via the existing embeddings module — same
    // clip + cosine-with-jaccard-fallback pattern as lineage::diff_synthesis.
    // cosine_similarity returns None on any init/encode failure (e.g. model
    // not downloaded), which degrades cleanly to the jaccard path. The
    // jaccard field stays populated either way (back-compat).
    let o_clip: String = original.chars().take(2000).collect();
    let b_clip: String = blind.chars().take(2000).collect();
    let similarity = super::embeddings::cosine_similarity(&o_clip, &b_clip);
    let (drift_score, similarity_method) = drift_from_similarity(jaccard, similarity);
    let confidence_normal = confidence_token(original);
    let confidence_blind = confidence_token(blind);
    let confidence_drift = confidence_normal != confidence_blind;
    DiffScores {
        drift_score,
        similarity: similarity.map(round4),
        similarity_method,
        jaccard: round4(jaccard),
        confidence_normal,
        confidence_blind,
        confidence_drift,
    }
}

/// Resolve a session's cabinet label to a registry key (accepts key or display name).
fn find_cabinet_key_by_label(config: &Arc<Config>, label: &str) -> Option<String> {
    if config.cabinets.contains_key(label) {
        return Some(label.to_string());
    }
    config
        .cabinets
        .iter()
        .find(|(_, cab)| cab.name == label)
        .map(|(k, _)| k.clone())
}

/// Read sessions/index.jsonl and return entries satisfying the drift criteria:
/// deliberation mode in [`is_drift_eligible_mode`] AND ts >= cutoff. Newest first.
fn eligible_candidates(window_days: u32, limit: Option<usize>) -> Vec<Value> {
    let path = sessions_dir().join("index.jsonl");
    if !path.exists() {
        return vec![];
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let cutoff = (Utc::now() - Duration::days(window_days as i64)).to_rfc3339();
    let mut out: Vec<Value> = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let v = match serde_json::from_str::<Value>(&line) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let mode = v.get("mode").and_then(|x| x.as_str()).unwrap_or("");
        if !is_drift_eligible_mode(mode) {
            continue;
        }
        let ts = v
            .get("ts")
            .or_else(|| v.get("timestamp"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if ts.is_empty() || ts < cutoff.as_str() {
            continue;
        }
        out.push(v);
    }
    // Newest first
    out.sort_by(|a, b| {
        b.get("ts")
            .or_else(|| b.get("timestamp"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .cmp(
                a.get("ts")
                    .or_else(|| a.get("timestamp"))
                    .and_then(|x| x.as_str())
                    .unwrap_or(""),
            )
    });
    if let Some(n) = limit {
        out.truncate(n);
    }
    out
}

/// Replay a session in blind mode (same deliberation mode, no precedent), return
/// (synthesis, final_convergence).
async fn shadow(
    config: &Arc<Config>,
    cabinet_label: &str,
    topic: &str,
    mode: Mode,
) -> Option<(String, f64)> {
    let key = find_cabinet_key_by_label(config, cabinet_label)?;
    let session = deliberate::run(
        config, &key, topic, "", mode, true, false, false, None, "best", false, "grok", false,
    )
    .await
    .ok()?;
    let synthesis = session.synthesis?;
    let conv = session
        .rounds
        .last()
        .map(|r| r.convergence_score)
        .unwrap_or(0.0);
    Some((synthesis, conv))
}

/// Public entry — run a drift report for the given window. Writes to
/// runs/drift_<YYYYMMDD>.md and returns a summary dict.
pub async fn run_drift_report(
    config: &Arc<Config>,
    window_days: u32,
    limit: Option<usize>,
) -> Value {
    let candidates = eligible_candidates(window_days, limit);
    if candidates.is_empty() {
        return json!({
            "sessions_analyzed": 0,
            "report_path": Value::Null,
            "reason": "no eligible sessions in window",
        });
    }

    eprintln!(
        "[drift] {} candidate(s) within {} day(s)",
        candidates.len(),
        window_days
    );
    let mut rows: Vec<DriftRow> = Vec::new();
    for (idx, meta) in candidates.iter().enumerate() {
        let sid = meta
            .get("id")
            .or_else(|| meta.get("session_id"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if sid.is_empty() {
            eprintln!(
                "[drift] [{}/{}] no session_id in candidate, skip",
                idx + 1,
                candidates.len()
            );
            continue;
        }
        let full = match precedent::load_session(sid) {
            Some(s) => s,
            None => {
                eprintln!(
                    "[drift] [{}/{}] {}: load_session returned None, skip",
                    idx + 1,
                    candidates.len(),
                    sid
                );
                continue;
            }
        };
        let cabinet_label = &full.cabinet_name;
        let topic = full.topic.clone();
        eprintln!(
            "[drift] [{}/{}] {} cabinet='{}' topic='{}'",
            idx + 1,
            candidates.len(),
            sid,
            cabinet_label,
            topic.chars().take(60).collect::<String>()
        );
        let original = full.synthesis.clone().unwrap_or_default();
        let original_conv = full
            .rounds
            .last()
            .map(|r| r.convergence_score)
            .unwrap_or(0.0);

        let replay_mode = match session_mode_to_deliberate(&full.mode) {
            Some(m) => m,
            None => {
                eprintln!(
                    "[drift] [{}/{}] {} mode {:?} not replayable, skip",
                    idx + 1,
                    candidates.len(),
                    sid,
                    full.mode
                );
                continue;
            }
        };
        let (blind_synth, blind_conv) = match shadow(config, cabinet_label, &topic, replay_mode)
            .await
        {
            Some(x) => {
                eprintln!(
                    "[drift] [{}/{}] {} shadow done, synth_len={}",
                    idx + 1,
                    candidates.len(),
                    sid,
                    x.0.len()
                );
                x
            }
            None => {
                eprintln!(
                    "[drift] [{}/{}] {} shadow returned None (cabinet '{}' not reproducible or run failed), skip",
                    idx + 1,
                    candidates.len(),
                    sid,
                    cabinet_label
                );
                continue;
            }
        };
        let scores = diff(&original, &blind_synth);
        let preview: String = blind_synth.chars().take(600).collect();

        rows.push(DriftRow {
            session_id: sid.to_string(),
            topic: topic.chars().take(200).collect(),
            cabinet: cabinet_label.clone(),
            ts: full.timestamp.to_rfc3339(),
            original_convergence: original_conv,
            blind_convergence: blind_conv,
            drift_score: scores.drift_score,
            similarity: scores.similarity,
            similarity_method: scores.similarity_method,
            jaccard: scores.jaccard,
            confidence_normal: scores.confidence_normal,
            confidence_blind: scores.confidence_blind,
            confidence_drift: scores.confidence_drift,
            blind_synthesis_preview: preview,
        });
    }

    let drift_values: Vec<f64> = rows.iter().map(|r| r.drift_score).collect();
    let avg_drift = if drift_values.is_empty() {
        0.0
    } else {
        drift_values.iter().sum::<f64>() / drift_values.len() as f64
    };
    let confidence_changed = rows.iter().filter(|r| r.confidence_drift).count();
    let high: Vec<&DriftRow> = rows.iter().filter(|r| r.drift_score >= 0.4).collect();

    let today = Utc::now().format("%Y%m%d").to_string();
    let dir = runs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let out_path = dir.join(format!("drift_{}.md", today));

    let mut md = String::new();
    md.push_str(&format!("# Drift Report — {}\n\n", today));
    md.push_str(&format!("**Window:** last {} day(s)\n", window_days));
    md.push_str(&format!("**Sessions analyzed:** {}\n", rows.len()));
    md.push_str(&format!(
        "**Avg drift score:** {:.3}  (0 = identical, 1 = fully divergent)\n",
        avg_drift
    ));
    md.push_str(&format!(
        "**Confidence changed:** {}/{} sessions\n",
        confidence_changed,
        rows.len()
    ));
    md.push_str(&format!(
        "**High-drift sessions (≥ 0.40):** {}\n\n",
        high.len()
    ));
    md.push_str("## Reading this report\n\n");
    md.push_str("Drift score = `1 − similarity(original_synthesis, blind_synthesis)`. ");
    md.push_str("Similarity is semantic cosine (fastembed MiniLM-L6-v2) when the ");
    md.push_str("embedding model is available, falling back to jaccard word overlap — ");
    md.push_str("each row's `similarity_method` records which was used. ");
    md.push_str("When drift is persistently high, the precedent engine is anchoring ");
    md.push_str("the council toward old conclusions. When confidence flips between ");
    md.push_str("original and blind runs, the precedent is doing real work — investigate ");
    md.push_str("those sessions individually.\n\n");

    if !high.is_empty() {
        md.push_str("## High-drift sessions\n\n");
        md.push_str("| Session | Drift | Confidence | Cabinet | Topic |\n");
        md.push_str("|---|---|---|---|---|\n");
        let mut sorted = high.clone();
        sorted.sort_by(|a, b| {
            b.drift_score
                .partial_cmp(&a.drift_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for r in sorted {
            let cd = if r.confidence_drift {
                format!(
                    "{} → {}",
                    r.confidence_normal.as_deref().unwrap_or("—"),
                    r.confidence_blind.as_deref().unwrap_or("—")
                )
            } else {
                r.confidence_normal
                    .clone()
                    .unwrap_or_else(|| "—".to_string())
            };
            md.push_str(&format!(
                "| `{}` | {:.3} | {} | {} | {} |\n",
                r.session_id,
                r.drift_score,
                cd,
                r.cabinet,
                r.topic.chars().take(60).collect::<String>(),
            ));
        }
        md.push('\n');
    }

    md.push_str("## All sessions\n\n");
    md.push_str("| Session | Drift | Sim | Jacc | Confidence | Cabinet |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    let mut all_sorted: Vec<&DriftRow> = rows.iter().collect();
    all_sorted.sort_by(|a, b| {
        b.drift_score
            .partial_cmp(&a.drift_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for r in &all_sorted {
        let cd = if r.confidence_drift {
            format!(
                "{} → {}",
                r.confidence_normal.as_deref().unwrap_or("—"),
                r.confidence_blind.as_deref().unwrap_or("—")
            )
        } else {
            r.confidence_normal
                .clone()
                .unwrap_or_else(|| "—".to_string())
        };
        let sim = match r.similarity {
            Some(s) => format!("{:.3}", s),
            None => "—".to_string(),
        };
        md.push_str(&format!(
            "| `{}` | {:.3} | {} | {:.3} | {} | {} |\n",
            r.session_id, r.drift_score, sim, r.jaccard, cd, r.cabinet,
        ));
    }
    md.push_str("\n## Per-session detail\n\n");
    for r in &all_sorted {
        md.push_str(&format!(
            "### `{}` — {}\n\n",
            r.session_id,
            r.topic.chars().take(80).collect::<String>()
        ));
        md.push_str(&format!("- Drift: **{:.3}**\n", r.drift_score));
        md.push_str(&format!(
            "- Confidence: {} → {}{}\n",
            r.confidence_normal.as_deref().unwrap_or("—"),
            r.confidence_blind.as_deref().unwrap_or("—"),
            if r.confidence_drift {
                "  ⚠️ CHANGED"
            } else {
                ""
            }
        ));
        md.push_str(&format!("- Cabinet: {}\n", r.cabinet));
        md.push_str(&format!(
            "- Original convergence: {:.0}%, blind: {:.0}%\n\n",
            r.original_convergence * 100.0,
            r.blind_convergence * 100.0
        ));
        md.push_str("**Blind synthesis preview:**\n\n```\n");
        md.push_str(&r.blind_synthesis_preview);
        md.push_str("\n```\n\n");
    }

    let _ = std::fs::write(&out_path, &md);

    // Build per-row JSON for downstream weekly digests.
    let rows_json: Vec<Value> = rows.iter().map(drift_row_json).collect();

    json!({
        "sessions_analyzed": rows.len(),
        "avg_drift": (avg_drift * 10000.0).round() / 10000.0,
        "confidence_changed": confidence_changed,
        "high_drift_count": high.len(),
        "report_path": out_path.to_string_lossy(),
        "report_filename": out_path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "rows": rows_json,
    })
}

pub fn iso_mtime_pub(p: &std::path::Path) -> String {
    iso_mtime(p)
}

/// Compute top-N anchoring patterns: keywords most correlated with drift.
/// For each keyword across affected sessions, weight average drift by
/// occurrence count. Mirrors Python weekly_drift._top_anchoring().
fn top_anchoring(rows: &[Value], top_n: usize) -> Vec<Value> {
    if rows.is_empty() {
        return vec![];
    }

    let index_path = sessions_dir().join("index.jsonl");
    let meta: HashMap<String, Value> = if index_path.exists() {
        let file = match std::fs::File::open(&index_path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        std::io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .filter_map(|v| {
                let id = v
                    .get("id")
                    .or_else(|| v.get("session_id"))
                    .and_then(|x| x.as_str())
                    .map(String::from)?;
                Some((id, v))
            })
            .collect()
    } else {
        return vec![];
    };

    let mut bucket: HashMap<String, Vec<f64>> = HashMap::new();
    for r in rows {
        let sid = r.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
        let drift = r.get("drift_score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let keywords = meta
            .get(sid)
            .and_then(|m| m.get("keywords"))
            .and_then(|k| k.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        for kw in keywords {
            bucket.entry(kw.to_string()).or_default().push(drift);
        }
    }

    let mut scored: Vec<Value> = bucket
        .iter()
        .filter(|(_, drifts)| !drifts.is_empty())
        .map(|(kw, drifts)| {
            let avg = drifts.iter().sum::<f64>() / drifts.len() as f64;
            let score = avg * (1.0 + 0.3 * (drifts.len() as f64 - 1.0));
            json!({
                "keyword": kw,
                "avg_drift": (avg * 10000.0).round() / 10000.0,
                "session_count": drifts.len(),
                "score": (score * 10000.0).round() / 10000.0,
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        b.get("score")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
            .partial_cmp(&a.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(top_n);
    scored
}

// ─── drift/weekly/run — aggregate the last N drift reports ───────

pub async fn run_weekly_summary(
    config: &Arc<Config>,
    window_days: u32,
    limit: Option<usize>,
    post_webhooks: bool,
) -> Value {
    let drift_summary = run_drift_report(config, window_days, limit).await;

    // Build the flat shape the React WeeklyDriftCard expects.
    let rows = drift_summary
        .get("rows")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let sessions_analyzed = drift_summary
        .get("sessions_analyzed")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let avg_drift = drift_summary
        .get("avg_drift")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let confidence_flips = drift_summary
        .get("confidence_changed")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let high_drift_count = drift_summary
        .get("high_drift_count")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let report_filename = drift_summary
        .get("report_filename")
        .and_then(|x| x.as_str())
        .map(String::from);
    let report_path = drift_summary
        .get("report_path")
        .and_then(|x| x.as_str())
        .map(String::from);
    let reason = drift_summary
        .get("reason")
        .and_then(|x| x.as_str())
        .map(String::from);

    // Headline = highest-drift session
    let headline_session = rows
        .iter()
        .max_by(|a, b| {
            let av = a.get("drift_score").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let bv = b.get("drift_score").and_then(|x| x.as_f64()).unwrap_or(0.0);
            av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|r| {
            json!({
                "session_id": r.get("session_id"),
                "topic": r.get("topic"),
                "drift_score": r.get("drift_score"),
                "confidence_normal": r.get("confidence_normal"),
                "confidence_blind": r.get("confidence_blind"),
                "confidence_changed": r.get("confidence_drift"),
            })
        });

    let top_anchoring = top_anchoring(&rows, 3);

    let summary = json!({
        "ts": Utc::now().to_rfc3339(),
        "window_days": window_days,
        "sessions_analyzed": sessions_analyzed,
        "avg_drift": avg_drift,
        "confidence_flips": confidence_flips,
        "high_drift_count": high_drift_count,
        "top_anchoring": top_anchoring,
        "report_filename": report_filename,
        "report_path": report_path,
        "headline_session": headline_session,
        "reason": reason,
    });

    let today = Utc::now().format("%Y%m%d").to_string();
    let week_path = runs_dir().join(format!("weekly_drift_{}.json", today));
    let webhook_results = if post_webhooks {
        post_weekly_webhooks(&summary).await
    } else {
        json!({})
    };

    // Re-build summary with webhook results included, then persist
    let mut final_summary = summary.clone();
    if let Some(obj) = final_summary.as_object_mut() {
        obj.insert("webhooks".to_string(), webhook_results);
    }

    let _ = std::fs::write(
        &week_path,
        serde_json::to_vec_pretty(&final_summary).unwrap_or_default(),
    );
    final_summary
}

fn build_webhook_context(summary: &Value) -> tera::Context {
    let mut ctx = tera::Context::new();
    let drift = summary
        .get("avg_drift")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    ctx.insert(
        "ts",
        &summary.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
    );
    ctx.insert(
        "window_days",
        &summary
            .get("window_days")
            .and_then(|v| v.as_u64())
            .unwrap_or(7),
    );
    ctx.insert(
        "sessions_analyzed",
        &summary
            .get("sessions_analyzed")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    );
    ctx.insert("avg_drift", &format!("{:.3}", drift));
    ctx.insert(
        "confidence_flips",
        &summary
            .get("confidence_flips")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    );
    ctx.insert(
        "high_drift_count",
        &summary
            .get("high_drift_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    );
    ctx.insert(
        "report_filename",
        &summary
            .get("report_filename")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    ctx.insert(
        "drift_emoji",
        if drift > 0.4 {
            &"🔴"
        } else if drift > 0.2 {
            &"🟡"
        } else {
            &"🟢"
        },
    );
    ctx.insert(
        "drift_color",
        if drift > 0.4 {
            &0xff4444u32
        } else if drift > 0.2 {
            &0xffaa00u32
        } else {
            &0x00ff9du32
        },
    );
    ctx.insert(
        "top_anchoring",
        &summary
            .get("top_anchoring")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    );
    ctx
}

fn load_webhook_tera() -> Option<tera::Tera> {
    let base = super::project_root().join("prompts").join("webhooks");
    if !base.exists() {
        return None;
    }
    let glob = format!("{}/*.tera", base.display());
    tera::Tera::new(&glob).ok()
}

async fn http_post(client: &reqwest::Client, url: &str, body: &Value) -> String {
    match client
        .post(url)
        .json(body)
        .header("User-Agent", "council-warroom/3.0")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(resp) => format!("ok ({})", resp.status().as_u16()),
        // T24: reqwest embeds the request URL in its Display — for the Telegram
        // webhook that URL carries the bot token. This error string is persisted
        // into the weekly drift summary, so strip the URL before it escapes.
        Err(e) => format!("err: {}", e.without_url()),
    }
}

async fn post_weekly_webhooks(summary: &Value) -> Value {
    let ctx = build_webhook_context(summary);
    let tera = load_webhook_tera();
    let client = reqwest::Client::new();
    let mut results = serde_json::Map::new();

    if let Ok(url) = std::env::var("COUNCIL_WEBHOOK_SLACK") {
        let body = tera
            .as_ref()
            .and_then(|t| t.render("slack.tera", &ctx).ok())
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let status = match body {
            Some(payload) => http_post(&client, &url, &payload).await,
            None => "err: template render failed".to_string(),
        };
        results.insert("slack".into(), json!(status));
    } else {
        results.insert("slack".into(), json!("skipped (not configured)"));
    }

    if let Ok(url) = std::env::var("COUNCIL_WEBHOOK_DISCORD") {
        let body = tera
            .as_ref()
            .and_then(|t| t.render("discord.tera", &ctx).ok())
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let status = match body {
            Some(payload) => http_post(&client, &url, &payload).await,
            None => "err: template render failed".to_string(),
        };
        results.insert("discord".into(), json!(status));
    } else {
        results.insert("discord".into(), json!("skipped (not configured)"));
    }

    if let Ok(config) = std::env::var("COUNCIL_WEBHOOK_TELEGRAM") {
        let parts: Vec<&str> = config.splitn(2, '|').collect();
        if parts.len() == 2 {
            let token = parts[0].trim();
            let token = if token.starts_with("bot") {
                token.to_string()
            } else {
                format!("bot{}", token)
            };
            let chat_id = parts[1].trim();
            let text = tera
                .as_ref()
                .and_then(|t| t.render("telegram.tera", &ctx).ok())
                .unwrap_or_else(|| {
                    format!(
                        "Council Weekly Drift — {} sessions, avg drift {:.3}",
                        summary
                            .get("sessions_analyzed")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                        summary
                            .get("avg_drift")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    )
                });
            let url = format!("https://api.telegram.org/{}/sendMessage", token);
            let payload = json!({"chat_id": chat_id, "text": text, "parse_mode": "Markdown"});
            let status = http_post(&client, &url, &payload).await;
            results.insert("telegram".into(), json!(status));
        } else {
            results.insert(
                "telegram".into(),
                json!("err: format <BOT_TOKEN>|<CHAT_ID>"),
            );
        }
    } else {
        results.insert("telegram".into(), json!("skipped (not configured)"));
    }

    if let Ok(url) = std::env::var("COUNCIL_WEBHOOK_GENERIC") {
        let status = http_post(&client, &url, summary).await;
        results.insert("generic".into(), json!(status));
    } else {
        results.insert("generic".into(), json!("skipped (not configured)"));
    }

    Value::Object(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix 8 (T24): the Telegram webhook URL carries the bot token, and a failed
    /// POST's status string is persisted into the weekly drift summary. Force a
    /// connection error against a closed local port and assert the token (and the
    /// URL it rides in) never reach the returned status.
    #[tokio::test]
    async fn http_post_strips_token_url_on_error() {
        let token = "bot123456:AA-SENTINELtelegramTOKEN0123456789abcdef";
        let url = format!("http://127.0.0.1:1/{token}/sendMessage");
        let client = reqwest::Client::new();
        let status = http_post(&client, &url, &json!({"x": 1})).await;
        assert!(
            status.starts_with("err:"),
            "expected an error status, got: {status}"
        );
        assert!(
            !status.contains(token),
            "bot token leaked into error status: {status}"
        );
        assert!(
            !status.contains("sendMessage"),
            "request url leaked into error status: {status}"
        );
    }

    /// feature contract pinned contract: when the embedding path is available the drift
    /// score comes from cosine and `similarity_method` says "cosine"; when it
    /// is not, the score falls back to jaccard and the method records it.
    /// Hermetic — exercises the scoring decision without touching fastembed.
    #[test]
    fn eligible_candidates_finds_recent_warroom_sessions_on_disk() {
        let index = sessions_dir().join("index.jsonl");
        if !index.exists() {
            return;
        }
        let candidates = eligible_candidates(7, Some(3));
        assert!(
            !candidates.is_empty(),
            "expected teardown/harden sessions in {:?} within 7d",
            index
        );
    }

    #[test]
    fn drift_eligible_modes_cover_warroom_defaults() {
        assert!(is_drift_eligible_mode("teardown"));
        assert!(is_drift_eligible_mode("harden"));
        assert!(is_drift_eligible_mode("pathfind"));
        assert!(is_drift_eligible_mode("normal"));
        assert!(!is_drift_eligible_mode("contrarian"));
        assert!(!is_drift_eligible_mode("blind"));
    }

    #[test]
    fn session_mode_to_deliberate_maps_active_modes() {
        assert_eq!(
            session_mode_to_deliberate(&SessionMode::Harden),
            Some(Mode::Harden)
        );
        assert_eq!(session_mode_to_deliberate(&SessionMode::Contrarian), None);
    }

    #[test]
    fn drift_from_similarity_prefers_cosine_and_falls_back_to_jaccard() {
        let (drift, method) = drift_from_similarity(0.25, Some(0.9));
        assert!((drift - 0.1).abs() < 1e-9, "drift from cosine: {drift}");
        assert_eq!(method, "cosine");

        let (drift, method) = drift_from_similarity(0.25, None);
        assert!((drift - 0.75).abs() < 1e-9, "drift from jaccard: {drift}");
        assert_eq!(method, "jaccard");
    }

    #[test]
    fn drift_from_similarity_rounds_to_four_decimals() {
        let (drift, _) = drift_from_similarity(0.0, Some(0.123_456_78));
        assert_eq!(drift, 0.8765);
    }

    /// Identical texts: jaccard 1.0 → drift 0.0 on the fallback path.
    #[test]
    fn jaccard_identical_texts_score_one() {
        let a = words_set("ship the council release tomorrow");
        let b = words_set("ship the council release tomorrow");
        assert_eq!(jaccard_score(&a, &b), 1.0);
        let (drift, method) = drift_from_similarity(jaccard_score(&a, &b), None);
        assert_eq!(drift, 0.0);
        assert_eq!(method, "jaccard");
    }

    fn sample_row(
        similarity: Option<f64>,
        similarity_method: &'static str,
        jaccard: f64,
    ) -> DriftRow {
        DriftRow {
            session_id: "sid".into(),
            topic: "t".into(),
            cabinet: "warroom".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            original_convergence: 0.8,
            blind_convergence: 0.7,
            drift_score: 0.3,
            similarity,
            similarity_method,
            jaccard,
            confidence_normal: Some("HIGH".into()),
            confidence_blind: Some("HIGH".into()),
            confidence_drift: false,
            blind_synthesis_preview: "preview".into(),
        }
    }

    /// PR fix: on the jaccard fallback the JSON `similarity` is the jaccard
    /// value (never null) while the legacy `jaccard` field is preserved.
    #[test]
    fn drift_row_json_falls_back_to_jaccard_for_similarity() {
        let row = sample_row(None, "jaccard", 0.42);
        let v = drift_row_json(&row);
        assert_eq!(v["similarity"], json!(0.42), "fallback emits jaccard value");
        assert!(!v["similarity"].is_null(), "similarity must not be null");
        assert_eq!(v["similarity_method"], json!("jaccard"));
        assert_eq!(v["jaccard"], json!(0.42), "legacy jaccard field retained");
    }

    /// When cosine is available, `similarity` carries the cosine value and the
    /// jaccard field still reports its own (different) score.
    #[test]
    fn drift_row_json_uses_cosine_when_present() {
        let row = sample_row(Some(0.91), "cosine", 0.42);
        let v = drift_row_json(&row);
        assert_eq!(v["similarity"], json!(0.91));
        assert_eq!(v["similarity_method"], json!("cosine"));
        assert_eq!(v["jaccard"], json!(0.42));
    }
}
