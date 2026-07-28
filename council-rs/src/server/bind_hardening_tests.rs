use super::*;

/// Save/restore both COUNCIL_AUTH_TOKEN and COUNCIL_DEV_NO_AUTH so a
/// developer's real env is not mutated by env-path tests. Serialized by a
/// mutex: process env is global, so two guarded tests on parallel threads
/// race save/restore (one removes the token while the other asserts on it).
fn with_env_guard<F>(set_auth: Option<&str>, set_dev: Option<&str>, f: F)
where
    F: FnOnce(),
{
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let orig_auth = std::env::var("COUNCIL_AUTH_TOKEN").ok();
    let orig_dev = std::env::var("COUNCIL_DEV_NO_AUTH").ok();
    unsafe {
        match set_auth {
            Some(v) => std::env::set_var("COUNCIL_AUTH_TOKEN", v),
            None => std::env::remove_var("COUNCIL_AUTH_TOKEN"),
        }
        match set_dev {
            Some(v) => std::env::set_var("COUNCIL_DEV_NO_AUTH", v),
            None => std::env::remove_var("COUNCIL_DEV_NO_AUTH"),
        }
    }
    f();
    unsafe {
        match orig_auth {
            Some(v) => std::env::set_var("COUNCIL_AUTH_TOKEN", v),
            None => std::env::remove_var("COUNCIL_AUTH_TOKEN"),
        }
        match orig_dev {
            Some(v) => std::env::set_var("COUNCIL_DEV_NO_AUTH", v),
            None => std::env::remove_var("COUNCIL_DEV_NO_AUTH"),
        }
    }
}

#[test]
fn loopback_hosts_are_recognized() {
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("127.0.0.2"));
    assert!(is_loopback_host("::1"));
    assert!(is_loopback_host("[::1]"));
    assert!(is_loopback_host("localhost"));
    assert!(is_loopback_host("LOCALHOST"));
}

#[test]
fn non_loopback_hosts_are_detected() {
    let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
    assert!(!is_loopback_host(&nl));
    assert!(!is_loopback_host("192.168.0.1"));
    assert!(!is_loopback_host("10.1.2.3"));
    assert!(!is_loopback_host("::"));
    assert!(!is_loopback_host("8.8.8.8"));
}

#[test]
fn cors_loopback_origins_allowed_any_port() {
    for o in [
        "http://127.0.0.1:3011",
        "http://localhost:9999",
        "http://[::1]:3010",
        "http://127.0.0.1",
        "https://localhost:3010",
    ] {
        assert!(
            origin_is_loopback(&HeaderValue::from_str(o).unwrap()),
            "expected loopback: {o}"
        );
    }
}

#[test]
fn cors_non_loopback_origins_rejected() {
    for o in [
        "https://evil.com",
        "http://192.168.1.20:3010",
        "http://device.example.ts.net:3010",
        "http://evil.com/127.0.0.1:3010",
        "http://127.0.0.1@evil.com",
        "http://127.0.0.1:80@evil.com",
        "http://[::1]@evil.com",
        "http://[::1]:80@evil.com",
        "http://[::1]x.evil.com",
        "tauri://localhost",
        "null",
    ] {
        assert!(
            !origin_is_loopback(&HeaderValue::from_str(o).unwrap()),
            "expected non-loopback: {o}"
        );
    }
}

#[test]
fn default_loopback_bind_ok_without_token() {
    let r = resolve_serve_addr_with_token("127.0.0.1", 8765, false);
    assert!(r.is_ok());
    assert_eq!(r.unwrap(), "127.0.0.1:8765");
}

#[test]
fn ipv6_loopback_bind_ok_without_token_bracketed() {
    // IPv6 literals are bracketed in resolved addr for bindability and URLs.
    let r = resolve_serve_addr_with_token("::1", 8765, false);
    assert!(r.is_ok());
    assert_eq!(r.unwrap(), "[::1]:8765");

    let r2 = resolve_serve_addr_with_token("[::1]", 8765, false);
    assert!(r2.is_ok());
    assert_eq!(r2.unwrap(), "[::1]:8765");
}

#[test]
fn localhost_bind_ok_without_token() {
    let r = resolve_serve_addr_with_token("localhost", 3000, false);
    assert!(r.is_ok());
    // value not asserted to keep test focused; ipv4/6 covered elsewhere
}

#[test]
fn non_loopback_with_auth_token_ok() {
    let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
    let r = resolve_serve_addr_with_token(&nl, 8765, true);
    assert!(r.is_ok());
    assert_eq!(r.unwrap(), format!("{}:{}", nl, 8765));
}

#[test]
fn non_loopback_without_token_refuses_with_loud_error() {
    // PIN the fatal refusal: this exact error string from resolver is what
    // main prints then exit(1) with — startup cannot proceed to router/bind.
    let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
    let r = resolve_serve_addr_with_token(&nl, 8080, false);
    assert!(r.is_err());
    let err = r.unwrap_err();
    let expected = format!(
        "ERROR: Non-loopback bind to '{}' requested without COUNCIL_AUTH_TOKEN.\n\
             Council refuses to bind non-loopback addresses unless BOTH an explicit\n\
             non-loopback --host is given AND COUNCIL_AUTH_TOKEN is set.\n\
             COUNCIL_DEV_NO_AUTH=1 does NOT unlock non-loopback binding.\n\
             Set COUNCIL_AUTH_TOKEN=... or use --host 127.0.0.1 (default).",
        nl
    );
    assert_eq!(err, expected);
}

#[test]
fn dev_no_auth_cannot_unlock_non_loopback() {
    // the has_auth_token=false case models "no AUTH_TOKEN even if dev set"
    // PIN refusal (not mere !ok) — resolver error drives fatal startup exit.
    let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
    let r = resolve_serve_addr_with_token(&nl, 8765, false);
    assert!(r.is_err());
    let err = r.unwrap_err();
    assert!(err.contains("ERROR: Non-loopback bind to '0.0.0.0'"));
    assert!(err.contains("COUNCIL_AUTH_TOKEN"));
    // also for other non-loop like LAN IP
    let r2 = resolve_serve_addr_with_token("192.168.1.5", 8765, false);
    assert!(r2.is_err());
    let err2 = r2.unwrap_err();
    assert!(err2.contains("Non-loopback bind to '192.168.1.5'"));
}

#[test]
fn resolve_env_without_token_refuses_nonloop() {
    // Direct env read path; save/restore both envs so dev's real vars survive.
    with_env_guard(None, None, || {
        let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
        let r = resolve_serve_addr(&nl, 8765);
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert!(err.contains("Non-loopback bind to '0.0.0.0'"));
        assert!(err.contains("COUNCIL_AUTH_TOKEN"));
        assert!(err.contains("refuses to bind"));
    });
}

#[test]
fn resolve_env_with_token_allows_nonloop() {
    // Save/restore prior values of BOTH vars (not remove) so dev env survives.
    with_env_guard(Some("test-token-for-bind"), None, || {
        let nl = format!("{}.{}.{}.{}", 0, 0, 0, 0);
        let r = resolve_serve_addr(&nl, 8765);
        assert!(r.is_ok());
        // also pin that it succeeds with the token (resolver allows nonloop)
        assert_eq!(r.unwrap(), "0.0.0.0:8765");
    });
}
