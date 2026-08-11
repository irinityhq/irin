//! Grok (xAI) provider client — DEPRECATED / migrating away.
//!
//! Use the local `grok` OAuth CLI instead (via "grok" or "grok_cli" provider slug).
//! This module is only for temp fallback or gateway.

use crate::types::{ProviderProvenance, ProviderResponse};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Instant;

/// Call Grok via xAI's v1/responses endpoint.
pub async fn ask(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let key = match std::env::var("XAI_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            return ProviderResponse {
                error: Some("XAI_API_KEY not set".into()),
                ..Default::default()
            };
        }
    };

    let model = if model.is_empty() { "grok-4.3" } else { model };

    // Build input array — system first (cache prefix), user last
    let mut input = Vec::new();
    if !system.is_empty() {
        input.push(json!({"role": "system", "content": system}));
    }
    input.push(json!({"role": "user", "content": prompt}));

    let payload = json!({
        "model": model,
        "input": input,
        "temperature": 0.7,
        "max_output_tokens": 4096,
        "stream": false,
    });

    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post("https://api.x.ai/v1/responses")
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .timeout(super::request_timeout())
        .json(&payload)
        .send()
        .await;

    let latency_ms = t0.elapsed().as_millis() as u64;

    match resp {
        Ok(r) => match r.json::<Value>().await {
            Ok(data) => parse_v1_response(data, model, latency_ms)
                .with_provider_provenance(ProviderProvenance::api("xai_api")),
            Err(e) => ProviderResponse {
                error: Some(format!("JSON parse error: {}", e)),
                latency_ms,
                ..Default::default()
            },
        },
        Err(e) => ProviderResponse {
            error: Some(format!("HTTP error: {}", e)),
            latency_ms,
            ..Default::default()
        },
    }
}

/// Call Grok with web_search tool enabled (for Sheldon validator).
pub async fn ask_with_web_search(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let key = match std::env::var("XAI_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            return ProviderResponse {
                error: Some("XAI_API_KEY not set".into()),
                ..Default::default()
            };
        }
    };

    let model = if model.is_empty() { "grok-4.3" } else { model };

    let mut input = Vec::new();
    if !system.is_empty() {
        input.push(json!({"role": "system", "content": system}));
    }
    input.push(json!({"role": "user", "content": prompt}));

    let payload = json!({
        "model": model,
        "input": input,
        "tools": [{"type": "web_search"}],
        "temperature": 0.7,
        "max_output_tokens": 4096,
        "stream": false,
    });

    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post("https://api.x.ai/v1/responses")
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .timeout(super::request_timeout())
        .json(&payload)
        .send()
        .await;

    let latency_ms = t0.elapsed().as_millis() as u64;

    match resp {
        Ok(r) => match r.json::<Value>().await {
            Ok(data) => parse_v1_response(data, model, latency_ms)
                .with_provider_provenance(ProviderProvenance::api_web("xai_api")),
            Err(e) => ProviderResponse {
                error: Some(format!("JSON parse error: {}", e)),
                latency_ms,
                ..Default::default()
            },
        },
        Err(e) => ProviderResponse {
            error: Some(format!("HTTP error: {}", e)),
            latency_ms,
            ..Default::default()
        },
    }
}

/// Parse v1/responses shape — shared by Grok and GPT.
pub fn parse_v1_response(data: Value, model: &str, latency_ms: u64) -> ProviderResponse {
    // Check for API error — v1/responses includes "error": null on success
    if let Some(err) = data.get("error")
        && !err.is_null()
    {
        return ProviderResponse {
            error: Some(format!("API error: {}", err)),
            latency_ms,
            ..Default::default()
        };
    }

    // Extract text — try output_text first, then dig into output array
    let mut text = data
        .get("output_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if text.is_empty()
        && let Some(output) = data.get("output").and_then(|v| v.as_array())
    {
        for item in output {
            if item.get("type").and_then(|v| v.as_str()) == Some("message")
                && let Some(content) = item.get("content").and_then(|v| v.as_array())
            {
                for part in content {
                    if part.get("type").and_then(|v| v.as_str()) == Some("output_text")
                        && let Some(t) = part.get("text").and_then(|v| v.as_str())
                    {
                        text.push_str(t);
                    }
                }
            }
        }
    }

    let usage = data.get("usage").cloned().unwrap_or(json!({}));
    let tokens_in = usage
        .get("input_tokens")
        .or(usage.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let tokens_out = usage
        .get("output_tokens")
        .or(usage.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let cached_in = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .or(usage.get("cached_input_tokens"))
        .and_then(|v| v.as_u64())
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
mod parse_v1_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_output_text_and_usage() {
        let data = json!({
            "output_text": "hello council",
            "model": "grok-returned",
            "usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 3 }
            },
            "error": null
        });
        let resp = parse_v1_response(data, "fallback-model", 42);
        assert!(resp.error.is_none());
        assert_eq!(resp.text, "hello council");
        assert_eq!(resp.model, "grok-returned");
        assert_eq!(resp.tokens_in, 11);
        assert_eq!(resp.tokens_out, 7);
        assert_eq!(resp.cached_in, 3);
        assert_eq!(resp.latency_ms, 42);
    }

    #[test]
    fn valid_nested_output_array_message() {
        let data = json!({
            "output": [{
                "type": "message",
                "content": [
                    { "type": "output_text", "text": "part-a " },
                    { "type": "output_text", "text": "part-b" }
                ]
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 9,
                "cached_input_tokens": 1
            }
        });
        let resp = parse_v1_response(data, "fallback-model", 9);
        assert!(resp.error.is_none());
        assert_eq!(resp.text, "part-a part-b");
        assert_eq!(resp.tokens_in, 5);
        assert_eq!(resp.tokens_out, 9);
        assert_eq!(resp.cached_in, 1);
        assert_eq!(resp.model, "fallback-model");
    }

    #[test]
    fn empty_object_yields_empty_text_no_error() {
        let resp = parse_v1_response(json!({}), "m", 1);
        assert!(resp.error.is_none());
        assert_eq!(resp.text, "");
        assert_eq!(resp.tokens_in, 0);
        assert_eq!(resp.tokens_out, 0);
        assert_eq!(resp.model, "m");
        assert_eq!(resp.latency_ms, 1);
    }

    #[test]
    fn error_payload_surfaces_api_error() {
        let data = json!({
            "error": { "message": "rate limited", "type": "rate_limit" }
        });
        let resp = parse_v1_response(data, "m", 3);
        assert!(
            resp.error
                .as_deref()
                .is_some_and(|e| e.contains("API error") && e.contains("rate limited")),
            "got {:?}",
            resp.error
        );
        assert_eq!(resp.text, "");
        assert_eq!(resp.latency_ms, 3);
    }

    #[test]
    fn malformed_usage_types_default_to_zero() {
        // Non-numeric usage fields must not panic; fall back to zero.
        let data = json!({
            "output_text": "ok",
            "usage": {
                "input_tokens": "nope",
                "output_tokens": true
            }
        });
        let resp = parse_v1_response(data, "m", 2);
        assert!(resp.error.is_none());
        assert_eq!(resp.text, "ok");
        assert_eq!(resp.tokens_in, 0);
        assert_eq!(resp.tokens_out, 0);
    }
}
