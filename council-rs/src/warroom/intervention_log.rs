//! Mirror of warroom/backend/intervention_log.py — read-only side.
//!
//! Format (one JSON per line in sessions/intervention_log.jsonl):
//!   {session_id, action, payload, round_num, convergence_at_pause, ts, logged_at}

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::PathBuf;

use chrono::{Duration, Utc};
use serde_json::{Value, json};

use super::sessions_dir;

fn log_path() -> PathBuf {
    sessions_dir().join("intervention_log.jsonl")
}

fn session_index_path() -> PathBuf {
    sessions_dir().join("index.jsonl")
}

/// Load all entries, optionally filtered by window_days (cutoff in UTC).
pub fn load_all(window_days: Option<i64>) -> Vec<Value> {
    let path = log_path();
    if !path.exists() {
        return vec![];
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let cutoff = window_days.map(|d| (Utc::now() - Duration::days(d)).to_rfc3339());

    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|entry| {
            if let Some(c) = &cutoff {
                entry
                    .get("ts")
                    .and_then(|t| t.as_str())
                    .map(|t| t >= c.as_str())
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .collect()
}

/// Load session-index keyed by session_id (for cabinet/keyword cross-reference).
fn load_session_meta() -> HashMap<String, Value> {
    let path = session_index_path();
    if !path.exists() {
        return HashMap::new();
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            // index.jsonl uses "session_id" in our Rust port
            // (Python uses "id" — accept both for robustness).
            let id = v
                .get("session_id")
                .or_else(|| v.get("id"))
                .and_then(|x| x.as_str())
                .map(String::from);
            if let Some(id) = id {
                out.insert(id, v);
            }
        }
    }
    out
}

/// Aggregate operator behaviour for /api/patterns.
pub fn patterns(window_days: Option<i64>) -> Value {
    let entries = load_all(window_days);
    let session_meta = load_session_meta();

    if entries.is_empty() {
        return json!({
            "total": 0,
            "session_count": 0,
            "actions": {},
            "by_round": {},
            "by_cabinet": {},
            "convergence_buckets": {},
            "top_keywords": [],
            "avg_convergence_at_pause": 0.0,
            "sequences": [],
            "multi_intervention_sessions": 0,
            "recent": [],
            "window_days": window_days,
        });
    }

    // Action counter
    let mut actions: BTreeMap<String, u64> = BTreeMap::new();
    for e in &entries {
        let a = e
            .get("action")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        *actions.entry(a).or_insert(0) += 1;
    }

    // By round (string keys to match Python output)
    let mut by_round: BTreeMap<String, u64> = BTreeMap::new();
    for e in &entries {
        let r = e.get("round_num").and_then(|x| x.as_i64()).unwrap_or(0);
        *by_round.entry(r.to_string()).or_insert(0) += 1;
    }

    // By cabinet -> action counts
    let mut by_cabinet: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for e in &entries {
        let sid = e.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
        let cab = session_meta
            .get(sid)
            .and_then(|m| m.get("cabinet"))
            .and_then(|x| x.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let action = e
            .get("action")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        *by_cabinet
            .entry(cab)
            .or_default()
            .entry(action)
            .or_insert(0) += 1;
    }

    // Convergence buckets
    let mut buckets: BTreeMap<&str, u64> = [
        ("0-20%", 0),
        ("20-40%", 0),
        ("40-60%", 0),
        ("60-80%", 0),
        ("80-100%", 0),
    ]
    .into();
    let mut convs: Vec<f64> = Vec::new();
    for e in &entries {
        let c = match e.get("convergence_at_pause").and_then(|x| x.as_f64()) {
            Some(c) => c,
            None => continue,
        };
        convs.push(c);
        let key = if c < 0.2 {
            "0-20%"
        } else if c < 0.4 {
            "20-40%"
        } else if c < 0.6 {
            "40-60%"
        } else if c < 0.8 {
            "60-80%"
        } else {
            "80-100%"
        };
        *buckets.get_mut(key).unwrap() += 1;
    }
    let avg_conv = if convs.is_empty() {
        0.0
    } else {
        convs.iter().sum::<f64>() / convs.len() as f64
    };

    // Top keywords — from session meta, weighted by intervention count
    let mut keywords: HashMap<String, u64> = HashMap::new();
    for e in &entries {
        let sid = e.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
        if let Some(m) = session_meta.get(sid)
            && let Some(kws) = m.get("keywords").and_then(|x| x.as_array())
        {
            for kw in kws {
                if let Some(s) = kw.as_str() {
                    *keywords.entry(s.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let mut keyword_vec: Vec<(String, u64)> = keywords.into_iter().collect();
    keyword_vec.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top_keywords: Vec<Value> = keyword_vec
        .into_iter()
        .take(15)
        .map(|(k, v)| json!([k, v]))
        .collect();

    // Sequences per session (sorted by round_num)
    let mut seq_by_session: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for e in &entries {
        let sid = e
            .get("session_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let action = e
            .get("action")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        let r = e.get("round_num").and_then(|x| x.as_i64()).unwrap_or(0);
        seq_by_session.entry(sid).or_default().push((r, action));
    }
    let session_count = seq_by_session.len();
    let sequences: Vec<Vec<String>> = seq_by_session
        .into_values()
        .filter(|s| s.len() > 1)
        .map(|mut s| {
            s.sort_by_key(|(r, _)| *r);
            s.into_iter().map(|(_, a)| a).collect()
        })
        .collect();
    let multi_intervention_sessions = sequences.len();

    // Recent: last 25 entries, reversed (newest first)
    let recent: Vec<Value> = entries.iter().rev().take(25).cloned().collect();

    json!({
        "total": entries.len(),
        "session_count": session_count,
        "actions": actions,
        "by_round": by_round,
        "by_cabinet": by_cabinet,
        "convergence_buckets": buckets,
        "avg_convergence_at_pause": (avg_conv * 1000.0).round() / 1000.0,
        "top_keywords": top_keywords,
        "sequences": sequences,
        "multi_intervention_sessions": multi_intervention_sessions,
        "recent": recent,
        "window_days": window_days,
    })
}
