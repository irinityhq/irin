//! Clock-skew circuit breaker shared by Outbox storage and staged recovery.
//!
//! The cap and the rejection counter sit below both callers. Outbox storage
//! does not reach up into the dispatcher for either.

/// P2 — clock-skew circuit-breaker cap on the created-time normalization delta
/// (`outbox_insert_with_skew_normalize`). A normalization delta above this many ms means the
/// per-tenant monotonic floor (`prior_max`) is poisoned by an NTP forward-glitch (or this row's
/// clock jumped backward by more than the cap) — staging would float the absolute auth window
/// forward by the delta, defeating the auth-window policy on the money path. The breaker refuses
/// to stage past this (fail-safe: blocks dispatch, never spends). Pick a cap ABOVE any plausible
/// legitimate skew (NTP step correction is sub-second; a same-millisecond same-tenant burst nudges
/// created_at forward only ~1ms per row) and BELOW the authorization horizon
/// (`DIRECTIVE_STAGE_TTL_MS`, default 90s, min 30s) so the cap can never silently swallow a
/// window's worth of skew. Default 5000ms (5s). Env-tunable per the T21c remediation-speed
/// precedent. Clamped to [1000, 10000]: floor 1s keeps a load-burst of monotonic +1ms bumps from
/// tripping it; ceiling 10s stays strictly below the 30s minimum TTL horizon.
pub const MAX_ALLOWED_SKEW_MS_DEFAULT: i64 = 5_000;
pub const MAX_ALLOWED_SKEW_MS_MIN: i64 = 1_000;
pub const MAX_ALLOWED_SKEW_MS_MAX: i64 = 10_000;

/// Pure clamp — unit-testable without touching process env.
pub fn clamp_max_allowed_skew_ms(raw: Option<i64>) -> i64 {
    match raw {
        None => MAX_ALLOWED_SKEW_MS_DEFAULT,
        Some(v) => {
            let clamped = v.clamp(MAX_ALLOWED_SKEW_MS_MIN, MAX_ALLOWED_SKEW_MS_MAX);
            if clamped != v {
                tracing::warn!(
                    requested = v,
                    clamped,
                    min = MAX_ALLOWED_SKEW_MS_MIN,
                    max = MAX_ALLOWED_SKEW_MS_MAX,
                    "MAX_ALLOWED_SKEW_MS out of band; clamped"
                );
            }
            clamped
        }
    }
}

pub fn max_allowed_skew_ms() -> i64 {
    clamp_max_allowed_skew_ms(
        std::env::var("MAX_ALLOWED_SKEW_MS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok()),
    )
}

/// P2 — count of directives REFUSED at stage time because their created-time
/// normalization delta exceeded `MAX_ALLOWED_SKEW_MS` (clock-skew circuit-breaker in
/// `outbox_insert_with_skew_normalize`). The breaker fails safe (refuses to stage, never
/// spends), so without this counter a poisoned per-tenant `prior_max` would silently reject
/// every later directive for that tenant with no operator signal. A non-zero, growing value
/// means a host clock glitched forward and poisoned the monotonic floor — page on it. Bumped
/// at the point of refusal (the rejecting tx rolls back; the refusal itself is the event).
static DIRECTIVE_CLOCK_SKEW_REJECTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn directive_clock_skew_rejected_total() -> u64 {
    DIRECTIVE_CLOCK_SKEW_REJECTED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn bump_directive_clock_skew_rejected(n: u64) {
    DIRECTIVE_CLOCK_SKEW_REJECTED.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}
