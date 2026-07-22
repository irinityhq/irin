//! Generic OpenAI-compatible provider client
//!
//! Handles any provider that speaks the /v1/chat/completions protocol:
//! NVIDIA NIM, Nous, DeepSeek, OpenRouter, Together, Groq, etc.
//!
//! Port of Python's `_openai_compat_ask`. The `reasoning_content` field
//! (used by Nous Hermes and NVIDIA Nemotron) is merged into the response
//! text with `<reasoning>` tags, matching the Python behavior.

use crate::types::{ProviderProvenance, ProviderResponse};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Instant;

/// Provider configs - (env_var, env_var_fallback, base_url).
fn provider_config(provider: &str) -> (&'static str, &'static str, &'static str) {
    match provider {
        // "nvidia" is the canonical slug for the NVIDIA integrate NIM backend.
        // "nim" is a legacy alias (historical label drift for cost-control cabinets).
        // They resolve to the exact same endpoint and key. We are normalizing to "nvidia".
        "nvidia" | "nim" => ("NVIDIA_API_KEY", "", "https://integrate.api.nvidia.com/v1"),
        "nous" => (
            "NOUS_API_KEY",
            "",
            "https://inference-api.nousresearch.com/v1",
        ),
        "deepseek" => ("DEEPSEEK_API_KEY", "", "https://api.deepseek.com/v1"),
        "groq" => ("GROQ_API_KEY", "", "https://api.groq.com/openai/v1"),
        "together" => ("TOGETHER_API_KEY", "", "https://api.together.xyz/v1"),
        "fireworks" => (
            "FIREWORKS_API_KEY",
            "",
            "https://api.fireworks.ai/inference/v1",
        ),
        "openrouter" => ("OPENROUTER_API_KEY", "", "https://openrouter.ai/api/v1"),
        "mistral" => ("MISTRAL_API_KEY", "", "https://api.mistral.ai/v1"),
        "perplexity" => ("PERPLEXITY_API_KEY", "", "https://api.perplexity.ai"),
        "sambanova" => ("SAMBANOVA_API_KEY", "", "https://api.sambanova.ai/v1"),
        "cerebras" => ("CEREBRAS_API_KEY", "", "https://api.cerebras.ai/v1"),
        "kimi" => ("MOONSHOT_API_KEY", "", "https://api.moonshot.cn/v1"),
        "cohere" => ("COHERE_API_KEY", "", "https://api.cohere.com/v2"),
        _ => ("", "", ""),
    }
}

/// Call any OpenAI-compatible provider.
///
/// `max_tokens` can be tuned per use-case (e.g., 512 for NIM convergence judge).
pub async fn ask(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    max_tokens: u32,
) -> ProviderResponse {
    let (env_key, env_fallback, base_url) = provider_config(provider);

    if base_url.is_empty() {
        return ProviderResponse {
            error: Some(format!("Unknown openai-compat provider: {}", provider)),
            ..Default::default()
        };
    }

    let key = std::env::var(env_key).or_else(|_| {
        if env_fallback.is_empty() {
            Err(std::env::VarError::NotPresent)
        } else {
            std::env::var(env_fallback)
        }
    });

    let key = match key {
        Ok(k) => k,
        Err(_) => {
            return ProviderResponse {
                error: Some(format!("{} not set (provider: {})", env_key, provider)),
                ..Default::default()
            };
        }
    };

    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": prompt}));

    let payload = build_chat_payload(provider, model, messages, max_tokens, false);

    let url = format!("{}/chat/completions", base_url);
    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .timeout(super::request_timeout())
        .json(&payload)
        .send()
        .await;

    let latency_ms = t0.elapsed().as_millis() as u64;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body_text = match r.text().await {
                Ok(t) => t,
                Err(e) => {
                    return ProviderResponse {
                        error: Some(format!("HTTP body: {e}")),
                        latency_ms,
                        ..Default::default()
                    };
                }
            };
            let data: Value = match serde_json::from_str(&body_text) {
                Ok(v) => v,
                Err(e) => {
                    let preview = if body_text.len() > 200 {
                        format!("{}…", &body_text[..200])
                    } else {
                        body_text.clone()
                    };
                    return ProviderResponse {
                        error: Some(format!(
                            "JSON parse (HTTP {}): {e}; body: {preview}",
                            status.as_u16()
                        )),
                        latency_ms,
                        ..Default::default()
                    };
                }
            };
            if !status.is_success() {
                let detail = data
                    .get("error")
                    .and_then(|e| e.get("message").and_then(|m| m.as_str()))
                    .or_else(|| data.get("error").and_then(|e| e.as_str()))
                    .unwrap_or(&body_text);
                let msg = format!("HTTP {}: {detail}", status.as_u16());
                return ProviderResponse {
                    error: Some(enrich_nvidia_error_hint(provider, &msg)),
                    latency_ms,
                    ..Default::default()
                };
            }
            let mut out = parse_chat_completions(data, model, latency_ms)
                .with_provider_provenance(ProviderProvenance::api(format!("{provider}_api")));
            if let Some(err) = out.error.as_mut() {
                *err = enrich_nvidia_error_hint(provider, err);
            }
            out
        }
        Err(e) => ProviderResponse {
            error: Some(format!("HTTP: {}", e)),
            latency_ms,
            ..Default::default()
        },
    }
}

/// Accumulated state while parsing an SSE `stream: true` response (N01).
///
/// Kept separate from the network so the line/event parsing is unit-testable
/// against a canned SSE body (see tests).
#[derive(Debug, Default)]
pub struct SseAccumulator {
    /// Visible content (mirrors `choices[0].delta.content`).
    pub content: String,
    /// Reasoning deltas (mirrors `choices[0].delta.reasoning_content`).
    pub reasoning: String,
    pub model: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cached_in: u32,
    /// API error surfaced inside the stream body.
    pub error: Option<String>,
}

impl SseAccumulator {
    /// Apply a single decoded SSE `data:` JSON chunk. Returns the *visible*
    /// content delta (if any) so the caller can forward it as a `seat_chunk`.
    /// Reasoning deltas are accumulated but not forwarded as visible chunks —
    /// they are merged into the final text exactly like the non-stream parser.
    pub fn apply_chunk(&mut self, chunk: &Value) -> Option<String> {
        if let Some(err) = chunk.get("error")
            && !err.is_null()
        {
            let msg = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| err.as_str().unwrap_or("unknown error"));
            self.error = Some(format!("API error: {}", msg));
            return None;
        }

        if let Some(m) = chunk.get("model").and_then(|v| v.as_str())
            && !m.is_empty()
        {
            self.model = m.to_string();
        }

        // Usage may arrive on the final chunk (stream_options.include_usage)
        // or alongside the last delta — accumulate whenever present.
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            if let Some(v) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                self.tokens_in = v as u32;
            }
            if let Some(v) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.tokens_out = v as u32;
            }
            if let Some(v) = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .or_else(|| {
                    usage
                        .get("prompt_cache_hit_tokens")
                        .and_then(|v| v.as_u64())
                })
            {
                self.cached_in = v as u32;
            }
        }

        let delta = chunk
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("delta"))
            .cloned()
            .unwrap_or(json!({}));

        if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str())
            && !r.is_empty()
        {
            self.reasoning.push_str(r);
        }

        match delta.get("content").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => {
                self.content.push_str(c);
                Some(c.to_string())
            }
            _ => None,
        }
    }

    /// Merge reasoning + content for the final ProviderResponse.
    pub fn merged_text(&self) -> String {
        merge_message_text(&self.content, &self.reasoning)
    }

    /// Build the final `ProviderResponse` (identical shape to the buffered
    /// `parse_chat_completions`).
    pub fn into_response(self, fallback_model: &str, latency_ms: u64) -> ProviderResponse {
        if let Some(err) = self.error {
            return ProviderResponse {
                error: Some(err),
                latency_ms,
                ..Default::default()
            };
        }
        let resp_model = if self.model.is_empty() {
            fallback_model.to_string()
        } else {
            self.model.clone()
        };
        ProviderResponse {
            text: self.merged_text(),
            model: resp_model,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cached_in: self.cached_in,
            latency_ms,
            cost_usd: 0.0,
            error: None,
            gateway_provenance: None,
            gateway_attempts: Vec::new(),
            provider_provenance: None,
        }
    }
}

/// Max size of an in-flight SSE buffer (the partial/leftover trailing line plus
/// any newly appended bytes) before we abort the stream. A well-behaved provider
/// terminates `data:` lines with `\n`; a single line larger than this means the
/// peer is misbehaving and would otherwise grow memory unbounded within the
/// request timeout. 1 MiB is far above any legitimate SSE chunk.
const SSE_LEFTOVER_CAP: usize = 1024 * 1024;

/// Returns a stream-error message if the in-flight SSE buffer has exceeded
/// [`SSE_LEFTOVER_CAP`], otherwise `None`. Uses the `stream read:` prefix so the
/// breach surfaces through the same error lane as a network read failure.
fn sse_leftover_overflow(buf: &str) -> Option<String> {
    if buf.len() > SSE_LEFTOVER_CAP {
        Some(format!(
            "stream read: SSE partial line exceeded {SSE_LEFTOVER_CAP}-byte cap (no line terminator from provider)"
        ))
    } else {
        None
    }
}

/// Feed a complete (or partial) SSE buffer to the accumulator, line by line.
///
/// Pulls `data: {json}` lines, ignores `[DONE]` and comment/blank lines, and
/// invokes `on_delta` with each visible content delta. Returns the leftover
/// (incomplete trailing line) so a byte-stream caller can carry it forward.
fn feed_sse_buffer(buf: &str, acc: &mut SseAccumulator, on_delta: &mut impl FnMut(&str)) -> String {
    // The final element after the last '\n' is an incomplete line; keep it.
    let mut lines: Vec<&str> = buf.split('\n').collect();
    let leftover = lines.pop().unwrap_or("").to_string();
    for line in lines {
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            continue; // comments / event: lines / blanks
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(chunk) = serde_json::from_str::<Value>(data)
            && let Some(delta) = acc.apply_chunk(&chunk)
        {
            on_delta(&delta);
        }
    }
    leftover
}

fn is_nemotron_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("nemotron")
}

fn is_nvidia_provider(provider: &str) -> bool {
    provider == "nvidia" || provider == "nim"
}

fn nim_enable_thinking() -> bool {
    matches!(
        std::env::var("COUNCIL_NIM_ENABLE_THINKING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

fn include_reasoning_in_text() -> bool {
    matches!(
        std::env::var("COUNCIL_INCLUDE_REASONING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

/// Seat-visible text: content only by default. Reasoning is opt-in via
/// `COUNCIL_INCLUDE_REASONING=1` (legacy Nous/Hermes deliberation path).
/// Reasoning-only responses (empty `content`) are suppressed unless opted in.
pub fn merge_message_text(content: &str, reasoning: &str) -> String {
    if include_reasoning_in_text() && !reasoning.is_empty() && !content.is_empty() {
        format!("<reasoning>\n{reasoning}\n</reasoning>\n\n{content}")
    } else if include_reasoning_in_text() && content.is_empty() && !reasoning.is_empty() {
        reasoning.to_string()
    } else if !content.is_empty() {
        content.to_string()
    } else {
        String::new()
    }
}

fn enrich_nvidia_error_hint(provider: &str, msg: &str) -> String {
    if !is_nvidia_provider(provider) {
        return msg.to_string();
    }
    let lower = msg.to_ascii_lowercase();
    if lower.contains("resourceexhausted")
        || lower.contains("too many request")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("request limit")
    {
        return format!(
            "{msg}\n\
             NIM free tier: quota or concurrency cap hit (often 32–40 RPM + burst penalties). \
             Wait 30–60+ minutes, avoid parallel nvidia seats, and space smokes ≥5s apart."
        );
    }
    if lower.contains("403")
        || lower.contains("function_not_found")
        || lower.contains("function not found")
        || lower.contains("model_not_found")
        || lower.contains("model not found")
        || lower.contains("not invokable")
        || lower.contains("not-invokable")
        || lower.contains("access denied")
        || lower.contains("no access")
        || lower.contains("not authorized")
        || lower.contains("not authorised")
        || lower.contains("permission denied")
        || lower.contains("entitlement")
        || lower.contains("not entitled")
    {
        return format!(
            "{msg}\n\
             NVIDIA NIM model access varies by account. Confirm the API key and model entitlement, \
             then select the starter-nvidia cabinet or a model from \
             council-rs/config/nim-invokable-allowlist.txt; catalog listing alone does not guarantee invocation access."
        );
    }
    msg.to_string()
}

pub(crate) fn build_chat_payload(
    provider: &str,
    model: &str,
    messages: Vec<Value>,
    max_tokens: u32,
    stream: bool,
) -> Value {
    let nemotron = is_nvidia_provider(provider) && is_nemotron_model(model);
    let thinking = nemotron && nim_enable_thinking();
    let temperature = if nemotron && !thinking { 0.0 } else { 0.7 };

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": temperature,
    });
    if stream {
        payload["stream"] = json!(true);
        payload["stream_options"] = json!({ "include_usage": true });
    }
    if nemotron {
        payload["chat_template_kwargs"] = json!({ "enable_thinking": thinking });
    }
    payload
}

/// Streaming variant of [`ask`] (N01).
///
/// Sends `stream: true`, parses the SSE delta stream, and forwards each visible
/// content delta to `on_delta`. Returns a `ProviderResponse` byte-identical in
/// shape to the buffered [`ask`]. On any network/HTTP error the response carries
/// `error` exactly like [`ask`] — there is no mid-stream fallback to buffered
/// mode (per the lane contract).
pub async fn ask_streaming(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
) -> ProviderResponse {
    let (env_key, env_fallback, base_url) = provider_config(provider);

    if base_url.is_empty() {
        return ProviderResponse {
            error: Some(format!("Unknown openai-compat provider: {}", provider)),
            ..Default::default()
        };
    }

    let key = std::env::var(env_key).or_else(|_| {
        if env_fallback.is_empty() {
            Err(std::env::VarError::NotPresent)
        } else {
            std::env::var(env_fallback)
        }
    });
    let key = match key {
        Ok(k) => k,
        Err(_) => {
            return ProviderResponse {
                error: Some(format!("{} not set (provider: {})", env_key, provider)),
                ..Default::default()
            };
        }
    };

    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": prompt}));

    let payload = build_chat_payload(provider, model, messages, max_tokens, true);

    let url = format!("{}/chat/completions", base_url);
    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .timeout(super::request_timeout())
        .json(&payload)
        .send()
        .await;

    let r = match resp {
        Ok(r) => r,
        Err(e) => {
            return ProviderResponse {
                error: Some(format!("HTTP: {}", e)),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }
    };

    let r = match r.error_for_status() {
        Ok(r) => r,
        Err(e) => {
            let message = format!("HTTP: {}", e);
            return ProviderResponse {
                error: Some(enrich_nvidia_error_hint(provider, &message)),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }
    };

    let mut acc = SseAccumulator::default();
    let mut buf = String::new();
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                return ProviderResponse {
                    error: Some(format!("stream read: {}", e)),
                    latency_ms: t0.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        buf = feed_sse_buffer(&buf, &mut acc, &mut |d| on_delta(d));
        // Guard against a misbehaving provider streaming an unbounded single
        // line (no newline ever arrives): the leftover would grow without limit
        // within the request timeout. Abort via the stream-error path on breach.
        if let Some(err) = sse_leftover_overflow(&buf) {
            return ProviderResponse {
                error: Some(err),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }
    }
    // Flush any complete trailing line left in the buffer.
    if !buf.is_empty() {
        buf.push('\n');
        let _ = feed_sse_buffer(&buf, &mut acc, &mut |d| on_delta(d));
    }

    let latency_ms = t0.elapsed().as_millis() as u64;
    let mut out = acc
        .into_response(model, latency_ms)
        .with_provider_provenance(ProviderProvenance::api(format!("{provider}_api")));
    if let Some(err) = out.error.as_mut() {
        *err = enrich_nvidia_error_hint(provider, err);
    }
    out
}

/// Parse /v1/chat/completions response.
///
/// Handles the `reasoning_content` field from reasoning models (Nous Hermes,
/// NVIDIA Nemotron). By default only `content` is returned to seats; reasoning
/// is included only when `COUNCIL_INCLUDE_REASONING=1`.
///
/// Also handles NIM's `prompt_tokens_details: null` — the Python bug that
/// crashed on `.get()` on None. In Rust, we use `.and_then()` chains that
/// naturally handle null/missing JSON values.
pub fn parse_chat_completions(data: Value, model: &str, latency_ms: u64) -> ProviderResponse {
    // Check for API error
    if let Some(err) = data.get("error")
        && !err.is_null()
    {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| err.as_str().unwrap_or("unknown error"));
        return ProviderResponse {
            error: Some(format!("API error: {msg}")),
            latency_ms,
            ..Default::default()
        };
    }

    let choice = data
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));

    let msg = choice.get("message").cloned().unwrap_or(json!({}));

    // Content — may be null for reasoning-only models
    let content = msg
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Reasoning content — Nous Hermes, Nemotron
    let reasoning = msg
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Merge — seats see content; reasoning only when opted in.
    let text = merge_message_text(&content, &reasoning);

    // Usage — handles NIM's `prompt_tokens_details: null` safely via and_then chains
    let usage = data.get("usage").cloned().unwrap_or(json!({}));
    let tokens_in = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let tokens_out = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    // NIM returns prompt_tokens_details: null (not missing). Rust's and_then
    // naturally handles this — no crash like Python's dict.get().get() on None.
    let cached_in = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            usage
                .get("prompt_cache_hit_tokens")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(0) as u32;

    let resp_model = data
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(model)
        .to_string();

    ProviderResponse {
        text,
        model: resp_model,
        tokens_in,
        tokens_out,
        cached_in,
        latency_ms,
        cost_usd: 0.0,
        error: None,
        gateway_provenance: None,
        gateway_attempts: Vec::new(),
        provider_provenance: None,
    }
}

#[cfg(test)]
mod sse_tests {
    use super::*;

    /// Serializes tests that read or mutate `COUNCIL_INCLUDE_REASONING` —
    /// process env is shared across parallel test threads, so an opted-in
    /// window in one test must not leak into a default-off assertion in
    /// another (observed as a CI-only flake under runner load).
    static REASONING_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reasoning_env_lock() -> std::sync::MutexGuard<'static, ()> {
        REASONING_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a canned SSE body through the buffer feeder and return
    /// (forwarded deltas in order, final accumulator).
    fn drive(body: &str) -> (Vec<String>, SseAccumulator) {
        let mut acc = SseAccumulator::default();
        let mut deltas: Vec<String> = Vec::new();
        // Split into arbitrary byte boundaries to exercise the leftover carry.
        let mut buf = String::new();
        for piece in body.as_bytes().chunks(7) {
            buf.push_str(&String::from_utf8_lossy(piece));
            buf = feed_sse_buffer(&buf, &mut acc, &mut |d| deltas.push(d.to_string()));
        }
        if !buf.is_empty() {
            buf.push('\n');
            let _ = feed_sse_buffer(&buf, &mut acc, &mut |d| deltas.push(d.to_string()));
        }
        (deltas, acc)
    }

    #[test]
    fn parses_deltas_and_final_text() {
        let body = "\
data: {\"model\":\"m1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\", world\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\
\n\
data: [DONE]\n";
        let (deltas, acc) = drive(body);
        assert_eq!(deltas, vec!["Hello".to_string(), ", world".to_string()]);
        let resp = acc.into_response("fallback", 12);
        assert_eq!(resp.text, "Hello, world");
        assert_eq!(resp.model, "m1");
        assert_eq!(resp.tokens_in, 5);
        assert_eq!(resp.tokens_out, 2);
        assert_eq!(resp.latency_ms, 12);
        assert!(resp.error.is_none());
    }

    #[test]
    fn reasoning_merged_only_when_opted_in() {
        let _guard = reasoning_env_lock();
        unsafe {
            std::env::remove_var("COUNCIL_INCLUDE_REASONING");
        }
        assert_eq!(merge_message_text("answer", "think"), "answer");

        unsafe {
            std::env::remove_var("COUNCIL_INCLUDE_REASONING");
        }
        assert_eq!(merge_message_text("", "think"), "");

        unsafe {
            std::env::set_var("COUNCIL_INCLUDE_REASONING", "1");
        }
        assert_eq!(
            merge_message_text("answer", "think"),
            "<reasoning>\nthink\n</reasoning>\n\nanswer"
        );
        unsafe {
            std::env::remove_var("COUNCIL_INCLUDE_REASONING");
        }
    }

    #[test]
    fn reasoning_stream_content_only_by_default() {
        let _guard = reasoning_env_lock();
        unsafe {
            std::env::remove_var("COUNCIL_INCLUDE_REASONING");
        }
        let body = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\
data: [DONE]\n";
        let (deltas, acc) = drive(body);
        assert_eq!(deltas, vec!["answer".to_string()]);
        assert_eq!(acc.merged_text(), "answer");
    }

    #[test]
    fn nemotron_payload_disables_thinking_by_default() {
        let payload = build_chat_payload(
            "nvidia",
            "nvidia/nemotron-3-super-120b-a12b",
            vec![json!({"role": "user", "content": "hi"})],
            64,
            false,
        );
        assert_eq!(payload["temperature"], 0.0);
        assert_eq!(payload["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn nvidia_rate_limit_error_keeps_free_tier_guidance() {
        let enriched = enrich_nvidia_error_hint("nvidia", "HTTP 429: rate limit exceeded");
        assert!(enriched.contains("NIM free tier: quota or concurrency cap hit"));
        assert!(enriched.contains("Wait 30–60+ minutes"));
        assert!(!enriched.contains("model access varies by account"));
    }

    #[test]
    fn nvidia_entitlement_errors_recommend_the_starter_allowlist() {
        for message in [
            "HTTP 403: forbidden",
            "HTTP status client error (403 Forbidden)",
            "HTTP 404: FUNCTION_NOT_FOUND",
            "HTTP 400: model not invokable for this account",
            "HTTP 400: model entitlement missing",
        ] {
            let enriched = enrich_nvidia_error_hint("nvidia", message);
            assert!(
                enriched.contains("model access varies by account"),
                "missing account guidance for: {message}"
            );
            assert!(enriched.contains("starter-nvidia"));
            assert!(enriched.contains("config/nim-invokable-allowlist.txt"));
        }
    }

    #[test]
    fn non_nvidia_errors_are_not_enriched() {
        let message = "HTTP 403: model access denied";
        assert_eq!(enrich_nvidia_error_hint("openrouter", message), message);
    }

    #[test]
    fn surfaces_stream_error() {
        let body = "data: {\"error\":{\"message\":\"rate limited\"}}\n";
        let (deltas, acc) = drive(body);
        assert!(deltas.is_empty());
        let resp = acc.into_response("fallback", 3);
        assert_eq!(resp.error.as_deref(), Some("API error: rate limited"));
        assert!(resp.text.is_empty());
    }

    #[test]
    fn oversized_partial_line_trips_leftover_cap() {
        // A provider streaming an endless newline-free `data:` line produces an
        // ever-growing leftover. Simulate the streaming loop: feed the buffer,
        // carry the leftover forward, and assert the cap guard fires instead of
        // letting memory grow unbounded.
        let mut acc = SseAccumulator::default();
        let mut buf = String::new();
        // One oversized chunk with no line terminator (cap + slack).
        let oversized = "x".repeat(SSE_LEFTOVER_CAP + 1024);
        buf.push_str(&oversized);
        buf = feed_sse_buffer(&buf, &mut acc, &mut |_| {});
        // Nothing was parsed (no complete line) and the whole thing is leftover.
        assert_eq!(buf.len(), oversized.len());
        let err = sse_leftover_overflow(&buf).expect("oversized leftover must trip the cap");
        assert!(err.contains("cap"), "error must mention the cap: {err}");
        assert!(
            err.starts_with("stream read:"),
            "uses the stream-error lane"
        );

        // A buffer at/under the cap must NOT trip the guard.
        let small = "data: {".to_string();
        assert!(sse_leftover_overflow(&small).is_none());
    }

    #[test]
    fn ignores_non_data_lines_and_falls_back_to_model() {
        let body = "\
: keep-alive comment\n\
event: message\n\
data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\
data: [DONE]\n";
        let (deltas, acc) = drive(body);
        assert_eq!(deltas, vec!["x".to_string()]);
        let resp = acc.into_response("fallback-model", 1);
        assert_eq!(resp.model, "fallback-model");
        assert_eq!(resp.text, "x");
    }
}
