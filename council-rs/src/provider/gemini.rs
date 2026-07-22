//! Gemini (Vertex AI) provider — ADC token caching, routing via `gemini_routing.yaml`.
//!
//! The exact `gemini_vertex` transport uses this path directly. The legacy
//! `gemini` provider slug still prefers `agy_cli` (Antigravity `agy -p`).
//! See dispatch in mod.rs and registry. thinkingConfig/levels are Vertex-specific.

use crate::provider::gemini_route;
use crate::types::{ProviderProvenance, ProviderResponse};
use reqwest::Client;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_VERTEX_LOCATION: &str = "global";

struct TokenCache {
    token: String,
    expires: f64,
}
static GEMINI_CACHE: LazyLock<Mutex<TokenCache>> = LazyLock::new(|| {
    Mutex::new(TokenCache {
        token: String::new(),
        expires: 0.0,
    })
});

fn get_token() -> Result<String, String> {
    let now = unix_epoch_secs(SystemTime::now())?;
    let mut cache = GEMINI_CACHE.lock().unwrap();
    if !cache.token.is_empty() && now < cache.expires {
        return Ok(cache.token.clone());
    }
    let out = std::process::Command::new("gcloud")
        .args(["auth", "print-access-token"])
        .output()
        .map_err(|e| format!("gcloud: {e}"))?;
    if !out.status.success() {
        return Err("gcloud auth failed".into());
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    cache.token = token.clone();
    cache.expires = now + 3000.0; // 50 min TTL
    Ok(token)
}

fn unix_epoch_secs(now: SystemTime) -> Result<f64, String> {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .map_err(|e| format!("system clock before UNIX_EPOCH: {e}"))
}

fn clean_project(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "your-gcp-project" || trimmed == "(unset)" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn select_vertex_project(
    vertex_project: Option<&str>,
    google_cloud_project: Option<&str>,
    gcloud_project: Option<&str>,
) -> Result<String, String> {
    vertex_project
        .and_then(clean_project)
        .or_else(|| google_cloud_project.and_then(clean_project))
        .or_else(|| gcloud_project.and_then(clean_project))
        .ok_or_else(|| {
            "VERTEX_PROJECT or GOOGLE_CLOUD_PROJECT must be set, or `gcloud config get-value project` must return a real project".to_string()
        })
}

fn gcloud_config_project() -> Option<String> {
    let out = std::process::Command::new("gcloud")
        .args(["config", "get-value", "project", "--quiet"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    clean_project(&String::from_utf8_lossy(&out.stdout))
}

pub(crate) fn resolve_vertex_project() -> Result<String, String> {
    select_vertex_project(
        std::env::var("VERTEX_PROJECT").ok().as_deref(),
        std::env::var("GOOGLE_CLOUD_PROJECT").ok().as_deref(),
        gcloud_config_project().as_deref(),
    )
}

pub(crate) fn resolve_vertex_location() -> String {
    std::env::var("VERTEX_LOCATION")
        .or_else(|_| std::env::var("GOOGLE_CLOUD_LOCATION"))
        .unwrap_or_else(|_| DEFAULT_VERTEX_LOCATION.to_string())
}

pub fn has_vertex_project_config() -> bool {
    resolve_vertex_project().is_ok()
}

pub fn is_vertex_available() -> bool {
    has_vertex_project_config()
        && std::process::Command::new("gcloud")
            .args(["auth", "print-access-token"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

pub async fn ask(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let project = match resolve_vertex_project() {
        Ok(p) => p,
        Err(e) => {
            return ProviderResponse {
                error: Some(e),
                ..Default::default()
            };
        }
    };
    let location = resolve_vertex_location();
    let token = match get_token() {
        Ok(t) => t,
        Err(e) => {
            return ProviderResponse {
                error: Some(e),
                ..Default::default()
            };
        }
    };
    let mdl = gemini_route::resolve_wire_model(model);
    let gen_cfg = gemini_route::resolve_generation(&mdl);
    let host = if location == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    let url = format!(
        "https://{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{mdl}:generateContent"
    );
    let mut body = json!({
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        "generationConfig": {
            "temperature": gen_cfg.temperature,
            "maxOutputTokens": gen_cfg.max_output_tokens,
            "thinkingConfig": {"thinkingLevel": gen_cfg.thinking_level},
        },
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .timeout(super::request_timeout())
        .json(&body)
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
                    return ProviderResponse {
                        error: Some(format!("JSON: {e}")),
                        latency_ms,
                        ..Default::default()
                    };
                }
            };
            if !status.is_success() {
                return ProviderResponse {
                    error: Some(format!(
                        "Gemini HTTP {}: {}",
                        status.as_u16(),
                        gemini_error_detail(&data).unwrap_or_else(|| truncate_json(&data))
                    )),
                    latency_ms,
                    ..Default::default()
                };
            }
            parse_generate_content(data, &mdl, latency_ms)
                .with_provider_provenance(ProviderProvenance::api("vertex_gemini_api"))
        }
        Err(e) => ProviderResponse {
            error: Some(format!("HTTP: {e}")),
            latency_ms,
            ..Default::default()
        },
    }
}

fn parse_generate_content(data: Value, model: &str, latency_ms: u64) -> ProviderResponse {
    if let Some(detail) = gemini_error_detail(&data) {
        return ProviderResponse {
            error: Some(format!("Gemini API error: {detail}")),
            latency_ms,
            ..Default::default()
        };
    }

    let parts = data
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array());
    let text = parts
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let u = data.get("usageMetadata").cloned().unwrap_or(json!({}));
    let tokens_in = u
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let tokens_out = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    // When Gemini returns 200 with no candidate text — the thinking budget
    // consumed maxOutputTokens before any visible token landed, the response
    // was safety-filtered, or another terminal state hit before output —
    // surface it as an error. Otherwise callers treat silent "" as success.
    if text.is_empty() {
        let finish_reason = data
            .pointer("/candidates/0/finishReason")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();
        let thoughts = u
            .get("thoughtsTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        return ProviderResponse {
            text: String::new(),
            model: model.to_string(),
            tokens_in,
            tokens_out,
            cached_in: 0,
            latency_ms,
            cost_usd: 0.0,
            error: Some(format!(
                "Gemini returned no text (finishReason: {}, thoughts: {}, candidatesTokenCount: {})",
                finish_reason, thoughts, tokens_out
            )),
            gateway_provenance: None,
            gateway_attempts: Vec::new(),
            provider_provenance: Some(ProviderProvenance::api("vertex_gemini_api")),
        };
    }
    ProviderResponse {
        text,
        model: model.to_string(),
        tokens_in,
        tokens_out,
        cached_in: 0,
        latency_ms,
        cost_usd: 0.0,
        error: None,
        gateway_provenance: None,
        gateway_attempts: Vec::new(),
        provider_provenance: None,
    }
}

fn gemini_error_detail(data: &Value) -> Option<String> {
    let err = data.get("error")?;
    let code = err.get("code").and_then(|v| v.as_i64());
    let status = err.get("status").and_then(|v| v.as_str());
    let message = err.get("message").and_then(|v| v.as_str());
    Some(match (code, status, message) {
        (Some(code), Some(status), Some(message)) => format!("{status} ({code}): {message}"),
        (_, Some(status), Some(message)) => format!("{status}: {message}"),
        (_, _, Some(message)) => message.to_string(),
        _ => truncate_json(err),
    })
}

fn truncate_json(data: &Value) -> String {
    data.to_string().chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_project_resolution_rejects_placeholder() {
        let err = select_vertex_project(Some("your-gcp-project"), None, None).unwrap_err();
        assert!(err.contains("VERTEX_PROJECT"));
    }

    #[test]
    fn vertex_project_resolution_accepts_google_cloud_project_fallback() {
        let project = select_vertex_project(None, Some("real-project"), None).unwrap();
        assert_eq!(project, "real-project");
    }

    #[test]
    fn vertex_project_resolution_prefers_vertex_project() {
        let project = select_vertex_project(
            Some("vertex-project"),
            Some("google-project"),
            Some("gcloud-project"),
        )
        .unwrap();
        assert_eq!(project, "vertex-project");
    }

    #[test]
    fn gemini_error_shape_is_not_reported_as_unknown_no_text() {
        let resp = parse_generate_content(
            json!({
                "error": {
                    "code": 403,
                    "status": "PERMISSION_DENIED",
                    "message": "Project is invalid"
                }
            }),
            "gemini-test",
            12,
        );
        let err = resp.error.unwrap();
        assert!(err.contains("PERMISSION_DENIED"));
        assert!(err.contains("Project is invalid"));
        assert!(!err.contains("UNKNOWN"));
    }

    #[test]
    fn unix_epoch_secs_rejects_pre_epoch_time_without_panic() {
        let err = unix_epoch_secs(UNIX_EPOCH - std::time::Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("before UNIX_EPOCH"));
    }
}
