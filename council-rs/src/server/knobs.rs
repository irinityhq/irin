// Shared deliberate / WS start-field parsers (moved from server.rs).

use crate::mode::Mode;

/// Parsed fields from a War Room WS `{ type: "start", payload: ... }` message.
#[derive(Debug, Clone)]
pub(crate) struct WsStartFields {
    pub topic: String,
    pub cabinet_name: String,
    pub context: String,
    pub blind: bool,
    pub max_rounds: Option<u32>,
    pub pause_after_each_round: bool,
    pub frame_check: bool,
    /// Scope auditor (steering / boundary review) — if true, run the scope_auditor role.
    pub scope_auditor: bool,
    pub mode: Mode,
    pub custom_cabinet: Option<crate::types::Cabinet>,
    pub parent_session_id: Option<String>,
    pub swaps: Vec<serde_json::Value>,
    pub validate: bool,
    pub validate_provider: String,
    pub validate_gate: bool,
    pub worker_provenance: Option<sovereign_protocol::types::WorkerProvenanceGuard>,
    pub budget_max_usd: Option<f64>,
    pub tier: String,
    pub then_tear_down: bool,
    pub auto_specops_threshold: f64,
    pub smoke_only: bool,
    /// Per-session gateway routing (feature contract). `None` falls back to the
    /// process-wide `COUNCIL_VIA_GATEWAY` state.
    pub via_gateway: Option<bool>,
    /// UPPERCASE GREEN/YELLOW/RED, normalized from the lowercase wire values.
    pub sensitivity: Option<String>,
    /// Direct-fire single-shot mode (feature contract): contrarian | munger | kiss |
    /// specops | premortem.
    pub direct_fire: Option<String>,
}

#[derive(Debug)]
pub(crate) struct WsStartParseOutcome {
    pub fields: WsStartFields,
    /// Non-fatal: `then_tear_down` requested with non-pathfind mode — coerced to Pathfind.
    pub coerce_then_tear_down: bool,
}

pub(crate) const WS_MAX_ROUNDS_CAP: u32 = 6;

pub(crate) fn normalize_ws_tier(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("sovereign") => "sovereign".to_string(),
        Some("strict_sovereign") => "strict_sovereign".to_string(),
        Some("best") => "best".to_string(),
        _ => "best".to_string(),
    }
}

pub(crate) fn clamp_ws_max_rounds(requested: Option<u32>, cabinet_rounds: u32) -> u32 {
    let cap = cabinet_rounds.min(WS_MAX_ROUNDS_CAP);
    requested.unwrap_or(cabinet_rounds).clamp(1, cap)
}

/// Strictness for the shared engine-knob field parsers (feature contract).
///
/// The WS start payload keeps its Phase 5 wire contract: unknown `mode`/`tier`
/// and non-positive `budget_max_usd` silently coerce to defaults (mode-union
/// clients depend on it). `POST /api/deliberate` uses the SAME value rules via
/// the same parsers but rejects invalid values with a 4xx instead of coercing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnobStrictness {
    Lenient,
    Strict,
}

pub(crate) fn parse_mode_field(
    payload: &serde_json::Value,
    strict: KnobStrictness,
) -> Result<Mode, String> {
    let Some(v) = payload.get("mode").filter(|v| !v.is_null()) else {
        return Ok(Mode::TearDown);
    };
    match v.as_str() {
        Some("teardown") => Ok(Mode::TearDown),
        Some("pathfind") => Ok(Mode::Pathfind),
        Some("harden") => Ok(Mode::Harden),
        _ if strict == KnobStrictness::Strict => Err(format!(
            "mode: must be one of teardown|pathfind|harden, got {v}"
        )),
        _ => Ok(Mode::TearDown),
    }
}

pub(crate) fn parse_tier_field(
    payload: &serde_json::Value,
    strict: KnobStrictness,
) -> Result<String, String> {
    let raw = payload.get("tier");
    if strict == KnobStrictness::Strict
        && let Some(v) = raw.filter(|v| !v.is_null())
    {
        match v.as_str().map(str::trim) {
            Some("best" | "sovereign" | "strict_sovereign") => {}
            _ => {
                return Err(format!(
                    "tier: must be one of best|sovereign|strict_sovereign, got {v}"
                ));
            }
        }
    }
    Ok(normalize_ws_tier(raw.and_then(|v| v.as_str())))
}

/// Default upper bound for `budget_max_usd` on the strict REST path, in USD.
/// Overridable via `COUNCIL_MAX_BUDGET_USD`.
const DEFAULT_MAX_BUDGET_USD: f64 = 10.0;

/// Resolve the strict-path budget ceiling. Reads `COUNCIL_MAX_BUDGET_USD` and
/// falls back to `DEFAULT_MAX_BUDGET_USD` when unset or unparseable/non-positive.
pub(super) fn max_budget_usd() -> f64 {
    std::env::var("COUNCIL_MAX_BUDGET_USD")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|&n| n.is_finite() && n > 0.0)
        .unwrap_or(DEFAULT_MAX_BUDGET_USD)
}

pub(crate) fn parse_budget_field(
    payload: &serde_json::Value,
    strict: KnobStrictness,
) -> Result<Option<f64>, String> {
    let Some(v) = payload.get("budget_max_usd").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let parsed = v.as_f64().filter(|&n| n.is_finite() && n > 0.0);
    if parsed.is_none() && strict == KnobStrictness::Strict {
        return Err(format!(
            "budget_max_usd: must be a finite number > 0, got {v}"
        ));
    }
    // Strict (REST) rejects an over-ceiling budget rather than silently
    // clamping; WS stays lenient and keeps whatever the client sent.
    if strict == KnobStrictness::Strict
        && let Some(n) = parsed
    {
        let max = max_budget_usd();
        if n > max {
            return Err(format!("budget_max_usd: must be <= {max}, got {n}"));
        }
    }
    Ok(parsed)
}

pub(crate) fn parse_bool_field(
    payload: &serde_json::Value,
    key: &str,
    default: bool,
    strict: KnobStrictness,
) -> Result<bool, String> {
    let Some(v) = payload.get(key).filter(|v| !v.is_null()) else {
        return Ok(default);
    };
    match v.as_bool() {
        Some(b) => Ok(b),
        None if strict == KnobStrictness::Strict => {
            Err(format!("{key}: must be a boolean, got {v}"))
        }
        None => Ok(default),
    }
}

/// Engine knobs accepted by `POST /api/deliberate` (feature contract) — same value rules
/// as the WS start payload, parsed Strict (invalid → 4xx).
#[derive(Debug, PartialEq)]
pub(crate) struct DeliberateKnobs {
    pub mode: Mode,
    pub tier: String,
    pub budget_max_usd: Option<f64>,
    pub validate: bool,
    pub validate_gate: bool,
    pub blind: bool,
    pub cabinet_name: Option<String>,
}

pub(crate) fn parse_deliberate_knobs(
    payload: &serde_json::Value,
) -> Result<DeliberateKnobs, String> {
    let strict = KnobStrictness::Strict;
    let mode = parse_mode_field(payload, strict)?;
    let tier = parse_tier_field(payload, strict)?;
    let budget_max_usd = parse_budget_field(payload, strict)?;
    let validate = parse_bool_field(payload, "validate", false, strict)?;
    let validate_gate = parse_bool_field(payload, "validate_gate", false, strict)?;
    let blind = parse_bool_field(payload, "blind", false, strict)?;
    let cabinet_name = match payload.get("cabinet_name").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v.as_str().map(str::trim) {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => {
                return Err(format!("cabinet_name: must be a non-empty string, got {v}"));
            }
        },
    };
    Ok(DeliberateKnobs {
        mode,
        tier,
        budget_max_usd,
        validate,
        validate_gate,
        blind,
        cabinet_name,
    })
}

/// Maximum accepted `topic` length, in bytes, on the WS deliberate entry points.
/// Without an explicit cap the only bound is tungstenite's ~64 MiB default frame
/// size, so a single client could pin server memory with a giant topic. 64 KiB is
/// far larger than any real deliberation prompt while keeping the parse cheap.
pub(crate) const MAX_WS_TOPIC_BYTES: usize = 64 * 1024;

/// Maximum accepted `context` length, in bytes, on the WS deliberate entry
/// points. Same unbounded-input surface as the topic, but `context` carries
/// supplementary background (often pasted material) rather than a one-line
/// prompt, so it gets its own, more generous cap — still far below tungstenite's
/// ~64 MiB default frame size that would otherwise be the only bound.
pub(crate) const MAX_WS_CONTEXT_BYTES: usize = 256 * 1024;

/// Shared WS start payload parsing for `handle_ws` and unit tests.
pub(crate) fn parse_ws_start_fields(
    payload: &serde_json::Value,
    smoke_only_env: bool,
) -> Result<WsStartParseOutcome, String> {
    let topic = payload
        .get("topic")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if topic.len() > MAX_WS_TOPIC_BYTES {
        return Err(format!(
            "topic: exceeds max length of {MAX_WS_TOPIC_BYTES} bytes (got {})",
            topic.len()
        ));
    }
    let cabinet_name = payload
        .get("cabinet_name")
        .and_then(|v| v.as_str())
        .unwrap_or("standard")
        .to_string();
    let context = payload
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if context.len() > MAX_WS_CONTEXT_BYTES {
        return Err(format!(
            "context: exceeds max length of {MAX_WS_CONTEXT_BYTES} bytes (got {})",
            context.len()
        ));
    }
    let blind = parse_bool_field(payload, "blind", false, KnobStrictness::Lenient)?;
    let max_rounds = payload
        .get("max_rounds")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let pause_after_each_round = payload
        .get("pause_after_each_round")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let frame_check = payload
        .get("frame_check")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let scope_auditor = payload
        .get("scope_auditor")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut mode = parse_mode_field(payload, KnobStrictness::Lenient)?;
    let then_tear_down = payload
        .get("then_tear_down")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut coerce_then_tear_down = false;
    if then_tear_down && mode != Mode::Pathfind {
        mode = Mode::Pathfind;
        coerce_then_tear_down = true;
    }
    let custom_cabinet: Option<crate::types::Cabinet> = payload
        .get("custom_cabinet")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let parent_session_id = payload
        .get("parent_session_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let swaps: Vec<serde_json::Value> = payload
        .get("swaps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let validate = parse_bool_field(payload, "validate", false, KnobStrictness::Lenient)?;
    let validate_provider = payload
        .get("validate_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("grok_hermes")
        .to_string();
    let validate_gate = parse_bool_field(payload, "validate_gate", false, KnobStrictness::Lenient)?;
    let worker_provenance = match payload.get("worker_provenance") {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("worker_provenance: invalid JSON: {e}")),
    }?;
    let budget_max_usd = parse_budget_field(payload, KnobStrictness::Lenient)?;
    let tier = parse_tier_field(payload, KnobStrictness::Lenient)?;
    let auto_specops_threshold = payload
        .get("auto_specops_threshold")
        .and_then(|v| v.as_f64())
        .filter(|&n| n.is_finite() && (0.0..=1.0).contains(&n))
        .unwrap_or(0.8);
    // COUNCIL_WS_SMOKE_ONLY=1 means the server runs ONLY the synthetic shim —
    // a real (paid) deliberation must never fire on such a server, so the env
    // var alone forces smoke mode regardless of the payload. The env is the
    // sole authority: a client cannot force a synthetic run in production (env
    // unset) by sending smoke_only, nor request a real run when env is set.
    let smoke_only = smoke_only_env;
    let via_gateway = payload.get("via_gateway").and_then(|v| v.as_bool());
    // Pinned wire contract (feature contract): lowercase green|yellow|red ONLY — other
    // strings hard-error like worker_provenance. Normalized to UPPERCASE for
    // the provider layer / X-Sensitivity-Level gateway header.
    let sensitivity = match payload.get("sensitivity") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_str() {
            Some(level @ ("green" | "yellow" | "red")) => Some(level.to_ascii_uppercase()),
            _ => {
                return Err(format!(
                    "sensitivity: must be one of green|yellow|red, got {v}"
                ));
            }
        },
    };
    // Pinned wire contract (feature contract): unknown direct_fire values hard-error.
    let direct_fire = match payload.get("direct_fire") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_str() {
            Some(mode) if crate::engine::direct_fire::spec(mode).is_some() => {
                Some(mode.to_string())
            }
            _ => {
                return Err(format!(
                    "direct_fire: must be one of contrarian|munger|kiss|specops|premortem, got {v}"
                ));
            }
        },
    };

    Ok(WsStartParseOutcome {
        fields: WsStartFields {
            topic,
            cabinet_name,
            context,
            blind,
            max_rounds,
            pause_after_each_round,
            frame_check,
            scope_auditor,
            mode,
            custom_cabinet,
            parent_session_id,
            swaps,
            validate,
            validate_provider,
            validate_gate,
            worker_provenance,
            budget_max_usd,
            tier,
            then_tear_down,
            auto_specops_threshold,
            smoke_only,
            via_gateway,
            sensitivity,
            direct_fire,
        },
        coerce_then_tear_down,
    })
}

#[cfg(test)]
#[path = "knobs_tests.rs"]
mod knobs_tests;
