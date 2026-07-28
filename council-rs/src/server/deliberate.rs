// HTTP POST /api/deliberate handler + error classification (moved from server.rs).

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::AppState;
use crate::engine::context::RequestContext;
use crate::engine::deliberate as engine_deliberate;
use crate::types::{SessionOrigin, SynthesisMode};

const STATUS_CLIENT_CLOSED: u16 = 499;

pub(super) fn handler_timeout() -> Duration {
    std::env::var("COUNCIL_HANDLER_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(crate::provider::request_timeout)
}

#[derive(Deserialize)]
pub(super) struct DeliberateRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    max_tokens: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    temperature: Option<f64>,
    #[serde(default)]
    council_auto_escalate: Option<bool>,
    #[serde(default)]
    worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
    /// feature contract engine knobs (mode, tier, budget_max_usd, validate,
    /// validate_gate, blind, cabinet_name) — captured raw so
    /// `parse_deliberate_knobs` can share the WS field parsers and return the
    /// WS-style parse errors as 4xx. Unrecognized keys are ignored, matching
    /// the WS payload posture.
    #[serde(flatten)]
    knobs: serde_json::Map<String, serde_json::Value>,
}

pub(super) fn openai_error(
    status: StatusCode,
    err_type: &str,
    code: &str,
    message: &str,
) -> Response {
    let body = serde_json::json!({
        "error": { "type": err_type, "code": code, "message": message }
    });
    (status, axum::Json(body)).into_response()
}

#[derive(Debug)]
pub(super) enum HandlerError {
    Cancelled,
    QuorumFailed(anyhow::Error),
    Unavailable(anyhow::Error),
    Internal(anyhow::Error),
}

pub(super) fn classify_engine_error(e: anyhow::Error) -> HandlerError {
    let msg = format!("{:#}", e).to_lowercase();
    if msg.contains("cancelled") {
        HandlerError::Cancelled
    } else if msg.contains("quorum") || msg.contains("all seats failed") {
        HandlerError::QuorumFailed(e)
    } else if msg.contains("provider unavailable") || msg.contains("connection refused") {
        HandlerError::Unavailable(e)
    } else {
        HandlerError::Internal(e)
    }
}

/// Drop guard — fires `CancellationToken::cancel()` when the handler future
/// is dropped (client disconnect, response stream cancel, etc.).
pub(super) struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// `POST /api/deliberate` — Phase 0.5 council endpoint.
///
/// Body shape mirrors `/v1/chat/completions`. The Gateway adapter (see spec
/// §5.2) is responsible for wrapping the upstream user messages in the
/// injection-isolation envelope before forwarding here; this handler treats
/// the body verbatim.
///
/// feature contract: the body also accepts optional engine knobs — `mode`, `tier`,
/// `budget_max_usd`, `validate`, `validate_gate`, `blind`, `cabinet_name` —
/// with the same value rules as the WS start payload (shared parsers).
/// Invalid values 4xx in this handler's openai_error shape.
pub(super) async fn deliberate_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<DeliberateRequest>,
) -> Response {
    // feature contract: engine-knob parity with the WS start payload — shared parsers,
    // Strict mode (invalid values 4xx instead of the WS silent coercion).
    let knobs =
        match super::knobs::parse_deliberate_knobs(&serde_json::Value::Object(req.knobs.clone())) {
            Ok(k) => k,
            Err(e) => {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "invalid_parameter",
                    &e,
                );
            }
        };

    let (base_model, requested_hash) = match req.model.split_once('@') {
        Some((m, h)) => (m, Some(h)),
        None => (req.model.as_str(), None),
    };

    if base_model == "council-audit" {
        let text = req
            .messages
            .iter()
            .filter_map(|m| {
                if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                    m.get("content").and_then(|c| c.as_str())
                } else {
                    None
                }
            })
            .next_back()
            .unwrap_or("");

        let id = text.trim();
        let id = id.strip_prefix("session_id:").unwrap_or(id).trim();
        let id = id.strip_prefix("trace_id:").unwrap_or(id).trim();

        match crate::precedent::load_session(id) {
            Some(session) => {
                let body = serde_json::json!({
                    "id":      format!("chatcmpl-{}", session.session_id),
                    "object":  "chat.completion",
                    "created": session.timestamp.timestamp(),
                    "model":   req.model,
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": serde_json::to_string_pretty(&session).unwrap_or_default() },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens":     0,
                        "completion_tokens": 0,
                        "total_tokens":      0
                    }
                });
                return axum::Json(body).into_response();
            }
            None => {
                return openai_error(
                    StatusCode::NOT_FOUND,
                    "invalid_request_error",
                    "session_not_found",
                    &format!("Session not found: {}", id),
                );
            }
        }
    }

    // feature contract: body cabinet_name (WS rule: existence checked at load) overrides
    // the model→cabinet mapping; otherwise the Phase 0.5 mapping holds.
    let (cabinet_name, cabinet_from_body) = match knobs.cabinet_name.as_deref() {
        Some(name) => (name.to_string(), true),
        None => match base_model {
            "council-triage" => ("triage".to_string(), false),
            "council-warroom" => ("warroom".to_string(), false),
            _ => {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "unknown_council_model",
                    &format!("Unknown council model: {}", base_model),
                );
            }
        },
    };

    // resolve_cabinet_owned (feature contract): registry hit clones; a miss falls
    // back to <base_dir>/cabinets/<name>.yaml so a cabinet saved after startup
    // (named in the request body) is launchable and hash-pinnable here exactly
    // as the engine resolves it. Built-in triage/warroom always hit the
    // registry, so a miss on those is a genuine server-side load failure.
    let cabinet = match state.config.resolve_cabinet_owned(&cabinet_name) {
        Ok(c) => c,
        // A cabinet the client named is a client error; a failure to load a
        // model-derived built-in (triage/warroom) is a server error.
        Err(e) if cabinet_from_body => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unknown_cabinet",
                &format!("{e:#}"),
            );
        }
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "cabinet_load_error",
                &format!("Failed to load cabinet: {e}"),
            );
        }
    };

    if let Some(h) = requested_hash
        && h != cabinet.hash
    {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "model_hash_mismatch",
            &format!(
                "Requested model hash {} does not match current cabinet configuration.",
                h
            ),
        );
    }

    // Sticky-pin: if unpinned, pin it for the session
    let model_in = if requested_hash.is_some() {
        req.model.clone()
    } else {
        format!("{}@{}", base_model, cabinet.hash)
    };

    // §6.5: X-Parent-Request-Id, when present, is the gateway's wrapper
    // request id; threaded onto every seat call so the ledger can fold seat
    // cost into the parent row.
    let parent_request_id = headers
        .get("X-Parent-Request-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let topic = req
        .messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|r| r.as_str())?;
            let content = m.get("content").and_then(|c| c.as_str())?;
            Some(format!("[{}]: {}", role, content))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let context = String::new();

    let cancel = CancellationToken::new();
    let _drop_guard = CancelOnDrop(cancel.clone());

    let req_ctx = RequestContext {
        parent_request_id,
        council_session_id: None, // engine fills this in after mint
        depth: 0,
        council_auto_escalate: req.council_auto_escalate.unwrap_or(false),
        // /api/deliberate keeps process-wide gateway routing (feature contract is a WS
        // surface) — None falls back to COUNCIL_VIA_GATEWAY.
        via_gateway: None,
        sensitivity: None,
    };

    // model_in already assigned sticky hash above
    let cancel_for_engine = cancel.clone();

    // Cost-exhaustion guard (audit #6): cap concurrent deliberations. Acquired
    // here — after cheap validation, so malformed requests never consume a slot,
    // and immediately before the expensive engine fan-out. Non-blocking: a full
    // pool fails fast with 429 instead of queueing (queued callers would pile up
    // holding connections + memory). The permit is held for the rest of the
    // handler and released on drop when it returns.
    let _deliberation_permit = match state.deliberate_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            // H1 saturation signal (matches the file's eprintln! warn convention):
            // a sustained stream of these is the cost-exhaustion guard doing its job
            // OR the cap being too low for legitimate load — operators tune via
            // COUNCIL_MAX_CONCURRENT_DELIBERATIONS. /api/health surfaces the live
            // permit count.
            eprintln!("⚠️  deliberate_at_capacity: 429 (audit #6 concurrency cap reached)");
            return openai_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "council_at_capacity",
                "Council is at capacity for concurrent deliberations; retry shortly",
            );
        }
    };

    // `tokio::select!` is not itself a Future — wrap it in an async block so
    // we can apply `tokio::time::timeout`.
    let outcome = tokio::time::timeout(handler_timeout(), async {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(HandlerError::Cancelled),
            r = engine_deliberate::run_with_cancel(
                &state.config,
                &cabinet_name,
                &topic,
                &context,
                // feature contract: knobs parsed from the request body; defaults match
                // the previous hardcoded literals (TearDown, blind=false,
                // budget $1.00, tier "best", validate off).
                knobs.mode,
                /* blind         */ knobs.blind,
                /* frame_check   */ true,
                /* verbose       */ false,
                /* budget_max    */ knobs.budget_max_usd.or(Some(1.0)),
                /* tier          */ &knobs.tier,
                /* validate      */ knobs.validate,
                /* validate_prov */ "grok_cli",
                /* validate_gate */ knobs.validate_gate,
                SessionOrigin::Api,
                req_ctx,
                req.worker_provenance,
                Some(cancel_for_engine.clone()),
            ) => r.map_err(classify_engine_error),
        }
    })
    .await;

    let session = match outcome {
        Ok(Ok(s)) => s,
        Ok(Err(HandlerError::Cancelled)) => {
            return openai_error(
                StatusCode::from_u16(STATUS_CLIENT_CLOSED).unwrap(),
                "client_error",
                "client_closed_request",
                "Client disconnected before deliberation finished",
            );
        }
        Ok(Err(HandlerError::QuorumFailed(e))) => {
            eprintln!("⚠️  quorum_failed: {:?}", e);
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "quorum_failed",
                "Deliberation failed to reach quorum",
            );
        }
        Ok(Err(HandlerError::Unavailable(e))) => {
            eprintln!("❌ council_unavailable: {:?}", e);
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "council_unavailable",
                "Council engine unreachable",
            );
        }
        Ok(Err(HandlerError::Internal(e))) => {
            eprintln!("❌ internal: {:?}", e);
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal_error",
                "Internal council error",
            );
        }
        Err(_elapsed) => {
            return openai_error(
                StatusCode::GATEWAY_TIMEOUT,
                "server_error",
                "council_timeout",
                "Deliberation exceeded budget",
            );
        }
    };

    // D2: structurally validate the directive_proposal_v1 fence before returning
    // 200. The triage Chair is contracted to emit a well-formed
    // `irin.directive.proposal.v1` fence; reject malformed machine-output with
    // 422 here so the gateway never dead-letters a 200-wrapped bad proposal. Gateway
    // stays the second validator (cross-field tenant / in_response_to exact-match
    // against the live escalation). Only runs for cabinets in directive-proposal mode.
    let is_directive_proposal = state
        .config
        .resolve_cabinet_owned(&cabinet_name)
        .map(|c| c.synthesis_mode == SynthesisMode::DirectiveProposalV1)
        .unwrap_or(false);
    if is_directive_proposal {
        let synthesis = session.synthesis.as_deref().unwrap_or("");
        if let Err(reason) =
            crate::engine::directive_fence::validate_directive_proposal_v1(synthesis)
        {
            eprintln!("⚠️  malformed_directive_proposal: {reason}");
            return openai_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "server_error",
                "malformed_directive_proposal",
                &format!("council-triage produced a malformed directive proposal: {reason}"),
            );
        }
    }

    // Token math — sum seat tokens across every round + chair tokens.
    let (seat_in, seat_out) = session
        .rounds
        .iter()
        .flat_map(|r| r.responses.iter())
        .fold((0u64, 0u64), |(p, c), s| {
            (p + s.tokens_in as u64, c + s.tokens_out as u64)
        });

    let chair_in = session.chair_tokens_in as u64;
    let chair_out = session.chair_tokens_out as u64;

    let prompt_tokens = seat_in + chair_in;
    let completion_tokens = seat_out + chair_out;
    let total_cost = session.total_cost_usd;

    let body = json!({
        "id":      format!("chatcmpl-{}", session.session_id),
        "object":  "chat.completion",
        "created": session.timestamp.timestamp(),
        "model":   model_in,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": session.synthesis.clone().unwrap_or_default()
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens":     prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens":      prompt_tokens + completion_tokens
        }
    });

    let mut body_obj = body.as_object().unwrap().clone();
    if session.specops_triggered {
        let usage_obj = body_obj.get_mut("usage").unwrap().as_object_mut().unwrap();
        usage_obj.insert(
            "extra_charges".to_string(),
            json!([{
                "reason": "specops_escalation",
                "cost_usd": session.specops_cost_usd,
            }]),
        );
    }

    let mut resp = axum::Json(body_obj).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&session.session_id) {
        h.insert("X-Council-Session-Id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("{:.4}", total_cost)) {
        h.insert("X-Total-Cost-Usd", v);
    }
    if let Ok(v) = HeaderValue::from_str(&chair_out.to_string()) {
        h.insert("X-Chair-Tokens", v);
    }
    if session.specops_triggered {
        h.insert(
            "X-Council-Specops-Triggered",
            HeaderValue::from_static("true"),
        );
    }
    resp
}

#[cfg(test)]
mod deliberate_cap_tests {
    use super::super::{DEFAULT_MAX_DELIBERATIONS, resolve_max_deliberations};

    #[test]
    fn unset_uses_default() {
        assert_eq!(resolve_max_deliberations(None), DEFAULT_MAX_DELIBERATIONS);
    }

    #[test]
    fn valid_value_is_used() {
        assert_eq!(resolve_max_deliberations(Some("8".to_string())), 8);
        assert_eq!(resolve_max_deliberations(Some("1".to_string())), 1);
        // surrounding whitespace is tolerated
        assert_eq!(resolve_max_deliberations(Some("  3 ".to_string())), 3);
    }

    #[test]
    fn zero_falls_back_to_default_not_deadlock() {
        // A 0-permit Semaphore would block every deliberation forever — must
        // fail closed to the safe default, never to "no service".
        assert_eq!(
            resolve_max_deliberations(Some("0".to_string())),
            DEFAULT_MAX_DELIBERATIONS
        );
    }

    #[test]
    fn garbage_and_negative_fall_back_to_default() {
        for raw in ["", "abc", "-2", "4.5", "  "] {
            assert_eq!(
                resolve_max_deliberations(Some(raw.to_string())),
                DEFAULT_MAX_DELIBERATIONS,
                "input {raw:?} must fall back to default"
            );
        }
    }
}
