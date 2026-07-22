//! Shared Watch API response helpers, admin-token compare, and canary tenant guard.
//!
//! Private helpers stay `pub(super)` so sibling modules can use them without
//! widening crate visibility.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sovereign_protocol::types::ProblemDetails;

fn into_problem_response(status: StatusCode, details: ProblemDetails) -> Response {
    let mut resp = (status, Json(details)).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    resp
}

pub(super) fn problem(status: StatusCode, title: &str, detail: &str) -> Response {
    into_problem_response(
        status,
        ProblemDetails::new(title, detail).with_status(status.as_u16()),
    )
}

pub(super) fn problem_with_tenant(
    status: StatusCode,
    title: &str,
    detail: &str,
    tenant: &str,
) -> Response {
    into_problem_response(
        status,
        ProblemDetails::new(title, detail)
            .with_status(status.as_u16())
            .with_extension("tenant", tenant),
    )
}

pub(super) fn problem_with_id(status: StatusCode, title: &str, detail: &str, id: &str) -> Response {
    into_problem_response(
        status,
        ProblemDetails::new(title, detail)
            .with_status(status.as_u16())
            .with_extension("id", id),
    )
}

pub(super) fn problem_with_tenant_id(
    status: StatusCode,
    title: &str,
    detail: &str,
    tenant: &str,
    id: &str,
) -> Response {
    into_problem_response(
        status,
        ProblemDetails::new(title, detail)
            .with_status(status.as_u16())
            .with_extension("tenant", tenant)
            .with_extension("id", id),
    )
}

pub(super) fn json_response<T: serde::Serialize>(status: StatusCode, data: T) -> Response {
    (status, Json(data)).into_response()
}

pub const FORCE_WAKE_DEFAULT_TENANT: &str = "sovereign";

/// Shared watch-plane admin-token comparator. This is the single fail-closed
/// bearer check for every admin-gated surface on the watch plane — the
/// directive outbox (claim/heartbeat/worker_ack/nack/ack, list/get plaintext,
/// tenant-policy), the arming kill switch (/watch/admin/producer/disarm), and
/// force-wake / clear-quarantine.
///
/// Properties:
/// * **Constant-time** (P2 — length-oracle parity with
///   `ArmPrincipals::authenticate`): both sides are SHA-256-hashed to a fixed
///   32-byte digest before the `subtle::ConstantTimeEq` compare, so no
///   length-dependent early return leaks where the tokens first diverge.
/// * **Length-bounded**: a provided bearer longer than 128 bytes is rejected
///   before hashing (real admin tokens are never that long). This caps the
///   work an unauthenticated caller can force us to do (unbounded SHA-256
///   input) and is length-only — it reveals nothing about the secret.
/// * **Fail-closed**: an empty configured token (no token set) and a missing
///   bearer both always reject.
pub(crate) fn admin_token_matches(expected: &str, provided: Option<&str>) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    if expected.is_empty() {
        return false;
    }
    let Some(given) = provided else {
        return false;
    };
    // Belt-and-suspenders: bound the hashing work. Real admin tokens are never
    // anywhere near this long; reject before SHA-256 so an attacker can't make
    // us hash unbounded input. Constant w.r.t. the secret (length-only).
    if given.len() > 128 {
        return false;
    }
    let expected_digest = Sha256::digest(expected.as_bytes());
    let given_digest = Sha256::digest(given.as_bytes());
    expected_digest.ct_eq(&given_digest).into()
}

/// Single-tenant authorization guard.
///
/// The outbox surface gates on a GLOBAL admin token, so a token holder could
/// otherwise target ANY tenant via `X-Tenant-Scope` (mutations) or the path
/// tenant (list/get/tenant-policy). Today only the `sovereign` tenant exists,
/// so the gap is dormant — but it would go LIVE SILENTLY the instant a second
/// tenant is configured. This converts that silent failure into a loud, fail-closed
/// 403 so the gap can never ship unnoticed.
///
/// The allowed tenant is a deployment setting (`WATCH_CANARY_TENANT`, resolved
/// once at boot via [`resolve_canary_tenant`]), defaulting to `sovereign`.
/// Until per-tenant capability tokens are available, every other tenant is
/// rejected with 403.
pub(crate) const CANARY_TENANT_DEFAULT: &str = FORCE_WAKE_DEFAULT_TENANT; // "sovereign"

/// Env var naming the single tenant the watch outbox surface accepts. Unset or
/// empty falls back to [`CANARY_TENANT_DEFAULT`] ("sovereign"), preserving the
/// fail-closed default.
pub(crate) const CANARY_TENANT_ENV_VAR: &str = "WATCH_CANARY_TENANT";

/// Pure, testable resolution core for the configured tenant. Takes the raw
/// `std::env::var` result so the policy
/// can be table-tested with NO env mutation (no `#[serial]` race). Semantics:
///
///   * `Err(VarError::NotPresent)` → `Ok(CANARY_TENANT_DEFAULT)` — UNSET stays a
///     silent default ("sovereign"); absence must never brick a deploy
///     (UNCHANGED from the original behavior).
///   * `Err(VarError::NotUnicode(_))` → `Err(_)` — a non-unicode value has no
///     valid tenant meaning; fail to boot rather than silently fall back.
///   * `Ok(s)` where `s.trim().is_empty()` → `Err(_)` — an EXPLICIT empty /
///     whitespace value is a misconfiguration (a deploy in that state would 403
///     every request); fail at boot, loud, not silently.
///   * `Ok(s)` → `Ok(s.trim().to_string())` — the trimmed configured value.
///
/// The ONLY new hard-fail vs. the original is an explicit-but-malformed var
/// (empty/whitespace/non-unicode). This GENERALIZES the guard; it does not
/// weaken it — the silent-default-on-absence path is preserved exactly.
pub fn resolve_canary_from(raw: Result<String, std::env::VarError>) -> Result<String, String> {
    match raw {
        Err(std::env::VarError::NotPresent) => Ok(CANARY_TENANT_DEFAULT.to_string()),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "{CANARY_TENANT_ENV_VAR} is set to a non-unicode value (no valid tenant meaning)"
        )),
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Err(format!(
                    "{CANARY_TENANT_ENV_VAR} is set but empty/whitespace (a deploy in this state \
                     would 403 every outbox request); unset it to use the default \
                     '{CANARY_TENANT_DEFAULT}', or set a real tenant"
                ))
            } else {
                Ok(trimmed.to_string())
            }
        }
    }
}

/// Resolve the configured canary tenant once at boot. Thin wrapper over the
/// pure [`resolve_canary_from`] core (which owns the fail-closed policy). Reads
/// [`CANARY_TENANT_ENV_VAR`]; `Ok` carries the tenant (default "sovereign" when
/// unset), `Err` carries a boot-abort reason for an explicit-but-malformed var.
/// Call ONCE at startup and store the resolved value on the watch state/config
/// so the guard does not re-read env per request.
pub fn resolve_canary_tenant() -> Result<String, String> {
    resolve_canary_from(std::env::var(CANARY_TENANT_ENV_VAR))
}

/// Reject any resolved tenant scope that is not the configured single tenant.
/// `configured` is the boot-resolved canary tenant (see [`resolve_canary_tenant`]),
/// defaulting to "sovereign". Returns `Some(Response)` (403 +
/// `single_tenant_violation`) on a foreign scope — after a loud
/// `tracing::error!` naming the rejected scope — and `None` when the scope
/// matches the configured tenant (proceed). `Option` not `Result` to avoid the
/// large-`Err` clippy lint (a `Response` is a big variant). This is a Wave-1
/// tripwire, removed in Wave 2.
pub(super) fn assert_canary_tenant(scope: &str, configured: &str) -> Option<Response> {
    if scope != configured {
        tracing::error!(
            rejected_scope = %scope,
            canary_tenant = %configured,
            "single-tenant tripwire fired: outbox surface refused a non-canary tenant scope \
             (Wave-1 guard; per-tenant capability tokens land in Wave 2)"
        );
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "single_tenant_violation"})),
            )
                .into_response(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{resolve_canary_from, CANARY_TENANT_DEFAULT};
    use std::env::VarError;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // W1 re-gate P0 (review): table-test the PURE resolution core.
    // No env mutation, no #[serial] — the policy is exercised by feeding the
    // `std::env::var` Result shape directly. This closes the "fallback parse has
    // zero direct coverage" P0.
    #[test]
    fn resolve_canary_from_policy_table() {
        // A non-unicode OsString → VarError::NotUnicode. 0x80 is a lone
        // continuation byte, invalid UTF-8.
        let non_unicode = VarError::NotUnicode(OsString::from_vec(vec![0x80]));

        // `expected`: Some(tenant) = expect Ok(tenant); None = expect Err (boot-abort).
        let cases: Vec<(Result<String, VarError>, Option<&str>)> = vec![
            // UNSET → silent default "sovereign" (UNCHANGED; absence must not brick boot).
            (Err(VarError::NotPresent), Some(CANARY_TENANT_DEFAULT)),
            // Explicit empty → boot-abort.
            (Ok(String::new()), None),
            // Explicit whitespace-only → boot-abort.
            (Ok("   ".to_string()), None),
            // Real value → trimmed value.
            (Ok("phase3-smoke".to_string()), Some("phase3-smoke")),
            // Surrounding whitespace trimmed.
            (Ok("  sovereign  ".to_string()), Some("sovereign")),
            // Non-unicode → boot-abort.
            (Err(non_unicode), None),
        ];

        for (raw, expected) in cases {
            let label = format!("{raw:?}");
            let got = resolve_canary_from(raw);
            match expected {
                Some(want) => assert_eq!(
                    got.as_deref(),
                    Ok(want),
                    "expected Ok({want:?}) for input {label}"
                ),
                None => assert!(
                    got.is_err(),
                    "expected Err (boot-abort) for input {label}, got {got:?}"
                ),
            }
        }
    }

    // Defense-in-depth: the default fallback is exactly the historical
    // hard-coded value — proves the "unset stays sovereign" guarantee is wired
    // to the same const the guard compares against.
    #[test]
    fn resolve_canary_from_unset_is_historical_default() {
        assert_eq!(
            resolve_canary_from(Err(VarError::NotPresent)).as_deref(),
            Ok("sovereign")
        );
        assert_eq!(CANARY_TENANT_DEFAULT, "sovereign");
    }
}
