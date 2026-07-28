// Guard route handlers (moved from main.rs).

use axum::{extract::Json, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

use crate::decontaminator;
use crate::enforcer;
use crate::AppState;

#[derive(Deserialize)]
pub(super) struct GuardInputRequest {
    #[serde(default)]
    content: String,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "unknown".to_string()
}

#[derive(Serialize)]
pub(super) struct GuardInputResponse {
    verdict: decontaminator::ScanVerdict,
    blocked: bool,
    blocked_reason: String,
    original_hash: String,
    cleaned_hash: String,
    threat_count: usize,
    threats: Vec<decontaminator::ThreatDetection>,
    latency_ms: u64,
}

#[derive(Deserialize)]
pub(super) struct GuardToolRequest {
    #[serde(default)]
    tool: String,
    #[serde(default)]
    args: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize)]
pub(super) struct GuardToolResponse {
    allowed: bool,
    tool: String,
    violations: Vec<String>,
    latency_ms: u64,
}

#[derive(Serialize)]
pub(super) struct GuardToolError {
    allowed: bool,
    reason: String,
    tool: String,
    arg: String,
    latency_ms: u64,
}

#[derive(Deserialize)]
pub(super) struct GuardSovereigntyRequest {
    #[serde(default)]
    action_desc: String,
    #[serde(default)]
    action_type: String,
    #[serde(default = "default_energy")]
    energy: f64,
}

fn default_energy() -> f64 {
    1.0
}

#[derive(Serialize)]
pub(super) struct GuardSovereigntyResponse {
    allowed: bool,
    score: f64,
    kappa: f64,
    c_alignment: f64,
    d_risk: f64,
    energy: f64,
    question_boost: bool,
    latency_ms: u64,
}

pub(super) async fn guard_input(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<GuardInputRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();

    if req.content.is_empty() {
        return Json(GuardInputResponse {
            verdict: decontaminator::ScanVerdict::Clean,
            blocked: false,
            blocked_reason: String::new(),
            original_hash: String::new(),
            cleaned_hash: String::new(),
            threat_count: 0,
            threats: vec![],
            latency_ms: 0,
        });
    }

    let result = state.decon.scan(&req.content);
    let latency_ms = t0.elapsed().as_millis() as u64;

    if result.blocked {
        warn!(
            source = %req.source,
            verdict = ?result.verdict,
            threats = result.threat_count,
            latency_ms,
            "guard/input: blocked"
        );
    }

    Json(GuardInputResponse {
        verdict: result.verdict,
        blocked: result.blocked,
        blocked_reason: result.blocked_reason,
        original_hash: result.original_hash,
        cleaned_hash: result.cleaned_hash,
        threat_count: result.threat_count,
        threats: result.threats,
        latency_ms,
    })
}

pub(super) async fn guard_scan_debug(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<GuardInputRequest>,
) -> impl IntoResponse {
    // Debug-only. Returns the FULL internal decontaminator scan struct (rule-set
    // fingerprint, per-rule verdicts, original/cleaned hashes) — a UDS caller could
    // use it to probe which obfuscations slip the guard. This handler is REGISTERED
    // ONLY when GATEWAY_DEBUG_GUARD_SCAN=1 (see router construction); when disabled
    // the route is absent, so every method + malformed body uniformly hits the 404
    // fallback and the route's existence is not disclosed. Gating at registration
    // (not inside the handler) is required because the `Json` extractor would
    // otherwise 400/415 a bad body — and a non-404 status leaks existence.
    // Production traffic uses /guard/input — the Lua frontend never calls /guard/scan.
    let result = state.decon.scan(&req.content);
    Json(serde_json::to_value(result).unwrap())
}

pub(super) async fn guard_tool(Json(req): Json<GuardToolRequest>) -> impl IntoResponse {
    let t0 = Instant::now();

    match enforcer::enforce(&req.tool, &req.args, None, None) {
        Ok(result) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(GuardToolResponse {
                        allowed: result.allowed,
                        tool: result.tool,
                        violations: result.violations,
                        latency_ms,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(violation) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            warn!(
                tool = %req.tool,
                reason = %violation.reason,
                latency_ms,
                "guard/tool: violation"
            );
            (
                StatusCode::FORBIDDEN,
                Json(
                    serde_json::to_value(GuardToolError {
                        allowed: false,
                        reason: violation.reason,
                        tool: violation.tool,
                        arg: violation.arg,
                        latency_ms,
                    })
                    .unwrap(),
                ),
            )
        }
    }
}

pub(super) async fn guard_sovereignty(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<GuardSovereigntyRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();

    let result = state
        .sovereignty
        .evaluate(&req.action_desc, &req.action_type, req.energy);
    let latency_ms = t0.elapsed().as_millis() as u64;

    if !result.allowed {
        warn!(
            action_type = %req.action_type,
            kappa = result.kappa,
            latency_ms,
            "guard/sovereignty: blocked"
        );
        (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::to_value(GuardSovereigntyResponse {
                    allowed: result.allowed,
                    score: result.score,
                    kappa: result.kappa,
                    c_alignment: result.c_alignment,
                    d_risk: result.d_risk,
                    energy: result.energy,
                    question_boost: result.question_boost,
                    latency_ms,
                })
                .unwrap(),
            ),
        )
    } else {
        (
            StatusCode::OK,
            Json(
                serde_json::to_value(GuardSovereigntyResponse {
                    allowed: result.allowed,
                    score: result.score,
                    kappa: result.kappa,
                    c_alignment: result.c_alignment,
                    d_risk: result.d_risk,
                    energy: result.energy,
                    question_boost: result.question_boost,
                    latency_ms,
                })
                .unwrap(),
            ),
        )
    }
}
