//! Staged-directive authorization window.
//!
//! The skew-cap ceiling is asserted in this module so clock-skew policy does
//! not depend on the TTL, while an operator-tunable cap still cannot swallow
//! a whole authorization window.

/// A4a/T21c — the staged-directive authorization TTL: `expires_at_ms = now + this`.
/// The worker-leg dispatch fence (`WatchDb::claim_outbox`) refuses to dispatch a
/// directive past this window. Env-tunable so an operator can WIDEN it at RUNTIME
/// when `gw_watch_directive_ttl_expired_total` fires, instead of waiting for a
/// rebuild+redeploy (Council T21c ruling: remediation must not be slower than the
/// failure on a money path). Default-ON posture preserved (the fence is always on;
/// only the window is tunable). Default 90s, clamped to [30s, 300s].
pub const DIRECTIVE_STAGE_TTL_MS_DEFAULT: i64 = 90_000;
pub const DIRECTIVE_STAGE_TTL_MS_MIN: i64 = 30_000;
pub const DIRECTIVE_STAGE_TTL_MS_MAX: i64 = 300_000;

/// Pure clamp — unit-testable without touching process env.
pub fn clamp_stage_ttl_ms(raw: Option<i64>) -> i64 {
    match raw {
        None => DIRECTIVE_STAGE_TTL_MS_DEFAULT,
        Some(v) => {
            let clamped = v.clamp(DIRECTIVE_STAGE_TTL_MS_MIN, DIRECTIVE_STAGE_TTL_MS_MAX);
            if clamped != v {
                tracing::warn!(
                    requested = v,
                    clamped,
                    min = DIRECTIVE_STAGE_TTL_MS_MIN,
                    max = DIRECTIVE_STAGE_TTL_MS_MAX,
                    "DIRECTIVE_STAGE_TTL_MS out of band; clamped"
                );
            }
            clamped
        }
    }
}

pub fn directive_stage_ttl_ms() -> i64 {
    clamp_stage_ttl_ms(
        std::env::var("DIRECTIVE_STAGE_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok()),
    )
}

// Compile-time invariant: the skew-cap ceiling must stay strictly below the minimum
// authorization horizon (`DIRECTIVE_STAGE_TTL_MS_MIN`), so no operator-tunable skew cap can
// ever be set to a value that swallows a whole TTL window's worth of skew.
const _: () =
    assert!(crate::watch::clock_skew::MAX_ALLOWED_SKEW_MS_MAX < DIRECTIVE_STAGE_TTL_MS_MIN);
