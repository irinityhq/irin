//! Global request rate limiter for the sidecar UDS (audit F-3, defense-in-depth).
//!
//! The sidecar binds a Unix domain socket locked to mode 0o660 + a trusted gid,
//! so the blast radius is local processes in the gateway group. Before this, the
//! only rate limiting lived in `AuthService::check`, reachable solely via
//! `/auth/check` (the `/v1/` fast path). Every other internal route — `/guard/*`,
//! `/ledger/*`, `/cache/*`, `/route/*`, `/budget/*`, `/policy/*`, `/council/*`,
//! `/watch/*`, `/librarian/*`, `/admin/*`, `/vertex/token` — had no limit at all,
//! so a runaway or compromised local client could flood them unbounded.
//!
//! This adds one global token bucket in front of the whole router as a flood
//! backstop. It is intentionally a single shared bucket with a HIGH default
//! ceiling, NOT a production throttle.
//!
//! NOT doing here (scoped out by design — this item is rated LOW /
//! defense-in-depth because the socket is already 0o660 + gid-gated):
//! - per-peer (`SO_PEERCRED`) keying — overkill for a 0o660/gid-gated socket; the
//!   socket permissions already bound *who* may connect. Revisit only if multiple
//!   mutually-distrusting local callers ever share the socket.
//! - replacing the per-key/per-IP buckets in `AuthService::check` — those still
//!   govern the `/v1/` proxy path; this layer is additive, not a replacement.
//! - throttling `/health` — liveness must never 429.

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::auth::TokenBucket;

/// Default global ceiling: 6000 req/min (100 req/s) across all non-health routes.
/// High enough not to interfere with the Lua metrics poller (`/council/stats`,
/// `/watch/stats`) or `/auth/check` throughput; low enough to cap a local flood.
/// Override with `SIDECAR_GLOBAL_RPM`.
pub const DEFAULT_GLOBAL_RPM: u32 = 6000;

/// Resolve the global RPM from an env string. Falls back to the default on
/// absent / empty / non-numeric / zero input. Zero is treated as a misconfig
/// (it would mean "deny everything") rather than honored, so a fat-fingered
/// `SIDECAR_GLOBAL_RPM=0` cannot brick the sidecar.
pub fn resolve_global_rpm(raw: Option<String>) -> u32 {
    raw.and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_GLOBAL_RPM)
}

/// Shared global token bucket, cloneable into the axum layer state.
#[derive(Clone)]
pub struct GlobalRateLimiter {
    bucket: Arc<Mutex<TokenBucket>>,
    rpm: u32,
}

impl GlobalRateLimiter {
    /// Build with an explicit RPM. Burst capacity == rpm (tolerate a one-minute
    /// burst at the steady rate before shaping kicks in).
    pub fn new(rpm: u32) -> Self {
        Self {
            bucket: Arc::new(Mutex::new(TokenBucket::new(0, rpm))),
            rpm,
        }
    }

    /// Seconds a rejected caller should wait before retrying: one token's refill
    /// period at the steady rate (`60 / rpm`), floored at 1s. Coarse but honest
    /// backoff guidance for the `Retry-After` header.
    pub fn retry_after_secs(&self) -> u64 {
        ((60.0 / self.rpm as f64).ceil() as u64).max(1)
    }

    /// Build from `SIDECAR_GLOBAL_RPM` (or the default).
    pub fn from_env() -> Self {
        Self::new(resolve_global_rpm(std::env::var("SIDECAR_GLOBAL_RPM").ok()))
    }

    /// Try to admit one request. `false` → bucket empty (caller should 429).
    pub fn try_admit(&self) -> bool {
        // Mutex poisoning (a panic while another request held the lock) must not
        // wedge the gateway: recover the guard and keep serving.
        let mut bucket = self.bucket.lock().unwrap_or_else(|p| p.into_inner());
        bucket.consume(1.0)
    }
}

/// Axum middleware: admit non-`/health` requests through the global bucket, or
/// reject with `429 Too Many Requests`. Wire as the outermost layer so floods
/// are shed before any per-route work runs.
pub async fn global_rate_limit(
    State(limiter): State<GlobalRateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    if !limiter.try_admit() {
        let mut resp = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "rate_limited",
                "detail": "sidecar global rate limit exceeded; retry shortly",
            })),
        )
            .into_response();
        if let Ok(val) = limiter.retry_after_secs().to_string().parse() {
            resp.headers_mut().insert(header::RETRY_AFTER, val);
        }
        return resp;
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_on_absent_empty_garbage_and_zero() {
        assert_eq!(resolve_global_rpm(None), DEFAULT_GLOBAL_RPM);
        assert_eq!(resolve_global_rpm(Some("".into())), DEFAULT_GLOBAL_RPM);
        assert_eq!(resolve_global_rpm(Some("  ".into())), DEFAULT_GLOBAL_RPM);
        assert_eq!(resolve_global_rpm(Some("abc".into())), DEFAULT_GLOBAL_RPM);
        assert_eq!(resolve_global_rpm(Some("-5".into())), DEFAULT_GLOBAL_RPM);
        assert_eq!(resolve_global_rpm(Some("0".into())), DEFAULT_GLOBAL_RPM);
    }

    #[test]
    fn resolve_honors_valid_value_with_whitespace() {
        assert_eq!(resolve_global_rpm(Some("120".into())), 120);
        assert_eq!(resolve_global_rpm(Some(" 250 ".into())), 250);
    }

    #[test]
    fn bucket_admits_up_to_capacity_then_rejects() {
        // capacity == rpm; refill over microseconds between calls is << 1 token,
        // so the (capacity+1)th call within the same instant is rejected.
        let limiter = GlobalRateLimiter::new(3);
        assert!(limiter.try_admit());
        assert!(limiter.try_admit());
        assert!(limiter.try_admit());
        assert!(
            !limiter.try_admit(),
            "4th request must exceed capacity of 3"
        );
    }

    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt; // oneshot

    fn test_app(rpm: u32) -> Router {
        let limiter = GlobalRateLimiter::new(rpm);
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/work", get(|| async { "done" }))
            .layer(axum::middleware::from_fn_with_state(
                limiter,
                global_rate_limit,
            ))
    }

    async fn status_of(app: &Router, path: &str) -> StatusCode {
        app.clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn non_health_route_429s_after_capacity() {
        let app = test_app(1);
        assert_eq!(status_of(&app, "/work").await, StatusCode::OK);
        assert_eq!(
            status_of(&app, "/work").await,
            StatusCode::TOO_MANY_REQUESTS,
            "2nd /work within the same instant must exceed capacity of 1"
        );
    }

    #[tokio::test]
    async fn rejected_request_carries_retry_after_header() {
        let app = test_app(1);
        assert_eq!(status_of(&app, "/work").await, StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/work").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("429 must carry Retry-After");
        // rpm=1 → 60/1 = 60s.
        assert_eq!(retry.to_str().unwrap(), "60");
    }

    #[test]
    fn retry_after_is_one_second_floor_at_high_rpm() {
        assert_eq!(GlobalRateLimiter::new(6000).retry_after_secs(), 1);
        assert_eq!(GlobalRateLimiter::new(60).retry_after_secs(), 1);
        assert_eq!(GlobalRateLimiter::new(30).retry_after_secs(), 2);
    }

    #[tokio::test]
    async fn health_is_exempt_even_when_bucket_drained() {
        let app = test_app(1);
        // Drain the single token on a non-health route.
        assert_eq!(status_of(&app, "/work").await, StatusCode::OK);
        assert_eq!(
            status_of(&app, "/work").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // /health must still pass — liveness never 429s.
        for _ in 0..5 {
            assert_eq!(status_of(&app, "/health").await, StatusCode::OK);
        }
    }
}
