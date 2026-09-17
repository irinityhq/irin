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

fn load_routing_yaml<T: serde::de::DeserializeOwned + Default>(
    base_dir: &std::path::Path,
    filename: &str,
) -> T {
    let path = base_dir.join(filename);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return T::default(),
    };
    match serde_yaml::from_str::<T>(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("⚠️  {filename} parse error ({e}); using built-in defaults");
            T::default()
        }
    }
}

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
                let provider_label = if route.wire_provider.trim().is_empty() {
                    "default"
                } else {
                    route.wire_provider.as_str()
                };
                eprintln!(
                    "   ↪ hermes_cli: '{}' → {} / {} (operator adapter)",
                    model.trim(),
                    provider_label,
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
    // Resolves the first usable `grok` binary (home install paths before PATH);
    // no version-output fingerprinting, which broke on every upstream republish.
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
/// Council gathered through the native evidence pipeline.
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
    if provider == "grok_hermes" || provider == "hermes_cli" {
        let route = grok_route::resolve_hermes_seat(model).unwrap_or_else(|| {
            grok_route::HermesSeatResolution {
                wire_model: model.trim().to_string(),
                wire_provider: std::env::var("HERMES_SEAT_PROVIDER")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
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
        "gemini_cli" => agent_cli::ask_gemini(prompt, system, model).await,
        "codex_cli" => agent_cli::ask_codex(prompt, system, model).await,
        "agy_cli" => {
            let resolved = agy_route::resolve_agy_model(model);
            agent_cli::ask_agy(prompt, system, &resolved).await
        }
        // Deterministic no-spend fixtures used by engine orchestration tests.
        // `mock-claim-validator` returns a parseable Sheldon report; `mock-gated-seat`
        // embeds the claim string so gate redaction can be observed when applied.
        "mock" => {
            let text = match model {
                "mock-claim-validator" => {
                    r#"[{"claim":"UNIQUE_CONTRADICTED_CLAIM_XYZ_12345","seat":"seat_a","verdict":"CONTRADICTED","evidence_citations":["fixture evidence"],"reasoning":"no-spend fixture","confidence":0.95,"impact":"HIGH"}]"#
                        .to_string()
                }
                "mock-gated-seat" => {
                    "Seat analysis states UNIQUE_CONTRADICTED_CLAIM_XYZ_12345 with certainty."
                        .to_string()
                }
                "mock-slack-token" => concat!("xoxb-", "0000000000FAKEFIXTURE").to_string(),
                // Opposite polarity fixtures so the keyword judge does not
                // early-converge a multi-round characterization run.
                "mock-seat-agree" => "I agree and support this approach.".to_string(),
                "mock-seat-disagree" => "I disagree and reject this approach.".to_string(),
                _ => format!(
                    "Mock response from {} for prompt: {}",
                    model,
                    prompt.chars().take(20).collect::<String>()
                ),
            };
            ProviderResponse {
                text,
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
            }
        }

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
        // Same resolver used for seat spawn (home paths before PATH).
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
        // Env-keyed OpenAI-compat seats already in registry/dispatch (B-25).
        // Keep this list explicit — do not derive from KNOWN_KEYS.
        ("kimi", gw || env_nonempty("MOONSHOT_API_KEY")),
        ("sambanova", gw || env_nonempty("SAMBANOVA_API_KEY")),
        ("cerebras", gw || env_nonempty("CEREBRAS_API_KEY")),
        // Deterministic no-spend fixture for direct (non-governed) characterization.
        // Gateway has no mock adapter — never publish mock as available under gw.
        ("mock", !gw),
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

    // Tests that read or mutate process-global provider state (the grok
    // routing store via set_base_dir, hermes seat availability, or
    // check_providers_with_gateway) serialize here. Assertion-free locking:
    // without it a routing fixture installed by the dispatch matrix is
    // visible to concurrent availability checks.
    static PROVIDER_GLOBAL_STATE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[test]
    fn governed_mode_only_promotes_cli_transports_with_gateway_adapters() {
        let _guard = PROVIDER_GLOBAL_STATE_LOCK.blocking_lock();
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
    fn mock_provider_availability_flips_with_gateway_mode() {
        let _guard = PROVIDER_GLOBAL_STATE_LOCK.blocking_lock();
        let direct: std::collections::HashMap<_, _> =
            check_providers_with_gateway(false).into_iter().collect();
        let governed: std::collections::HashMap<_, _> =
            check_providers_with_gateway(true).into_iter().collect();
        assert_eq!(direct.get("mock"), Some(&true), "direct mode exposes mock");
        assert_eq!(
            governed.get("mock"),
            Some(&false),
            "governed mode hides mock"
        );
    }

    #[test]
    fn kimi_sambanova_cerebras_availability_follows_env_keys() {
        // B-25: cabinet seats on these providers must pass/fail preflight with
        // the matching key — do not derive the list from KNOWN_KEYS.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _state = PROVIDER_GLOBAL_STATE_LOCK.blocking_lock();
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keys = [
            ("kimi", "MOONSHOT_API_KEY"),
            ("sambanova", "SAMBANOVA_API_KEY"),
            ("cerebras", "CEREBRAS_API_KEY"),
        ];
        let previous: Vec<_> = keys
            .iter()
            .map(|(_, env)| (*env, std::env::var_os(env)))
            .collect();
        for (_, env) in &keys {
            unsafe {
                std::env::remove_var(env);
            }
        }
        let without: std::collections::HashMap<_, _> =
            check_providers_with_gateway(false).into_iter().collect();
        for (slug, _) in &keys {
            assert_eq!(
                without.get(slug),
                Some(&false),
                "{slug} must fail closed without its key"
            );
        }
        for (slug, env) in &keys {
            unsafe {
                std::env::set_var(env, "test-key-not-for-live-calls");
            }
            let with: std::collections::HashMap<_, _> =
                check_providers_with_gateway(false).into_iter().collect();
            assert_eq!(
                with.get(slug),
                Some(&true),
                "{slug} must pass preflight with {env} set"
            );
            unsafe {
                std::env::remove_var(env);
            }
        }
        for (env, prev) in previous {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(env, v),
                    None => std::env::remove_var(env),
                }
            }
        }
    }

    #[test]
    fn liveness_provider_check_is_env_only_and_retains_documented_slugs() {
        // Must not depend on host CLI install state. `gateway` may be true when
        // the process env already has GW_API_KEY; host-only CLI seats must stay
        // false because liveness never shells out. Holds the state lock so a
        // concurrent dispatch test cannot flip GW_API_KEY between the two
        // env reads below.
        let _guard = PROVIDER_GLOBAL_STATE_LOCK.blocking_lock();
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

    // Characterization fixtures ahead of consolidating scattered provider
    // policy: these pin today's dispatch decisions so a consolidation PR must
    // preserve the same selected route — or the same refusal — offline. They
    // never execute a real provider CLI, network call, or the Gateway.
    //
    // Every test that mutates provider env (XAI_API_KEY, GW_API_KEY,
    // COUNCIL_HERMES_SEAT*, COUNCIL_GROK_CLI_FALLBACK_API) or the routing
    // store takes PROVIDER_GLOBAL_STATE_LOCK: two lock classes would let one
    // test restore a key into another's unkeyed cell and arm a live call.
    // The fallback/GW key OnceLocks also initialize from the first dispatch
    // call, so serialized dispatch tests see deterministic cached flags.

    fn dispatch_ctx(via_gateway: Option<bool>) -> RequestContext {
        RequestContext {
            via_gateway,
            ..Default::default()
        }
    }

    // Matrix test plus the availability tests above share this lock; see
    // PROVIDER_GLOBAL_STATE_LOCK for why.

    fn save_env(names: &[&'static str]) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
        names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect()
    }

    fn restore_env(saved: Vec<(&'static str, Option<std::ffi::OsString>)>) {
        for (name, prev) in saved {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn arm_fallback_and_remove_api_key() {
        // Arm the fallback escape hatch and remove the API credential, so
        // every refusal below proves the fallback is key-gated, not absent.
        // The OnceLock caches the flag from the first dispatch call, so every
        // dispatch test sets the same value before its first call.
        unsafe {
            std::env::set_var("COUNCIL_GROK_CLI_FALLBACK_API", "1");
            std::env::remove_var("XAI_API_KEY");
        }
    }

    #[tokio::test]
    async fn unknown_provider_refuses_without_substitution() {
        let resp = ask_with_opts_and_context(
            "no-such-transport",
            "prompt",
            "system",
            "model",
            8,
            &dispatch_ctx(None),
        )
        .await;
        assert_eq!(
            resp.error.as_deref(),
            Some("Unknown provider: no-such-transport")
        );
    }

    #[tokio::test]
    async fn canonical_grok_api_missing_key_never_switches_transport() {
        let _guard = PROVIDER_GLOBAL_STATE_LOCK.lock().await;
        let saved = save_env(&["XAI_API_KEY"]);
        arm_fallback_and_remove_api_key();

        // Canonical transports are pure: no key means a keyed refusal on the
        // selected transport — never a silent hop to a CLI transport.
        let resp = ask_with_opts_and_context(
            "grok_api",
            "prompt",
            "system",
            "grok-4.3",
            8,
            &dispatch_ctx(None),
        )
        .await;
        assert_eq!(resp.error.as_deref(), Some("XAI_API_KEY not set"));

        restore_env(saved);
    }

    #[tokio::test]
    async fn governed_dispatch_shields_every_transport_behind_gateway() {
        let _guard = PROVIDER_GLOBAL_STATE_LOCK.lock().await;
        let saved = save_env(&["GW_API_KEY"]);
        unsafe {
            std::env::remove_var("GW_API_KEY");
        }

        // Governed mode is a transport invariant resolved before every
        // native/API/CLI special case: native, CLI seat, alias, and even
        // unknown transports all refuse at the Gateway client, never at a
        // direct provider call.
        for provider in [
            "grok_api",
            "grok_cli",
            "grok",
            "claude_code",
            "codex_cli",
            "no-such-transport",
        ] {
            let resp = ask_with_opts_and_context(
                provider,
                "prompt",
                "system",
                "model",
                8,
                &dispatch_ctx(Some(true)),
            )
            .await;
            assert_eq!(
                resp.error.as_deref(),
                Some("GW_API_KEY not set"),
                "{provider} must route through the Gateway client"
            );
        }

        restore_env(saved);
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write fixture");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");
    }

    fn write_routing_fixture(
        dir: &std::path::Path,
        use_hermes_for_api_only: bool,
        seats_entry_grok_43: bool,
    ) {
        let mut yaml = String::new();
        yaml.push_str("cli_models: {}\n");
        yaml.push_str("api_only_ids:\n  - grok-4.3\n");
        yaml.push_str("api_only_prefixes: []\n");
        yaml.push_str("cli_default_label: grok-cli-default\n");
        yaml.push_str("cli_pinned_label_prefix: grok-cli-\n");
        yaml.push_str(if use_hermes_for_api_only {
            "use_hermes_for_api_only: true\n"
        } else {
            "use_hermes_for_api_only: false\n"
        });
        yaml.push_str("hermes_label_prefix: hermes-cli-\n");
        yaml.push_str("hermes:\n");
        yaml.push_str("  adapter_protocol: script\n");
        yaml.push_str("  default_adapter: scripts/hermes-seat-adapter.sh\n");
        if seats_entry_grok_43 {
            yaml.push_str("hermes_seats:\n  grok-4.3: {}\n");
        } else {
            yaml.push_str("hermes_seats: {}\n");
        }
        std::fs::write(dir.join("grok_routing.yaml"), yaml).expect("write routing fixture");
    }

    #[tokio::test]
    async fn api_only_grok_seat_decision_table_fails_closed_offline() {
        let _guard = PROVIDER_GLOBAL_STATE_LOCK.lock().await;
        let saved = save_env(&[
            "XAI_API_KEY",
            "COUNCIL_GROK_CLI_FALLBACK_API",
            "COUNCIL_HERMES_SEAT",
            "COUNCIL_HERMES_SEAT_BIN",
        ]);
        let cwd = std::env::current_dir().expect("cwd");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let fixture_dir = std::env::temp_dir().join(format!(
            "irin-dispatch-fixture-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");

        // grok-4.3 is API-only in the fixture, so dispatch never spawns the
        // local `grok` CLI. The operator adapter override pins hermes
        // resolution for every cell, so host CLI installs and the repo's own
        // adapter script cannot change the outcome.
        arm_fallback_and_remove_api_key();
        unsafe {
            std::env::set_var(
                "COUNCIL_HERMES_SEAT_BIN",
                fixture_dir.join("no-adapter-here.sh"),
            );
        }

        // Seat disabled by the operator: refusal names the disabling flag.
        unsafe {
            std::env::set_var("COUNCIL_HERMES_SEAT", "0");
        }
        write_routing_fixture(&fixture_dir, true, true);
        grok_route::set_base_dir(&fixture_dir);
        let resp = ask_with_opts_and_context(
            "grok_cli",
            "prompt",
            "system",
            "grok-4.3",
            8,
            &dispatch_ctx(None),
        )
        .await;
        let err = resp.error.expect("seat disabled must refuse");
        assert!(err.contains("COUNCIL_HERMES_SEAT=0"), "got: {err}");
        assert!(err.contains("API-only"), "got: {err}");

        // Seat preferred but api-only seats are not routed to Hermes.
        unsafe {
            std::env::remove_var("COUNCIL_HERMES_SEAT");
        }
        write_routing_fixture(&fixture_dir, false, false);
        grok_route::set_base_dir(&fixture_dir);
        let resp = ask_with_opts_and_context(
            "grok",
            "prompt",
            "system",
            "grok-4.3",
            8,
            &dispatch_ctx(None),
        )
        .await;
        let err = resp.error.expect("missing route must refuse");
        assert!(err.contains("no Hermes route configured"), "got: {err}");

        // Route configured but the adapter binary is unusable.
        write_routing_fixture(&fixture_dir, true, true);
        grok_route::set_base_dir(&fixture_dir);
        let resp = ask_with_opts_and_context(
            "grok_cli",
            "prompt",
            "system",
            "grok-4.3",
            8,
            &dispatch_ctx(None),
        )
        .await;
        let err = resp.error.expect("unusable adapter must refuse");
        assert!(
            err.contains("Hermes seat adapter not available"),
            "got: {err}"
        );
        // The fallback escape hatch is armed but the API key is absent, so the
        // refusal stays a seat refusal instead of becoming an xAI API call.
        assert!(!err.contains("XAI_API_KEY not set"), "got: {err}");

        // Usable stub adapter: both `grok` and `grok_cli` seats execute the
        // operator adapter and return its stdout.
        #[cfg(unix)]
        {
            let stub = fixture_dir.join("hermes-stub.sh");
            write_executable(&stub, "#!/bin/sh\nprintf 'hermes-stub-seat-ok'\n");
            unsafe {
                std::env::set_var("COUNCIL_HERMES_SEAT_BIN", &stub);
            }
            for provider in ["grok", "grok_cli"] {
                let resp = ask_with_opts_and_context(
                    provider,
                    "prompt",
                    "system",
                    "grok-4.3",
                    8,
                    &dispatch_ctx(None),
                )
                .await;
                assert_eq!(resp.error, None, "{provider} stub seat must succeed");
                assert!(
                    resp.text.contains("hermes-stub-seat-ok"),
                    "{provider} must return adapter stdout, got: {}",
                    resp.text
                );
            }
        }

        grok_route::set_base_dir(&cwd);
        std::fs::remove_dir_all(&fixture_dir).expect("remove fixture dir");
        restore_env(saved);
    }

    #[test]
    fn readonly_cli_agent_transport_table() {
        // grok_build/grok_cli/codex_cli are read-only seats; hermes_cli is an
        // operator adapter (can spend), and the rest are full transports.
        for provider in ["grok_build", "grok_cli", "codex_cli"] {
            assert!(is_readonly_cli_agent_provider(provider), "{provider}");
        }
        for provider in [
            "hermes_cli",
            "grok",
            "grok_hermes",
            "claude_code",
            "gemini_cli",
            "agy_cli",
            "grok_api",
            "openrouter",
            "mock",
        ] {
            assert!(!is_readonly_cli_agent_provider(provider), "{provider}");
        }
    }

    #[test]
    fn validator_native_search_table() {
        // Only Grok-family transports keep their own web/X search tools in
        // direct mode; gateway routing is buffered transport and drops them.
        let direct = dispatch_ctx(None);
        for provider in ["grok_build", "grok", "grok_cli", "grok_api"] {
            assert!(
                validator_has_native_search(provider, &direct),
                "{provider} keeps native search direct"
            );
        }
        for provider in ["claude", "codex_cli", "gemini", "openrouter", "mock"] {
            assert!(
                !validator_has_native_search(provider, &direct),
                "{provider} has no native search"
            );
        }
        let governed = dispatch_ctx(Some(true));
        assert!(!validator_has_native_search("grok", &governed));
        assert!(!validator_has_native_search("grok_api", &governed));
    }
}
