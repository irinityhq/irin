//! Provider clients — unified async interface to LLM providers
//!
//! Canonical transport IDs keep API, subscription CLI, and adapter seats
//! distinct. Legacy family aliases remain accepted for saved cabinets.
//! See `grok_routing.yaml` for CLI `-m` mapping. True parallel fan-out via JoinSet.

pub mod agent_cli;
pub mod agy_route;
pub mod claude;
pub mod claude_route;
pub mod deepseek;
pub mod gateway;
pub mod gemini;
pub mod gemini_route;
pub mod gpt;
pub mod grok;
pub mod grok_route;
pub mod hermes_cli;
pub mod ollama;
pub mod openai_compat;
pub mod together;

use crate::engine::context::RequestContext;
use crate::types::ProviderResponse;
use std::sync::OnceLock;
use std::time::Duration;

static NIM_SLUG_WARNED: OnceLock<()> = OnceLock::new();

/// Canonical provider slug. `nim` is a legacy alias for `nvidia` (same NIM endpoint).
pub fn canonical_provider_name(provider: &str) -> String {
    if provider == "nim" {
        if NIM_SLUG_WARNED.set(()).is_ok() {
            eprintln!(
                "⚠️  provider slug 'nim' is deprecated — use 'nvidia' (same NVIDIA NIM endpoint)"
            );
        }
        "nvidia".to_string()
    } else {
        provider.to_string()
    }
}

pub(crate) fn grok_cli_fallback_api() -> bool {
    // Temporary escape hatch only. Default OFF.
    // Since the xAI API is being deprecated, this should be phased out.
    // Set to 1 + have XAI_API_KEY only for short-term migration if CLI is broken.
    *GROK_CLI_FALLBACK_API.get_or_init(|| match std::env::var("COUNCIL_GROK_CLI_FALLBACK_API") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "0" && v != "false"
        }
        Err(_) => false,
    })
}

fn grok_cli_response_usable(resp: &ProviderResponse) -> bool {
    resp.error.is_none() && !resp.text.trim().is_empty()
}

fn api_only_grok_seat_error(model: &str, detail: &str) -> ProviderResponse {
    ProviderResponse {
        model: format!("hermes-cli-{}", model.trim()),
        error: Some(format!(
            "grok_cli: model '{}' is API-only — {detail} (install Hermes adapter or set COUNCIL_GROK_CLI_FALLBACK_API=1 with XAI_API_KEY; will not silently use grok-build)",
            model.trim()
        )),
        ..Default::default()
    }
}

/// `grok_cli` seats: Hermes adapter for API-tier models; Grok Build for local CLI ids only.
async fn dispatch_grok_cli_seat(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let routing = grok_route::routing_snapshot();
    let api_only = grok_route::is_api_only_model(model, &routing);

    if api_only {
        if hermes_cli::prefer_hermes_seat()
            && let Some(route) = grok_route::resolve_hermes_seat(model)
        {
            if hermes_cli::is_hermes_seat_available() {
                eprintln!(
                    "   ↪ hermes_cli: '{}' → {} / {} (operator adapter)",
                    model.trim(),
                    route.wire_provider,
                    route.wire_model
                );
                let resp = hermes_cli::ask_hermes(prompt, system, &route).await;
                if grok_cli_response_usable(&resp) {
                    return resp;
                }
                if grok_cli_fallback_api() && std::env::var("XAI_API_KEY").is_ok() {
                    eprintln!(
                        "   ↪ hermes_cli failed — falling back to xAI API ({})",
                        resp.error.as_deref().unwrap_or("empty response")
                    );
                    return grok::ask(prompt, system, model).await;
                }
                return resp;
            }
            if grok_cli_fallback_api() && std::env::var("XAI_API_KEY").is_ok() {
                eprintln!(
                    "   ↪ hermes_cli: adapter unavailable for '{}' — falling back to xAI API",
                    model.trim()
                );
                return grok::ask(prompt, system, model).await;
            }
            return api_only_grok_seat_error(
                model,
                "Hermes seat adapter not available (set COUNCIL_HERMES_SEAT_BIN or scripts/hermes-seat-adapter.sh)",
            );
        }
        if grok_cli_fallback_api() && std::env::var("XAI_API_KEY").is_ok() {
            eprintln!(
                "   ↪ grok_cli: Hermes disabled for '{}' — falling back to xAI API",
                model.trim()
            );
            return grok::ask(prompt, system, model).await;
        }
        let detail = if hermes_cli::prefer_hermes_seat() {
            "no Hermes route configured"
        } else {
            "COUNCIL_HERMES_SEAT=0"
        };
        return api_only_grok_seat_error(model, detail);
    }

    agent_cli::ask_grok(prompt, system, model).await
}

pub(crate) fn prefer_grok_cli() -> bool {
    *PREFER_GROK_CLI.get_or_init(|| match std::env::var("COUNCIL_PREFER_GROK_CLI") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

pub(crate) fn is_grok_cli_available() -> bool {
    // Prefer home install paths + fingerprint Grok Build CLI output so PATH
    // homonyms (e.g. nvm npm `grok-dev`) cannot mark the seat available/unavailable
    // incorrectly or get spawned for cabinet work.
    agent_cli::is_grok_cli_available()
}

static VIA_GATEWAY: OnceLock<bool> = OnceLock::new();
static SENSITIVITY: OnceLock<String> = OnceLock::new();
static PREFER_GROK_CLI: OnceLock<bool> = OnceLock::new();
static GROK_CLI_FALLBACK_API: OnceLock<bool> = OnceLock::new();
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 1_800;

pub fn init_gateway(enabled: bool, sensitivity: String) {
    let _ = VIA_GATEWAY.set(enabled);
    let _ = SENSITIVITY.set(sensitivity);
}

fn is_via_gateway() -> bool {
    *VIA_GATEWAY.get().unwrap_or(&false)
}

/// Process-wide gateway default — what a session falls back to when the WS
/// start payload omits `via_gateway` (feature contract). Reflects `COUNCIL_VIA_GATEWAY`
/// / `--via-gateway` captured by `init_gateway` at startup.
pub fn default_via_gateway() -> bool {
    is_via_gateway()
}

/// Process-wide sensitivity default (UPPERCASE, e.g. "GREEN") — the fallback
/// when the WS start payload omits `sensitivity`.
pub fn default_sensitivity() -> String {
    sensitivity_level().to_string()
}

/// Resolve a per-request gateway override against the process default.
fn resolve_via_gateway(override_flag: Option<bool>) -> bool {
    override_flag.unwrap_or_else(is_via_gateway)
}

pub fn is_cli_agent_provider(provider: &str) -> bool {
    matches!(
        provider,
        "grok_build"
            | "grok_hermes"
            | "claude_code"
            | "gemini_agy"
            | "gemini_cli"
            | "codex_cli"
            | "grok_cli"
            | "agy_cli"
            | "hermes_cli"
    )
}

/// Whether `ask_streaming_with_context` can stream token deltas for a provider
/// (N01). Only the OpenAI-compatible family speaks SSE here — native clients
/// (grok/claude/gpt/gemini), CLI agents, the gateway, and locals fall back to
/// buffered `ask` (zero chunks is always legal). When a session routes via the
/// gateway, even an SSE-shaped provider must use the buffered gateway path, so
/// gateway routing disables streaming.
pub fn is_streaming_capable(provider: &str, via_gateway: bool) -> bool {
    if should_route_via_gateway(provider, via_gateway) {
        return false;
    }
    matches!(
        provider,
        "nvidia"
            | "nim"
            | "nous"
            | "groq"
            | "fireworks"
            | "openrouter"
            | "mistral"
            | "perplexity"
            | "sambanova"
            | "cerebras"
            | "kimi"
            | "cohere"
            | "lmstudio"
            | "localai"
            | "llamacpp"
    )
}

pub fn is_readonly_cli_agent_provider(provider: &str) -> bool {
    matches!(provider, "grok_build" | "grok_cli" | "codex_cli")
}

fn should_route_via_gateway(_provider: &str, via_gateway: bool) -> bool {
    via_gateway
}

/// Whether Sheldon's selected transport can invoke its own web/X search tools.
/// Gateway routing is buffered provider transport and does not preserve local
/// CLI or xAI search tools, so gathered evidence remains mandatory there.
pub fn validator_has_native_search(provider: &str, ctx: &RequestContext) -> bool {
    let provider = canonical_provider_name(provider);
    if should_route_via_gateway(&provider, resolve_via_gateway(ctx.via_gateway)) {
        return false;
    }
    matches!(
        provider.as_str(),
        "grok_build" | "grok" | "grok_cli" | "grok_api"
    )
}

fn sensitivity_level() -> &'static str {
    SENSITIVITY.get().map(|s| s.as_str()).unwrap_or("GREEN")
}

pub fn request_timeout() -> Duration {
    let secs = std::env::var("COUNCIL_PROVIDER_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

pub const VALID_SENSITIVITY_LEVELS: &[&str] = &["GREEN", "YELLOW", "RED"];

/// Provider dispatch — routes by provider name to the correct client.
///
/// Native providers (grok now = local OAuth CLI by default, claude, gpt, gemini) use dedicated clients.
/// All OpenAI-compatible providers (nvidia/nim, nous, deepseek, groq,
/// together, fireworks, openrouter, etc.) route through openai_compat.
pub async fn ask(provider: &str, prompt: &str, system: &str, model: &str) -> ProviderResponse {
    ask_with_opts(provider, prompt, system, model, 4096).await
}

/// Provider dispatch with explicit RequestContext (Phase 0.5 §6.5).
///
/// Native providers ignore the context. Only the gateway-routed branch reads
/// `ctx.parent_request_id` and emits `X-Parent-Request-Id` so the Gateway
/// ledger can attribute seat cost to its council wrapper (§6.4).
pub async fn ask_with_context(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    ctx: &RequestContext,
) -> ProviderResponse {
    ask_with_opts_and_context(provider, prompt, system, model, 4096, ctx).await
}

/// Streaming dispatch (N01) — forwards visible token deltas via `on_delta`
/// while returning the same `ProviderResponse` shape as `ask_with_context`.
///
/// Only the OpenAI-compatible family streams (see `is_streaming_capable`); every
/// other provider transparently falls back to the buffered `ask_with_context`
/// (so `on_delta` is simply never called — zero chunks is legal). Callers gate
/// on `is_streaming_capable` to decide whether to bother passing a real sink;
/// this fn stays correct either way. Does NOT change the signature of the
/// existing `ask*` family (judge/sheldon/synthesis are untouched).
pub async fn ask_streaming_with_context(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    ctx: &RequestContext,
    on_delta: impl FnMut(&str),
) -> ProviderResponse {
    if is_streaming_capable(provider, resolve_via_gateway(ctx.via_gateway)) {
        return openai_compat::ask_streaming(provider, prompt, system, model, 4096, on_delta).await;
    }
    // Non-streaming provider: buffered call, no deltas forwarded.
    let _ = on_delta;
    ask_with_context(provider, prompt, system, model, ctx).await
}

/// Sheldon claim_validator dispatch — honors `req_ctx` gateway routing. Grok Build
/// and explicit xAI API transports may search natively; Hermes consumes the evidence
/// Council gathered through xmcp and the native evidence pipeline.
pub async fn ask_validator(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    ctx: &RequestContext,
) -> ProviderResponse {
    let provider = canonical_provider_name(provider);
    if should_route_via_gateway(&provider, resolve_via_gateway(ctx.via_gateway)) {
        return ask_with_context(&provider, prompt, system, model, ctx).await;
    }
    match provider.as_str() {
        "grok_build" => agent_cli::ask_grok_with_web_search(prompt, system, model).await,
        "grok_api" => grok::ask_with_web_search(prompt, system, model).await,
        "grok" | "grok_cli" => {
            if prefer_grok_cli() && is_grok_cli_available() {
                let cli_resp = agent_cli::ask_grok_with_web_search(prompt, system, model).await;
                if grok_cli_fallback_api()
                    && !grok_cli_response_usable(&cli_resp)
                    && std::env::var("XAI_API_KEY").is_ok()
                {
                    eprintln!(
                        "   ↪ grok_cli call empty/failed — (temporary) falling back to xAI API ({})",
                        cli_resp.error.as_deref().unwrap_or("empty response")
                    );
                    return grok::ask_with_web_search(prompt, system, model).await;
                }
                cli_resp
            } else {
                grok::ask_with_web_search(prompt, system, model).await // deprecated (API)
            }
        }
        other => ask_with_context(other, prompt, system, model, ctx).await,
    }
}

/// Provider dispatch with configurable max_tokens.
/// Used by convergence judge (512 for NIM) and other tuned callers.
pub async fn ask_with_opts(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    max_tokens: u32,
) -> ProviderResponse {
    ask_with_opts_and_context(
        provider,
        prompt,
        system,
        model,
        max_tokens,
        &RequestContext::default(),
    )
    .await
}

/// Full-fat dispatch — used internally; public callers should reach for
/// `ask`, `ask_with_opts`, or `ask_with_context` instead.
pub async fn ask_with_opts_and_context(
    provider: &str,
    prompt: &str,
    system: &str,
    model: &str,
    max_tokens: u32,
    ctx: &RequestContext,
) -> ProviderResponse {
    let provider = canonical_provider_name(provider);
    let via_gateway = resolve_via_gateway(ctx.via_gateway);

    // Governed mode is a transport invariant, not a preference. Resolve it
    // before any native/API/CLI special case so no provider can silently
    // bypass Gateway while the proceeding is labelled governed.
    if via_gateway {
        let sensitivity = ctx
            .sensitivity
            .as_deref()
            .unwrap_or_else(|| sensitivity_level());
        return gateway::ask(
            prompt,
            system,
            model,
            &provider,
            max_tokens,
            sensitivity,
            ctx,
        )
        .await;
    }

    // Canonical transport IDs are deliberately pure: selecting one transport
    // cannot fall through to another based on model name, key presence, or a
    // preference flag. Legacy aliases below retain their historical behavior.
    if provider == "grok_build" {
        return agent_cli::ask_grok(prompt, system, model).await;
    }
    if provider == "grok_hermes" {
        let route = grok_route::resolve_hermes_seat(model).unwrap_or_else(|| {
            grok_route::HermesSeatResolution {
                wire_model: model.trim().to_string(),
                wire_provider: std::env::var("HERMES_SEAT_PROVIDER")
                    .unwrap_or_else(|_| "xai".into()),
                response_label: format!("hermes-cli-{}", model.trim()),
                cabinet_model: model.trim().to_string(),
            }
        });
        return hermes_cli::ask_hermes(prompt, system, &route).await;
    }

    // `grok` = Hermes / Grok Build CLI first; xAI API only via explicit fallback env.
    // `grok_cli` / `hermes_cli` = seat subprocess transports (see grok_routing.yaml).
    if provider == "grok" {
        let cli_resp = dispatch_grok_cli_seat(prompt, system, model).await;
        if grok_cli_fallback_api()
            && !grok_cli_response_usable(&cli_resp)
            && std::env::var("XAI_API_KEY").is_ok()
        {
            eprintln!(
                "   ↪ grok_cli call empty/failed — falling back to xAI API ({})",
                cli_resp.error.as_deref().unwrap_or("empty response")
            );
            return grok::ask(prompt, system, model).await;
        }
        return cli_resp;
    }

    if provider == "grok_cli" {
        let cli_resp = dispatch_grok_cli_seat(prompt, system, model).await;
        if grok_cli_fallback_api()
            && !grok_cli_response_usable(&cli_resp)
            && std::env::var("XAI_API_KEY").is_ok()
            && grok_route::resolve_cli_model(model).api_id_substituted
        {
            eprintln!(
                "   ↪ grok_cli: cabinet model '{}' is API-only — falling back to xAI API ({})",
                model.trim(),
                cli_resp.error.as_deref().unwrap_or("empty response")
            );
            return grok::ask(prompt, system, model).await;
        }
        return cli_resp;
    }

    if provider == "hermes_cli" {
        let route = grok_route::resolve_hermes_seat(model).unwrap_or_else(|| {
            grok_route::HermesSeatResolution {
                wire_model: model.trim().to_string(),
                wire_provider: std::env::var("HERMES_SEAT_PROVIDER")
                    .unwrap_or_else(|_| "xai".into()),
                response_label: format!("hermes-cli-{}", model.trim()),
                cabinet_model: model.trim().to_string(),
            }
        });
        return hermes_cli::ask_hermes(prompt, system, &route).await;
    }

    match provider.as_str() {
        // Native providers — custom API shapes
        "grok_api" => grok::ask(prompt, system, model).await,
        "claude_api" => claude::api_ask(prompt, system, model).await,
        "claude_code" => claude::ask_code(prompt, system, model).await,
        "openai_api" => gpt::api_ask(prompt, system, model).await,
        "gemini_vertex" => gemini::ask(prompt, system, model).await,
        "gemini_agy" => {
            let resolved = agy_route::resolve_agy_model(model);
            agent_cli::ask_agy(prompt, system, &resolved).await
        }
        "grok" => grok::ask(prompt, system, model).await, // deprecated (API); only reached if no grok_cli available
        "claude" => claude::ask(prompt, system, model).await,
        "gpt" => gpt::ask(prompt, system, model).await,
        "gemini" => {
            // Prefer agy_cli (Antigravity, tied to ultra subs) over legacy Vertex.
            // Vertex is being phased out for most users; agy -p is primary.
            // Set COUNCIL_GEMINI_VERTEX_FALLBACK=1 to allow legacy Vertex path.
            if crate::provider::agent_cli::is_agy_cli_available() {
                let resolved = agy_route::resolve_agy_model(model);
                agent_cli::ask_agy(prompt, system, &resolved).await
            } else if std::env::var_os("COUNCIL_GEMINI_VERTEX_FALLBACK").is_some() {
                gemini::ask(prompt, system, model).await
            } else {
                ProviderResponse {
                    error: Some(
                        "agy CLI not found (primary for gemini); install agy or set COUNCIL_GEMINI_VERTEX_FALLBACK=1 for legacy Vertex".into(),
                    ),
                    ..Default::default()
                }
            }
        }
        "grok_cli" => dispatch_grok_cli_seat(prompt, system, model).await,
        "hermes_cli" => {
            let route = grok_route::resolve_hermes_seat(model).unwrap_or_else(|| {
                grok_route::HermesSeatResolution {
                    wire_model: model.trim().to_string(),
                    wire_provider: std::env::var("HERMES_SEAT_PROVIDER")
                        .unwrap_or_else(|_| "xai".into()),
                    response_label: format!("hermes-cli-{}", model.trim()),
                    cabinet_model: model.trim().to_string(),
                }
            });
            hermes_cli::ask_hermes(prompt, system, &route).await
        }
        "gemini_cli" => agent_cli::ask_gemini(prompt, system, model).await,
        "codex_cli" => agent_cli::ask_codex(prompt, system, model).await,
        "agy_cli" => {
            let resolved = agy_route::resolve_agy_model(model);
            agent_cli::ask_agy(prompt, system, &resolved).await
        }
        "mock" => ProviderResponse {
            text: format!(
                "Mock response from {} for prompt: {}",
                model,
                prompt.chars().take(20).collect::<String>()
            ),
            model: model.to_string(),
            tokens_in: 10,
            tokens_out: 10,
            cached_in: 0,
            latency_ms: 5,
            cost_usd: 0.0,
            error: None,
            gateway_provenance: None,
            gateway_attempts: Vec::new(),
            provider_provenance: Some(crate::types::ProviderProvenance::new(
                "mock", "mock", "none", "none",
            )),
        },

        "deepseek" => deepseek::ask(prompt, system, model, max_tokens).await,
        "together" => together::ask(prompt, system, model, max_tokens).await,

        // OpenAI-compatible providers — all use /v1/chat/completions
        "nvidia" | "nous" | "groq" | "fireworks" | "openrouter" | "mistral" | "perplexity"
        | "sambanova" | "cerebras" | "kimi" | "cohere" => {
            openai_compat::ask(&provider, prompt, system, model, max_tokens).await
        }

        // Local providers (Ollama, LM Studio)
        "ollama" => ollama::ask(prompt, system, model, max_tokens).await,
        "lmstudio" | "localai" | "llamacpp" => {
            openai_compat::ask(&provider, prompt, system, model, max_tokens).await
        }

        _ => ProviderResponse {
            error: Some(format!("Unknown provider: {}", provider)),
            ..Default::default()
        },
    }
}

/// True when env var is present and non-empty after trim.
/// Empty assignments (`NVIDIA_API_KEY=`) are common in gateway.env placeholders and
/// must not count as "configured" — they also used to clobber real keys at runtime load.
pub(crate) fn env_nonempty(name: &str) -> bool {
    std::env::var(name)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Check which providers have valid credentials.
/// In gateway mode, only transports with real Gateway adapters may inherit
/// Gateway availability. Host-only OAuth transports still require their local
/// executable/adapter.
///
/// **Warning:** this path may shell out to optional CLIs (`claude`, `codex`,
/// `gcloud`, `grok`, …). Do **not** call it from liveness (`/api/health`).
/// Use [`check_providers_liveness`] for cheap probes and
/// [`check_providers_with_gateway`] / `/api/discover` for full discovery.
pub fn check_providers() -> Vec<(&'static str, bool)> {
    check_providers_with_gateway(is_via_gateway())
}

/// Cheap, deterministic provider summary for liveness (`GET /api/health`).
///
/// **Never shells out.** Only env-var presence, the explicit gateway flag, and a
/// bounded local TCP probe for Ollama. Exact CLI readiness belongs on
/// `GET /api/discover` (and deliberation-time `check_providers_with_gateway`).
pub fn check_providers_liveness(gw: bool) -> Vec<(&'static str, bool)> {
    let mut out = vec![
        ("gateway", env_nonempty("GW_API_KEY")),
        // Liveness: API-key / gateway inheritance only — no CLI version probes.
        ("grok", gw || env_nonempty("XAI_API_KEY")),
        ("grok_api", gw || env_nonempty("XAI_API_KEY")),
        // Host-only CLI seats are not claimed available from env alone.
        ("grok_build", false),
        ("grok_hermes", false),
        ("claude", gw || env_nonempty("ANTHROPIC_API_KEY")),
        ("claude_api", gw || env_nonempty("ANTHROPIC_API_KEY")),
        ("claude_code", false),
        ("gpt", gw || env_nonempty("OPENAI_API_KEY")),
        ("openai_api", gw || env_nonempty("OPENAI_API_KEY")),
        ("codex_cli", false),
        (
            "gemini",
            gw || (std::env::var_os("COUNCIL_GEMINI_VERTEX_FALLBACK").is_some()
                && (env_nonempty("VERTEX_PROJECT") || env_nonempty("GOOGLE_CLOUD_PROJECT"))),
        ),
        ("gemini_agy", false),
        (
            "gemini_vertex",
            gw || env_nonempty("VERTEX_PROJECT") || env_nonempty("GOOGLE_CLOUD_PROJECT"),
        ),
        ("grok_cli", gw),
        ("gemini_cli", gw),
        ("agy_cli", gw),
        ("hermes_cli", gw),
        ("nvidia", gw || env_nonempty("NVIDIA_API_KEY")),
        ("nous", gw || env_nonempty("NOUS_API_KEY")),
        ("deepseek", gw || env_nonempty("DEEPSEEK_API_KEY")),
        ("groq", gw || env_nonempty("GROQ_API_KEY")),
        ("openrouter", gw || env_nonempty("OPENROUTER_API_KEY")),
        ("mistral", gw || env_nonempty("MISTRAL_API_KEY")),
        ("together", gw || env_nonempty("TOGETHER_API_KEY")),
        ("fireworks", gw || env_nonempty("FIREWORKS_API_KEY")),
        ("perplexity", gw || env_nonempty("PERPLEXITY_API_KEY")),
        ("cohere", gw || env_nonempty("COHERE_API_KEY")),
    ];

    // Cheap local TCP only — no subprocess.
    if std::net::TcpStream::connect_timeout(
        &"127.0.0.1:11434".parse().unwrap(),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
    {
        out.push(("ollama", true));
    }

    out
}

/// `check_providers` with an explicit gateway flag — used by per-session
/// `via_gateway` (feature contract) so seat filtering matches the session's routing,
/// not just the process default.
///
/// May shell out to optional CLIs. Not for `/api/health` — use
/// [`check_providers_liveness`] there.
pub fn check_providers_with_gateway(gw: bool) -> Vec<(&'static str, bool)> {
    let mut out = vec![
        // Gateway (routes through local AI ops layer)
        ("gateway", env_nonempty("GW_API_KEY")),
        // 'grok' = local OAuth CLI primary (API deprecated).
        // Available via CLI or (temp) XAI key or gateway.
        (
            "grok",
            gw || is_grok_cli_available() || env_nonempty("XAI_API_KEY"),
        ),
        ("grok_api", gw || env_nonempty("XAI_API_KEY")),
        ("grok_build", is_grok_cli_available()),
        (
            "grok_hermes",
            crate::provider::hermes_cli::is_hermes_seat_available(),
        ),
        (
            "claude",
            gw || env_nonempty("ANTHROPIC_API_KEY")
                || std::process::Command::new("claude")
                    .arg("--version")
                    .stderr(std::process::Stdio::null())
                    .output()
                    .is_ok(),
        ),
        ("claude_api", gw || env_nonempty("ANTHROPIC_API_KEY")),
        (
            "claude_code",
            gw || crate::provider::claude::is_claude_cli_available(),
        ),
        (
            "gpt",
            gw || env_nonempty("OPENAI_API_KEY")
                || std::process::Command::new("codex")
                    .arg("--version")
                    .stderr(std::process::Stdio::null())
                    .output()
                    .is_ok(),
        ),
        ("openai_api", gw || env_nonempty("OPENAI_API_KEY")),
        (
            "codex_cli",
            gw || crate::provider::agent_cli::is_codex_cli_available(),
        ),
        (
            "gemini",
            gw || crate::provider::agent_cli::is_agy_cli_available()
                || (std::env::var_os("COUNCIL_GEMINI_VERTEX_FALLBACK").is_some()
                    && gemini::has_vertex_project_config()
                    && std::process::Command::new("gcloud")
                        .args(["auth", "print-access-token"])
                        .stderr(std::process::Stdio::null())
                        .output()
                        .is_ok_and(|o| o.status.success())),
        ),
        (
            "gemini_agy",
            crate::provider::agent_cli::is_agy_cli_available(),
        ),
        ("gemini_vertex", gw || gemini::is_vertex_available()),
        // Same resolver used for seat spawn (home paths + version fingerprint).
        ("grok_cli", gw || is_grok_cli_available()),
        (
            "gemini_cli",
            gw || std::process::Command::new("gemini")
                .arg("--version")
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success()),
        ),
        (
            "agy_cli",
            gw || std::process::Command::new("agy")
                .arg("--version")
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success()),
        ),
        (
            "hermes_cli",
            gw || crate::provider::hermes_cli::is_hermes_seat_available(),
        ),
        // Sovereign / OpenAI-compatible providers (non-empty keys only)
        ("nvidia", gw || env_nonempty("NVIDIA_API_KEY")),
        ("nous", gw || env_nonempty("NOUS_API_KEY")),
        ("deepseek", gw || env_nonempty("DEEPSEEK_API_KEY")),
        ("groq", gw || env_nonempty("GROQ_API_KEY")),
        ("openrouter", gw || env_nonempty("OPENROUTER_API_KEY")),
        ("mistral", gw || env_nonempty("MISTRAL_API_KEY")),
        ("together", gw || env_nonempty("TOGETHER_API_KEY")),
        ("fireworks", gw || env_nonempty("FIREWORKS_API_KEY")),
        ("perplexity", gw || env_nonempty("PERPLEXITY_API_KEY")),
        ("cohere", gw || env_nonempty("COHERE_API_KEY")),
    ];

    // Local probes
    if std::net::TcpStream::connect_timeout(
        &"127.0.0.1:11434".parse().unwrap(),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
    {
        out.push(("ollama", true));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_provider_maps_nim_to_nvidia() {
        assert_eq!(canonical_provider_name("nim"), "nvidia");
        assert_eq!(canonical_provider_name("grok"), "grok");
        for provider in [
            "grok_api",
            "grok_build",
            "grok_hermes",
            "claude_code",
            "claude_api",
            "codex_cli",
            "openai_api",
            "gemini_agy",
            "gemini_vertex",
            "gemini_cli",
        ] {
            assert_eq!(canonical_provider_name(provider), provider);
        }
    }

    #[test]
    fn governed_mode_routes_cli_agent_models_through_gateway() {
        assert!(should_route_via_gateway("grok_cli", true));
        assert!(should_route_via_gateway("hermes_cli", true));
        assert!(should_route_via_gateway("codex_cli", true));
        assert!(should_route_via_gateway("gemini_cli", true));
        assert!(should_route_via_gateway("agy_cli", true));
    }

    #[test]
    fn governed_mode_only_promotes_cli_transports_with_gateway_adapters() {
        let direct = check_providers_with_gateway(false)
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let governed = check_providers_with_gateway(true)
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        for provider in ["claude_code", "gemini_cli", "codex_cli"] {
            assert_eq!(governed.get(provider), Some(&true), "{provider}");
        }
        for provider in ["grok_build", "grok_hermes", "gemini_agy"] {
            assert_eq!(governed.get(provider), direct.get(provider), "{provider}");
        }
    }

    #[test]
    fn liveness_provider_check_is_env_only_and_retains_documented_slugs() {
        // Must not depend on host CLI install state. `gateway` may be true when
        // the process env already has GW_API_KEY; host-only CLI seats must stay
        // false because liveness never shells out.
        let rows = check_providers_liveness(false);
        let map: std::collections::HashMap<_, _> = rows.into_iter().collect();
        for required in [
            "gateway",
            "grok",
            "claude",
            "gpt",
            "openai_api",
            "nvidia",
            "openrouter",
        ] {
            assert!(
                map.contains_key(required),
                "missing liveness slug {required}"
            );
        }
        assert_eq!(map.get("gateway"), Some(&env_nonempty("GW_API_KEY")));
        assert_eq!(map.get("grok_build"), Some(&false));
        assert_eq!(map.get("claude_code"), Some(&false));
        assert_eq!(map.get("codex_cli"), Some(&false));
        assert_eq!(map.get("gemini_agy"), Some(&false));

        let governed = check_providers_liveness(true)
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        // Gateway flag promotes transports that inherit gateway adapters.
        assert_eq!(governed.get("claude"), Some(&true));
        assert_eq!(governed.get("gpt"), Some(&true));
        assert_eq!(governed.get("grok_cli"), Some(&true));
        // Host-only seats stay false even under gw (no CLI probe on liveness).
        assert_eq!(governed.get("grok_build"), Some(&false));
        assert_eq!(governed.get("gemini_agy"), Some(&false));
    }

    #[test]
    fn non_cli_providers_follow_gateway_flag() {
        // "grok" is now CLI OAuth by default
        assert!(should_route_via_gateway("grok", true));
        assert!(should_route_via_gateway("openrouter", true));
        assert!(!should_route_via_gateway("grok", false));
    }

    #[test]
    fn grok_implicit_prefers_cli_only_when_not_via_gateway() {
        // "grok" means the local OAuth CLI by default (xAI API deprecated).
        // Must still respect via_gateway for routing.
        assert!(should_route_via_gateway("grok", true));
        assert!(!should_route_via_gateway("grok", false));
        // Explicit CLI transports remain direct only when the proceeding is direct.
        assert!(should_route_via_gateway("grok_cli", true));
        assert!(!should_route_via_gateway("grok_cli", false));
    }

    #[test]
    fn streaming_capable_only_for_openai_compat_family() {
        // OpenAI-compatible family streams when NOT routed via gateway.
        assert!(is_streaming_capable("openrouter", false));
        assert!(is_streaming_capable("nous", false));
        assert!(is_streaming_capable("groq", false));
        assert!(is_streaming_capable("lmstudio", false));
        // Native clients + CLI agents never stream here.
        assert!(!is_streaming_capable("grok", false));
        assert!(!is_streaming_capable("claude", false));
        assert!(!is_streaming_capable("gpt", false));
        assert!(!is_streaming_capable("gemini", false));
        assert!(!is_streaming_capable("grok_cli", false));
        assert!(!is_streaming_capable("mock", false));
        // Gateway routing forces the buffered path even for SSE-shaped providers.
        assert!(!is_streaming_capable("openrouter", true));
        // Governed CLI-labelled models also use the buffered Gateway path.
        assert!(!is_streaming_capable("grok_cli", true));
    }

    #[test]
    fn per_session_override_beats_process_default() {
        // No test calls init_gateway, so the process default is off — an
        // explicit Some(true)/Some(false) must win, None falls back.
        assert!(resolve_via_gateway(Some(true)));
        assert!(!resolve_via_gateway(Some(false)));
        assert!(!resolve_via_gateway(None));
    }
}
