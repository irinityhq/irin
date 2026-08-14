// ==========================================================================
// routes — HTTP handlers + router assembly (moved from main.rs).
// Pure structure extract: handler bodies and route table unchanged.
// ==========================================================================

mod auth;
mod budget;
mod cache;
mod guard;
mod health;
mod ledger;
mod librarian;
mod policy;
mod routing;
mod vertex;
mod watch;

use axum::{
    middleware as axum_mw,
    response::IntoResponse,
    routing::{delete, get, post},
    Router,
};
use std::sync::Arc;
use std::time::Duration;

use crate::AppState;

async fn request_id_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum_mw::Next,
) -> impl IntoResponse {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let span = tracing::info_span!("request", request_id = %request_id);
    let _guard = span.enter();
    drop(_guard);

    let resp = {
        let _entered = span.enter();
        next.run(req).await
    };
    resp
}

/// Inputs required to assemble the axum router (beyond AppState).
pub(crate) struct BuildRouterParts {
    pub state: Arc<AppState>,
    pub watch_quarantine: Arc<crate::watch::quarantine::QuarantineState>,
    pub arm_principals: Arc<crate::watch::api::ArmPrincipals>,
    pub arm_stage_ttl: Duration,
    pub watch_admin_token: String,
    pub arm_notifier: Arc<crate::watch::api::ArmNotifier>,
    pub arm_deviation: Arc<crate::watch::api::ArmDeviationTags>,
    pub attest_keys: Arc<crate::watch::attest::AttestKeyRegistry>,
}

/// Audit F-1: `/guard/scan` registers only when this returns true.
///
/// Pure env-seam (no process-global mutation) so unit tests can pin the gate
/// without racing other tests. Production passes
/// `std::env::var("GATEWAY_DEBUG_GUARD_SCAN").ok().as_deref()`.
pub(crate) fn guard_scan_enabled_from(raw: Option<&str>) -> bool {
    raw == Some("1")
}

/// Assemble the full UDS router. Route registrations match main.rs order.
pub(crate) fn build_router(parts: BuildRouterParts) -> Router {
    let BuildRouterParts {
        state,
        watch_quarantine,
        arm_principals,
        arm_stage_ttl,
        watch_admin_token,
        arm_notifier,
        arm_deviation,
        attest_keys,
    } = parts;

    let mut app = Router::new()
        .route("/health", get(health::health))
        // Guard endpoints
        .route("/guard/input", post(guard::guard_input))
        .route("/guard/tool", post(guard::guard_tool))
        .route("/guard/sovereignty", post(guard::guard_sovereignty))
        // Ledger endpoint
        .route("/ledger/record", post(ledger::record_ledger))
        .route("/ledger/verify", get(ledger::ledger_verify))
        .route("/ledger/export", get(ledger::ledger_export))
        // Cache endpoints
        .route("/cache/check", post(cache::cache_check))
        .route("/cache/store", post(cache::cache_store))
        // Routing endpoints
        .route("/route/decide", post(routing::route_decide))
        .route("/route/outcome", post(routing::route_outcome))
        // Budget endpoints
        .route("/budget/check", post(budget::budget_check))
        .route("/budget/record", post(budget::budget_record))
        // Policy endpoint
        .route("/policy/evaluate", post(policy::policy_evaluate))
        // Auth / Admin endpoints
        .route("/auth/check", post(auth::auth_check))
        .route("/auth/ip-check", post(auth::auth_ip_check))
        .route("/admin/keys", post(auth::admin_provision_key))
        .route("/admin/keys/revoke", post(auth::admin_revoke_key))
        .route("/auth/rotate", post(auth::auth_rotate_key))
        // Vertex ADC token endpoint
        .route("/vertex/token", get(vertex::vertex_token_handler))
        // Council endpoint (spec §5.8): per-caller concurrency + idempotency
        // for Phase 0.5 council-* models. The Lua router calls these UDS
        // endpoints in a peek → lock → claim sequence and a paired
        // unlock + store-or-fail cleanup from cost.lua's log phase.
        .route("/council/lock", post(crate::council::council_lock))
        .route("/council/unlock", post(crate::council::council_unlock))
        .route(
            "/council/idempotency/peek",
            post(crate::council::council_idem_peek),
        )
        .route(
            "/council/idempotency/claim",
            post(crate::council::council_idem_claim),
        )
        .route(
            "/council/idempotency/store",
            post(crate::council::council_idem_store),
        )
        .route(
            "/council/idempotency/fail",
            post(crate::council::council_idem_fail),
        )
        // P1-C: scrape target for the Lua poller that surfaces council
        // counters (active_swept_total, unlock_missing_grant_total) +
        // gauges (active_locks, active_caller_keys) on /metrics.
        .route("/council/stats", get(crate::council::council_stats))
        // T31 — P0-5 closure: walk per-tenant hash chain.
        .route(
            "/watch/verify-chain/{tenant}",
            get(watch::watch_verify_chain),
        )
        // T27 — registered sentinels + per-sentinel stats.
        .route("/watch/list/{tenant}", get(watch::watch_list))
        // T28 — single-scalar liveness gauge.
        .route("/watch/temperature/{tenant}", get(watch::watch_temperature))
        // Gate 4 — exact human-facing Watch snapshot. Admin auth + canary
        // guard are enforced in the sidecar handler.
        .route("/watch/ui-snapshot/{tenant}", get(watch::watch_ui_snapshot))
        // T29 — descending fire log with cursor pagination.
        .route("/watch/audit/{tenant}", get(watch::watch_audit))
        // T30 — admin-authed manual fire trigger (constant-time Bearer compare).
        .route(
            "/watch/force-wake/{sentinel}",
            post(watch::watch_force_wake),
        )
        // T32 — admin-authed quarantine + hard-kill release.
        .route(
            "/watch/quarantine/{sentinel}",
            delete(watch::watch_clear_quarantine),
        )
        // T33.P1-D — JSON scrape target for the Lua poller that surfaces
        // `gw_watch_audit_infra_errors_total` + `gw_watch_persist_failures_total`
        // on /metrics. Matches council_stats precedent — sidecar exposes
        // JSON state, Lua owns Prometheus formatting.
        .route("/watch/stats", get(watch::watch_stats))
        .route(
            "/watch/tenant-policy/{tenant}",
            get(watch::watch_get_tenant_policy),
        )
        .route(
            "/watch/tenant-policy/{tenant}",
            post(watch::watch_set_tenant_policy),
        )
        // PR1 — admin mint of structured execute capability tokens.
        .route(
            "/watch/capability-token/mint",
            post(watch::watch_mint_capability_token),
        )
        // P1 — Directive outbox surface (read/list, verification pubkey, ack).
        .route("/watch/outbox/pubkey", get(watch::watch_outbox_pubkey))
        .route("/watch/outbox/{tenant}", get(watch::watch_list_outbox))
        .route("/watch/outbox/{tenant}/{id}", get(watch::watch_get_outbox))
        .route("/watch/outbox/{id}/ack", post(watch::watch_ack_outbox))
        .route("/watch/outbox/claim", post(watch::watch_claim_outbox))
        .route(
            "/watch/outbox/{id}/heartbeat",
            post(watch::watch_heartbeat_outbox),
        )
        .route(
            "/watch/outbox/{id}/worker_ack",
            post(watch::watch_worker_ack_outbox),
        )
        .route("/watch/outbox/{id}/nack", post(watch::watch_nack_outbox))
        // Dual-custody local-attest ceremony: legacy single-shot arm is 410
        // Gone; arming requires stage (principal bearer) + confirm (bearer
        // plus an enrolled enclave/security-key signature over the staged
        // challenge — the same principal may perform both legs).
        // The five arm/disarm routes (stage, pending, status, confirm,
        // disarm) live in the lib crate so the exact wiring is oneshot-tested.
        .merge(crate::watch::api::arm_admin_router(
            crate::watch::api::ArmAdminRouterState {
                quarantine: watch_quarantine.clone(),
                principals: arm_principals.clone(),
                stage_ttl: arm_stage_ttl,
                admin_token: watch_admin_token.clone(),
                notifier: arm_notifier.clone(),
                deviation: arm_deviation.clone(),
                attest_keys: attest_keys.clone(),
                // B6 (T1 MF-1): derive the real-arm permission ONCE from the
                // EMBEDDED build identity and production-lane eligibility.
                // Dirty, unidentifiable, and local/source images can only run
                // DARK/rehearsal ceremonies (the producer never starts).
                allow_real_arm: crate::watch::attest::build_may_arm_for_real(
                    crate::watch::attest::build_is_dirty(),
                    crate::watch::attest::build_is_release_eligible(),
                ),
            },
        ))
        // Librarian Proxy endpoints (v0.3)
        .route("/librarian/commit", post(librarian::librarian_commit))
        .route(
            "/librarian/context/{tenant}",
            get(librarian::librarian_context),
        );

    // /guard/scan — debug-only decontaminator introspection. Registered ONLY when
    // GATEWAY_DEBUG_GUARD_SCAN=1 so that when disabled the route is entirely absent:
    // every method (GET/POST/...) and every malformed/wrong-content-type body
    // uniformly hits the 404 fallback, disclosing nothing about its existence.
    // Registration-time gating avoids an in-handler `Json` extractor returning
    // a distinguishable 400/415. Production never sets it.
    if guard_scan_enabled_from(std::env::var("GATEWAY_DEBUG_GUARD_SCAN").ok().as_deref()) {
        app = app.route("/guard/scan", post(guard::guard_scan_debug));
    }

    // Audit F-3: global flood backstop over the whole UDS router. Added as the
    // outermost layer (last `.layer` = runs first) so excess local traffic is
    // shed with a 429 before any per-route work; `/health` is exempted inside
    // the middleware so liveness probes never trip it.
    let global_limiter = crate::ratelimit::GlobalRateLimiter::from_env();
    app.layer(axum_mw::from_fn(request_id_layer))
        .layer(axum_mw::from_fn_with_state(
            global_limiter,
            crate::ratelimit::global_rate_limit,
        ))
        .with_state(state.clone())
}

#[cfg(test)]
mod sibling_guard_tests {
    use super::guard_scan_enabled_from;

    /// Audit F-1 park: only the exact `1` value enables the debug route.
    #[test]
    fn guard_scan_enabled_only_for_exact_one() {
        assert!(!guard_scan_enabled_from(None));
        assert!(!guard_scan_enabled_from(Some("")));
        assert!(!guard_scan_enabled_from(Some("0")));
        assert!(!guard_scan_enabled_from(Some("true")));
        assert!(!guard_scan_enabled_from(Some("1 ")));
        assert!(guard_scan_enabled_from(Some("1")));
    }
}
