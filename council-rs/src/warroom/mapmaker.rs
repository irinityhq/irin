//! Mapmaker — pre-flight execution brief.
//!
//! Bundles a directory via safe_map, sends to Grok or Gemini, writes the
//! resulting Execution Map to runs/maps/MAPMAKER_<ts>_<slug>.md.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{runs_dir, safe_map};
use crate::config::Config;
use crate::provider;

fn maps_dir() -> PathBuf {
    runs_dir().join("maps")
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

pub fn list_briefs(limit: usize) -> Vec<Value> {
    let dir = maps_dir();
    if !dir.exists() {
        return vec![];
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut briefs: Vec<(PathBuf, String, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?.to_string();
            if !name.starts_with("MAPMAKER_") || !name.ends_with(".md") {
                return None;
            }
            let mtime = p.metadata().ok()?.modified().ok()?;
            Some((p, name, mtime))
        })
        .collect();
    briefs.sort_by_key(|x| std::cmp::Reverse(x.2));
    briefs.truncate(limit);
    briefs
        .into_iter()
        .map(|(p, name, _)| {
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            json!({
                "name": name,
                "size": size,
                "mtime": iso_mtime(&p),
            })
        })
        .collect()
}

pub fn get_brief(name: &str) -> Option<Value> {
    if !name.starts_with("MAPMAKER_") || !name.ends_with(".md") {
        return None;
    }
    if name.contains('/') || name.contains("..") {
        return None;
    }
    let path = maps_dir().join(name);
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

const MAPMAKER_SYSTEM: &str = "You are the Mapmaker for the Council deliberation \
    system. You produce precise, compressed Execution Maps. You never write code \
    — you tell the executor exactly which files to change, in what order, and how \
    to verify.";

const MAPMAKER_PROMPT: &str = "TASK: {task}\n\n\
    CODEBASE CONTEXT ({file_count} files, {bundle_bytes} bytes):\n{bundle}\n\n\
    PRODUCE THE FOLLOWING MAP. Be precise, not comprehensive — under 2000 tokens.\n\n\
    ## 1. Affected Files\n\
    List EVERY file that must change. For each: path · why (one sentence) · est. lines.\n\n\
    ## 2. Files NOT to Touch\n\
    Files related but should NOT change. Explain why (shared interface, callers, etc.).\n\n\
    ## 3. Dependency Context\n\
    For each affected file: imports it pulls in scope · callers that import IT · shared \
    state or side effects to be aware of.\n\n\
    ## 4. Execution Plan\n\
    Numbered steps. Each step: exact file · exact change (function/region/operation) · \
    ordering dependency if any.\n\n\
    ## 5. Validation Steps\n\
    Exact commands to verify: import check · test command · any manual check.\n\n\
    ## 6. Blast Radius\n\
    What breaks if this goes wrong? Rollback path?\n\n\
    Begin the map now.";

#[derive(Debug, Clone, Copy)]
pub enum MapmakerModel {
    Auto,
    Grok,
    Gemini,
}

impl MapmakerModel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "grok" => Some(Self::Grok),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }
}

fn select_model(task: &str, file_count: usize) -> MapmakerModel {
    let t = task.to_lowercase();
    const BREADTH: &[&str] = &[
        "full codebase",
        "all of",
        "everything",
        "every file",
        "across the",
        "monorepo",
        "whole repo",
    ];
    const FOCUS: &[&str] = &[
        "integration",
        "cross-bu",
        "cross bu",
        "blueprint",
        "synthesis",
        "external tool",
    ];
    if BREADTH.iter().any(|s| t.contains(s)) {
        return MapmakerModel::Grok;
    }
    if FOCUS.iter().any(|s| t.contains(s)) {
        return MapmakerModel::Gemini;
    }
    if file_count > 15 {
        MapmakerModel::Grok
    } else {
        MapmakerModel::Gemini
    }
}

fn slug(text: &str, max_len: usize) -> String {
    let mut s = String::with_capacity(text.len());
    let mut last_was_underscore = true;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            s.push('_');
            last_was_underscore = true;
        }
    }
    let trimmed = s.trim_matches('_');
    let cut = trimmed.len().min(max_len);
    let out = trimmed[..cut].trim_matches('_').to_string();
    if out.is_empty() { "task".into() } else { out }
}

fn count_files_in_bundle(bundle: &str) -> usize {
    let mut n = bundle.matches("\n--- ").count();
    if bundle.starts_with("--- ") {
        n += 1;
    }
    n
}

/// Run the mapmaker pipeline. Returns a JSON Value matching the Python contract.
pub async fn run_mapmaker(
    config: &Arc<Config>,
    dir_path: &str,
    task: &str,
    model: MapmakerModel,
) -> Value {
    // Validate inputs — enforce the workspace allowlist before any provider upload
    let target = match safe_map::resolve_map_target(dir_path) {
        Ok(t) => t,
        Err(e) => return json!({"error": e}),
    };
    let task = task.trim();
    if task.is_empty() {
        return json!({"error": "task description is required"});
    }

    // Build code bundle (workspace-aware safe scan)
    let preview = safe_map::gather_map_preview(dir_path);
    if preview.get("error").is_some() {
        return preview;
    }
    let bundle = preview
        .get("preview")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if bundle.is_empty() {
        return json!({"error": "no code files found in directory"});
    }
    let bundle_bytes = preview
        .get("total_bytes")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let file_count = preview
        .get("file_count")
        .and_then(|x| x.as_u64())
        .unwrap_or_else(|| count_files_in_bundle(bundle) as u64);

    // Resolve model
    let selected = match model {
        MapmakerModel::Auto => select_model(task, file_count as usize),
        m => m,
    };
    let (provider_name, model_key) = match selected {
        MapmakerModel::Grok => ("grok_hermes", "grok_reasoning"),
        MapmakerModel::Gemini => ("gemini_agy", "gemini_flagship"),
        MapmakerModel::Auto => unreachable!(),
    };
    let model_id = config
        .models
        .models
        .get(model_key)
        .map(|m| m.id.clone())
        .unwrap_or_default();

    let prompt = MAPMAKER_PROMPT
        .replace("{task}", task)
        .replace("{bundle}", bundle)
        .replace("{file_count}", &file_count.to_string())
        .replace("{bundle_bytes}", &bundle_bytes.to_string());

    let started = std::time::Instant::now();
    let resp = provider::ask(provider_name, &prompt, MAPMAKER_SYSTEM, &model_id).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if let Some(err) = resp.error.as_ref() {
        return json!({
            "error": format!("{} call failed: {}", provider_name, err),
            "model": provider_name,
            "model_id": model_id,
            "file_count": file_count,
            "bundle_bytes": bundle_bytes,
        });
    }

    // T24: mapmaker output is raw provider text — scrub secret shapes before it
    // is written to the brief on disk (:brief_md) and returned in the "map" field.
    let map_text = crate::scrub::redact(resp.text.trim());
    if map_text.is_empty() {
        return json!({"error": "mapmaker returned empty response", "model": provider_name});
    }

    let cost_usd =
        config
            .models
            .estimate_cost(&resp.model, resp.tokens_in, resp.tokens_out, resp.cached_in);

    // Save brief
    let dir = maps_dir();
    let _ = std::fs::create_dir_all(&dir);
    let ts = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let task_slug = slug(task, 40);
    let brief_filename = format!("MAPMAKER_{}_{}.md", ts, task_slug);
    let brief_path = dir.join(&brief_filename);

    let header = format!(
        "# Execution Brief — {task}\n\n\
         > **Mapmaker:** {provider} ({model_id})  \n\
         > **Generated:** {ts} UTC  \n\
         > **Codebase:** `{dir}`  \n\
         > **Bundle:** {fc} files · {bb} bytes  \n\
         > **Tokens:** {ti} in · {to} out  \n\
         > **Cost:** ${cost:.4}  \n\
         > **Latency:** {lat:.1}s  \n\n\
         ---\n\n",
        task = task,
        provider = provider_name,
        model_id = resp.model,
        ts = ts,
        dir = target.display(),
        fc = file_count,
        bb = bundle_bytes,
        ti = resp.tokens_in,
        to = resp.tokens_out,
        cost = cost_usd,
        lat = elapsed_ms as f64 / 1000.0,
    );
    let brief_md = format!("{}{}\n", header, map_text);
    let brief_path_saved = std::fs::write(&brief_path, &brief_md)
        .ok()
        .map(|_| brief_path.to_string_lossy().to_string());

    json!({
        "model": provider_name,
        "model_id": resp.model,
        "map": map_text,
        "task": task,
        "directory": target.to_string_lossy(),
        "file_count": file_count,
        "bundle_bytes": bundle_bytes,
        "tokens_in": resp.tokens_in,
        "tokens_out": resp.tokens_out,
        "cost_usd": cost_usd,
        "latency_ms": elapsed_ms,
        "brief_filename": brief_filename,
        "brief_path": brief_path_saved,
    })
}
