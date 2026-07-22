//! Adapter for upstream librarian `/ask` responses.
//!
//! Buffered JSON responses require top-level keys (no extras allowed):
//!   {answer, sources, model, cabinet, latency_ms, chunks_used}
//! SSE responses use `data: {type:"meta", ...}` followed by OpenAI-style
//! chat-completion chunks and a final `data: [DONE]`.
//!
//! Each source requires {path, score}; optional {snippet, corpus, trust_tier}.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShapeError {
    #[error("expected JSON object, got {0}")]
    NotObject(String),
    #[error("missing required fields: {0:?}")]
    MissingTopLevel(Vec<String>),
    #[error("unknown fields: {0:?}")]
    UnknownTopLevel(Vec<String>),
    #[error("source[{0}] not a dict")]
    SourceNotObject(usize),
    #[error("source[{0}] missing fields: {1:?}")]
    SourceMissing(usize, Vec<String>),
    #[error("malformed SSE data: {0}")]
    MalformedSse(String),
    #[error("upstream SSE error: {0}")]
    UpstreamSse(String),
    #[error("SSE stream ended without answer text")]
    SseMissingAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibrarianSource {
    pub path: String,
    pub score: f64,
    #[serde(default)]
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_tier: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibrarianAnswer {
    pub answer: String,
    pub sources: Vec<LibrarianSource>,
    pub model: String,
    pub latency_ms: f64,
}

const REQUIRED: &[&str] = &[
    "answer",
    "sources",
    "model",
    "cabinet",
    "latency_ms",
    "chunks_used",
];
const SOURCE_REQUIRED: &[&str] = &["path", "score"];

pub fn parse(raw: &Value) -> Result<LibrarianAnswer, ShapeError> {
    let obj = match raw.as_object() {
        Some(o) => o,
        None => {
            return Err(ShapeError::NotObject(value_kind(raw).into()));
        }
    };
    let mut missing: Vec<String> = REQUIRED
        .iter()
        .filter(|k| !obj.contains_key(**k))
        .map(|s| (*s).to_string())
        .collect();
    missing.sort();
    if !missing.is_empty() {
        return Err(ShapeError::MissingTopLevel(missing));
    }
    let mut extra: Vec<String> = obj
        .keys()
        .filter(|k| !REQUIRED.contains(&k.as_str()))
        .cloned()
        .collect();
    extra.sort();
    if !extra.is_empty() {
        return Err(ShapeError::UnknownTopLevel(extra));
    }

    let sources = parse_sources(
        obj.get("sources")
            .ok_or_else(|| ShapeError::MissingTopLevel(vec!["sources".into()]))?,
    )?;
    Ok(LibrarianAnswer {
        answer: obj.get("answer").map(value_to_string).unwrap_or_default(),
        sources,
        model: obj.get("model").map(value_to_string).unwrap_or_default(),
        latency_ms: obj
            .get("latency_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
    })
}

pub fn parse_sse(raw: &str) -> Result<LibrarianAnswer, ShapeError> {
    let mut answer = String::new();
    let mut sources = Vec::new();
    let mut model = String::new();

    for line in raw.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }

        let event: Value =
            serde_json::from_str(data).map_err(|e| ShapeError::MalformedSse(e.to_string()))?;
        if let Some(error) = event.get("error") {
            return Err(ShapeError::UpstreamSse(error_message(error)));
        }

        if event.get("type").and_then(|v| v.as_str()) == Some("meta") {
            if let Some(m) = event.get("model") {
                model = value_to_string(m);
            }
            if let Some(s) = event.get("sources") {
                sources = parse_sources(s)?;
            }
            continue;
        }

        if model.is_empty()
            && let Some(m) = event.get("model")
        {
            model = value_to_string(m);
        }
        if let Some(choices) = event.get("choices").and_then(|v| v.as_array()) {
            for choice in choices {
                if let Some(delta) = choice.get("delta").and_then(|v| v.as_object()) {
                    if let Some(content) = delta.get("content") {
                        answer.push_str(&value_to_string(content));
                    } else if let Some(reasoning) = delta.get("reasoning_content") {
                        answer.push_str(&value_to_string(reasoning));
                    }
                }
            }
        }
    }

    if answer.is_empty() {
        return Err(ShapeError::SseMissingAnswer);
    }

    Ok(LibrarianAnswer {
        answer,
        sources,
        model,
        latency_ms: 0.0,
    })
}

fn parse_sources(raw: &Value) -> Result<Vec<LibrarianSource>, ShapeError> {
    let sources_raw = raw
        .get("sources")
        .and_then(|v| v.as_array())
        .or_else(|| raw.as_array())
        .ok_or(ShapeError::SourceNotObject(0))?;
    let mut sources = Vec::with_capacity(sources_raw.len());
    for (i, s) in sources_raw.iter().enumerate() {
        let so = s.as_object().ok_or(ShapeError::SourceNotObject(i))?;
        let mut s_missing: Vec<String> = SOURCE_REQUIRED
            .iter()
            .filter(|k| !so.contains_key(**k))
            .map(|s| (*s).to_string())
            .collect();
        s_missing.sort();
        if !s_missing.is_empty() {
            return Err(ShapeError::SourceMissing(i, s_missing));
        }
        sources.push(LibrarianSource {
            path: so.get("path").map(value_to_string).unwrap_or_default(),
            score: so.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
            snippet: so.get("snippet").map(value_to_string).unwrap_or_default(),
            corpus: so.get("corpus").map(value_to_string),
            trust_tier: so
                .get("trust_tier")
                .and_then(|v| v.as_u64())
                .map(|n| n.min(u8::MAX as u64) as u8),
        });
    }
    Ok(sources)
}

fn error_message(v: &Value) -> String {
    v.get("message")
        .map(value_to_string)
        .unwrap_or_else(|| value_to_string(v))
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn happy_path() {
        let raw = json!({
            "answer": "hi",
            "sources": [{
                "path":"a.md","score":0.9,"snippet":"s",
                "corpus":"knowledge","trust_tier":2
            }],
            "model": "gemma-2",
            "cabinet": "research-default",
            "latency_ms": 12.0,
            "chunks_used": 1,
        });
        let a = parse(&raw).unwrap();
        assert_eq!(a.answer, "hi");
        assert_eq!(a.sources.len(), 1);
        assert_eq!(a.sources[0].path, "a.md");
        assert_eq!(a.sources[0].corpus.as_deref(), Some("knowledge"));
        assert_eq!(a.sources[0].trust_tier, Some(2));
    }

    #[test]
    fn source_metadata_optional() {
        let raw = json!({
            "answer": "hi",
            "sources": [{"path":"a.md","score":0.9}],
            "model": "gemma-2",
            "cabinet": "research-default",
            "latency_ms": 12.0,
            "chunks_used": 1,
        });
        let a = parse(&raw).unwrap();
        assert!(a.sources[0].corpus.is_none());
        assert!(a.sources[0].trust_tier.is_none());
    }

    #[test]
    fn rejects_legacy_file_field() {
        let raw = json!({
            "answer":"x",
            "sources":[{"file":"a.md","score":0.9}],
            "model":"m","cabinet":"c","latency_ms":0.0,"chunks_used":0,
        });
        match parse(&raw) {
            Err(ShapeError::SourceMissing(0, missing)) => {
                assert!(missing.iter().any(|m| m == "path"));
            }
            other => panic!("expected SourceMissing for legacy file field, got {other:?}"),
        }
    }

    #[test]
    fn missing_top_level() {
        let raw = json!({"answer":"hi"});
        assert!(matches!(parse(&raw), Err(ShapeError::MissingTopLevel(_))));
    }

    #[test]
    fn unknown_top_level() {
        let raw = json!({
            "answer":"x","sources":[],"model":"m","cabinet":"c",
            "latency_ms":0.0,"chunks_used":0,"extra":"nope"
        });
        assert!(matches!(parse(&raw), Err(ShapeError::UnknownTopLevel(_))));
    }

    #[test]
    fn source_missing_field() {
        let raw = json!({
            "answer":"x","sources":[{"path":"a"}],"model":"m","cabinet":"c",
            "latency_ms":0.0,"chunks_used":0
        });
        assert!(matches!(parse(&raw), Err(ShapeError::SourceMissing(0, _))));
    }

    #[test]
    fn parses_sse_chunks() {
        let raw = r#"data: {"type":"meta","cabinet":"research-fast","model":"gemma","sources":[{"path":"a.md","score":0.8,"corpus":"knowledge","trust_tier":2}]}

data: {"model":"gemma","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}

data: [DONE]
"#;
        let a = parse_sse(raw).unwrap();
        assert_eq!(a.answer, "Hello");
        assert_eq!(a.model, "gemma");
        assert_eq!(a.sources.len(), 1);
        assert_eq!(a.sources[0].path, "a.md");
        assert_eq!(a.sources[0].trust_tier, Some(2));
    }

    #[test]
    fn sse_error_is_failure() {
        let raw = r#"data: {"type":"meta","model":"gemma","sources":[]}

data: {"error":{"message":"boom","type":"soul_gate_error"}}

data: [DONE]
"#;
        assert!(matches!(
            parse_sse(raw),
            Err(ShapeError::UpstreamSse(msg)) if msg == "boom"
        ));
    }
}

// ---------------------------------------------------------------------------
// v0.3 Open Surface: Librarian Identity / Memory Context & Commit Proposals
// ---------------------------------------------------------------------------

/// Context supplied to deliberations (Identity + Memory) per tenant.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibrarianContext {
    #[serde(default)]
    pub identity: IdentityContext,
    #[serde(default)]
    pub memory: MemoryContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IdentityContext {
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub facts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryContext {
    #[serde(default)]
    pub recent_summaries: Vec<String>,
    #[serde(default)]
    pub active_commit: Option<String>,
}

/// A proposal sent to Librarian post-Worker (Phase 5/v0.3 hook).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitProposal {
    pub tenant_id: String,
    pub causal_fire_id: String,
    pub content: String,
    #[serde(default)]
    pub weight: f64,
}
