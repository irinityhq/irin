//! xmcp MCP client — Streamable HTTP transport for intel + X API tools.
//!
//! 3-step handshake: initialize → notifications/initialized → tools/call.
//! Used by Sheldon validator to gather evidence for claim grounding.

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Cap for local MCP /mcp responses (design xmcp regression gate: large/slow
/// xmcp must be capped/timed without unbounded body read). Matches evidence
/// pattern + RTK "provider-local ... must degrade".
const XMCP_MAX_BODY_BYTES: usize = 256 * 1024;

fn base_url() -> String {
    std::env::var("XMCP_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".into())
}

pub async fn is_available() -> bool {
    let url = format!("{}/health", base_url());

    Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

pub async fn call_tool(tool_name: &str, arguments: Value) -> Vec<Value> {
    call_tool_with_timeout(tool_name, arguments, 15).await
}

async fn call_tool_with_timeout(
    tool_name: &str,
    arguments: Value,
    timeout_secs: u64,
) -> Vec<Value> {
    let mcp_url = format!("{}/mcp", base_url());
    let client = Client::new();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    // Step 1: Initialize MCP session
    let init_payload = json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "council-validator", "version": VERSION}
        }
    });

    let init_resp = match client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .timeout(std::time::Duration::from_secs(10))
        .json(&init_payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let _ = init_resp.text().await;

    // Step 2: notifications/initialized
    let notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let mut notif_req = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .json(&notif);
    if !session_id.is_empty() {
        notif_req = notif_req.header("Mcp-Session-Id", &session_id);
    }
    if let Ok(r) = notif_req
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        let _ = r.text().await;
    }

    // Step 3: tools/call
    let call_payload = json!({
        "jsonrpc": "2.0", "id": 2,
        "method": "tools/call",
        "params": {"name": tool_name, "arguments": arguments}
    });
    let mut call_req = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&call_payload);
    if !session_id.is_empty() {
        call_req = call_req.header("Mcp-Session-Id", &session_id);
    }

    let body = match call_req.timeout(timeout).send().await {
        Ok(r) => {
            // Streaming cap (bytes_stream + early abort) *before* full materialization.
            // Fixes review Issue 6 (DoS on CRITICAL call_tool path for gate 3 / validate).
            // Mirrors evidence read_limited_body pattern. Init/notif left minimal (control msgs).
            // "without unbounded body read" now satisfied for evidence-relevant path.
            let mut buf = Vec::new();
            let mut stream = r.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(_) => return vec![],
                };
                if buf.len() + chunk.len() > XMCP_MAX_BODY_BYTES {
                    return vec![];
                }
                buf.extend_from_slice(&chunk);
            }
            String::from_utf8_lossy(&buf).to_string()
        }
        Err(_) => return vec![],
    };

    parse_sse_response(&body)
}

fn parse_sse_response(body: &str) -> Vec<Value> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(data_str) = line.strip_prefix("data: ")
            && let Ok(data) = serde_json::from_str::<Value>(data_str)
        {
            let result = &data["result"];
            // Standard MCP: content array with text items
            if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
                for item in content {
                    if item.get("type").and_then(|v| v.as_str()) == Some("text")
                        && let Some(text) = item.get("text").and_then(|v| v.as_str())
                    {
                        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                            return match parsed {
                                Value::Array(arr) => arr,
                                other => vec![other],
                            };
                        }
                        return vec![json!({"text": text})];
                    }
                }
            }
            // Fallback: structuredContent
            if let Some(structured) = result.get("structuredContent") {
                let r = structured.get("result").unwrap_or(structured);
                return match r {
                    Value::Array(arr) => arr.clone(),
                    other => vec![other.clone()],
                };
            }
        }
    }
    vec![]
}

/// Semantic search across xmcp bookmark corpus.
/// Falls back to intel_by_topic if semantic search returns empty.
pub async fn search_bookmarks(query: &str, limit: usize) -> Vec<Value> {
    let results = call_tool(
        "intel_semantic_search",
        json!({
            "query": query, "limit": limit
        }),
    )
    .await;
    if !results.is_empty() {
        return results;
    }
    let word = query.split_whitespace().next().unwrap_or(query);
    call_tool(
        "intel_by_topic",
        json!({
            "topic": word.to_lowercase(), "limit": limit
        }),
    )
    .await
}

/// Synchronous wrapper around xmcp `model_audit` for cabinet-load validation
/// (§4.3, P1 #15). Returns Ok(()) when every requested model_id is approved by
/// the vault. Returns Err(message) when at least one model_id is rejected.
///
/// Tolerance for offline / dev environments:
///   - `COUNCIL_SKIP_VAULT_CHECK=1` → unconditional Ok(()) (logged as WARN).
///   - xmcp server unreachable      → Ok(()) with WARN. Falls back to
///     "trust the cabinet YAML"; the alternative is to break every `cargo test`
///     / CLI invocation on machines without xmcp.
///   - xmcp reachable + responses available → strict gate: any UNKNOWN or
///     rejected model fails the cabinet load.
///
/// Tokio-runtime aware: works whether called from inside a tokio runtime
/// (via `block_in_place`) or from a plain `fn main`.
pub fn model_check_blocking(model_ids: &[&str]) -> Result<(), String> {
    if std::env::var("COUNCIL_SKIP_VAULT_CHECK").as_deref() == Ok("1") {
        eprintln!(
            "⚠️  COUNCIL_SKIP_VAULT_CHECK=1 — xmcp model_check bypassed for {} model(s)",
            model_ids.len()
        );
        return Ok(());
    }

    if model_ids.is_empty() {
        return Ok(());
    }

    let ids: Vec<String> = model_ids.iter().map(|s| s.to_string()).collect();

    // Run async work to completion. If we're already inside a tokio runtime,
    // use block_in_place; otherwise spin up a small runtime on a thread.
    let result: Result<Vec<Value>, String> = match tokio::runtime::Handle::try_current() {
        Ok(_handle) => {
            // Inside a runtime — use block_in_place so we don't deadlock the worker.
            std::thread::scope(|s| {
                s.spawn(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("rt build: {}", e))?;
                    Ok::<_, String>(rt.block_on(async {
                        if !is_available().await {
                            return Err("xmcp unreachable".to_string());
                        }
                        Ok(call_tool("model_check_batch", json!({ "ids": ids })).await)
                    }))
                })
                .join()
                .map_err(|_| "model_check thread panicked".to_string())?
            })?
        }
        Err(_) => {
            // No runtime — build one.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("rt build: {}", e))?;
            rt.block_on(async {
                if !is_available().await {
                    return Err("xmcp unreachable".to_string());
                }
                Ok(call_tool("model_check_batch", json!({ "ids": ids })).await)
            })
        }
    };

    let audit = match result {
        Ok(v) => v,
        Err(msg) if msg == "xmcp unreachable" => {
            eprintln!(
                "⚠️  xmcp unreachable — skipping model vault check for {} model(s). \
                 Set COUNCIL_SKIP_VAULT_CHECK=1 to suppress this warning.",
                model_ids.len()
            );
            return Ok(());
        }
        Err(msg) => {
            eprintln!(
                "⚠️  xmcp model_check failed ({}) — soft-pass for {} model(s).",
                msg,
                model_ids.len()
            );
            return Ok(());
        }
    };

    // Audit shape (xmcp model_check_batch): array of {model_id, status, ...}.
    // Status values (uppercased): APPROVED / LEGACY / RETIRING / BANNED / UNKNOWN.
    // We reject anything that is explicitly RETIRED/BANNED — UNKNOWN (and the
    // softer LEGACY/RETIRING) are treated as soft-pass (matches the "trust the
    // YAML when vault doesn't know" stance).
    let mut bad: Vec<String> = Vec::new();
    for row in &audit {
        let id = row
            .get("model_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let status = row
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        if status == "RETIRED" || status == "BANNED" {
            bad.push(format!("{} [{}]", id, status));
        }
    }
    if !bad.is_empty() {
        return Err(format!("xmcp model vault rejected: {}", bad.join(", ")));
    }

    Ok(())
}

/// Search recent X/Twitter posts for live claims verification.
/// Unwraps the X API nested response shape.
pub async fn search_posts(query: &str, limit: usize) -> Vec<Value> {
    let max_results = limit.clamp(10, 100);
    let raw = call_tool_with_timeout(
        "searchPostsRecent",
        json!({
            "query": query,
            "max_results": max_results,
            "tweet.fields": "created_at,public_metrics"
        }),
        20,
    )
    .await;

    if let Some(first) = raw.first() {
        if let Some(data) = first.get("data").and_then(|v| v.as_array()) {
            return data.iter().take(limit).cloned().collect();
        }
        if first
            .get("meta")
            .and_then(|m| m.get("result_count"))
            .and_then(|v| v.as_u64())
            == Some(0)
        {
            return vec![];
        }
    }
    raw
}

/// Hybrid search over local corpus (primary) + optional live X via hosted xapi.
/// Returns the full response object from intel_hybrid_search for structured
/// {corpus_hits, live_hits, merged, ...} access.
/// Default to corpus-only (use_live=false) to control cost; live gated by env.
pub async fn hybrid_search(focus: &str, use_live: bool, limit: usize) -> Value {
    let results = call_tool(
        "intel_hybrid_search",
        json!({
            "focus": focus,
            "use_live": use_live,
            "limit": limit,
            "live_limit": limit,
            "rerank": false,
            "verbose": true
        }),
    )
    .await;
    if let Some(first) = results.first() {
        first.clone()
    } else {
        json!({})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn mcp_handler(State(request_count): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        match request_count.fetch_add(1, Ordering::SeqCst) {
            0 => {
                let mut headers = HeaderMap::new();
                headers.insert("mcp-session-id", HeaderValue::from_static("session-1"));
                (
                    headers,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {}
                    })),
                )
                    .into_response()
            }
            1 => StatusCode::ACCEPTED.into_response(),
            _ => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"[{\\\"ok\\\":true}]\"}]}}\n\n",
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn call_tool_reads_streamed_sse_response_body() {
        let _guard = ENV_LOCK.lock().await;
        let previous_xmcp_url = std::env::var_os("XMCP_URL");

        let request_count = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/mcp", post(mcp_handler))
            .with_state(Arc::clone(&request_count));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind xmcp test server");
        let addr = listener.local_addr().expect("xmcp test server addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve xmcp test server");
        });

        unsafe {
            std::env::set_var("XMCP_URL", format!("http://{addr}"));
        }
        let result = call_tool_with_timeout("demo", json!({}), 2).await;
        match previous_xmcp_url {
            Some(value) => unsafe {
                std::env::set_var("XMCP_URL", value);
            },
            None => unsafe {
                std::env::remove_var("XMCP_URL");
            },
        }
        server.abort();

        assert_eq!(request_count.load(Ordering::SeqCst), 3);
        assert_eq!(result, vec![json!({"ok": true})]);
    }
}
