//! Local sentence-embedding index — pure Rust via fastembed-rs.
//!
//! Storage: `sessions/embeddings.jsonl` — one record per session:
//!   {"id": "abc123", "vec": [0.123, ...]}
//!
//! Vectors are L2-normalized 384-dim from MiniLM-L6-v2 (default) so cosine
//! similarity == dot product. The model file (~80MB ONNX) downloads on first
//! use to ~/.cache/fastembed/ and is cached after.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::sessions_dir;

fn jsonl_path() -> PathBuf {
    sessions_dir().join("embeddings.jsonl")
}

fn index_path() -> PathBuf {
    sessions_dir().join("index.jsonl")
}

const EMBED_DIM: usize = 384;

/// Lazy-initialized fastembed model. First .lock() may block on a model download.
static MODEL: OnceLock<Mutex<fastembed::TextEmbedding>> = OnceLock::new();

fn model() -> Result<&'static Mutex<fastembed::TextEmbedding>> {
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let m = fastembed::TextEmbedding::try_new(
        fastembed::InitOptions::new(fastembed::EmbeddingModel::AllMiniLML6V2)
            .with_show_download_progress(false),
    )
    .context("fastembed model init")?;
    let _ = MODEL.set(Mutex::new(m));
    MODEL.get().context("model not initialized after set")
}

fn embed_texts(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    let m = model()?;
    let mut guard = m
        .lock()
        .map_err(|e| anyhow::anyhow!("model lock poisoned: {}", e))?;
    let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let embeddings = guard.embed(refs, None).context("fastembed encode")?;
    Ok(embeddings)
}

/// Public: encode a batch of texts to their raw 384-dim L2-normalized vectors.
/// Returns `Err` when the model can't init/encode (offline first run, etc.).
/// Used by the N02 per-round divergence projection (`warroom::divergence`),
/// which must run inside `spawn_blocking` since this can block on a model
/// download and on the model `Mutex`.
pub fn embed_texts_public(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    embed_texts(texts)
}

/// Public: encode two texts and return cosine similarity.
/// Used by lineage::diff_synthesis once embeddings are available.
pub fn cosine_similarity(a: &str, b: &str) -> Option<f64> {
    let vecs = embed_texts(&[a.to_string(), b.to_string()]).ok()?;
    if vecs.len() != 2 {
        return None;
    }
    let dot: f32 = vecs[0].iter().zip(vecs[1].iter()).map(|(x, y)| x * y).sum();
    Some(dot as f64)
}

// ─── Index storage ────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct Record {
    id: String,
    vec: Vec<f32>,
}

fn load_records() -> Vec<Record> {
    let path = jsonl_path();
    if !path.exists() {
        return vec![];
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(std::io::Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn write_records(records: &[Record]) -> Result<()> {
    let path = jsonl_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(&path)?;
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(())
}

fn append_record(record: &Record) -> Result<()> {
    let path = jsonl_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

// ─── Session-index reading ────────────────────────────────────────

fn load_session_index() -> Vec<Value> {
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
        .map_while(std::io::Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn entry_id(e: &Value) -> Option<String> {
    e.get("id")
        .or_else(|| e.get("session_id"))
        .and_then(|x| x.as_str())
        .map(String::from)
}

fn index_text(e: &Value) -> String {
    let topic = e.get("topic").and_then(|x| x.as_str()).unwrap_or("");
    let digest = e
        .get("ruling_digest")
        .or_else(|| e.get("digest"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let keywords: Vec<&str> = e
        .get("keywords")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    format!(
        "{}\n\nKEYWORDS: {}\n\nRULING: {}",
        topic,
        keywords.join(" "),
        digest
    )
}

// ─── Public API ───────────────────────────────────────────────────

/// Build / extend the embeddings index.
/// Incremental unless `force=true`. Returns a JSON summary.
pub fn build_index(force: bool) -> Value {
    let entries = load_session_index();
    if entries.is_empty() {
        return json!({"built": false, "reason": "no session index", "total": 0});
    }

    let existing: Vec<Record> = if force { vec![] } else { load_records() };
    let existing_ids: HashSet<String> = existing.iter().map(|r| r.id.clone()).collect();

    let new_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| match entry_id(e) {
            Some(id) => !existing_ids.contains(&id),
            None => false,
        })
        .collect();

    if new_entries.is_empty() && !force {
        return json!({"built": false, "reason": "up to date", "total": existing.len()});
    }

    let texts: Vec<String> = new_entries.iter().map(|e| index_text(e)).collect();
    let vecs = match embed_texts(&texts) {
        Ok(v) => v,
        Err(e) => {
            return json!({
                "built": false,
                "error": format!("embed failed: {}", e),
                "total": existing.len(),
            });
        }
    };

    let mut new_records: Vec<Record> = new_entries
        .iter()
        .zip(vecs)
        .filter_map(|(e, v)| entry_id(e).map(|id| Record { id, vec: v }))
        .collect();

    let total = if force {
        // Replace file with just the new vectors
        if let Err(e) = write_records(&new_records) {
            return json!({"built": false, "error": format!("write failed: {}", e)});
        }
        new_records.len()
    } else {
        // Append new vectors to existing
        for r in &new_records {
            if let Err(e) = append_record(r) {
                return json!({"built": false, "error": format!("append failed: {}", e)});
            }
        }
        existing.len() + new_records.len()
    };

    let added = new_records.len();
    new_records.clear();
    json!({
        "built": true,
        "added": added,
        "total": total,
        "path": jsonl_path().to_string_lossy(),
    })
}

/// Append exactly one session's vector to the index. Cheap; called after a
/// new deliberation completes.
pub fn append_session(session_id: &str) -> bool {
    let entries = load_session_index();
    let target = match entries
        .iter()
        .find(|e| entry_id(e).as_deref() == Some(session_id))
    {
        Some(e) => e,
        None => return false,
    };
    let existing: HashSet<String> = load_records().iter().map(|r| r.id.clone()).collect();
    if existing.contains(session_id) {
        return false;
    }

    let texts = vec![index_text(target)];
    let vecs = match embed_texts(&texts) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if vecs.len() != 1 {
        return false;
    }
    append_record(&Record {
        id: session_id.to_string(),
        vec: vecs.into_iter().next().unwrap(),
    })
    .is_ok()
}

/// Semantic search — top-k matches above the threshold (cosine == dot product
/// since vectors are normalized).
pub fn semantic_search(query: &str, k: usize, threshold: f64) -> Vec<Value> {
    let records = load_records();
    if records.is_empty() {
        return vec![];
    }
    let qv = match embed_texts(&[query.to_string()]) {
        Ok(v) if v.len() == 1 => v.into_iter().next().unwrap(),
        _ => return vec![],
    };

    let entries = load_session_index();
    let by_id: std::collections::HashMap<String, &Value> = entries
        .iter()
        .filter_map(|e| entry_id(e).map(|id| (id, e)))
        .collect();

    let mut scored: Vec<(f64, &Record)> = records
        .iter()
        .map(|r| {
            let dot: f32 = r.vec.iter().zip(qv.iter()).map(|(a, b)| a * b).sum();
            (dot as f64, r)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut out: Vec<Value> = Vec::with_capacity(k);
    for (sim, r) in scored {
        if sim < threshold {
            break;
        }
        if let Some(meta) = by_id.get(&r.id) {
            let mut v = (*meta).clone();
            if let Some(o) = v.as_object_mut() {
                o.insert(
                    "similarity".into(),
                    json!((sim * 10000.0).round() / 10000.0),
                );
            }
            out.push(v);
            if out.len() >= k {
                break;
            }
        }
    }
    out
}

/// Cosine similarity of `query` against every indexed session vector.
///
/// Returns `None` when there are no vectors on disk or the query can't be
/// embedded (offline first run, model init failure) — callers fall back to
/// keyword-only ranking. Sessions indexed in `index.jsonl` but missing from
/// `embeddings.jsonl` (stale vector index) are simply absent from the map.
pub fn similarity_scores(query: &str) -> Option<std::collections::HashMap<String, f64>> {
    let records = load_records();
    if records.is_empty() {
        return None;
    }
    let qv = match embed_texts(&[query.to_string()]) {
        Ok(v) if v.len() == 1 => v.into_iter().next().unwrap(),
        _ => return None,
    };
    Some(
        records
            .iter()
            .map(|r| {
                let dot: f32 = r.vec.iter().zip(qv.iter()).map(|(a, b)| a * b).sum();
                (r.id.clone(), dot as f64)
            })
            .collect(),
    )
}

/// Cheap check for auto-mode dispatch — avoids full index parse on every keystroke.
pub fn is_present() -> bool {
    jsonl_path().exists()
}

/// Stats endpoint payload.
pub fn stats() -> Value {
    let model_name = "all-MiniLM-L6-v2";
    let device = "cpu (ONNX Runtime)";

    let path = jsonl_path();
    let session_idx_count = load_session_index().len();

    if !path.exists() {
        return json!({
            "available": true,
            "engine_mode": "semantic",
            "present": false,
            "model": model_name,
            "device": device,
            "session_index_count": session_idx_count,
            "session_count": 0,
            "vector_dim": EMBED_DIM,
            "size_bytes": 0,
            "stale": session_idx_count > 0,
            "path": path.to_string_lossy(),
        });
    }

    let records = load_records();
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    json!({
        "available": true,
        "engine_mode": "semantic",
        "present": true,
        "session_count": records.len(),
        "vector_dim": records.first().map(|r| r.vec.len()).unwrap_or(EMBED_DIM),
        "size_bytes": size,
        "session_index_count": session_idx_count,
        "stale": session_idx_count > records.len(),
        "model": model_name,
        "device": device,
        "path": path.to_string_lossy(),
    })
}
