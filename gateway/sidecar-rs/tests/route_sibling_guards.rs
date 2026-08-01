//! Audit F-1 / F-3 / F-4 / F-6 class — parked one-offs + contract locks.
//!
//! Product fixes for F-1 (debug env gate), F-3 (global UDS rate limit), and
//! F-6 (tenant-policy admin bearer) already live in sidecar-rs. This file
//! parks the remaining F-4 contract (ip-check stays UDS-internal; nginx must
//! not expose it) and locks the source wiring so a silent revert fails CI.

use std::fs;
use std::path::PathBuf;

fn repo_file(relative: &str) -> String {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // sidecar-rs → gateway → monorepo root
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("gateway/sidecar-rs → monorepo root");
    fs::read_to_string(root.join(relative)).unwrap_or_else(|e| {
        panic!("read {}: {e}", root.join(relative).display());
    })
}

/// Audit F-4 park: `/auth/ip-check` is a Lua→UDS internal probe. It must NOT
/// appear as an nginx `location` (siblings `/auth/rotate` are HTTP-exposed and
/// admin-gated). Trust boundary = UDS mode 0o660 + gid, not a bearer on this
/// path. Revisit only if nginx ever proxies this path.
#[test]
fn f4_nginx_does_not_expose_auth_ip_check() {
    let nginx = repo_file("gateway/nginx.conf");
    assert!(
        !nginx.contains("auth/ip-check"),
        "nginx.conf must not expose /auth/ip-check; that route is UDS-internal \
         for Lua sidecar.ip_check (audit F-4 park)"
    );
    // Sibling that IS intentionally HTTP-exposed stays present as a control.
    assert!(
        nginx.contains("location = /auth/rotate"),
        "sanity: /auth/rotate location still present in nginx.conf"
    );
}

/// Audit F-1 source lock: registration stays behind the env-seam helper.
#[test]
fn f1_guard_scan_registration_uses_env_seam() {
    let routes = repo_file("gateway/sidecar-rs/src/routes/mod.rs");
    assert!(
        routes.contains("guard_scan_enabled_from"),
        "build_router must gate /guard/scan via guard_scan_enabled_from"
    );
    assert!(
        routes.contains("GATEWAY_DEBUG_GUARD_SCAN"),
        "GATEWAY_DEBUG_GUARD_SCAN must remain the env key for F-1"
    );
    // Unconditional .route("/guard/scan" outside the helper gate is the bug.
    // The only allowed registration is inside the enabled_from if-block.
    let after_helper = routes
        .split("if guard_scan_enabled_from")
        .nth(1)
        .expect("expected guard_scan_enabled_from if-block");
    assert!(
        after_helper.contains(".route(\"/guard/scan\""),
        "/guard/scan route must sit inside the guard_scan_enabled_from block"
    );
}

/// Audit F-3 source lock: global flood backstop stays wired on build_router.
#[test]
fn f3_build_router_wires_global_rate_limit() {
    let routes = repo_file("gateway/sidecar-rs/src/routes/mod.rs");
    assert!(
        routes.contains("crate::ratelimit::global_rate_limit")
            || routes.contains("global_rate_limit"),
        "build_router must wire global_rate_limit (audit F-3)"
    );
    assert!(
        routes.contains("GlobalRateLimiter"),
        "build_router must construct GlobalRateLimiter"
    );
}
