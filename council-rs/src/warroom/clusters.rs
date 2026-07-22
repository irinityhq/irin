//! Topic clusters (N03) — hand-rolled k-means over the session embedding index.
//!
//! `GET /api/clusters` groups historical sessions by semantic similarity so the
//! War Room History dashboard can show a cluster-size tile and let the operator
//! filter the list. We reuse the existing `sessions/embeddings.jsonl` vectors
//! (MiniLM-L6-v2, L2-normalized) — no new model, no new heavy dep.
//!
//! Algorithm: k-means++ seeding with a **fixed deterministic RNG seed derived
//! from the index contents**, then Lloyd iterations. `k = clamp(sqrt(n/2), 2,
//! 20)`. Top terms per cluster come from a tf-idf-ish term frequency over the
//! member sessions' topics minus a small stopword list.
//!
//! Empty index → 200 with empty `clusters` (the caller returns the JSON
//! directly).

use std::collections::HashMap;

use chrono::Utc;
use serde_json::{Value, json};

use super::sessions_dir;

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "then", "else", "for", "of", "to", "in", "on",
    "at", "by", "with", "from", "as", "is", "are", "was", "were", "be", "been", "being", "this",
    "that", "these", "those", "it", "its", "we", "our", "you", "your", "i", "my", "should",
    "would", "could", "can", "will", "do", "does", "did", "how", "what", "why", "when", "where",
    "which", "who", "vs", "via", "about", "into", "out", "up", "down", "over", "under", "not",
    "no", "yes", "so", "than", "too", "very", "just", "more", "most", "some", "any", "all",
];

/// One session row pulled from both indices: id + topic + embedding vector.
struct SessionRow {
    id: String,
    topic: String,
    vec: Vec<f32>,
}

/// Build the cluster report. `k = clamp(sqrt(n/2), 2, 20)` over the session
/// embedding index. Returns the full `GET /api/clusters` JSON body.
pub fn build() -> Value {
    build_in(&sessions_dir())
}

/// Path-parametrized core of [`build`] — hermetically testable against a temp
/// sessions dir without mutating the process-global `COUNCIL_SESSIONS_DIR`.
pub fn build_in(dir: &std::path::Path) -> Value {
    let rows = load_rows(dir);
    let n = rows.len();
    if n == 0 {
        return json!({
            "clusters": [],
            "method": "kmeans",
            "k": 0,
            "n_sessions": 0,
            "generated_at": Utc::now().to_rfc3339(),
        });
    }

    let k = clamp_k(n);
    let dim = rows[0].vec.len();
    let points: Vec<&[f32]> = rows.iter().map(|r| r.vec.as_slice()).collect();
    let seed = deterministic_seed(&rows);
    let assignments = kmeans(&points, k, dim, seed);

    // Group member indices by cluster id.
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, &cid) in assignments.iter().enumerate() {
        members.entry(cid).or_default().push(idx);
    }

    let mut clusters: Vec<Value> = members
        .into_iter()
        .filter(|(_, idxs)| !idxs.is_empty())
        .map(|(cid, idxs)| {
            let topics: Vec<&str> = idxs.iter().map(|&i| rows[i].topic.as_str()).collect();
            let top_terms = top_terms(&topics, 6);
            // Cap member ids per cluster. The War Room History filter
            // (warroom/web/lib/clusters.ts `clusterSessionIds`) unions these
            // per selected cluster, so this cap bounds how many members a
            // single cluster can contribute to the filter. 100 covers virtually
            // all real clusters while keeping the response
            // payload bounded.
            const SESSION_IDS_CAP: usize = 100;
            let session_ids: Vec<String> = idxs
                .iter()
                .take(SESSION_IDS_CAP)
                .map(|&i| rows[i].id.clone())
                .collect();
            json!({
                "id": cid,
                "size": idxs.len(),
                "top_terms": top_terms,
                "session_ids": session_ids,
            })
        })
        .collect();

    // Largest clusters first; stable tiebreak by id for deterministic output.
    clusters.sort_by(|a, b| {
        let sb = b.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        let sa = a.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        sb.cmp(&sa).then_with(|| {
            a.get("id")
                .and_then(|x| x.as_u64())
                .cmp(&b.get("id").and_then(|x| x.as_u64()))
        })
    });

    json!({
        "clusters": clusters,
        "method": "kmeans",
        "k": k,
        "n_sessions": n,
        "generated_at": Utc::now().to_rfc3339(),
    })
}

/// `k = clamp(sqrt(n/2), 2, 20)`, never exceeding the sample count.
pub fn clamp_k(n: usize) -> usize {
    if n <= 2 {
        return n.max(1);
    }
    let raw = ((n as f64 / 2.0).sqrt()).round() as usize;
    raw.clamp(2, 20).min(n)
}

/// Load id + topic + embedding for every session that has a vector. Sessions
/// in the embedding index but missing from the session index (topic unknown)
/// are kept with an empty topic so cluster sizes still reflect reality.
fn load_rows(dir: &std::path::Path) -> Vec<SessionRow> {
    let vecs = load_vectors(dir);
    if vecs.is_empty() {
        return vec![];
    }
    let topics = load_topics(dir);
    vecs.into_iter()
        .map(|(id, vec)| {
            let topic = topics.get(&id).cloned().unwrap_or_default();
            SessionRow { id, topic, vec }
        })
        .collect()
}

fn load_vectors(dir: &std::path::Path) -> Vec<(String, Vec<f32>)> {
    let path = dir.join("embeddings.jsonl");
    if !path.exists() {
        return vec![];
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    use std::io::BufRead;
    let mut out = Vec::new();
    let mut dim: Option<usize> = None;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = match v.get("id").and_then(|x| x.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let vec: Vec<f32> = match v.get("vec").and_then(|x| x.as_array()) {
            Some(a) => a
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect(),
            None => continue,
        };
        if vec.is_empty() {
            continue;
        }
        // Skip ragged vectors (defensive — index is uniform 384-dim).
        match dim {
            Some(d) if vec.len() != d => continue,
            None => dim = Some(vec.len()),
            _ => {}
        }
        out.push((id, vec));
    }
    out
}

fn load_topics(dir: &std::path::Path) -> HashMap<String, String> {
    let path = dir.join("index.jsonl");
    if !path.exists() {
        return HashMap::new();
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    use std::io::BufRead;
    let mut out = HashMap::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            let id = v
                .get("session_id")
                .or_else(|| v.get("id"))
                .and_then(|x| x.as_str())
                .map(String::from);
            let topic = v
                .get("topic")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(id) = id {
                out.insert(id, topic);
            }
        }
    }
    out
}

/// Deterministic seed from the index contents — same sessions cluster the same
/// way across requests (FNV-1a over the concatenated ids).
fn deterministic_seed(rows: &[SessionRow]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for r in rows {
        for b in r.id.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash | 1 // never zero (xorshift needs a non-zero state)
}

/// Tiny deterministic xorshift64 RNG — no rand crate dep.
struct Xorshift(u64);
impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform f64 in [0, 1).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn sq_dist(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = *x as f64 - *y as f64;
            d * d
        })
        .sum()
}

/// Hand-rolled k-means with deterministic k-means++ seeding. Returns one
/// cluster id per input point.
pub fn kmeans(points: &[&[f32]], k: usize, dim: usize, seed: u64) -> Vec<usize> {
    let n = points.len();
    if n == 0 {
        return vec![];
    }
    let k = k.clamp(1, n);
    let mut rng = Xorshift(seed);

    // ── k-means++ seeding ──
    let mut centroids: Vec<Vec<f32>> = Vec::with_capacity(k);
    let first = (rng.next_u64() as usize) % n;
    centroids.push(points[first].to_vec());
    while centroids.len() < k {
        // Distance to nearest existing centroid for each point.
        let dists: Vec<f64> = points
            .iter()
            .map(|p| {
                centroids
                    .iter()
                    .map(|c| sq_dist(p, c))
                    .fold(f64::MAX, f64::min)
            })
            .collect();
        let total: f64 = dists.iter().sum();
        if total <= 0.0 {
            // All remaining points coincide with chosen centroids — pad with
            // arbitrary distinct points so we still return k centroids.
            centroids.push(points[centroids.len() % n].to_vec());
            continue;
        }
        let target = rng.next_f64() * total;
        let mut acc = 0.0;
        let mut chosen = n - 1;
        for (i, d) in dists.iter().enumerate() {
            acc += d;
            if acc >= target {
                chosen = i;
                break;
            }
        }
        centroids.push(points[chosen].to_vec());
    }

    // ── Lloyd iterations ──
    let mut assign = vec![0usize; n];
    for _ in 0..50 {
        let mut changed = false;
        for (i, p) in points.iter().enumerate() {
            let mut best = 0usize;
            let mut best_d = f64::MAX;
            for (ci, c) in centroids.iter().enumerate() {
                let d = sq_dist(p, c);
                if d < best_d {
                    best_d = d;
                    best = ci;
                }
            }
            if assign[i] != best {
                assign[i] = best;
                changed = true;
            }
        }
        // Recompute centroids as the mean of assigned points.
        let mut sums = vec![vec![0.0f64; dim]; k];
        let mut counts = vec![0usize; k];
        for (i, p) in points.iter().enumerate() {
            let cid = assign[i];
            counts[cid] += 1;
            for (s, &x) in sums[cid].iter_mut().zip(p.iter()) {
                *s += x as f64;
            }
        }
        for ci in 0..k {
            if counts[ci] == 0 {
                continue; // keep the old centroid for empty clusters
            }
            for (j, s) in sums[ci].iter().enumerate() {
                centroids[ci][j] = (*s / counts[ci] as f64) as f32;
            }
        }
        if !changed {
            break;
        }
    }
    assign
}

/// Term-frequency top terms across the member topics, minus stopwords and short
/// tokens. Deterministic ordering: count desc, then alphabetical.
fn top_terms(topics: &[&str], limit: usize) -> Vec<String> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for topic in topics {
        for raw in topic.split(|c: char| !c.is_alphanumeric()) {
            let term = raw.trim().to_lowercase();
            if term.len() < 3 || STOPWORDS.contains(&term.as_str()) {
                continue;
            }
            if term.chars().all(|c| c.is_numeric()) {
                continue;
            }
            *counts.entry(term).or_insert(0) += 1;
        }
    }
    let mut terms: Vec<(String, u64)> = counts.into_iter().collect();
    terms.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    terms.into_iter().take(limit).map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_k_follows_sqrt_rule() {
        assert_eq!(clamp_k(0), 1); // n.max(1) for the <=2 branch
        assert_eq!(clamp_k(1), 1);
        assert_eq!(clamp_k(2), 2);
        // sqrt(8/2)=2 -> clamped to 2
        assert_eq!(clamp_k(8), 2);
        // sqrt(50/2)=5
        assert_eq!(clamp_k(50), 5);
        // sqrt(1000/2)=22.36 -> clamped to 20
        assert_eq!(clamp_k(1000), 20);
        // sqrt(655/2)=18.1 -> 18
        assert_eq!(clamp_k(655), 18);
        // never exceeds n
        assert_eq!(clamp_k(3), 2);
    }

    #[test]
    fn kmeans_separates_two_obvious_clusters_deterministically() {
        // Two tight groups far apart in 2D.
        let a1 = [0.0f32, 0.0];
        let a2 = [0.1f32, 0.1];
        let a3 = [0.0f32, 0.2];
        let b1 = [10.0f32, 10.0];
        let b2 = [10.1f32, 9.9];
        let b3 = [9.8f32, 10.2];
        let points: Vec<&[f32]> = vec![&a1, &a2, &a3, &b1, &b2, &b3];

        let assign = kmeans(&points, 2, 2, 12345);
        // The three A points share a cluster; the three B points share another.
        assert_eq!(assign[0], assign[1]);
        assert_eq!(assign[1], assign[2]);
        assert_eq!(assign[3], assign[4]);
        assert_eq!(assign[4], assign[5]);
        assert_ne!(assign[0], assign[3], "A and B must be different clusters");

        // Determinism: same seed → identical assignment.
        let assign2 = kmeans(&points, 2, 2, 12345);
        assert_eq!(assign, assign2);
    }

    #[test]
    fn kmeans_handles_more_clusters_than_distinct_points() {
        let p1 = [1.0f32, 1.0];
        let p2 = [1.0f32, 1.0];
        let points: Vec<&[f32]> = vec![&p1, &p2];
        // k clamped to n; must not panic.
        let assign = kmeans(&points, 5, 2, 7);
        assert_eq!(assign.len(), 2);
    }

    #[test]
    fn top_terms_drops_stopwords_and_ranks_by_frequency() {
        let topics = vec![
            "Should we ship the auth refactor",
            "auth refactor risk analysis",
            "auth token rotation",
        ];
        let terms = top_terms(&topics, 5);
        // "auth" appears in all three → must rank first.
        assert_eq!(terms.first().map(String::as_str), Some("auth"));
        // Stopwords excluded.
        assert!(!terms.contains(&"the".to_string()));
        assert!(!terms.contains(&"we".to_string()));
        assert!(terms.contains(&"refactor".to_string()));
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "council_clusters_{tag}_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn build_empty_index_returns_empty_clusters() {
        // build_in against an empty temp dir — no env mutation, parallel-safe.
        let tmp = temp_dir("empty");
        let out = build_in(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(out["clusters"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(out["k"], 0);
        assert_eq!(out["n_sessions"], 0);
        assert_eq!(out["method"], "kmeans");
    }

    #[test]
    fn build_in_clusters_synthetic_vectors_and_surfaces_terms() {
        let tmp = temp_dir("synthetic");
        // Two semantic groups: 3 "auth" sessions near [1,0,0], 3 "pricing"
        // sessions near [0,1,0]. (Tiny 3-dim vectors stand in for embeddings.)
        let emb = [
            ("s1", [1.0, 0.0, 0.0]),
            ("s2", [0.9, 0.1, 0.0]),
            ("s3", [1.0, 0.05, 0.0]),
            ("s4", [0.0, 1.0, 0.0]),
            ("s5", [0.05, 0.95, 0.0]),
            ("s6", [0.0, 1.0, 0.1]),
        ];
        let emb_lines: String = emb
            .iter()
            .map(|(id, v)| json!({"id": id, "vec": v}).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.join("embeddings.jsonl"), emb_lines).unwrap();

        let idx = [
            ("s1", "auth refactor proposal"),
            ("s2", "auth token rotation"),
            ("s3", "auth login hardening"),
            ("s4", "pricing tier change"),
            ("s5", "pricing model review"),
            ("s6", "pricing experiment"),
        ];
        let idx_lines: String = idx
            .iter()
            .map(|(id, topic)| json!({"session_id": id, "topic": topic}).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.join("index.jsonl"), idx_lines).unwrap();

        let out = build_in(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(out["n_sessions"], 6);
        assert_eq!(out["method"], "kmeans");
        // k = clamp(sqrt(6/2),2,20) = clamp(1.73->2, 2, 20) = 2
        assert_eq!(out["k"], 2);
        let clusters = out["clusters"].as_array().unwrap();
        assert_eq!(clusters.len(), 2, "two well-separated groups");
        // Each cluster has size 3 and a coherent top term.
        for c in clusters {
            assert_eq!(c["size"], 3);
            let terms: Vec<&str> = c["top_terms"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t.as_str())
                .collect();
            assert!(
                terms.contains(&"auth") || terms.contains(&"pricing"),
                "cluster should surface its dominant term, got {terms:?}"
            );
            // session_ids capped at 50.
            assert!(c["session_ids"].as_array().unwrap().len() <= 50);
        }
    }
}
