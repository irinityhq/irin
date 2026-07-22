//! D2 — server-side structural validation of the `irin.directive.proposal.v1`
//! machine-output fence on the council-triage path.
//!
//! The triage Chair (`synthesis_mode: directive_proposal_v1`) is *instructed* to
//! emit exactly one `irin.directive.proposal.v1` JSON fence. This module is the
//! enforcement seam: `deliberate_handler` calls [`validate_directive_proposal_v1`]
//! on `session.synthesis` before returning 200, so a malformed proposal becomes a
//! structured 422 here instead of a silent dead-letter on the gateway side.
//!
//! ## Validation boundary (council-rs vs gateway)
//!
//! council-rs validates the **structure** of the fenced object — and deliberately
//! **mirrors the shared proposal.v1 shape rules the gateway already enforces**
//! (`gateway::watch::startup_probe::validate_proposal_v1_shape`, also invoked by the
//! live dispatcher, per Sentinel spec phase3-closed-signal-loop §3.2.1). Mirroring
//! is the whole point of D2: a proposal that passes here but fails the gateway shape
//! check would be a 200-wrapped directive that dead-letters downstream — exactly the
//! class D2 exists to close. Checked here:
//!   - exactly one `` ```json `` fence is present and parses as a JSON object
//!   - `schema == "irin.directive.proposal.v1"`, `authority == "recommend"`
//!   - `verdict ∈ {"Act","Dismiss"}`, `in_response_to` is a non-empty string
//!   - `rationale` is a non-empty string (required for **both** verdicts)
//!   - `Act` carries non-empty `job` / `stop_condition` / `return_expectation`, a
//!     `scope` object with a non-empty `tenant` + non-empty `subject`, and a
//!     non-empty `allowed_actions` array drawn from the [`ALLOWED_ACTIONS`] allowlist
//!   - `Dismiss` carries none of the four Act-only fields with a non-null value
//!     (absent or `null` is accepted, mirroring the gateway)
//!   - **closed keyset** (`additionalProperties:false`): every top-level key is in
//!     [`ALLOWED_TOP_KEYS`] and every `scope` key in [`ALLOWED_SCOPE_KEYS`]. This
//!     subsumes the old `council_session_id` / `council_cost_usd` blocklist and
//!     structurally rejects authority-elevation keys (`capability_token`, `prepare`,
//!     `execute`, …) the recommend-only posture forbids. The gateway receiver MIRRORS
//!     this fence in `startup_probe.rs` — closed top+scope keyset, same verb allowlist,
//!     and no `.trim()` on the verb check. The two are intentionally
//!     exact-match, so a proposal accepted here is accepted there. Both over-reject in the
//!     safe direction (dead-letter, loud) rather than over-accept. At v0.2 arming the
//!     elevation keys move INTO the allowed set on BOTH sides together.
//!
//! The gateway remains the **second** validator for cross-field *semantics* it alone
//! can check against the live escalation — `in_response_to` exact-match and
//! `scope.tenant` exact-match against the originating escalation row.
//!
//! Per the storage contract in `deliberate.rs`, `session.synthesis` may legitimately
//! carry raw Chair chatter *around* the fence (the gateway outbox guard and dispatcher
//! "parse only the fenced proposal.v1"). So this validator **extracts** the fence
//! from surrounding text rather than rejecting prose — it does not enforce the
//! "nothing outside the fence" instruction, only that a single well-formed fence
//! exists and its structure is sound.

use serde_json::{Map, Value};

const SCHEMA_ID: &str = "irin.directive.proposal.v1";
/// Act-only fields. Required (non-empty) when `verdict == "Act"`; must be absent or
/// `null` when `verdict == "Dismiss"`. (`scope` is additionally shape-checked.)
const ACT_ONLY_KEYS: [&str; 4] = ["job", "scope", "stop_condition", "return_expectation"];

/// Closed top-level keyset (HARDEN-A). The fence is `additionalProperties:false`: any
/// top-level key outside this set is rejected. This *subsumes* the old two-key
/// `FORBIDDEN_KEYS` blocklist (`council_session_id` / `council_cost_usd`) and, more
/// importantly, structurally rejects the authority-elevation keys the Chair prompt
/// forbids — `capability_token`, `prepare`, `execute`, `tokens`, `priority`, `origin`,
/// and anything not yet imagined. A blocklist is whack-a-mole; a closed set is final.
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
/// Closed keyset for the `scope` object (HARDEN-A). `capability_token` or any metadata
/// smuggled inside `scope` is rejected here.
const ALLOWED_SCOPE_KEYS: [&str; 3] = ["tenant", "subject", "allowed_actions"];

/// Allowlist for `scope.allowed_actions` (HARDEN-B). Minimal, safe, read-only-ish verbs.
/// A prompt-injected `["*","delete"]` would pass the old non-empty-strings check, persist
/// into precedent, and become a live grant under v0.2 arming — so the fence allowlists
/// rather than denylists. Allowlist > denylist for an authority surface. NOTE: this list
/// is what `--harden` settled on; widening it is a security decision, not a typo fix.
const ALLOWED_ACTIONS: [&str; 5] = ["read", "report", "notify", "review", "escalate"];

/// True iff `value` is a JSON string that is non-empty after trimming. Mirrors the
/// gateway's `is_none_or(|s| s.trim().is_empty())` rejection predicate (negated).
fn is_non_empty_str(value: Option<&Value>) -> bool {
    value
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

/// Extract the body of the single `` ```json `` fence in `synthesis`.
///
/// Line-aware (CommonMark-style fencing): an opener is a line whose trimmed text
/// is *exactly* `` ```json `` (info string exactly `json`) and the close is the next
/// line that is exactly `` ``` ``. This tolerates surrounding chatter the same way
/// the gateway extractor does, but — unlike naive substring matching — a literal
/// `` ```json `` appearing mid-line in prose or inside a JSON string value is not
/// mistaken for a fence, and `` ```jsonc ``/`` ```json-extra `` info strings are not
/// accepted. Errors if zero fences are present, more than one is present
/// (ambiguous), or the fence is unterminated.
fn extract_single_json_fence(synthesis: &str) -> Result<String, String> {
    let lines: Vec<&str> = synthesis.lines().collect();
    let openers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == "```json")
        .map(|(i, _)| i)
        .collect();

    match openers.len() {
        0 => return Err("no ```json directive proposal fence found".into()),
        1 => {}
        n => {
            return Err(format!(
                "ambiguous output: {n} ```json fences (expected exactly one)"
            ));
        }
    }

    let open = openers[0];
    let close = lines[open + 1..]
        .iter()
        .position(|l| l.trim() == "```")
        .map(|rel| open + 1 + rel)
        .ok_or("unterminated ```json fence (missing closing ```)")?;

    Ok(lines[open + 1..close].join("\n"))
}

/// Return the first key in `map` that is not in `allowed` (HARDEN-A closed keyset).
/// Both object levels that a valid proposal can contain — the top level and `scope` —
/// are closed sets, so this top-level-per-object check covers every nesting a
/// well-formed proposal reaches (`job`/`stop_condition`/`return_expectation` are
/// strings; `allowed_actions` is an array of strings — none carry nested objects that
/// survive the other field checks).
fn find_unexpected_key(map: &Map<String, Value>, allowed: &[&str]) -> Option<String> {
    map.keys().find(|k| !allowed.contains(&k.as_str())).cloned()
}

/// Validate the `Act`-arm `scope` object: it must have a non-empty `tenant`, a
/// non-empty `subject`, and a non-empty `allowed_actions` array of non-empty
/// strings. Mirrors the gateway's Act-scope checks exactly.
fn validate_act_scope(obj: &Map<String, Value>) -> Result<(), String> {
    let scope = obj
        .get("scope")
        .and_then(|s| s.as_object())
        .ok_or("verdict=Act requires a scope object")?;

    // HARDEN-A: closed scope keyset — no capability_token / metadata smuggled in scope.
    if let Some(key) = find_unexpected_key(scope, &ALLOWED_SCOPE_KEYS) {
        return Err(format!(
            "unexpected scope key \"{key}\" (closed keyset; scope permits only tenant, subject, allowed_actions)"
        ));
    }

    if !is_non_empty_str(scope.get("tenant")) {
        return Err("verdict=Act requires scope.tenant as a non-empty string".into());
    }
    if !is_non_empty_str(scope.get("subject")) {
        return Err("verdict=Act requires scope.subject as a non-empty string".into());
    }
    match scope.get("allowed_actions").and_then(|v| v.as_array()) {
        Some(arr)
            if !arr.is_empty()
                && arr
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())) => {}
        _ => {
            return Err(
                "verdict=Act requires scope.allowed_actions as a non-empty array of non-empty strings"
                    .into(),
            );
        }
    }
    // HARDEN-B: allowed_actions allowlist. A prompt-injected "*"/"delete"/"execute"
    // passes the non-empty check above but is a stored time-bomb under v0.2 arming.
    if let Some(bad) = scope
        .get("allowed_actions")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                // A3 (gateway merge the invariant): no .trim() — a whitespace-padded
                // verb is malformed → disallowed. Aligns with the gateway receiver
                // (startup_probe.rs ALLOWED_ACTIONS check); the two fences now match exactly.
                .find(|a| !ALLOWED_ACTIONS.contains(a))
        })
    {
        return Err(format!(
            "scope.allowed_actions contains disallowed verb \"{bad}\" (allowlist: {}; \"*\"/delete/write/execute/etc. are rejected)",
            ALLOWED_ACTIONS.join(", ")
        ));
    }
    Ok(())
}

/// Structurally validate the directive-proposal fence carried in `synthesis`.
///
/// Returns `Ok(())` when the fence is a well-formed `irin.directive.proposal.v1`
/// object matching the shared gateway shape; `Err(reason)` with a human-readable
/// cause otherwise. The caller maps the error to a 422 before returning 200.
pub fn validate_directive_proposal_v1(synthesis: &str) -> Result<(), String> {
    let body = extract_single_json_fence(synthesis)?;

    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("fence body is not valid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or("fence body must be a JSON object")?;

    if obj.get("schema").and_then(|v| v.as_str()) != Some(SCHEMA_ID) {
        return Err(format!("schema must be \"{SCHEMA_ID}\""));
    }
    // ── v0.2 ARMING CHECKLIST (D2b deferral; the invariant) ──────────
    // This recommend-only gate is the load-bearing control that makes capability_token
    // structurally inert. Turning on prepare/execute arming requires FOUR co-located
    // edits, all of which fail loud if forgotten:
    //   1. Relax THIS gate to accept "prepare"/"execute" (with capability-token verify).
    //   2. Add "capability_token" to ALLOWED_TOP_KEYS (else HARDEN-A keyset rejects it).
    //   3. Flip the intentionally-red negative tests: rejects_capability_token /
    //      rejects_prepare_authority / rejects_execute_authority (they fire here).
    //   4. Relax the Chair prompt's recommend-only restriction (engine/deliberate.rs).
    // The gateway dispatcher ALREADY supports the arming path — this fence is the
    // intentional recommend-only narrowing, not a gateway limitation.
    if obj.get("authority").and_then(|v| v.as_str()) != Some("recommend") {
        return Err("authority must be \"recommend\"".into());
    }

    let verdict = obj
        .get("verdict")
        .and_then(|v| v.as_str())
        .ok_or("verdict must be a string")?;
    if verdict != "Act" && verdict != "Dismiss" {
        return Err(format!(
            "verdict must be \"Act\" or \"Dismiss\" (got {verdict:?})"
        ));
    }

    if !is_non_empty_str(obj.get("in_response_to")) {
        return Err("in_response_to must be a non-empty string".into());
    }
    // rationale is always required (Act AND Dismiss) per spec §3.2.1.
    if !is_non_empty_str(obj.get("rationale")) {
        return Err("rationale must be a non-empty string (required for Act and Dismiss)".into());
    }

    // HARDEN-A: closed top-level keyset. Rejects council_session_id / council_cost_usd
    // (old blocklist, now a subset) AND capability_token / prepare / execute / tokens /
    // priority / origin / anything-not-listed — structurally, not by enumeration.
    if let Some(key) = find_unexpected_key(obj, &ALLOWED_TOP_KEYS) {
        return Err(format!(
            "unexpected top-level key \"{key}\" (closed keyset; e.g. capability_token, prepare, execute, council_session_id, council_cost_usd are not permitted)"
        ));
    }

    if verdict == "Act" {
        for key in ["job", "stop_condition", "return_expectation"] {
            if !is_non_empty_str(obj.get(key)) {
                return Err(format!(
                    "verdict=Act requires \"{key}\" as a non-empty string"
                ));
            }
        }
        validate_act_scope(obj)?;
    } else {
        // Dismiss: the Act-only fields must be absent or null (gateway parity — a
        // present-but-null field is tolerated and stripped downstream).
        for key in ACT_ONLY_KEYS {
            if obj.get(key).is_some_and(|v| !v.is_null()) {
                return Err(format!(
                    "verdict=Dismiss must omit key \"{key}\" (absent or null)"
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act_fence() -> String {
        r#"```json
{
  "schema": "irin.directive.proposal.v1",
  "authority": "recommend",
  "verdict": "Act",
  "in_response_to": "esc-123",
  "rationale": "the ledger gap is anomalous and warrants a look",
  "job": "investigate the ledger gap",
  "scope": { "tenant": "acme", "subject": "ledger.db", "allowed_actions": ["read", "report"] },
  "stop_condition": "gap explained or 30m elapsed",
  "return_expectation": "a written finding"
}
```"#
            .to_string()
    }

    fn dismiss_fence() -> String {
        r#"```json
{
  "schema": "irin.directive.proposal.v1",
  "authority": "recommend",
  "verdict": "Dismiss",
  "in_response_to": "esc-123",
  "rationale": "benign, no action warranted"
}
```"#
            .to_string()
    }

    #[test]
    fn accepts_well_formed_act() {
        assert!(validate_directive_proposal_v1(&act_fence()).is_ok());
    }

    #[test]
    fn accepts_well_formed_dismiss() {
        assert!(validate_directive_proposal_v1(&dismiss_fence()).is_ok());
    }

    #[test]
    fn accepts_dismiss_with_null_act_fields() {
        // Gateway parity: Dismiss tolerates Act-only fields present as null.
        let with_nulls = r#"```json
{
  "schema": "irin.directive.proposal.v1",
  "authority": "recommend",
  "verdict": "Dismiss",
  "in_response_to": "esc-123",
  "rationale": "benign",
  "job": null,
  "scope": null,
  "stop_condition": null,
  "return_expectation": null
}
```"#;
        assert!(
            validate_directive_proposal_v1(with_nulls).is_ok(),
            "Dismiss with explicit nulls must pass (gateway accepts and strips them)"
        );
    }

    #[test]
    fn extracts_fence_from_surrounding_chatter() {
        // Gateway tolerates chatter around the fence; so does this validator.
        let with_chatter = format!("Here is my ruling:\n\n{}\n\nThanks.", act_fence());
        assert!(validate_directive_proposal_v1(&with_chatter).is_ok());
    }

    #[test]
    fn rejects_missing_fence() {
        let err = validate_directive_proposal_v1("just prose, no fence").unwrap_err();
        assert!(err.contains("no ```json"), "got: {err}");
    }

    #[test]
    fn rejects_multiple_fences() {
        let two = format!("{}\n{}", dismiss_fence(), dismiss_fence());
        let err = validate_directive_proposal_v1(&two).unwrap_err();
        assert!(err.contains("ambiguous"), "got: {err}");
    }

    #[test]
    fn rejects_unterminated_fence() {
        let bad = "```json\n{ \"schema\": \"irin.directive.proposal.v1\"";
        let err = validate_directive_proposal_v1(bad).unwrap_err();
        assert!(err.contains("unterminated"), "got: {err}");
    }

    #[test]
    fn rejects_invalid_json() {
        let bad = "```json\n{ not json }\n```";
        let err = validate_directive_proposal_v1(bad).unwrap_err();
        assert!(err.contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn rejects_non_object_body() {
        let bad = "```json\n[1, 2, 3]\n```";
        let err = validate_directive_proposal_v1(bad).unwrap_err();
        assert!(err.contains("must be a JSON object"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_schema() {
        let bad = dismiss_fence().replace(SCHEMA_ID, "irin.directive.proposal.v2");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("schema must be"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_authority() {
        let bad = dismiss_fence().replace("\"recommend\"", "\"command\"");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("authority must be"), "got: {err}");
    }

    #[test]
    fn rejects_bad_verdict() {
        let bad = dismiss_fence().replace("\"Dismiss\"", "\"Maybe\"");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("verdict must be"), "got: {err}");
    }

    #[test]
    fn rejects_empty_in_response_to() {
        let bad = dismiss_fence().replace("\"esc-123\"", "\"\"");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("in_response_to"), "got: {err}");
    }

    #[test]
    fn rejects_dismiss_missing_rationale() {
        let bad =
            dismiss_fence().replace(",\n  \"rationale\": \"benign, no action warranted\"", "");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("rationale"), "got: {err}");
    }

    #[test]
    fn rejects_act_missing_rationale() {
        let bad = act_fence().replace(
            "  \"rationale\": \"the ledger gap is anomalous and warrants a look\",\n",
            "",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("rationale"), "got: {err}");
    }

    #[test]
    fn rejects_act_missing_required_key() {
        let bad = act_fence().replace(
            "  \"stop_condition\": \"gap explained or 30m elapsed\",\n",
            "",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("stop_condition"), "got: {err}");
    }

    #[test]
    fn rejects_act_empty_string_field() {
        let bad = act_fence().replace("\"investigate the ledger gap\"", "\"   \"");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("job"), "got: {err}");
    }

    #[test]
    fn rejects_act_scope_not_object() {
        let bad = act_fence().replace(
            "\"scope\": { \"tenant\": \"acme\", \"subject\": \"ledger.db\", \"allowed_actions\": [\"read\", \"report\"] }",
            "\"scope\": \"acme\"",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("scope object"), "got: {err}");
    }

    #[test]
    fn rejects_act_missing_scope_tenant() {
        let bad = act_fence().replace("\"tenant\": \"acme\", ", "");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("scope.tenant"), "got: {err}");
    }

    #[test]
    fn rejects_act_missing_scope_subject() {
        let bad = act_fence().replace("\"subject\": \"ledger.db\", ", "");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("scope.subject"), "got: {err}");
    }

    #[test]
    fn rejects_act_missing_allowed_actions() {
        let bad = act_fence().replace(", \"allowed_actions\": [\"read\", \"report\"]", "");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("allowed_actions"), "got: {err}");
    }

    #[test]
    fn rejects_act_empty_allowed_actions() {
        let bad = act_fence().replace("[\"read\", \"report\"]", "[]");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("allowed_actions"), "got: {err}");
    }

    #[test]
    fn rejects_act_allowed_actions_with_empty_string() {
        let bad = act_fence().replace("[\"read\", \"report\"]", "[\"read\", \"  \"]");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("allowed_actions"), "got: {err}");
    }

    #[test]
    fn rejects_dismiss_carrying_act_keys() {
        // A non-null Act-only field on a Dismiss must be rejected.
        let bad = dismiss_fence().replace(
            "\"rationale\": \"benign, no action warranted\"",
            "\"rationale\": \"benign\", \"job\": \"do a thing\"",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("must omit"), "got: {err}");
    }

    #[test]
    fn rejects_nested_forbidden_key() {
        // Forbidden keys must be rejected at any depth, not just
        // the top level. A nested `scope.council_cost_usd` must not slip through.
        let bad = act_fence().replace(
            "\"allowed_actions\": [\"read\", \"report\"]",
            "\"allowed_actions\": [\"read\", \"report\"], \"council_cost_usd\": 0.42",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("council_cost_usd"), "got: {err}");
    }

    #[test]
    fn tolerates_literal_fence_marker_in_prose() {
        // A literal "```json" appearing mid-line in chatter must
        // not be counted as a second fence (no false-positive "ambiguous" 422).
        let with_inline = format!(
            "I considered whether to emit a ```json block inline, but here is the ruling:\n\n{}",
            dismiss_fence()
        );
        assert!(
            validate_directive_proposal_v1(&with_inline).is_ok(),
            "inline mention of the fence marker must not be miscounted"
        );
    }

    #[test]
    fn rejects_non_json_info_string() {
        // "```jsonc" / "```json-extra" are not a `json` fence.
        let jsonc = dismiss_fence().replacen("```json", "```jsonc", 1);
        let err = validate_directive_proposal_v1(&jsonc).unwrap_err();
        assert!(err.contains("no ```json"), "got: {err}");
    }

    #[test]
    fn rejects_forbidden_council_session_id() {
        let bad = dismiss_fence().replace(
            "\"rationale\": \"benign, no action warranted\"",
            "\"rationale\": \"benign\", \"council_session_id\": \"abc\"",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("council_session_id"), "got: {err}");
    }

    #[test]
    fn rejects_forbidden_council_cost_usd() {
        let bad = dismiss_fence().replace(
            "\"rationale\": \"benign, no action warranted\"",
            "\"rationale\": \"benign\", \"council_cost_usd\": 0.42",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("council_cost_usd"), "got: {err}");
    }

    // ── HARDEN-A/B negative tests (the invariant) ──────────────────
    // Several of these are INTENTIONALLY red at v0.2 arming: when `capability_token`
    // and authority `prepare`/`execute` become legitimate, the author MUST relax
    // `directive_fence.rs:164` + add the key to ALLOWED_TOP_KEYS + flip these tests +
    // relax the Chair prompt. The red test fires at the exact line that must change.

    #[test]
    fn rejects_capability_token() {
        // HARDEN-A: capability_token is structurally inert under recommend (gateway
        // only honors it for prepare/execute, which authority!=recommend already
        // rejects) — but the closed keyset rejects it outright as defense-in-depth.
        // GOES RED AT v0.2 ARMING.
        let bad = dismiss_fence().replace(
            "\"rationale\": \"benign, no action warranted\"",
            "\"rationale\": \"benign\", \"capability_token\": \"tok-abc\"",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("capability_token"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let bad = dismiss_fence().replace(
            "\"rationale\": \"benign, no action warranted\"",
            "\"rationale\": \"benign\", \"priority\": \"high\"",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("priority"), "got: {err}");
    }

    #[test]
    fn rejects_prepare_authority() {
        // GOES RED AT v0.2 ARMING (authority elevation lands).
        let bad = dismiss_fence().replace("\"recommend\"", "\"prepare\"");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("authority must be"), "got: {err}");
    }

    #[test]
    fn rejects_execute_authority() {
        // GOES RED AT v0.2 ARMING (authority elevation lands).
        let bad = dismiss_fence().replace("\"recommend\"", "\"execute\"");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("authority must be"), "got: {err}");
    }

    #[test]
    fn rejects_unexpected_scope_key() {
        let bad = act_fence().replace(
            "\"allowed_actions\": [\"read\", \"report\"]",
            "\"allowed_actions\": [\"read\"], \"capability_token\": \"x\"",
        );
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("scope key"), "got: {err}");
    }

    #[test]
    fn rejects_wildcard_allowed_actions() {
        // HARDEN-B: the "*"/delete time-bomb that would persist into precedent.
        let bad = act_fence().replace("[\"read\", \"report\"]", "[\"read\", \"*\"]");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("disallowed verb"), "got: {err}");
    }

    #[test]
    fn rejects_dangerous_allowed_action_verb() {
        let bad = act_fence().replace("[\"read\", \"report\"]", "[\"read\", \"delete\"]");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("disallowed verb"), "got: {err}");
    }

    #[test]
    fn accepts_escalate_allowed_action() {
        // `escalate` is in the allowlist — must pass.
        let ok = act_fence().replace("[\"read\", \"report\"]", "[\"read\", \"escalate\"]");
        assert!(validate_directive_proposal_v1(&ok).is_ok());
    }

    #[test]
    fn rejects_whitespace_padded_allowed_action() {
        // A padded but otherwise allowlisted verb is malformed and must be rejected.
        // This pins cross-component symmetry with the Gateway receiver.
        let bad = act_fence().replace("[\"read\", \"report\"]", "[\"read\", \" report \"]");
        let err = validate_directive_proposal_v1(&bad).unwrap_err();
        assert!(err.contains("disallowed verb"), "got: {err}");
    }

    #[test]
    fn boot_probe_dismiss_passes_closed_keyset() {
        // Blind spot #1: the phase3-startup-probe-v1 path emits a minimal Dismiss.
        // If the closed keyset rejected it, startup itself would dead-letter. Its
        // exact key shape must survive the closed keyset.
        let probe = r#"```json
{
  "schema": "irin.directive.proposal.v1",
  "authority": "recommend",
  "verdict": "Dismiss",
  "in_response_to": "phase3-startup-probe-v1",
  "rationale": "startup probe — no action"
}
```"#;
        assert!(
            validate_directive_proposal_v1(probe).is_ok(),
            "boot-probe Dismiss must pass the closed keyset"
        );
    }
}
