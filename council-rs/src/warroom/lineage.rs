//! Mirror of warroom/backend/lineage.py — append-only fork records + diff.
//!
//! Records: sessions/lineage.jsonl
//!   {child_id, parent_id, swaps, cabinet_label, ts}

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::sessions_dir;

fn lineage_path() -> PathBuf {
    sessions_dir().join("lineage.jsonl")
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "to", "of", "and", "or", "but", "in", "on", "for", "with", "as",
    "by", "at", "this", "that", "be", "it", "we", "you", "they", "i", "have", "has", "had", "was",
    "were", "will", "would", "should", "could", "do", "does", "did", "not", "no", "from", "into",
    "than", "more", "less", "may", "might",
];

fn load_all() -> Vec<Value> {
    let path = lineage_path();
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
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .collect()
}

pub fn parent_of(child_id: &str) -> Option<Value> {
    load_all()
        .into_iter()
        .find(|r| r.get("child_id").and_then(|x| x.as_str()) == Some(child_id))
}

pub fn children_of(parent_id: &str) -> Vec<Value> {
    load_all()
        .into_iter()
        .filter(|r| r.get("parent_id").and_then(|x| x.as_str()) == Some(parent_id))
        .collect()
}

pub fn record_fork(
    child: &str,
    parent: &str,
    swaps: &[Value],
    cabinet_label: &str,
) -> std::io::Result<()> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;
    let rec = json!({
        "child_id": child,
        "parent_id": parent,
        "swaps": swaps,
        "cabinet_label": cabinet_label,
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(lineage_path())?;
    writeln!(f, "{}", serde_json::to_string(&rec).unwrap())?;
    Ok(())
}

fn words(text: &str) -> HashMap<String, u32> {
    let mut m: HashMap<String, u32> = HashMap::new();
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    for w in text.to_lowercase().split(|c: char| !c.is_alphabetic()) {
        if w.len() < 3 {
            continue;
        }
        if stop.contains(w) {
            continue;
        }
        *m.entry(w.to_string()).or_insert(0) += 1;
    }
    m
}

fn confidence(text: &str) -> Option<String> {
    for token in ["HIGH", "MEDIUM", "LOW"] {
        if text.contains(token) {
            return Some(token.to_string());
        }
    }
    None
}

fn unified_diff(a: &[String], b: &[String], from: &str, to: &str, max: usize) -> Vec<String> {
    // Minimal LCS-based unified diff. Good enough for synthesis text.
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(max);
    out.push(format!("--- {}", from));
    out.push(format!("+++ {}", to));
    let (mut i, mut j) = (0, 0);
    while i < n && j < m && out.len() < max {
        if a[i] == b[j] {
            out.push(format!(" {}", a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(format!("-{}", a[i]));
            i += 1;
        } else {
            out.push(format!("+{}", b[j]));
            j += 1;
        }
    }
    while i < n && out.len() < max {
        out.push(format!("-{}", a[i]));
        i += 1;
    }
    while j < m && out.len() < max {
        out.push(format!("+{}", b[j]));
        j += 1;
    }
    out
}

fn jaccard(a: &HashMap<String, u32>, b: &HashMap<String, u32>) -> f64 {
    let aset: HashSet<&String> = a.keys().collect();
    let bset: HashSet<&String> = b.keys().collect();
    let inter: usize = aset.intersection(&bset).count();
    let uni: usize = aset.union(&bset).count();
    if uni == 0 {
        0.0
    } else {
        inter as f64 / uni as f64
    }
}

pub fn diff_synthesis(parent: &Value, child: &Value) -> Value {
    let p_text = parent
        .get("synthesis")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let c_text = child
        .get("synthesis")
        .and_then(|x| x.as_str())
        .unwrap_or("");

    let pw = words(p_text);
    let cw = words(c_text);
    let p_total: u32 = pw.values().sum();
    let c_total: u32 = cw.values().sum();
    let jac = jaccard(&pw, &cw);

    // Semantic similarity via fastembed when available; falls back to None
    // (frontend then displays "—" and uses jaccard for drift).
    let p_clip: String = p_text.chars().take(2000).collect();
    let c_clip: String = c_text.chars().take(2000).collect();
    let semantic_sim = super::embeddings::cosine_similarity(&p_clip, &c_clip);

    let p_conf = confidence(p_text);
    let c_conf = confidence(c_text);

    let p_lines: Vec<String> = p_text.lines().take(300).map(String::from).collect();
    let c_lines: Vec<String> = c_text.lines().take(300).map(String::from).collect();
    let p_id = parent
        .get("session_id")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let c_id = child
        .get("session_id")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let diff_lines = unified_diff(
        &p_lines,
        &c_lines,
        &format!("parent ({})", p_id),
        &format!("child ({})", c_id),
        400,
    );

    let p_only: HashSet<&String> = pw.keys().filter(|k| !cw.contains_key(*k)).collect();
    let c_only: HashSet<&String> = cw.keys().filter(|k| !pw.contains_key(*k)).collect();
    let unique_to_parent: Vec<String> = {
        let mut v: Vec<&String> = p_only.into_iter().collect();
        v.sort_by(|a, b| pw[*b].cmp(&pw[*a]).then(a.cmp(b)));
        v.into_iter().take(20).cloned().collect()
    };
    let unique_to_child: Vec<String> = {
        let mut v: Vec<&String> = c_only.into_iter().collect();
        v.sort_by(|a, b| cw[*b].cmp(&cw[*a]).then(a.cmp(b)));
        v.into_iter().take(20).cloned().collect()
    };

    let confidence_changed = p_conf != c_conf;
    let drift = match semantic_sim {
        Some(s) => ((1.0 - s) * 10000.0).round() / 10000.0,
        None => ((1.0 - jac) * 10000.0).round() / 10000.0,
    };
    json!({
        "similarity": semantic_sim.map(|s| (s * 10000.0).round() / 10000.0),
        "jaccard": (jac * 10000.0).round() / 10000.0,
        "drift": drift,
        "parent_confidence": p_conf,
        "child_confidence": c_conf,
        "confidence_changed": confidence_changed,
        "parent_word_count": p_total,
        "child_word_count": c_total,
        "diff_lines": diff_lines,
        "unique_to_parent": unique_to_parent,
        "unique_to_child": unique_to_child,
        "parent_synthesis": p_text,
        "child_synthesis": c_text,
        "parent_id": parent.get("session_id"),
        "child_id": child.get("session_id"),
    })
}
