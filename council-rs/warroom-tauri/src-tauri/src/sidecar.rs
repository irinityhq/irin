//! Sidecar spawn composition helpers for the council `--serve` process.
//!
//! Pure functions extracted from the spawn path so the env/restart logic is
//! testable without an `AppHandle` (same convention as `paths.rs`).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Optional Gateway child credentials (Keychain-sourced). Never log `api_key`
/// or `watch_admin_token`.
#[derive(Debug, Clone)]
pub struct GatewayChildCredentials {
    pub api_key: String,
    pub gateway_url: String,
    /// Watch/Outbox admin read token (Keychain-held). Re-injected on governed
    /// spawns only; `None`/empty leaves governance reads 503.
    pub watch_admin_token: Option<String>,
}

/// Gateway-related env keys that must never be inherited from the parent/login shell.
/// Empty-string overwrite is used because tauri-plugin-shell has no env_remove.
pub const GATEWAY_SCRUB_ENV_KEYS: &[&str] = &[
    "GW_API_KEY",
    "GATEWAY_URL",
    "COUNCIL_VIA_GATEWAY",
    "COUNCIL_GATEWAY_TOKEN",
    "COUNCIL_GATEWAY_KEY_ID",
    "WATCH_ADMIN_TOKEN",
    "WATCH_CANARY_TENANT",
    "BOOTSTRAP_TOKEN",
    "AUTH_PEPPER",
];

/// Compose the env pairs for a `council --serve` sidecar spawn.
///
/// - `COUNCIL_CORS_ORIGINS` is always set.
/// - Debug builds force `COUNCIL_DEV_NO_AUTH=1`; release builds pass the trimmed
///   pairing token as `COUNCIL_AUTH_TOKEN` when non-empty.
/// - `via_gateway`: `Some(true)` → `COUNCIL_VIA_GATEWAY=1`, `Some(false)` → `=0`,
///   `None` → unset (child inherits the parent env). `"0"` is used instead of
///   removal because tauri-plugin-shell has no `env_remove` and council treats
///   anything other than `"1"`/`"true"` as off (src/main.rs via_gateway parse).
/// - When `via_gateway` is `Some(true)`, `gateway_creds` must supply Keychain-sourced
///   `GW_API_KEY` + fixed loopback `GATEWAY_URL`. Packaged installs never import
///   `GW_API_KEY` from the login shell.
/// - **Always** scrub inherited Gateway vars first; `COUNCIL_VIA_GATEWAY=0` alone is
///   not credential removal. Selective re-injection happens only for governed mode.
pub fn compose_sidecar_env(
    cors_origins: &str,
    debug_build: bool,
    auth_token: Option<&str>,
    via_gateway: Option<bool>,
    librarian_base: Option<&str>,
    gateway_creds: Option<&GatewayChildCredentials>,
) -> Vec<(String, String)> {
    let mut env = vec![("COUNCIL_CORS_ORIGINS".to_string(), cors_origins.to_string())];
    if debug_build {
        env.push(("COUNCIL_DEV_NO_AUTH".to_string(), "1".to_string()));
    } else if let Some(token) = auth_token.map(str::trim).filter(|t| !t.is_empty()) {
        env.push(("COUNCIL_AUTH_TOKEN".to_string(), token.to_string()));
    }

    // Scrub inherited Gateway credential/route vars for every explicit route decision.
    if via_gateway.is_some() {
        for key in GATEWAY_SCRUB_ENV_KEYS {
            env.push(((*key).to_string(), String::new()));
        }
    }

    match via_gateway {
        Some(true) => {
            upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
            // The BFF tenant must match the desktop pack sidecar or every
            // Watch/Outbox admin read 403s (non-secret, fixed pack contract).
            upsert_env(
                &mut env,
                "WATCH_CANARY_TENANT",
                crate::gateway_pack::PACK_WATCH_CANARY_TENANT,
            );
            if let Some(creds) = gateway_creds {
                let key = creds.api_key.trim();
                if !key.is_empty() {
                    // Replace scrubbed empty value with Keychain-sourced key.
                    upsert_env(&mut env, "GW_API_KEY", key);
                }
                let url = creds.gateway_url.trim();
                if !url.is_empty() {
                    upsert_env(&mut env, "GATEWAY_URL", url);
                }
                if let Some(tok) = creds
                    .watch_admin_token
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    // Re-arm the Watch/Outbox read surface emptied by the scrub.
                    upsert_env(&mut env, "WATCH_ADMIN_TOKEN", tok);
                }
            }
        }
        Some(false) => {
            upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "0");
            // Scrub already cleared GW_API_KEY / GATEWAY_URL / related.
        }
        None => {}
    }
    if let Some(lb) = librarian_base {
        if !lb.trim().is_empty() {
            env.push(("LIBRARIAN_BASE_URL".to_string(), lb.to_string()));
        }
    }
    env
}

fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, v)) = env.iter_mut().find(|(k, _)| k == key) {
        *v = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

/// Compose the CLI args for a `council --serve` sidecar spawn.
///
/// `base_dir` is the resolved writable base (packaged Application Support
/// overlay or council-rs repo root). Binary resolution and the spawn cwd are
/// independent; `--base-dir` is authoritative for cabinets/prompts/models.yaml.
///
/// `web_dist` is the packaged War Room static export (`--web-dist`). Development
/// builds pass `None` so behavior stays unchanged (no permanent :3010 server).
/// When set (non-blank), Council serves the export on the same loopback origin
/// for private phone access; the Tauri webview still uses its embedded
/// `frontendDist`.
pub fn compose_sidecar_args(base_dir: &str, port: u16, web_dist: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--base-dir".to_string(),
        base_dir.to_string(),
        "--serve".to_string(),
        "--port".to_string(),
        port.to_string(),
    ];
    if let Some(dist) = web_dist.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--web-dist".to_string());
        args.push(dist.to_string());
    }
    args
}

/// Wait (bounded) for a TCP port to become bindable on 127.0.0.1.
///
/// `CommandChild::kill` returns before the OS reaps the process, so an
/// immediate respawn after a restart can lose the bind race on :8765.
/// Returns `true` once a probe bind succeeds (probe listener is dropped
/// immediately), `false` on timeout — callers may spawn anyway and let the
/// log pump surface a bind failure.
pub fn wait_for_port_release(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                drop(listener);
                return true;
            }
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CouncilServerProbe {
    Unavailable,
    MatchingBuild,
    DifferentBuild,
}

/// Identify a healthy Council only when its embedded source identity matches
/// the desktop bundle built from the same checkout.
///
/// Health intentionally omits local filesystem paths. Build identity preserves
/// source provenance without reintroducing that private path disclosure.
pub fn probe_council_server(
    port: u16,
    timeout: Duration,
    expected_sha: &str,
    expected_dirty: bool,
    auth_token: Option<&str>,
) -> CouncilServerProbe {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, timeout) else {
        return CouncilServerProbe::Unavailable;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let auth_token = auth_token.map(str::trim).filter(|token| !token.is_empty());
    let request = match auth_token {
        Some(token) => format!(
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
        None => {
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                .to_string()
        }
    };
    if stream.write_all(request.as_bytes()).is_err() {
        return CouncilServerProbe::Unavailable;
    }
    let mut response = String::new();
    if stream
        .take(64 * 1024)
        .read_to_string(&mut response)
        .is_err()
    {
        return CouncilServerProbe::Unavailable;
    }
    let status_ok = response.starts_with("HTTP/1.1 200 ") || response.starts_with("HTTP/1.0 200 ");
    if !status_ok {
        return CouncilServerProbe::Unavailable;
    }
    let Some((_, body)) = response.split_once("\r\n\r\n") else {
        return CouncilServerProbe::Unavailable;
    };
    let Ok(health) = serde_json::from_str::<serde_json::Value>(body) else {
        return CouncilServerProbe::Unavailable;
    };
    if health.get("council_version").is_none() {
        return CouncilServerProbe::Unavailable;
    }
    let actual_sha = health.get("build_sha").and_then(serde_json::Value::as_str);
    let actual_dirty = health
        .get("build_dirty")
        .and_then(serde_json::Value::as_bool);
    if expected_sha != "unknown"
        && actual_sha == Some(expected_sha)
        && actual_dirty == Some(expected_dirty)
    {
        CouncilServerProbe::MatchingBuild
    } else {
        CouncilServerProbe::DifferentBuild
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn compose_always_sets_cors_origins() {
        let env = compose_sidecar_env("http://127.0.0.1:8765", false, None, None, None, None);
        assert_eq!(
            env_value(&env, "COUNCIL_CORS_ORIGINS"),
            Some("http://127.0.0.1:8765")
        );
    }

    #[test]
    fn compose_debug_forces_no_auth_and_ignores_token() {
        let env = compose_sidecar_env("o", true, Some("secret"), None, None, None);
        assert_eq!(env_value(&env, "COUNCIL_DEV_NO_AUTH"), Some("1"));
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), None);
    }

    #[test]
    fn compose_release_passes_trimmed_token() {
        let env = compose_sidecar_env("o", false, Some("  tok-123  "), None, None, None);
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), Some("tok-123"));
        assert_eq!(env_value(&env, "COUNCIL_DEV_NO_AUTH"), None);
    }

    #[test]
    fn compose_release_skips_empty_or_whitespace_token() {
        for token in [None, Some(""), Some("   ")] {
            let env = compose_sidecar_env("o", false, token, None, None, None);
            assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), None, "{token:?}");
        }
    }

    #[test]
    fn compose_via_gateway_true_sets_1() {
        let env = compose_sidecar_env("o", false, None, Some(true), None, None);
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("1"));
    }

    #[test]
    fn compose_via_gateway_false_sets_explicit_0() {
        // "0" (not removal) — council only treats "1"/"true" as on, and the
        // child inherits the parent env so an unset var could leak gateway mode.
        // Scrub sets empty strings for credential keys (not omit) so login/parent
        // values cannot win when Command.env is applied.
        let env = compose_sidecar_env("o", false, None, Some(false), None, None);
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("0"));
        assert_eq!(env_value(&env, "GW_API_KEY"), Some(""));
        assert_eq!(env_value(&env, "GATEWAY_URL"), Some(""));
    }

    #[test]
    fn compose_via_gateway_none_leaves_env_inherited() {
        let env = compose_sidecar_env("o", false, None, None, None, None);
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), None);
    }

    #[test]
    fn compose_via_gateway_combines_with_release_token() {
        let env = compose_sidecar_env("o", false, Some("tok"), Some(true), None, None);
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), Some("tok"));
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("1"));
    }

    #[test]
    fn compose_via_gateway_injects_keychain_creds_only_when_on() {
        let fake_key = format!("gw_{}", "a".repeat(32));
        let watch_token = "cd".repeat(32);
        let creds = GatewayChildCredentials {
            api_key: fake_key.clone(),
            gateway_url: "http://127.0.0.1:18080".into(),
            watch_admin_token: Some(watch_token.clone()),
        };
        let on = compose_sidecar_env("o", false, None, Some(true), None, Some(&creds));
        assert_eq!(env_value(&on, "COUNCIL_VIA_GATEWAY"), Some("1"));
        assert_eq!(env_value(&on, "GW_API_KEY"), Some(fake_key.as_str()));
        assert_eq!(
            env_value(&on, "GATEWAY_URL"),
            Some("http://127.0.0.1:18080")
        );
        // Governed spawns re-arm the Watch/Outbox read surface after the scrub.
        assert_eq!(env_value(&on, "WATCH_ADMIN_TOKEN"), Some(watch_token.as_str()));
        // Governed spawns pin the pack canary tenant so BFF admin reads do not 403.
        assert_eq!(env_value(&on, "WATCH_CANARY_TENANT"), Some("canary"));
        let off = compose_sidecar_env("o", false, None, Some(false), None, Some(&creds));
        assert_eq!(env_value(&off, "COUNCIL_VIA_GATEWAY"), Some("0"));
        // Scrub sets empty string (not omit) so inherited parent values cannot win.
        assert_eq!(env_value(&off, "GW_API_KEY"), Some(""));
        assert_eq!(env_value(&off, "GATEWAY_URL"), Some(""));
        assert_eq!(env_value(&off, "COUNCIL_GATEWAY_TOKEN"), Some(""));
        // Direct spawns get no watch-admin token even when creds carry one.
        assert_eq!(env_value(&off, "WATCH_ADMIN_TOKEN"), Some(""));
        // Direct spawns keep no pack tenant; Council falls back to `sovereign`.
        assert_eq!(env_value(&off, "WATCH_CANARY_TENANT"), Some(""));
    }

    #[test]
    fn compose_via_gateway_without_watch_token_leaves_read_surface_scrubbed() {
        let creds = GatewayChildCredentials {
            api_key: format!("gw_{}", "b".repeat(32)),
            gateway_url: "http://127.0.0.1:18080".into(),
            watch_admin_token: None,
        };
        let on = compose_sidecar_env("o", false, None, Some(true), None, Some(&creds));
        assert_eq!(env_value(&on, "COUNCIL_VIA_GATEWAY"), Some("1"));
        assert_eq!(env_value(&on, "WATCH_ADMIN_TOKEN"), Some(""));
    }

    #[test]
    fn compose_scrubs_gateway_vars_before_selective_inject() {
        let off = compose_sidecar_env("o", false, None, Some(false), None, None);
        // Credential/token keys stay scrubbed empty; route flag is explicit "0".
        for key in GATEWAY_SCRUB_ENV_KEYS {
            if *key == "COUNCIL_VIA_GATEWAY" {
                assert_eq!(env_value(&off, key), Some("0"), "route flag {key}");
            } else {
                assert_eq!(env_value(&off, key), Some(""), "missing scrub for {key}");
            }
        }
        assert_ne!(env_value(&off, "COUNCIL_VIA_GATEWAY"), Some("1"));
    }

    #[test]
    fn compose_args_default_base_dir_pins_full_arg_order() {
        let args = compose_sidecar_args("/repo/council-rs", 8765, None);
        assert_eq!(
            args,
            vec![
                "--base-dir",
                "/repo/council-rs",
                "--serve",
                "--port",
                "8765"
            ]
        );
    }

    #[test]
    fn compose_args_sets_base_dir_and_port() {
        let args = compose_sidecar_args("/elsewhere/base", 8765, None);
        assert_eq!(
            args,
            vec!["--base-dir", "/elsewhere/base", "--serve", "--port", "8765"]
        );
    }

    #[test]
    fn compose_args_and_env_are_independent_channels() {
        // base-dir travels in ARGS, via_gateway/auth in ENV — independent channels.
        let args = compose_sidecar_args("/custom", 8765, None);
        let env = compose_sidecar_env("o", false, Some("tok"), Some(true), None, None);
        assert_eq!(args[1], "/custom");
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("1"));
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), Some("tok"));
        assert!(!args.iter().any(|a| a.contains("gateway")));
        assert!(!env.iter().any(|(k, _)| k.contains("BASE_DIR")));
    }

    #[test]
    fn compose_args_packaged_web_dist_appends_flag() {
        let args = compose_sidecar_args(
            "/app/support/council-base",
            8765,
            Some("/App/Contents/Resources/warroom-web"),
        );
        assert_eq!(
            args,
            vec![
                "--base-dir",
                "/app/support/council-base",
                "--serve",
                "--port",
                "8765",
                "--web-dist",
                "/App/Contents/Resources/warroom-web",
            ]
        );
    }

    #[test]
    fn compose_args_development_omits_web_dist() {
        let args = compose_sidecar_args("/repo/council-rs", 8765, None);
        assert!(!args.iter().any(|a| a == "--web-dist"));
        // Blank/whitespace web_dist is treated as absent (dev unchanged).
        for dist in [Some(""), Some("   ")] {
            let args = compose_sidecar_args("/repo/council-rs", 8765, dist);
            assert!(
                !args.iter().any(|a| a == "--web-dist"),
                "blank web_dist should not append: {dist:?}"
            );
        }
    }

    #[test]
    fn wait_for_port_release_true_when_port_free() {
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        assert!(wait_for_port_release(port, Duration::from_millis(500)));
    }

    #[test]
    fn wait_for_port_release_times_out_while_held_then_succeeds() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!wait_for_port_release(port, Duration::from_millis(250)));
        drop(listener);
        assert!(wait_for_port_release(port, Duration::from_millis(500)));
    }

    #[test]
    fn probe_council_server_accepts_matching_build_without_local_path() {
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let body = serde_json::json!({
                "council_version": "0.1.0",
                "build_sha": SHA,
                "build_dirty": false,
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert_eq!(
            probe_council_server(port, Duration::from_secs(1), SHA, false, None),
            CouncilServerProbe::MatchingBuild
        );
        server.join().unwrap();
    }

    #[test]
    fn probe_council_server_rejects_different_build() {
        const EXPECTED_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        const ACTUAL_SHA: &str = "76543210fedcba9876543210fedcba9876543210";
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let body = serde_json::json!({
                "council_version": "0.1.0",
                "build_sha": ACTUAL_SHA,
                "build_dirty": false,
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert_eq!(
            probe_council_server(port, Duration::from_secs(1), EXPECTED_SHA, false, None),
            CouncilServerProbe::DifferentBuild
        );
        server.join().unwrap();
    }

    #[test]
    fn probe_council_server_sends_bearer_without_printing_it() {
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        const TOKEN: &str = "test-only-token";
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("Authorization: Bearer test-only-token\r\n"));
            let body = serde_json::json!({
                "council_version": "0.1.0",
                "build_sha": SHA,
                "build_dirty": true,
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert_eq!(
            probe_council_server(port, Duration::from_secs(1), SHA, true, Some(TOKEN)),
            CouncilServerProbe::MatchingBuild
        );
        server.join().unwrap();
    }

    #[test]
    fn probe_council_server_rejects_unrelated_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        });
        assert_eq!(
            probe_council_server(port, Duration::from_secs(1), "expected-sha", false, None),
            CouncilServerProbe::Unavailable
        );
        server.join().unwrap();
    }
}
