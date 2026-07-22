//! Gateway provider client — routes LLM calls through the local AI Gateway
//!
//! When COUNCIL_VIA_GATEWAY=1 or --via-gateway is set, all provider calls
//! route through Gateway at localhost:18080 for audit, decon, cost tracking,
//! and routing intelligence. The original provider/model is preserved in the
//! request body so Gateway can route correctly.

use crate::engine::context::RequestContext;
use crate::provider::openai_compat::parse_chat_completions;
use crate::types::{GatewayProvenance, ProviderProvenance, ProviderResponse};
use reqwest::Client;
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static CLIENT: OnceLock<Client> = OnceLock::new();
static GW_URL: OnceLock<String> = OnceLock::new();
static GW_KEY: OnceLock<String> = OnceLock::new();
static VERBOSE: OnceLock<bool> = OnceLock::new();

fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        Client::builder()
            .pool_max_idle_per_host(10)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client")
    })
}

fn gateway_url() -> &'static str {
    GW_URL
        .get_or_init(|| {
            std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:18080".into())
        })
        .as_str()
}

pub fn init(key: String, verbose: bool) {
    let _ = GW_KEY.set(key);
    let _ = VERBOSE.set(verbose);
}

fn gateway_key() -> Result<&'static str, String> {
    // Lazy env fallback (feature contract): per-session `via_gateway` can route through
    // the gateway in a `--serve` process that never ran the CLI gateway init,
    // so read GW_API_KEY from the environment on first use. `init()` still
    // wins when it runs first (CLI --via-gateway path reads the same env var).
    let key = GW_KEY.get_or_init(|| std::env::var("GW_API_KEY").unwrap_or_default());
    if key.is_empty() {
        Err("GW_API_KEY not set".into())
    } else {
        Ok(key.as_str())
    }
}

fn is_verbose() -> bool {
    *VERBOSE.get().unwrap_or(&false)
}

/// Reachability check for the unauthenticated Gateway health surface.
async fn reachability_check() -> Result<(), String> {
    let url = format!("{}/health", gateway_url());
    let resp = client()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("Gateway unreachable at {}: {}", url, e))?;

    if !resp.status().is_success() {
        return Err(format!("Gateway health check returned {}", resp.status()));
    }

    Ok(())
}

/// Fetch the authenticated Gateway model catalog without invoking a provider.
async fn model_catalog(key: &str) -> Result<ModelCatalog, String> {
    let url = format!("{}/v1/models", gateway_url());
    let response = client()
        .get(&url)
        .header("Authorization", format!("Bearer {}", key))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("Gateway model preflight failed: {e}"))?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("GW_API_KEY is invalid (401 Unauthorized)".into());
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err("GW_API_KEY rejected (403 Forbidden)".into());
    }
    if status.is_server_error() {
        return Err(format!("Gateway returned {status} during model preflight"));
    }
    if !status.is_success() {
        return Err(format!("Gateway model preflight returned {status}"));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("Gateway model catalog was invalid JSON: {e}"))?;
    parse_model_catalog(&body)
}

#[derive(Debug)]
struct ModelCatalog {
    registered: std::collections::HashSet<String>,
    ready: std::collections::HashSet<String>,
    transports: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

fn parse_model_catalog(body: &Value) -> Result<ModelCatalog, String> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gateway model catalog omitted data".to_string())?;
    let mut registered = std::collections::HashSet::new();
    let mut ready = std::collections::HashSet::new();
    let mut transports = std::collections::HashMap::new();
    for row in data {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "Gateway model catalog contained an invalid id".to_string())?;
        let is_ready = row
            .get("ready")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("Gateway model catalog omitted readiness for {id}"))?;
        registered.insert(id.to_string());
        if is_ready {
            ready.insert(id.to_string());
        }
        let row_transports = row
            .get("transports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|transport| !transport.is_empty())
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();
        transports.insert(id.to_string(), row_transports);
    }
    if registered.is_empty() {
        return Err("Gateway model catalog was empty".into());
    }
    Ok(ModelCatalog {
        registered,
        ready,
        transports,
    })
}

/// One exact Council dispatch identity. `transport` names the concrete caller
/// adapter (`grok_api`, `grok_build`, `claude_code`, etc.); `model` is the wire
/// model requested through that adapter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransportModel {
    pub transport: String,
    pub model: String,
}

impl TransportModel {
    pub fn new(transport: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            transport: transport.into(),
            model: model.into(),
        }
    }
}

/// Health + authentication check used by the CLI startup path.
pub async fn health_check(key: &str) -> Result<(), String> {
    reachability_check().await?;
    model_catalog(key).await.map(|_| ())
}

/// Fail-closed readiness check for a per-session governed War Room start.
/// The key stays process-local; callers receive only a bounded error string.
pub async fn preflight(required_models: &[String]) -> Result<(), String> {
    preflight_with_alternatives(required_models, &[]).await
}

/// Fail closed for hard requirements while allowing utility cascades to use
/// any registered, ready candidate. This mirrors Council execution: every
/// seat/chair is mandatory, but judge/frame/validator cascades are alternatives.
pub async fn preflight_with_alternatives(
    required_models: &[String],
    alternative_groups: &[Vec<String>],
) -> Result<(), String> {
    let key = gateway_key()?;
    reachability_check().await?;
    let catalog = model_catalog(key).await?;
    validate_catalog(&catalog, required_models, alternative_groups)
}

/// Exact-transport governed preflight. Gateway advertises the transport IDs it
/// can genuinely honor for each model; absence is a hard failure before any
/// Council seat spends.
pub async fn preflight_pairs(required: &[TransportModel]) -> Result<(), String> {
    preflight_pairs_with_alternatives(required, &[]).await
}

pub async fn preflight_pairs_with_alternatives(
    required: &[TransportModel],
    alternative_groups: &[Vec<TransportModel>],
) -> Result<(), String> {
    let key = gateway_key()?;
    reachability_check().await?;
    let catalog = model_catalog(key).await?;
    validate_catalog_pairs(&catalog, required, alternative_groups)
}

fn pair_ready(catalog: &ModelCatalog, pair: &TransportModel) -> bool {
    catalog.ready.contains(&pair.model)
        && catalog
            .transports
            .get(&pair.model)
            .is_some_and(|transports| transports.contains(&pair.transport))
}

fn validate_catalog_pairs(
    catalog: &ModelCatalog,
    required: &[TransportModel],
    alternative_groups: &[Vec<TransportModel>],
) -> Result<(), String> {
    let required_models = required
        .iter()
        .map(|pair| pair.model.clone())
        .collect::<Vec<_>>();
    validate_catalog(catalog, &required_models, &[])?;

    let mut unavailable = required
        .iter()
        .filter(|pair| !pair_ready(catalog, pair))
        .map(|pair| format!("{}/{}", pair.transport, pair.model))
        .collect::<Vec<_>>();
    unavailable.sort();
    unavailable.dedup();
    if !unavailable.is_empty() {
        return Err(format!(
            "Gateway cannot honor required transport/model pair(s): {}",
            unavailable.join(", ")
        ));
    }

    for group in alternative_groups {
        if group.iter().any(|pair| pair_ready(catalog, pair)) {
            continue;
        }
        let mut candidates = group
            .iter()
            .map(|pair| format!("{}/{}", pair.transport, pair.model))
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.dedup();
        return Err(format!(
            "Gateway has no ready transport/model candidate for utility cascade: {}",
            candidates.join(", ")
        ));
    }
    Ok(())
}

fn validate_catalog(
    catalog: &ModelCatalog,
    required_models: &[String],
    alternative_groups: &[Vec<String>],
) -> Result<(), String> {
    let mut missing = required_models
        .iter()
        .filter(|model| !catalog.registered.contains(model.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!(
            "Gateway does not register required model(s): {}",
            missing.join(", ")
        ));
    }
    let mut unready = required_models
        .iter()
        .filter(|model| !catalog.ready.contains(model.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unready.sort();
    unready.dedup();
    if !unready.is_empty() {
        return Err(format!(
            "Gateway provider readiness failed for model(s): {}",
            unready.join(", ")
        ));
    }
    for group in alternative_groups {
        if group
            .iter()
            .any(|model| catalog.ready.contains(model.as_str()))
        {
            continue;
        }
        let mut candidates = group.clone();
        candidates.sort();
        candidates.dedup();
        return Err(format!(
            "Gateway has no ready candidate for utility cascade: {}",
            candidates.join(", ")
        ));
    }
    Ok(())
}

pub async fn ask(
    prompt: &str,
    system: &str,
    model: &str,
    original_provider: &str,
    max_tokens: u32,
    sensitivity: &str,
    ctx: &RequestContext,
) -> ProviderResponse {
    let key = match gateway_key() {
        Ok(k) => k,
        Err(e) => {
            return ProviderResponse {
                error: Some(e),
                ..Default::default()
            };
        }
    };

    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": prompt}));

    let payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.7,
    });

    let sovereign_mode = std::env::var("COUNCIL_SOVEREIGN_MODE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let council_request_id = uuid::Uuid::new_v4().to_string();

    let parent_request_id = ctx.parent_request_id.as_deref().unwrap_or("");

    let t0 = Instant::now();
    let resp = send_request(
        key,
        &payload,
        original_provider,
        sensitivity,
        sovereign_mode,
        &council_request_id,
        parent_request_id,
    )
    .await;
    let latency_ms = t0.elapsed().as_millis() as u64;

    match resp {
        Ok(r) => {
            let status = r.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let first_provenance = extract_provenance(r.headers());
                let retry_after = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(15);

                // Jitter: add 0-2s to avoid thundering herd
                let jitter = (t0.elapsed().subsec_nanos() % 2000) as u64;
                tokio::time::sleep(Duration::from_millis(retry_after * 1000 + jitter)).await;

                let retry_resp = send_request(
                    key,
                    &payload,
                    original_provider,
                    sensitivity,
                    sovereign_mode,
                    &council_request_id,
                    parent_request_id,
                )
                .await;
                let latency_ms = t0.elapsed().as_millis() as u64;
                return match retry_resp {
                    Ok(r2) => {
                        let response = handle_gateway_response(r2, model, latency_ms).await;
                        prepend_gateway_attempt(response, first_provenance)
                    }
                    Err(e) => gateway_error_response(
                        format!("Gateway retry failed: {}", e),
                        latency_ms,
                        first_provenance,
                    ),
                };
            }

            handle_gateway_response(r, model, latency_ms).await
        }
        Err(e) => ProviderResponse {
            error: Some(format!("Gateway unreachable: {}", e)),
            latency_ms,
            ..Default::default()
        },
    }
}

async fn handle_gateway_response(
    response: reqwest::Response,
    model: &str,
    latency_ms: u64,
) -> ProviderResponse {
    let status = response.status();
    let provenance = extract_provenance(response.headers());

    if status == reqwest::StatusCode::FORBIDDEN {
        return gateway_error_response(
            "ERR_GUARD_BLOCKED: Gateway decon blocked this request".into(),
            latency_ms,
            provenance,
        );
    }
    if status.is_server_error() {
        return gateway_error_response(format!("Gateway {}", status), latency_ms, provenance);
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let snippet = if body.len() > 200 {
            format!("{}...", &body[..200])
        } else {
            body
        };
        return gateway_error_response(
            format!("Gateway {} — {}", status, snippet),
            latency_ms,
            provenance,
        );
    }

    parse_gateway_response(response, model, latency_ms).await
}

fn provenance_attempts(provenance: Option<&GatewayProvenance>) -> Vec<GatewayProvenance> {
    provenance
        .filter(|p| !p.gateway_request_id.is_empty())
        .cloned()
        .into_iter()
        .collect()
}

fn prepend_gateway_attempt(
    mut response: ProviderResponse,
    provenance: Option<GatewayProvenance>,
) -> ProviderResponse {
    let Some(provenance) = provenance.filter(|p| !p.gateway_request_id.is_empty()) else {
        return response;
    };
    if !response
        .gateway_attempts
        .iter()
        .any(|p| p.gateway_request_id == provenance.gateway_request_id)
    {
        response.gateway_attempts.insert(0, provenance);
    }
    response
}

fn gateway_error_response(
    error: String,
    latency_ms: u64,
    gateway_provenance: Option<GatewayProvenance>,
) -> ProviderResponse {
    let gateway_attempts = provenance_attempts(gateway_provenance.as_ref());
    ProviderResponse {
        error: Some(error),
        latency_ms,
        gateway_provenance,
        gateway_attempts,
        provider_provenance: Some(ProviderProvenance::gateway()),
        ..Default::default()
    }
}

async fn send_request(
    key: &str,
    payload: &Value,
    original_provider: &str,
    sensitivity: &str,
    sovereign_mode: bool,
    council_request_id: &str,
    parent_request_id: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("{}/v1/chat/completions", gateway_url());
    let mut req = client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", key))
        .header("Content-Type", "application/json")
        .header("X-Council-Depth", "1")
        .header("X-Council-Transport-ID", original_provider)
        .header("X-Council-Original-Provider", original_provider)
        .header("X-Sensitivity-Level", sensitivity)
        .header("X-Council-Request-ID", council_request_id)
        .timeout(super::request_timeout());

    if sovereign_mode {
        req = req.header("X-Sovereign-Mode", "true");
    }

    // Phase 0.5 §6.5 (P0 #5): X-Parent-Request-Id threads the council session
    // through to seat calls so the Gateway ledger can attribute seat cost to
    // its wrapper. Only emitted when present (non-empty) — CLI/warroom
    // callers pass an empty string and the header is skipped, preserving the
    // existing single-call wire shape.
    if !parent_request_id.is_empty() {
        req = req.header("X-Parent-Request-Id", parent_request_id);
    }

    req.json(payload).send().await
}

async fn parse_gateway_response(
    resp: reqwest::Response,
    original_model: &str,
    latency_ms: u64,
) -> ProviderResponse {
    let provenance = extract_provenance(resp.headers());
    let gateway_attempts = provenance_attempts(provenance.as_ref());
    if let Some(ref p) = provenance
        && is_verbose()
    {
        eprintln!(
            "  🔗 Gateway: {} via {} (req: {}{})",
            p.routed_model,
            p.routed_provider,
            &p.gateway_request_id[..8.min(p.gateway_request_id.len())],
            if p.fallback_used { " [FALLBACK]" } else { "" }
        );
    }

    match resp.json::<Value>().await {
        Ok(data) => {
            let mut pr = parse_chat_completions(data, original_model, latency_ms);
            pr.gateway_provenance = provenance;
            pr.gateway_attempts = gateway_attempts;
            pr.provider_provenance = Some(ProviderProvenance::gateway());
            pr
        }
        Err(e) => ProviderResponse {
            error: Some(format!("Gateway response parse error: {}", e)),
            latency_ms,
            gateway_provenance: provenance,
            gateway_attempts,
            provider_provenance: Some(ProviderProvenance::gateway()),
            ..Default::default()
        },
    }
}

fn extract_provenance(headers: &reqwest::header::HeaderMap) -> Option<GatewayProvenance> {
    let routed_model = headers
        .get("x-routed-model")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let routed_provider = headers
        .get("x-routed-provider")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let fallback_used = headers
        .get("x-routed-fallback")
        .and_then(|v| v.to_str().ok())
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);

    // X-Request-ID is part of the upstream provider transport and providers
    // are free to replace it. Only the distinct Gateway-owned response header
    // can be correlated with the local Gateway ledger.
    let gateway_request_id = headers
        .get("x-gateway-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if routed_model.is_empty() && gateway_request_id.is_empty() {
        return None;
    }

    Some(GatewayProvenance {
        routed_model,
        routed_provider,
        fallback_used,
        gateway_request_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_catalog_requires_nonempty_openai_shape() {
        let models = parse_model_catalog(&json!({
            "object": "list",
            "data": [
                {"id": "grok-4.3", "object": "model", "ready": true},
                {"id": "claude-opus-4-8", "object": "model", "ready": false}
            ]
        }))
        .expect("valid catalog");
        assert!(models.registered.contains("grok-4.3"));
        assert!(models.registered.contains("claude-opus-4-8"));
        assert!(models.ready.contains("grok-4.3"));
        assert!(!models.ready.contains("claude-opus-4-8"));
        assert!(parse_model_catalog(&json!({"data": []})).is_err());
        assert!(parse_model_catalog(&json!({"models": []})).is_err());
        assert!(parse_model_catalog(&json!({"data": [{"id": "missing-ready"}]})).is_err());
    }

    #[test]
    fn utility_cascade_accepts_any_ready_candidate() {
        let catalog = parse_model_catalog(&json!({
            "data": [
                {"id": "grok-judge", "ready": false},
                {"id": "nvidia-judge", "ready": true},
                {"id": "hard-seat", "ready": true}
            ]
        }))
        .unwrap();
        let hard = vec!["hard-seat".to_string()];
        let cascade = vec![vec!["grok-judge".to_string(), "nvidia-judge".to_string()]];
        assert!(validate_catalog(&catalog, &hard, &cascade).is_ok());

        let unavailable = vec![vec!["grok-judge".to_string()]];
        assert!(validate_catalog(&catalog, &hard, &unavailable).is_err());
    }

    #[test]
    fn exact_transport_preflight_rejects_model_only_or_wrong_transport() {
        let catalog = parse_model_catalog(&json!({
            "data": [
                {
                    "id": "grok-4.3",
                    "ready": true,
                    "transports": ["grok_api", "grok", "grok_cli"]
                },
                {
                    "id": "claude-opus-4-8",
                    "ready": true,
                    "transports": ["claude_code", "claude"]
                }
            ]
        }))
        .unwrap();

        let supported = vec![TransportModel::new("grok_api", "grok-4.3")];
        assert!(validate_catalog_pairs(&catalog, &supported, &[]).is_ok());

        let unsupported = vec![TransportModel::new("grok_build", "grok-4.3")];
        let error = validate_catalog_pairs(&catalog, &unsupported, &[]).unwrap_err();
        assert!(error.contains("grok_build/grok-4.3"));

        let old_gateway_catalog = parse_model_catalog(&json!({
            "data": [{"id": "grok-4.3", "ready": true}]
        }))
        .unwrap();
        assert!(validate_catalog_pairs(&old_gateway_catalog, &supported, &[]).is_err());
    }

    #[test]
    fn exact_transport_utility_group_accepts_one_ready_pair() {
        let catalog = parse_model_catalog(&json!({
            "data": [
                {"id": "judge-a", "ready": true, "transports": ["nvidia"]},
                {"id": "judge-b", "ready": false, "transports": ["grok_api"]}
            ]
        }))
        .unwrap();
        let groups = vec![vec![
            TransportModel::new("grok_api", "judge-b"),
            TransportModel::new("nvidia", "judge-a"),
        ]];
        assert!(validate_catalog_pairs(&catalog, &[], &groups).is_ok());
    }

    #[test]
    fn provenance_prefers_gateway_owned_id_over_upstream_request_id() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-routed-model", "grok-4.3".parse().unwrap());
        headers.insert("x-routed-provider", "xai".parse().unwrap());
        headers.insert("x-request-id", "upstream-provider-id".parse().unwrap());
        headers.insert("x-gateway-request-id", "gateway-ledger-id".parse().unwrap());

        let provenance = extract_provenance(&headers).expect("gateway provenance");
        assert_eq!(provenance.gateway_request_id, "gateway-ledger-id");
        assert_eq!(provenance.routed_model, "grok-4.3");
        assert_eq!(provenance.routed_provider, "xai");
    }

    #[test]
    fn provenance_never_conflates_upstream_request_id() {
        let mut routed = reqwest::header::HeaderMap::new();
        routed.insert("x-routed-model", "gpt-5.6-sol".parse().unwrap());
        routed.insert("x-request-id", "upstream-provider-id".parse().unwrap());

        let provenance = extract_provenance(&routed).expect("routing provenance remains useful");
        assert!(provenance.gateway_request_id.is_empty());

        let mut upstream_only = reqwest::header::HeaderMap::new();
        upstream_only.insert("x-request-id", "upstream-provider-id".parse().unwrap());
        assert!(extract_provenance(&upstream_only).is_none());
    }

    #[test]
    fn gateway_error_keeps_authoritative_request_id_for_failed_cascade_attempt() {
        let response = gateway_error_response(
            "Gateway 502 Bad Gateway".into(),
            42,
            Some(GatewayProvenance {
                routed_model: "judge-model".into(),
                routed_provider: "xai".into(),
                fallback_used: false,
                gateway_request_id: "gw-failed-ledger-id".into(),
            }),
        );

        assert_eq!(response.latency_ms, 42);
        assert_eq!(
            response
                .gateway_provenance
                .as_ref()
                .expect("failed Gateway response provenance")
                .gateway_request_id,
            "gw-failed-ledger-id"
        );
        assert_eq!(response.gateway_attempts.len(), 1);
        assert_eq!(
            response.gateway_attempts[0].gateway_request_id,
            "gw-failed-ledger-id"
        );
        assert_eq!(
            response
                .provider_provenance
                .expect("Gateway transport provenance")
                .runner,
            "gateway"
        );
    }

    #[test]
    fn rate_limit_retry_preserves_both_authoritative_request_ids() {
        let retry_provenance = GatewayProvenance {
            routed_model: "judge-model".into(),
            routed_provider: "xai".into(),
            fallback_used: false,
            gateway_request_id: "gw-retry".into(),
        };
        let response = ProviderResponse {
            gateway_provenance: Some(retry_provenance.clone()),
            gateway_attempts: vec![retry_provenance],
            ..Default::default()
        };

        let response = prepend_gateway_attempt(
            response,
            Some(GatewayProvenance {
                routed_model: "judge-model".into(),
                routed_provider: "xai".into(),
                fallback_used: false,
                gateway_request_id: "gw-rate-limited".into(),
            }),
        );

        assert_eq!(response.gateway_attempts.len(), 2);
        assert_eq!(
            response.gateway_attempts[0].gateway_request_id,
            "gw-rate-limited"
        );
        assert_eq!(response.gateway_attempts[1].gateway_request_id, "gw-retry");
        assert_eq!(
            response
                .gateway_provenance
                .expect("final response provenance")
                .gateway_request_id,
            "gw-retry"
        );
    }
}
