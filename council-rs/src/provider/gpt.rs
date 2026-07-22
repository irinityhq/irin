//! GPT (OpenAI) provider — Codex CLI primary, Responses API fallback
//!
//! CLI: single implementation in `agent_cli::ask_codex` (also used by `codex_cli` provider).
//! API: `POST /v1/responses` when Codex is missing or auth fails.

use crate::provider::agent_cli;
use crate::types::{ProviderProvenance, ProviderResponse};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Instant;

/// Public entry — Codex CLI primary (ChatGPT/subscription login).
/// Responses API only if CLI missing and OPENAI_API_KEY set, or
/// `COUNCIL_GPT_API_FALLBACK=1` / force path.
pub async fn ask(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    if agent_cli::is_codex_cli_available() {
        let cli_resp = agent_cli::ask_codex(prompt, system, model).await;
        if cli_resp.error.is_some() && api_fallback_enabled() {
            eprintln!(
                "   ↪ codex_cli failed — COUNCIL_GPT_API_FALLBACK=1, trying OpenAI API ({})",
                cli_resp.error.as_deref().unwrap_or("error")
            );
            return api_ask(prompt, system, model).await;
        }
        return cli_resp;
    }
    if api_fallback_enabled()
        || std::env::var("OPENAI_API_KEY")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    {
        return api_ask(prompt, system, model).await;
    }
    ProviderResponse {
        error: Some(
            "codex CLI unavailable; install Codex for subscription seats, or set OPENAI_API_KEY / COUNCIL_GPT_API_FALLBACK=1".into(),
        ),
        ..Default::default()
    }
}

fn api_fallback_enabled() -> bool {
    matches!(
        std::env::var("COUNCIL_GPT_API_FALLBACK").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

/// Direct API path — when `codex` CLI is unavailable or unauthenticated.
pub(crate) async fn api_ask(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            return ProviderResponse {
                error: Some("codex CLI unavailable and OPENAI_API_KEY not set".into()),
                ..Default::default()
            };
        }
    };
    let model = if model.is_empty() {
        "gpt-5.6-sol"
    } else {
        model
    };

    let mut payload = json!({
        "model": model,
        "input": prompt,
        "max_output_tokens": 4096,
        "stream": false,
        "reasoning": {"effort": "medium"},
    });
    if !system.is_empty() {
        payload["instructions"] = json!(system);
    }
    if !model.starts_with("gpt-5.6")
        && !model.starts_with("gpt-5.5")
        && !model.starts_with("gpt-5.4")
        && !model.starts_with("o")
    {
        payload["temperature"] = json!(0.7);
    }

    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post("https://api.openai.com/v1/responses")
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .timeout(super::request_timeout())
        .json(&payload)
        .send()
        .await;
    let latency_ms = t0.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => match r.json::<Value>().await {
            Ok(data) => crate::provider::grok::parse_v1_response(data, model, latency_ms)
                .with_provider_provenance(ProviderProvenance::api("openai_api")),
            Err(e) => ProviderResponse {
                error: Some(format!("JSON: {e}")),
                latency_ms,
                ..Default::default()
            },
        },
        Err(e) => ProviderResponse {
            error: Some(format!("HTTP: {e}")),
            latency_ms,
            ..Default::default()
        },
    }
}
