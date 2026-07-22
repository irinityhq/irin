//! Claude (Anthropic) provider — Claude Code CLI primary, Messages API fallback
//!
//! Model routing: `claude_routing.yaml`. CLI: `claude -p` with seat constraints from yaml.

use crate::provider::claude_route;
use crate::types::{ProviderProvenance, ProviderResponse};
use reqwest::Client;
use serde_json::{Value, json};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;

pub fn is_claude_cli_available() -> bool {
    std::process::Command::new("claude")
        .arg("--version")
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

fn force_api_path() -> bool {
    matches!(
        std::env::var("COUNCIL_CLAUDE_FORCE_API").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

fn claude_cli_version() -> Option<String> {
    std::process::Command::new("claude")
        .arg("--version")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn check_cli_version() -> Result<(), String> {
    let routing = claude_route::routing_snapshot();
    let min_str = routing.min_cli_version.trim();
    if min_str.is_empty() {
        return Ok(());
    }
    let Some(min) = claude_route::parse_min_cli_version(min_str) else {
        return Ok(());
    };
    let raw = claude_cli_version().unwrap_or_default();
    let Some(have) = claude_route::parse_cli_version(&raw) else {
        return Err(format!(
            "claude CLI version unreadable ({raw:?}); need >= {min_str} — upgrade or set COUNCIL_CLAUDE_FORCE_API=1"
        ));
    };
    if claude_route::version_at_least(have, min) {
        Ok(())
    } else {
        Err(format!(
            "claude CLI {raw} < required {min_str} — upgrade or set COUNCIL_CLAUDE_FORCE_API=1"
        ))
    }
}

/// Public entry — Claude Code CLI (`claude -p`) primary for Max/subscription.
///
/// API is **opt-in only** (`COUNCIL_CLAUDE_FORCE_API=1` or
/// `COUNCIL_CLAUDE_API_FALLBACK=1`). Presence of `ANTHROPIC_API_KEY` in the
/// process env must not force the Messages API: the CLI inherits that var and
/// will burn API credits instead of the operator subscription.
pub async fn ask(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    if force_api_path() {
        return api_ask(prompt, system, model).await;
    }
    if is_claude_cli_available() {
        if let Err(msg) = check_cli_version() {
            return ProviderResponse {
                error: Some(msg),
                ..Default::default()
            };
        }
        match cli_ask(prompt, system, model).await {
            CliOutcome::Response(resp) => {
                if resp.error.is_some() && api_fallback_enabled() {
                    eprintln!(
                        "   ↪ claude_cli failed — COUNCIL_CLAUDE_API_FALLBACK=1, trying Messages API ({})",
                        resp.error.as_deref().unwrap_or("error")
                    );
                    return api_ask(prompt, system, model).await;
                }
                return *resp;
            }
            CliOutcome::NotFound => {
                if api_fallback_enabled() || env_nonempty_anthropic() {
                    return api_ask(prompt, system, model).await;
                }
                return ProviderResponse {
                    error: Some(
                        "claude CLI not found on PATH; install Claude Code or set COUNCIL_CLAUDE_FORCE_API=1 with ANTHROPIC_API_KEY".into(),
                    ),
                    ..Default::default()
                };
            }
        }
    }
    if api_fallback_enabled() || force_api_path() {
        return api_ask(prompt, system, model).await;
    }
    ProviderResponse {
        error: Some(
            "claude CLI unavailable; install Claude Code for subscription seats, or set COUNCIL_CLAUDE_FORCE_API=1".into(),
        ),
        ..Default::default()
    }
}

/// Pure Claude Code subscription transport. Unlike the legacy `claude` entry,
/// this never falls back to the Anthropic API.
pub(crate) async fn ask_code(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    if !is_claude_cli_available() {
        return ProviderResponse {
            error: Some("claude_code: Claude Code CLI unavailable".into()),
            ..Default::default()
        };
    }
    if let Err(msg) = check_cli_version() {
        return ProviderResponse {
            error: Some(msg),
            ..Default::default()
        };
    }
    match cli_ask(prompt, system, model).await {
        CliOutcome::Response(resp) => *resp,
        CliOutcome::NotFound => ProviderResponse {
            error: Some("claude_code: Claude Code CLI unavailable".into()),
            ..Default::default()
        },
    }
}

fn api_fallback_enabled() -> bool {
    matches!(
        std::env::var("COUNCIL_CLAUDE_API_FALLBACK").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

fn env_nonempty_anthropic() -> bool {
    std::env::var("ANTHROPIC_API_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

enum CliOutcome {
    Response(Box<ProviderResponse>),
    NotFound,
}

async fn cli_ask(prompt: &str, system: &str, model: &str) -> CliOutcome {
    let routing = claude_route::routing_snapshot();
    let resolved = claude_route::resolve_cli_model(model);
    let seat = &routing.seat;

    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--model", resolved.cli_model_arg.as_str()]);
    cmd.args(["--output-format", seat.output_format.as_str()]);
    cmd.args(["--permission-mode", seat.permission_mode.as_str()]);
    if seat.no_session_persistence {
        cmd.arg("--no-session-persistence");
    }
    if !system.is_empty() {
        cmd.args(["--system-prompt", system]);
    }
    // Force subscription/OAuth path: Claude Code prefers ANTHROPIC_API_KEY when
    // set and will hit the paid Messages API (credit-balance errors) instead of
    // the operator's Claude Max subscription.
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd.env_remove("CLAUDE_API_KEY");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let t0 = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CliOutcome::NotFound,
        Err(e) => {
            return CliOutcome::Response(Box::new(ProviderResponse {
                error: Some(format!("claude CLI spawn: {}", e)),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            }));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            return CliOutcome::Response(Box::new(ProviderResponse {
                error: Some(format!("claude CLI stdin write: {}", e)),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            }));
        }
        drop(stdin);
    }

    let cli_timeout = super::request_timeout();
    let timeout_error = format!("claude CLI timeout ({}s)", cli_timeout.as_secs());
    let output = match wait_with_timeout_output(child, cli_timeout, timeout_error, t0).await {
        Ok(o) => o,
        Err(resp) => return CliOutcome::Response(Box::new(resp)),
    };

    let latency_ms = t0.elapsed().as_millis() as u64;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr.chars().take(400).collect();
        let code = output.status.code().unwrap_or(-1);
        let err = format!("claude CLI exit {}: {}", code, snippet.trim());
        return CliOutcome::Response(Box::new(ProviderResponse {
            error: Some(err),
            latency_ms,
            ..Default::default()
        }));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let model_label = resolved.response_label;

    let resp = match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(data) => {
            if data
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let msg = data
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown CLI error");
                ProviderResponse {
                    error: Some(msg.to_string()),
                    latency_ms,
                    ..Default::default()
                }
            } else {
                let usage = data.get("usage").cloned().unwrap_or(json!({}));
                let tokens_in = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let tokens_out = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let cached_in = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let dur = data
                    .get("duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(latency_ms);
                let text = data
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ProviderResponse {
                    text,
                    model: model_label,
                    tokens_in,
                    tokens_out,
                    cached_in,
                    latency_ms: dur,
                    cost_usd: 0.0,
                    error: None,
                    gateway_provenance: None,
                    gateway_attempts: Vec::new(),
                    provider_provenance: Some(ProviderProvenance::cli_tools(
                        "claude_cli",
                        "reported_tokens_estimated_cost",
                    )),
                }
            }
        }
        Err(_) => ProviderResponse {
            text: stdout.trim().to_string(),
            model: model_label,
            tokens_in: 0,
            tokens_out: 0,
            cached_in: 0,
            latency_ms,
            cost_usd: 0.0,
            error: None,
            gateway_provenance: None,
            gateway_attempts: Vec::new(),
            provider_provenance: Some(ProviderProvenance::cli_tools(
                "claude_cli",
                "usage_unavailable",
            )),
        },
    };

    CliOutcome::Response(Box::new(resp))
}

async fn wait_with_timeout_output(
    mut child: Child,
    timeout_duration: Duration,
    timeout_error: String,
    t0: Instant,
) -> Result<Output, ProviderResponse> {
    let stdout_task = tokio::spawn(read_pipe(child.stdout.take()));
    let stderr_task = tokio::spawn(read_pipe(child.stderr.take()));

    let status = match timeout(timeout_duration, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(ProviderResponse {
                error: Some(format!("claude CLI wait: {}", e)),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            });
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(Duration::from_secs(5), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(ProviderResponse {
                error: Some(timeout_error),
                latency_ms: t0.elapsed().as_millis() as u64,
                ..Default::default()
            });
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_pipe<R>(pipe: Option<R>) -> Vec<u8>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf).await;
    }
    buf
}

/// Direct API path — when Claude Code CLI is unavailable or unauthenticated.
pub(crate) async fn api_ask(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            return ProviderResponse {
                error: Some(
                    "ANTHROPIC_API_KEY not set (COUNCIL_CLAUDE_FORCE_API=1 skips CLI). \
                     Unset COUNCIL_CLAUDE_FORCE_API to use `claude -p`, or export ANTHROPIC_API_KEY."
                        .into(),
                ),
                ..Default::default()
            };
        }
    };

    let routing = claude_route::routing_snapshot();
    let model = if model.is_empty() {
        routing.cli_default_model.as_str()
    } else {
        model
    };

    let mut payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 4096,
    });

    if let Some(adaptive) = claude_route::resolve_api_adaptive(model) {
        payload["thinking"] = json!({"type": "adaptive", "display": "omitted"});
        payload["output_config"] = json!({"effort": adaptive.effort});
        payload["max_tokens"] = json!(adaptive.max_tokens);
    } else {
        payload["temperature"] = json!(0.7);
    }

    if !system.is_empty() {
        payload["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"}
        }]);
    }

    let t0 = Instant::now();
    let client = Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
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
                            "JSON parse error (HTTP {}): {e}; body: {preview}",
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
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| body_text.chars().take(200).collect());
                return ProviderResponse {
                    error: Some(format!("Anthropic HTTP {}: {detail}", status.as_u16())),
                    latency_ms,
                    ..Default::default()
                };
            }
            parse_api_response(data, model, latency_ms)
        }
        Err(e) => ProviderResponse {
            error: Some(format!("HTTP error: {}", e)),
            latency_ms,
            ..Default::default()
        },
    }
}

fn parse_api_response(data: Value, model: &str, latency_ms: u64) -> ProviderResponse {
    if let Some(err) = data.get("error") {
        return ProviderResponse {
            error: Some(format!("API error: {}", err)),
            latency_ms,
            ..Default::default()
        };
    }

    let text = data
        .get("content")
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let usage = data.get("usage").cloned().unwrap_or(json!({}));
    let tokens_in = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let tokens_out = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let cached_in = usage
        .get("cache_read_input_tokens")
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
        provider_provenance: Some(ProviderProvenance::api("anthropic_api")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_fallback_is_opt_in() {
        // Default: subscription CLI only — no silent API fallback on auth strings.
        assert!(!api_fallback_enabled());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_cli_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().expect("spawn slow child");
        let pid = child.id().expect("child pid");

        let result = wait_with_timeout_output(
            child,
            Duration::from_millis(25),
            "test timeout".to_string(),
            Instant::now(),
        )
        .await;

        assert!(result.is_err());

        for _ in 0..20 {
            if !process_exists(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        panic!("timed-out child process {pid} survived timeout cleanup");
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
