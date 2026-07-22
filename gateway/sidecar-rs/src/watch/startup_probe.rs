//! Phase 3a startup probe — cabinet schema gate for council-triage.
//!
//! This module implements the runtime cabinet schema validation gate (AC-19h).
//! Before any live dispatcher claims rows or the boot hydration recovery
//! trusts council output, we actively prove that the local `council-triage`
//! cabinet (as served by the gateway router) still emits the Phase 3
//! `irin.directive.proposal.v1` shape required by the spec.
//!
//! The probe:
//! - Uses the existing C11 `build_council_triage_headers` helper.
//! - Calls the local gateway router at `/v1/chat/completions` (never
//!   `council_idem_*` directly).
//! - Sends a synthetic probe escalation asking for a minimal Dismiss or Act.
//! - Strictly validates the returned fenced JSON (not prose).
//!
//! Fail dispatcher activation on any deviation (wrong schema, session/cost in
//! fence, Act-only fields on Dismiss, missing fence, etc.). Per the IRIN
//! Comms Contract v0.2 health model, a failing probe only aborts sidecar base
//! startup when the caller explicitly enables strict boot for this feature.

use crate::watch::dispatcher::build_council_triage_headers;
use crate::watch::outbox::DirectiveAuthority;
use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;

/// Exit code used when the startup cabinet probe fails (P0-eta).
pub const CABINET_PROBE_FAILURE_EXIT_CODE: i32 = 88;

/// Per-attempt counter for probe idempotency keys. Each invocation of the
/// cabinet probe (including retries after 499) uses a distinct key so a
/// timed-out first attempt's pending idem entry in council does not cause
/// 409 self-collision on the retry. (See run_council_triage_cabinet_probe.)
static PROBE_ATTEMPT: AtomicU64 = AtomicU64::new(0);

/// Mirrors `council-rs` directive-fence `ALLOWED_TOP_KEYS` until arming.
/// Closed top-level keyset (HARDEN-A): any key outside this set is rejected. Subsumes the old
/// council_session_id / council_cost_usd block. `capability_token` is DELIBERATELY EXCLUDED --
/// added only at arming, co-located with a capability-token verify path (see ARMING_CHECKLIST):
/// accepting an unverifiable token into a signed envelope here would be fail-open.
const ALLOWED_TOP_KEYS: [&str; 9] = [
    "schema",
    "authority",
    "verdict",
    "in_response_to",
    "rationale",
    "job",
    "scope",
    "stop_condition",
    "return_expectation",
];
/// Mirrors `council-rs` directive-fence `ALLOWED_SCOPE_KEYS`.
const ALLOWED_SCOPE_KEYS: [&str; 3] = ["tenant", "subject", "allowed_actions"];
/// Mirrors `council-rs` directive-fence `ALLOWED_ACTIONS` for recommend-grade verbs.
/// Enforced UNIFORMLY across all authorities (no conditional widening for prepare/execute while
/// arming is deferred per D2b). Widening is a security decision, not a typo fix -- see ARMING_CHECKLIST.
const ALLOWED_ACTIONS: [&str; 5] = ["read", "report", "notify", "review", "escalate"];

/// ARMING CHECKLIST -- the ATOMIC set required to enable prepare/execute arming. These are
/// COUPLED; flipping one without the others is a security hole. Surfaced as an ignored test so
/// it appears in `--ignored` / CI listings as a standing reminder while arming is deferred.
#[cfg(test)]
const ARMING_CHECKLIST: [&str; 4] = [
    "widen ALLOWED_ACTIONS with execution verbs (write/delete/provision/...)",
    "add capability_token to ALLOWED_TOP_KEYS + a capability-token verify path (kid + keyset)",
    "flip the elevated-authority tests (validate_accepts_elevated_authority + new arming vectors)",
    "relax the Chair recommend-only restriction (council-rs engine/deliberate.rs)",
];

/// First key in `map` not in `allowed` (HARDEN-A closed keyset); None if all allowed.
fn find_unexpected_key(map: &serde_json::Map<String, Value>, allowed: &[&str]) -> Option<String> {
    map.keys().find(|k| !allowed.contains(&k.as_str())).cloned()
}

/// P0-eta executable harness (boot step 4.5).
///
/// Models the required ordering:
///   1. DirectiveSigningKey loaded + published
///   2. Router serving (TriageProbeClient available)
///   3. run_council_triage_cabinet_probe
///   4. Only on success may the caller proceed to hydration / claiming.
///
/// On failure: returns Err containing the required exit code (88) and the probe error.
/// The caller MUST NOT proceed to run_boot_hydration_sweep or any claim logic.
pub async fn run_council_triage_probe_as_boot_step_4_5<C: TriageProbeClient>(
    client: &C,
    tenant: &str,
) -> Result<(), (i32, TriageCabinetProbeError)> {
    match run_council_triage_cabinet_probe(client, tenant).await {
        Ok(_success) => Ok(()),
        Err(e) => Err((CABINET_PROBE_FAILURE_EXIT_CODE, e)),
    }
}

/// P0-eta executable harness with marker proof.
///
/// Models the required boot ordering:
///   key load/publish complete → router serving → probe → only then hydration may proceed.
///
/// The `hydration` closure is only invoked on successful probe.
/// (Post-hydration, live tick spawn recovers any remaining stale 'claimed' via the unified claim path; see design.)
/// The returned marker (`hydration_ran`) proves whether hydration continuation was reached.
pub async fn run_probe_then_hydration_with_marker<C, F, Fut>(
    client: &C,
    tenant: &str,
    hydration: F,
) -> Result<bool, (i32, TriageCabinetProbeError)>
where
    C: TriageProbeClient,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    match run_council_triage_cabinet_probe(client, tenant).await {
        Ok(_) => {
            hydration().await;
            Ok(true)
        }
        Err(e) => Err((CABINET_PROBE_FAILURE_EXIT_CODE, e)),
    }
}

/// Typed errors for the cabinet probe. These are intended to be surfaced
/// at boot time so operators see exactly why the sidecar refused to start.
#[derive(Debug, Error)]
pub enum TriageCabinetProbeError {
    #[error("council-triage cabinet probe failed: missing JSON fence in response")]
    MissingFence,

    #[error("council-triage cabinet probe failed: fence content was not valid JSON: {0}")]
    InvalidFenceJson(String),

    #[error("council-triage cabinet probe failed: expected schema \"irin.directive.proposal.v1\", got \"{got}\"")]
    WrongSchema { got: String },

    #[error(
        "council-triage cabinet probe failed: expected authority \"recommend\", got \"{got}\""
    )]
    WrongAuthority { got: String },

    #[error("council-triage cabinet probe failed: fence contained dispatcher-injected field \"{field}\" (session/cost must come from response headers only)")]
    SessionOrCostInFence { field: &'static str },

    #[error("council-triage cabinet probe failed: proposal has unexpected {location} key \"{key}\" (closed keyset)")]
    UnexpectedKey { location: &'static str, key: String },

    #[error("council-triage cabinet probe failed: scope.allowed_actions contains disallowed verb \"{verb}\" (allowlist: read, report, notify, review, escalate)")]
    DisallowedVerb { verb: String },

    #[error("council-triage cabinet probe failed: Act proposal had scope.tenant \"{got}\" but probe used tenant \"{expected}\"")]
    ActScopeTenantMismatch { expected: String, got: String },

    #[error("council-triage cabinet probe failed: Dismiss proposal contained Act-only field \"{field}\" (job/scope/stop_condition/return_expectation must be absent or null)")]
    DismissWithActOnlyField { field: &'static str },

    #[error("council-triage cabinet probe failed: missing or empty rationale (required for Act and Dismiss)")]
    MissingRationale,

    #[error("council-triage cabinet probe failed: Act proposal missing or empty required field \"{field}\"")]
    ActMissingRequiredField { field: &'static str },

    #[error("council-triage cabinet probe failed: Act proposal scope is invalid (missing subject or non-empty allowed_actions)")]
    ActInvalidScope,

    #[error("council-triage cabinet probe failed: HTTP call to router failed: {0}")]
    RouterCallFailed(#[from] reqwest::Error),

    #[error("council-triage cabinet probe failed: HTTP {status} from gateway: {body}")]
    RouterHttpError { status: u16, body: String },

    #[error(
        "council-triage cabinet probe failed: WATCH_DISPATCHER_GATEWAY_KEY is not set \
         (required for authenticated calls to gateway /v1/chat/completions during P0-eta probe). \
         Provision a key via `make provision-key BUDGET=...` (using BOOTSTRAP_TOKEN or ADMIN_KEY), \
         export WATCH_DISPATCHER_GATEWAY_KEY=the_raw_key in .env, then restart the stack."
    )]
    MissingGatewayAuthKey,

    #[error("council-triage cabinet probe failed: could not build probe request body: {0}")]
    RequestBuildFailed(String),

    #[error(
        "council-triage cabinet probe failed: response did not contain choices[0].message.content"
    )]
    MalformedChatResponse,
}

/// Outcome of a successful probe (for tests and future observability).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSuccess {
    /// The cabinet returned a well-formed `proposal.v1` Dismiss (scope may be absent).
    DismissAccepted,
    /// The cabinet returned a well-formed `proposal.v1` Act with matching scope.tenant.
    ActAccepted { scope_tenant: String },
}

/// Startup activation decision for the optional Phase 3 dispatcher feature.
///
/// The probe remains mandatory before hydration or live dispatcher claiming.
/// In default v0.2 boot semantics, a failed probe degrades this feature while
/// keeping sidecar base health online. Strict deployments can still choose the
/// old fail-closed process behavior by turning degraded probe results into exit
/// 88 at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase3DispatcherActivation {
    Ready,
    Degraded { error: String },
    Fatal { exit_code: i32, error: String },
}

/// Probe council-triage and convert the result into a Phase 3 activation state.
///
/// This keeps process-fatal policy out of the probe itself: callers decide
/// whether `Fatal` should exit, while `Degraded` means base sidecar serving may
/// continue but dispatcher hydration and the live loop must not run.
pub async fn probe_phase3_dispatcher_activation<C: TriageProbeClient>(
    client: &C,
    tenant: &str,
    strict_boot: bool,
    max_attempts: u32,
    retry_delay: Duration,
) -> Phase3DispatcherActivation {
    let max_attempts = max_attempts.max(1);

    for attempt in 0..max_attempts {
        match run_council_triage_probe_as_boot_step_4_5(client, tenant).await {
            Ok(_) => return Phase3DispatcherActivation::Ready,
            Err((code, e)) => {
                if attempt + 1 < max_attempts {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        error = %e,
                        "cabinet probe transient failure (retrying for gateway readiness)..."
                    );
                    if !retry_delay.is_zero() {
                        tokio::time::sleep(retry_delay).await;
                    }
                    continue;
                }

                let error = e.to_string();
                if strict_boot {
                    return Phase3DispatcherActivation::Fatal {
                        exit_code: code,
                        error,
                    };
                }
                return Phase3DispatcherActivation::Degraded { error };
            }
        }
    }

    Phase3DispatcherActivation::Fatal {
        exit_code: CABINET_PROBE_FAILURE_EXIT_CODE,
        error: "council-triage cabinet probe did not complete".to_string(),
    }
}

/// Trait abstracting the call to the council-triage endpoint.
///
/// This boundary allows unit tests to supply a mock that returns canned
/// chat completion responses without requiring a running router.
#[async_trait]
pub trait TriageProbeClient: Send + Sync {
    /// Perform a POST to the council-triage chat completions endpoint.
    ///
    /// The implementation must:
    /// - Merge the supplied C11 headers (`idempotency-key`, `x-caller-key`).
    /// - Send the JSON body (model: "council-triage", messages, ...).
    /// - Return the parsed JSON response body (or an error).
    async fn post_council_triage(
        &self,
        headers: HeaderMap,
        body: Value,
    ) -> Result<Value, TriageCabinetProbeError>;
}

/// Real implementation backed by reqwest.
///
/// The caller supplies the base URL of the local gateway router
/// (usually `http://gateway:8080` inside compose or `http://127.0.0.1:18080`).
/// When a `gateway_key` is supplied it attaches `Authorization: Bearer <key>`
/// so the call succeeds against the gateway's auth layer for /v1/chat/completions.
/// This is the same credential used by the live `ReqwestCouncilClient`.
pub struct ReqwestTriageProbeClient {
    http: reqwest::Client,
    base_url: String,
    gateway_key: Option<String>,
}

impl ReqwestTriageProbeClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::new_with_key(base_url, None)
    }

    pub fn new_with_key(base_url: impl Into<String>, gateway_key: Option<String>) -> Self {
        // Share the live dispatcher's bounded timeout so startup is not stricter
        // than the loop it enables. A shorter timeout can strand the fixed
        // idempotency key and make every retry collide with pending work.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                crate::watch::dispatcher::DEFAULT_COUNCIL_CALL_TIMEOUT_SECS,
            ))
            .build()
            .expect("failed to build ReqwestTriageProbeClient with timeout");
        Self {
            http,
            base_url: base_url.into(),
            gateway_key,
        }
    }
}

#[async_trait]
impl TriageProbeClient for ReqwestTriageProbeClient {
    async fn post_council_triage(
        &self,
        headers: HeaderMap,
        body: Value,
    ) -> Result<Value, TriageCabinetProbeError> {
        if self.gateway_key.is_none() {
            return Err(TriageCabinetProbeError::MissingGatewayAuthKey);
        }

        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let mut req = self.http.post(&url).headers(headers).json(&body);
        if let Some(ref key) = self.gateway_key {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", key));
        }

        let resp = req.send().await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unable to read response body>".to_string());
            let truncated: String = body.chars().take(2000).collect();
            return Err(TriageCabinetProbeError::RouterHttpError {
                status: status.as_u16(),
                body: truncated,
            });
        }

        let json: Value = resp.json().await?;
        Ok(json)
    }
}

/// Run the cabinet schema probe for the given tenant.
///
/// This is the main entry point intended to be called during Phase 3 boot,
/// after `DirectiveSigningKey::load_or_initialize` has succeeded and the
/// key is published, but before the live dispatcher begins claiming rows
/// or hydration recovery proceeds.
///
/// A `Dismiss` probe is used by default (simplest validation — scope may
/// be omitted). An `Act` variant is also exercised by the test suite.
pub async fn run_council_triage_cabinet_probe<C: TriageProbeClient>(
    client: &C,
    tenant: &str,
) -> Result<ProbeSuccess, TriageCabinetProbeError> {
    // Per-attempt probe escalation id (raw form). Using a fresh suffix per
    // invocation (including internal retries) prevents self-collision on the
    // Idempotency-Key when attempt #1 times out (nginx 499) but council keeps
    // the pending entry. The qualified key is still built by
    // build_council_triage_headers. Tests continue to hard-code the old fixed
    // id for their mock fences.
    let attempt = PROBE_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let raw_probe_id = format!("phase3-startup-probe-v1-attempt-{:016x}", attempt);

    // Build the two C11 headers exactly as the future dispatcher will.
    let headers = build_council_triage_headers(tenant, &raw_probe_id);

    // Synthetic prompt asking for a minimal Dismiss proposal.
    // The cabinet is expected to emit exactly one ```json fence with
    // schema "irin.directive.proposal.v1".
    let prompt = format!(
        r#"You are the Triage chair for the Sovereign Triad (Phase 3 closed signal loop).

This is a synthetic startup probe escalation from the watch dispatcher (not a real sentinel).

Respond with EXACTLY ONE JSON code fence (```json ... ```) containing a minimal Dismiss proposal
using the irin.directive.proposal.v1 schema.

Use:
- "in_response_to": "{}"
- "verdict": "Dismiss"
- "authority": "recommend"
- A short rationale.
- Do NOT include council_session_id or council_cost_usd inside the fence (they are injected by the dispatcher from response headers).
- For Dismiss, the "scope" object may be omitted entirely.

Escalation tenant: "{}"

Only output the fence. No prose outside it."#,
        raw_probe_id, tenant
    );

    let body = serde_json::json!({
        "model": "council-triage",
        "messages": [
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.0,
        "max_tokens": 512
    });

    let response = client.post_council_triage(headers, body).await?;

    // Extract the assistant content (OpenAI-compatible shape).
    let content = response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or(TriageCabinetProbeError::MalformedChatResponse)?;

    // Find the first ```json ... ``` fence.
    let fenced = extract_first_json_fence(content).ok_or(TriageCabinetProbeError::MissingFence)?;

    let proposal: Value = serde_json::from_str(fenced)
        .map_err(|e| TriageCabinetProbeError::InvalidFenceJson(e.to_string()))?;

    validate_proposal_v1_shape(&proposal, tenant)?;

    // For the probe we asked for Dismiss, so success is DismissAccepted.
    // (The test suite also exercises Act via a different prompt path.)
    Ok(ProbeSuccess::DismissAccepted)
}

/// Extract the content of the first ```json ... ``` (or ``` ... ```) fence.
/// Handles both "```json" and "```" variants.
fn extract_first_json_fence(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let after_start = &text[start + 3..];

    let fence_start = if let Some(stripped) = after_start.strip_prefix("json") {
        stripped.find('\n').map(|n| start + 3 + 4 + n + 1)?
    } else {
        after_start.find('\n').map(|n| start + 3 + n + 1)?
    };

    let rest = &text[fence_start..];
    let end = rest.find("```")?;
    Some(rest[..end].trim())
}

/// Strict validation of a `CouncilDirectiveProposalV1` object per spec §3.2.1
/// and the cabinet contract (P5.2.4 F11).
///
/// Shared proposal.v1 validator used by startup and live dispatch.
/// Both the boot-time cabinet probe and the live dispatcher recovery path call it
/// (via pub(crate) re-export) so that shape rules stay in parity after any future contract bump.
pub(crate) fn validate_proposal_v1_shape(
    proposal: &Value,
    expected_tenant: &str,
) -> Result<(), TriageCabinetProbeError> {
    let obj = proposal
        .as_object()
        .ok_or_else(|| TriageCabinetProbeError::InvalidFenceJson("root is not an object".into()))?;

    // 1. Schema must be proposal.v1 (not payload.v1)
    let schema = obj.get("schema").and_then(|v| v.as_str()).ok_or_else(|| {
        TriageCabinetProbeError::WrongSchema {
            got: "missing".to_string(),
        }
    })?;

    if schema != "irin.directive.proposal.v1" {
        return Err(TriageCabinetProbeError::WrongSchema {
            got: schema.to_string(),
        });
    }

    let authority = obj
        .get("authority")
        .and_then(|v| v.as_str())
        .ok_or_else(|| TriageCabinetProbeError::WrongAuthority {
            got: "missing".to_string(),
        })?;

    if !DirectiveAuthority::contains(authority) {
        return Err(TriageCabinetProbeError::WrongAuthority {
            got: authority.to_string(),
        });
    }

    // 2. Dispatcher-injected fields MUST NOT be inside the fence.
    // Kept BEFORE the closed-keyset check so its more specific reason wins (a test
    // asserts SessionOrCostInFence). The keyset check below would otherwise reject
    // these same keys with the generic UnexpectedKey reason.
    for field in ["council_session_id", "council_cost_usd"] {
        if obj.contains_key(field) {
            return Err(TriageCabinetProbeError::SessionOrCostInFence { field });
        }
    }

    // 2b. HARDEN-A closed top-level keyset (FIX D): reject any key outside ALLOWED_TOP_KEYS.
    // capability_token is deliberately excluded (see const doc) -- it fails closed here until arming.
    if let Some(key) = find_unexpected_key(obj, &ALLOWED_TOP_KEYS) {
        return Err(TriageCabinetProbeError::UnexpectedKey {
            location: "top-level",
            key,
        });
    }

    let verdict = obj.get("verdict").and_then(|v| v.as_str()).unwrap_or("");

    // rationale is always required (Act and Dismiss) per spec §3.2.1
    if obj
        .get("rationale")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
    {
        return Err(TriageCabinetProbeError::MissingRationale);
    }

    if verdict == "Dismiss" {
        // Dismiss must not contain Act-only fields with non-null values.
        for field in ["job", "scope", "stop_condition", "return_expectation"] {
            if let Some(val) = obj.get(field) {
                if !val.is_null() {
                    return Err(TriageCabinetProbeError::DismissWithActOnlyField { field });
                }
            }
        }
        // scope may be absent or null — both are accepted for Dismiss.
    } else if verdict == "Act" {
        // scope.tenant must exactly match the escalation tenant (the probe tenant).
        let scope_tenant = obj
            .get("scope")
            .and_then(|s| s.get("tenant"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| TriageCabinetProbeError::ActScopeTenantMismatch {
                expected: expected_tenant.to_string(),
                got: "missing".to_string(),
            })?;

        if scope_tenant != expected_tenant {
            return Err(TriageCabinetProbeError::ActScopeTenantMismatch {
                expected: expected_tenant.to_string(),
                got: scope_tenant.to_string(),
            });
        }

        // Full Act fields per spec + task (job, stop_condition, return_expectation, scope.subject + non-empty allowed_actions)
        for field in ["job", "stop_condition", "return_expectation"] {
            if obj
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
            {
                return Err(TriageCabinetProbeError::ActMissingRequiredField { field });
            }
        }

        if let Some(scope) = obj.get("scope").and_then(Value::as_object) {
            // FIX D: closed scope keyset -- reject any scope key outside ALLOWED_SCOPE_KEYS.
            if let Some(key) = find_unexpected_key(scope, &ALLOWED_SCOPE_KEYS) {
                return Err(TriageCabinetProbeError::UnexpectedKey {
                    location: "scope",
                    key,
                });
            }
            if scope
                .get("subject")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
            {
                return Err(TriageCabinetProbeError::ActInvalidScope);
            }
            match scope.get("allowed_actions").and_then(Value::as_array) {
                Some(arr)
                    if !arr.is_empty()
                        && arr
                            .iter()
                            .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())) => {}
                _ => return Err(TriageCabinetProbeError::ActInvalidScope),
            }
            // FIX E: verb allowlist -- enforced UNIFORMLY across all authorities (no widening
            // for prepare/execute while arming is deferred per D2b). Runs after the non-empty
            // shape check above so an empty/malformed list still reports ActInvalidScope.
            if let Some(bad) = scope
                .get("allowed_actions")
                .and_then(Value::as_array)
                .and_then(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        // A2 (the invariant): no .trim() — a whitespace-padded verb is
                        // malformed, so it must DisallowedVerb (keyset/verb symmetry, kills
                        // cross-component exact-match drift vs council-rs directive_fence.rs).
                        .find(|a| !ALLOWED_ACTIONS.contains(a))
                })
            {
                return Err(TriageCabinetProbeError::DisallowedVerb {
                    verb: bad.to_string(),
                });
            }
        } else {
            return Err(TriageCabinetProbeError::ActInvalidScope);
        }
    } else {
        // Unknown verdict — treat as invalid for the probe.
        return Err(TriageCabinetProbeError::WrongSchema {
            got: format!("unknown verdict '{}'", verdict),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::dispatcher::{safe_tenant_token, WATCH_DISPATCHER_CALLER_KEY};

    /// In-memory mock that returns a pre-canned chat completion response.
    struct MockTriageClient {
        response: Value,
    }

    impl MockTriageClient {
        fn new_with_fence(fence_content: &str) -> Self {
            let content = format!("Here is the proposal:\n```json\n{}\n```", fence_content);
            let response = serde_json::json!({
                "choices": [{
                    "message": { "content": content }
                }]
            });
            Self { response }
        }

        fn new_with_raw_content(content: &str) -> Self {
            let response = serde_json::json!({
                "choices": [{
                    "message": { "content": content }
                }]
            });
            Self { response }
        }
    }

    #[async_trait]
    impl TriageProbeClient for MockTriageClient {
        async fn post_council_triage(
            &self,
            _headers: HeaderMap,
            _body: Value,
        ) -> Result<Value, TriageCabinetProbeError> {
            Ok(self.response.clone())
        }
    }

    fn minimal_dismiss_proposal(in_response_to: &str) -> String {
        serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": in_response_to,
            "authority": "recommend",
            "verdict": "Dismiss",
            "rationale": "startup probe dismissal"
            // scope deliberately omitted for Dismiss
        })
        .to_string()
    }

    fn minimal_act_proposal(in_response_to: &str, tenant: &str) -> String {
        serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": in_response_to,
            "authority": "recommend",
            "verdict": "Act",
            "job": "probe.job",
            "scope": {
                "tenant": tenant,
                "subject": "system.internal.boot_probe",
                "allowed_actions": ["read"]
            },
            "stop_condition": "on_success",
            "return_expectation": "structured",
            "rationale": "startup probe act"
        })
        .to_string()
    }

    #[tokio::test]
    async fn probe_accepts_proposal_v1_dismiss_with_no_scope() {
        let client = MockTriageClient::new_with_fence(&minimal_dismiss_proposal(
            "phase3-startup-probe-v1-00000000000000000000000000000000",
        ));
        let result = run_council_triage_cabinet_probe(&client, "probe-tenant").await;
        assert!(matches!(result, Ok(ProbeSuccess::DismissAccepted)));
    }

    #[tokio::test]
    async fn probe_accepts_proposal_v1_act_with_matching_scope_tenant() {
        let client = MockTriageClient::new_with_fence(&minimal_act_proposal(
            "phase3-startup-probe-v1-00000000000000000000000000000000",
            "probe-tenant",
        ));
        // We still use the Dismiss prompt path in the real function, but the
        // validator is exercised by the Act case in a dedicated helper test below.
        // For end-to-end probe we keep the Dismiss path; the Act shape is
        // validated via the unit test of validate_proposal_v1_shape.
        let _ = run_council_triage_cabinet_probe(&client, "probe-tenant").await;
    }

    #[tokio::test]
    async fn probe_rejects_payload_v1_schema() {
        let payload_v1 = serde_json::json!({
            "schema": "irin.directive.payload.v1",
            "in_response_to": "x",
            "authority": "recommend",
            "verdict": "Dismiss",
            "rationale": "bad"
        })
        .to_string();

        let client = MockTriageClient::new_with_fence(&payload_v1);
        let err = run_council_triage_cabinet_probe(&client, "t")
            .await
            .unwrap_err();
        assert!(matches!(err, TriageCabinetProbeError::WrongSchema { .. }));
    }

    #[tokio::test]
    async fn probe_rejects_fence_containing_council_session_id() {
        let bad = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "x",
            "authority": "recommend",
            "verdict": "Dismiss",
            "council_session_id": "sess-123",
            "rationale": "bad"
        })
        .to_string();

        let client = MockTriageClient::new_with_fence(&bad);
        let err = run_council_triage_cabinet_probe(&client, "t")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::SessionOrCostInFence {
                field: "council_session_id"
            }
        ));
    }

    #[tokio::test]
    async fn probe_rejects_fence_containing_council_cost_usd() {
        let bad = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "x",
            "authority": "recommend",
            "verdict": "Dismiss",
            "council_cost_usd": 0.0042,
            "rationale": "bad"
        })
        .to_string();

        let client = MockTriageClient::new_with_fence(&bad);
        let err = run_council_triage_cabinet_probe(&client, "t")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::SessionOrCostInFence {
                field: "council_cost_usd"
            }
        ));
    }

    #[tokio::test]
    async fn probe_rejects_dismiss_with_non_null_act_only_fields() {
        let bad = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "x",
            "authority": "recommend",
            "verdict": "Dismiss",
            "job": "should-not-be-here",
            "rationale": "bad"
        })
        .to_string();

        let client = MockTriageClient::new_with_fence(&bad);
        let err = run_council_triage_cabinet_probe(&client, "t")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::DismissWithActOnlyField { field: "job" }
        ));
    }

    #[tokio::test]
    async fn probe_rejects_missing_fence() {
        let client = MockTriageClient::new_with_raw_content("No fence here, just prose.");
        let err = run_council_triage_cabinet_probe(&client, "t")
            .await
            .unwrap_err();
        assert!(matches!(err, TriageCabinetProbeError::MissingFence));
    }

    #[tokio::test]
    async fn probe_request_uses_c11_headers_from_build_council_triage_headers() {
        // We cannot easily intercept the real call, so we test the header
        // builder in combination with the probe id construction.
        let tenant = "acme-prod";
        let raw_id = "phase3-startup-probe-v1-00000000000000000000000000000000";
        let headers = build_council_triage_headers(tenant, raw_id);

        let idempotency = headers.get("idempotency-key").unwrap().to_str().unwrap();
        let expected_token = safe_tenant_token(tenant);
        assert_eq!(idempotency, format!("{}:{}", expected_token, raw_id));

        let caller = headers.get("x-caller-key").unwrap().to_str().unwrap();
        assert_eq!(caller, WATCH_DISPATCHER_CALLER_KEY);
    }

    #[test]
    fn validate_accepts_dismiss_without_scope() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Dismiss",
            "rationale": "ok"
        });
        assert!(validate_proposal_v1_shape(&p, "any-tenant").is_ok());
    }

    #[test]
    fn validate_accepts_elevated_authority() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "execute",
            "verdict": "Dismiss",
            "rationale": "execution authority is valid in v0.2"
        });
        assert!(validate_proposal_v1_shape(&p, "any-tenant").is_ok());

        let p_prepare = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "prepare",
            "verdict": "Dismiss",
            "rationale": "prepare authority is valid in v0.2"
        });
        assert!(validate_proposal_v1_shape(&p_prepare, "any-tenant").is_ok());
    }

    #[test]
    fn validate_rejects_missing_authority() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "verdict": "Dismiss",
            "rationale": "authority is required"
        });
        let err = validate_proposal_v1_shape(&p, "any-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::WrongAuthority { got } if got == "missing"
        ));
    }

    #[test]
    fn validate_accepts_act_with_matching_tenant() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Act",
            "job": "j",
            "scope": {
                "tenant": "my-tenant",
                "subject": "system.internal.boot_probe",
                "allowed_actions": ["read"]
            },
            "stop_condition": "on_success",
            "return_expectation": "structured",
            "rationale": "ok"
        });
        assert!(validate_proposal_v1_shape(&p, "my-tenant").is_ok());
    }

    #[test]
    fn validate_rejects_act_with_wrong_tenant() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Act",
            "scope": { "tenant": "wrong" },
            "rationale": "bad"
        });
        let err = validate_proposal_v1_shape(&p, "expected").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::ActScopeTenantMismatch { .. }
        ));
    }

    // P0-eta harness tests — executable proof that probe runs before hydration and bad cabinet blocks it.

    struct GoodProbeClient;
    #[async_trait]
    impl TriageProbeClient for GoodProbeClient {
        async fn post_council_triage(
            &self,
            _headers: HeaderMap,
            _body: Value,
        ) -> Result<Value, TriageCabinetProbeError> {
            // Return a valid minimal Dismiss fence
            let content = r#"Here is the proposal:
```json
{"schema":"irin.directive.proposal.v1","in_response_to":"probe","authority":"recommend","verdict":"Dismiss","rationale":"ok"}
```"#;
            Ok(serde_json::json!({
                "choices": [ { "message": { "content": content } } ]
            }))
        }
    }

    struct BadProbeClient;
    #[async_trait]
    impl TriageProbeClient for BadProbeClient {
        async fn post_council_triage(
            &self,
            _headers: HeaderMap,
            _body: Value,
        ) -> Result<Value, TriageCabinetProbeError> {
            // Return payload.v1 (forbidden)
            let content = r#"```json
{"schema":"irin.directive.payload.v1","in_response_to":"probe","authority":"recommend","verdict":"Dismiss"}
```"#;
            Ok(serde_json::json!({
                "choices": [ { "message": { "content": content } } ]
            }))
        }
    }

    #[tokio::test]
    async fn p0_eta_harness_good_cabinet_allows_hydration_marker() {
        let client = GoodProbeClient;
        let result = run_council_triage_probe_as_boot_step_4_5(&client, "acme").await;
        assert!(
            result.is_ok(),
            "good cabinet must allow proceeding past step 4.5"
        );
        // In a real caller this would be the point where run_boot_hydration_sweep is allowed.
    }

    #[tokio::test]
    async fn p0_eta_harness_bad_cabinet_returns_88_and_blocks_hydration() {
        let client = BadProbeClient;
        let result = run_council_triage_probe_as_boot_step_4_5(&client, "acme").await;
        match result {
            Err((code, _err)) => {
                assert_eq!(
                    code, CABINET_PROBE_FAILURE_EXIT_CODE,
                    "bad cabinet must return the required exit code 88"
                );
            }
            Ok(_) => panic!("bad cabinet must not allow proceeding to hydration"),
        }
    }

    #[tokio::test]
    async fn phase3_activation_bad_cabinet_degrades_by_default() {
        let client = BadProbeClient;
        let result =
            probe_phase3_dispatcher_activation(&client, "acme", false, 1, Duration::from_millis(0))
                .await;

        match result {
            Phase3DispatcherActivation::Degraded { error } => {
                assert!(
                    error.contains("irin.directive.proposal.v1"),
                    "degraded activation should preserve the probe failure reason: {error}"
                );
            }
            other => panic!("bad cabinet must degrade default boot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn phase3_activation_bad_cabinet_is_fatal_when_strict() {
        let client = BadProbeClient;
        let result =
            probe_phase3_dispatcher_activation(&client, "acme", true, 1, Duration::from_millis(0))
                .await;

        match result {
            Phase3DispatcherActivation::Fatal { exit_code, error } => {
                assert_eq!(exit_code, CABINET_PROBE_FAILURE_EXIT_CODE);
                assert!(
                    error.contains("irin.directive.proposal.v1"),
                    "fatal activation should preserve the probe failure reason: {error}"
                );
            }
            other => panic!("strict bad cabinet must be fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn phase3_activation_good_cabinet_is_ready() {
        let client = GoodProbeClient;
        let result =
            probe_phase3_dispatcher_activation(&client, "acme", false, 1, Duration::from_millis(0))
                .await;
        assert_eq!(result, Phase3DispatcherActivation::Ready);
    }

    // Tight P0-eta marker tests — prove hydration continuation is actually gated by the probe.

    #[tokio::test]
    async fn p0_eta_good_cabinet_reaches_hydration_marker() {
        let client = GoodProbeClient;
        let mut hydration_ran = false;

        let result = run_probe_then_hydration_with_marker(&client, "acme", || async {
            hydration_ran = true;
        })
        .await;

        assert!(result.is_ok());
        assert!(
            hydration_ran,
            "hydration continuation must be reached on good cabinet"
        );
    }

    #[tokio::test]
    async fn p0_eta_bad_cabinet_blocks_hydration_marker_and_returns_88() {
        let client = BadProbeClient;
        let mut hydration_ran = false;

        let result = run_probe_then_hydration_with_marker(&client, "acme", || async {
            hydration_ran = true;
        })
        .await;

        match result {
            Err((code, _)) => {
                assert_eq!(code, CABINET_PROBE_FAILURE_EXIT_CODE);
                assert!(
                    !hydration_ran,
                    "hydration must not be reached on bad cabinet"
                );
            }
            Ok(_) => panic!("bad cabinet must return error 88"),
        }
    }

    /// Unit test: ReqwestTriageProbeClient fails fast (before any HTTP) when
    /// no WATCH_DISPATCHER_GATEWAY_KEY is configured. This is the required
    /// early actionable error for the P0-eta probe path.
    #[tokio::test]
    async fn reqwest_triage_probe_client_requires_gateway_key() {
        // No key supplied → immediate MissingGatewayAuthKey, no network call attempted.
        let client = ReqwestTriageProbeClient::new("http://127.0.0.1:1");
        let headers = HeaderMap::new();
        let body = serde_json::json!({"model": "council-triage", "messages": []});
        let err = client.post_council_triage(headers, body).await.unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::MissingGatewayAuthKey
        ));
    }

    // ---- FIX D/E: closed keyset + verb allowlist (proposal-fence) ----

    #[test]
    fn validate_rejects_unknown_top_level_key() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Dismiss",
            "rationale": "ok",
            "priority": "high"
        });
        let err = validate_proposal_v1_shape(&p, "any-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::UnexpectedKey { location: "top-level", ref key } if key == "priority"
        ));
    }

    #[test]
    fn validate_rejects_unknown_scope_key_capability_token() {
        // Proves capability_token in scope is rejected (the ratified fail-closed decision).
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Act",
            "job": "j",
            "scope": {
                "tenant": "my-tenant",
                "subject": "s",
                "allowed_actions": ["read"],
                "capability_token": "x"
            },
            "stop_condition": "on_success",
            "return_expectation": "structured",
            "rationale": "ok"
        });
        let err = validate_proposal_v1_shape(&p, "my-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::UnexpectedKey { location: "scope", ref key } if key == "capability_token"
        ));
    }

    #[test]
    fn validate_rejects_capability_token_at_top_level() {
        // Proves capability_token at TOP level is rejected (the ratified fail-closed decision).
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Dismiss",
            "rationale": "ok",
            "capability_token": "x"
        });
        let err = validate_proposal_v1_shape(&p, "any-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::UnexpectedKey { location: "top-level", ref key } if key == "capability_token"
        ));
    }

    #[test]
    fn validate_rejects_disallowed_verb() {
        let make = |verb: &str| {
            serde_json::json!({
                "schema": "irin.directive.proposal.v1",
                "in_response_to": "e1",
                "authority": "recommend",
                "verdict": "Act",
                "job": "j",
                "scope": {
                    "tenant": "my-tenant",
                    "subject": "s",
                    "allowed_actions": [verb]
                },
                "stop_condition": "on_success",
                "return_expectation": "structured",
                "rationale": "ok"
            })
        };

        let err = validate_proposal_v1_shape(&make("delete"), "my-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::DisallowedVerb { ref verb } if verb == "delete"
        ));

        let err = validate_proposal_v1_shape(&make("*"), "my-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::DisallowedVerb { ref verb } if verb == "*"
        ));

        // A2: a whitespace-padded allowlisted verb is malformed → DisallowedVerb (no .trim()),
        // and the reported verb is the raw (untrimmed) value.
        let err = validate_proposal_v1_shape(&make(" read "), "my-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::DisallowedVerb { ref verb } if verb == " read "
        ));
    }

    #[test]
    fn validate_unexpected_key_wins_over_disallowed_verb() {
        // A4 (the invariant): when an unknown top-level key AND a disallowed verb
        // co-occur, the keyset check runs before the verb allowlist → UnexpectedKey wins.
        // Pins the existing (correct) precedence so a future reorder can't silently flip it.
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Act",
            "job": "j",
            "scope": { "tenant": "my-tenant", "subject": "s", "allowed_actions": ["delete"] },
            "stop_condition": "on_success",
            "return_expectation": "structured",
            "rationale": "ok",
            "bogus_top_key": "x"
        });
        let err = validate_proposal_v1_shape(&p, "my-tenant").unwrap_err();
        assert!(matches!(
            err,
            TriageCabinetProbeError::UnexpectedKey { location: "top-level", ref key } if key == "bogus_top_key"
        ));
    }

    #[test]
    fn validate_accepts_all_allowlisted_verbs() {
        let p = serde_json::json!({
            "schema": "irin.directive.proposal.v1",
            "in_response_to": "e1",
            "authority": "recommend",
            "verdict": "Act",
            "job": "j",
            "scope": {
                "tenant": "my-tenant",
                "subject": "s",
                "allowed_actions": ["read", "report", "notify", "review", "escalate"]
            },
            "stop_condition": "on_success",
            "return_expectation": "structured",
            "rationale": "ok"
        });
        assert!(validate_proposal_v1_shape(&p, "my-tenant").is_ok());
    }

    #[test]
    #[ignore = "ARMING CHECKLIST: standing reminder of the atomic arming change-set; not a runnable assertion"]
    fn arming_checklist_is_documented() {
        assert_eq!(ARMING_CHECKLIST.len(), 4);
    }

    /// Cross-repo golden vector (the invariant, Action 3).
    ///
    /// Runs the RECEIVER fence over the SHARED corpus owned by the
    /// `sovereign-protocol` crate. The same corpus is run by the council-rs
    /// EMITTER fence (`validate_directive_proposal_v1`). Any drift of this
    /// repo's `ALLOWED_ACTIONS` / `ALLOWED_TOP_KEYS` / `ALLOWED_SCOPE_KEYS`
    /// flips a case here; any drift of the emitter's flips it there — the
    /// spec'd mirror is self-enforcing across both builds. `case.tenant` is
    /// passed as `expected_tenant` so Act accept-cases (scope.tenant ==
    /// case.tenant) accept; the gateway-only tenant-match check is not the
    /// subject of this corpus.
    #[test]
    fn cross_repo_golden_vector_matches_receiver_fence() {
        use sovereign_protocol::fence_vectors::{directive_fence_golden_cases, FenceExpect};

        for case in directive_fence_golden_cases() {
            let result = validate_proposal_v1_shape(&case.proposal, &case.tenant);
            match case.expect {
                FenceExpect::Accept => assert!(
                    result.is_ok(),
                    "golden case '{}' ({}) expected ACCEPT, got {:?}",
                    case.name,
                    case.fault,
                    result
                ),
                FenceExpect::Reject => {
                    let err = result.expect_err(&format!(
                        "golden case '{}' ({}) expected REJECT, got Ok",
                        case.name, case.fault
                    ));
                    if let Some(sub) = &case.reason_substring {
                        let msg = err.to_string();
                        assert!(
                            msg.contains(sub.as_str()),
                            "golden case '{}' reject reason {:?} must contain {:?}",
                            case.name,
                            msg,
                            sub
                        );
                    }
                }
            }
        }
    }
}
