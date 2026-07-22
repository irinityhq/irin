//! Sidecar spawn composition helpers for the council `--serve` process.
//!
//! Pure functions extracted from the spawn path so the env/restart logic is
//! testable without an `AppHandle` (same convention as `paths.rs`).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Compose the env pairs for a `council --serve` sidecar spawn.
///
/// - `COUNCIL_CORS_ORIGINS` is always set.
/// - Debug builds force `COUNCIL_DEV_NO_AUTH=1`; release builds pass the trimmed
///   pairing token as `COUNCIL_AUTH_TOKEN` when non-empty.
/// - `via_gateway`: `Some(true)` → `COUNCIL_VIA_GATEWAY=1`, `Some(false)` → `=0`,
///   `None` → unset (child inherits the parent env). `"0"` is used instead of
///   removal because tauri-plugin-shell has no `env_remove` and council treats
///   anything other than `"1"`/`"true"` as off (src/main.rs via_gateway parse).
pub fn compose_sidecar_env(
    cors_origins: &str,
    debug_build: bool,
    auth_token: Option<&str>,
    via_gateway: Option<bool>,
    librarian_base: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![("COUNCIL_CORS_ORIGINS".to_string(), cors_origins.to_string())];
    if debug_build {
        env.push(("COUNCIL_DEV_NO_AUTH".to_string(), "1".to_string()));
    } else if let Some(token) = auth_token.map(str::trim).filter(|t| !t.is_empty()) {
        env.push(("COUNCIL_AUTH_TOKEN".to_string(), token.to_string()));
    }
    match via_gateway {
        Some(true) => env.push(("COUNCIL_VIA_GATEWAY".to_string(), "1".to_string())),
        Some(false) => env.push(("COUNCIL_VIA_GATEWAY".to_string(), "0".to_string())),
        None => {}
    }
    if let Some(lb) = librarian_base {
        if !lb.trim().is_empty() {
            env.push(("LIBRARIAN_BASE_URL".to_string(), lb.to_string()));
        }
    }
    env
}

/// Compose the CLI args for a `council --serve` sidecar spawn.
///
/// `council_root` (Settings "councilRoot") overrides the `--base-dir` value when
/// non-blank; otherwise the resolved council-rs repo root is used. Only the
/// `--base-dir` arg moves — binary resolution and the spawn cwd stay pinned to
/// the repo root (`--base-dir` is authoritative for cabinets/prompts/models.yaml
/// per the council CLI; `COUNCIL_RS_DIR` is the knob for relocating the whole
/// checkout including the binary).
pub fn compose_sidecar_args(
    council_rs_dir: &str,
    port: u16,
    council_root: Option<&str>,
) -> Vec<String> {
    let base_dir = council_root
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(council_rs_dir);
    vec![
        "--base-dir".to_string(),
        base_dir.to_string(),
        "--serve".to_string(),
        "--port".to_string(),
        port.to_string(),
    ]
}

/// Validate a user-supplied council root before it becomes `--base-dir`.
///
/// Rejects up front instead of spawning a doomed sidecar: council exits at
/// startup when the base dir lacks `cabinets/` (Config::load scans
/// `<base_dir>/cabinets`), and that failure would only surface via the log
/// pump while the spawn itself reports Ok. `~` is rejected because nothing in
/// the spawn path expands it.
pub fn validate_council_root(root: &str) -> Result<PathBuf, String> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return Err("council root is empty".to_string());
    }
    if trimmed.starts_with('~') {
        return Err(format!(
            "council root must be an absolute path; `~` is not expanded (got {trimmed})"
        ));
    }
    let path = Path::new(trimmed);
    if path.is_relative() {
        return Err(format!(
            "council root must be an absolute path (got {trimmed})"
        ));
    }
    if !path.is_dir() {
        return Err(format!("council root is not a directory: {trimmed}"));
    }
    if !path.join("cabinets").is_dir() {
        return Err(format!(
            "council root has no cabinets/ subdirectory — not a council base dir: {trimmed}"
        ));
    }
    path.canonicalize()
        .map_err(|e| format!("failed to canonicalize council root {trimmed}: {e}"))
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
    use std::fs;
    use std::thread;

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Minimal council base dir: `{root}/cabinets/` (the validator's sanity check).
    fn write_council_root_at(root: &Path) {
        fs::create_dir_all(root.join("cabinets")).unwrap();
    }

    #[test]
    fn compose_always_sets_cors_origins() {
        let env = compose_sidecar_env("http://127.0.0.1:8765", false, None, None, None);
        assert_eq!(
            env_value(&env, "COUNCIL_CORS_ORIGINS"),
            Some("http://127.0.0.1:8765")
        );
    }

    #[test]
    fn compose_debug_forces_no_auth_and_ignores_token() {
        let env = compose_sidecar_env("o", true, Some("secret"), None, None);
        assert_eq!(env_value(&env, "COUNCIL_DEV_NO_AUTH"), Some("1"));
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), None);
    }

    #[test]
    fn compose_release_passes_trimmed_token() {
        let env = compose_sidecar_env("o", false, Some("  tok-123  "), None, None);
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), Some("tok-123"));
        assert_eq!(env_value(&env, "COUNCIL_DEV_NO_AUTH"), None);
    }

    #[test]
    fn compose_release_skips_empty_or_whitespace_token() {
        for token in [None, Some(""), Some("   ")] {
            let env = compose_sidecar_env("o", false, token, None, None);
            assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), None, "{token:?}");
        }
    }

    #[test]
    fn compose_via_gateway_true_sets_1() {
        let env = compose_sidecar_env("o", false, None, Some(true), None);
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("1"));
    }

    #[test]
    fn compose_via_gateway_false_sets_explicit_0() {
        // "0" (not removal) — council only treats "1"/"true" as on, and the
        // child inherits the parent env so an unset var could leak gateway mode.
        let env = compose_sidecar_env("o", false, None, Some(false), None);
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("0"));
    }

    #[test]
    fn compose_via_gateway_none_leaves_env_inherited() {
        let env = compose_sidecar_env("o", false, None, None, None);
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), None);
    }

    #[test]
    fn compose_via_gateway_combines_with_release_token() {
        let env = compose_sidecar_env("o", false, Some("tok"), Some(true), None);
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), Some("tok"));
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("1"));
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
    fn compose_args_council_root_overrides_base_dir_only() {
        let args = compose_sidecar_args("/repo/council-rs", 8765, Some("/elsewhere/base"));
        assert_eq!(
            args,
            vec!["--base-dir", "/elsewhere/base", "--serve", "--port", "8765"]
        );
    }

    #[test]
    fn compose_args_blank_council_root_falls_back_to_repo_root() {
        for root in [Some(""), Some("   "), None] {
            let args = compose_sidecar_args("/repo/council-rs", 8765, root);
            assert_eq!(args[1], "/repo/council-rs", "{root:?}");
        }
    }

    #[test]
    fn compose_args_council_root_combines_with_via_gateway_env() {
        // councilRoot travels in ARGS, via_gateway/auth in ENV — both optional,
        // both set here; neither channel leaks into the other.
        let args = compose_sidecar_args("/repo", 8765, Some("/custom"));
        let env = compose_sidecar_env("o", false, Some("tok"), Some(true), None);
        assert_eq!(args[1], "/custom");
        assert_eq!(env_value(&env, "COUNCIL_VIA_GATEWAY"), Some("1"));
        assert_eq!(env_value(&env, "COUNCIL_AUTH_TOKEN"), Some("tok"));
        assert!(!args.iter().any(|a| a.contains("gateway")));
        assert!(!env.iter().any(|(k, _)| k.contains("BASE_DIR")));
    }

    #[test]
    fn validate_council_root_accepts_dir_with_cabinets() {
        let tmp = std::env::temp_dir().join(format!("council-root-ok-{}", std::process::id()));
        write_council_root_at(&tmp);
        let got = validate_council_root(tmp.to_str().unwrap()).unwrap();
        assert_eq!(got, tmp.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_council_root_trims_whitespace() {
        let tmp = std::env::temp_dir().join(format!("council-root-trim-{}", std::process::id()));
        write_council_root_at(&tmp);
        let padded = format!("  {}  ", tmp.display());
        let got = validate_council_root(&padded).unwrap();
        assert_eq!(got, tmp.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_council_root_rejects_missing_dir() {
        let tmp = std::env::temp_dir().join(format!("council-root-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let err = validate_council_root(tmp.to_str().unwrap()).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn validate_council_root_rejects_dir_without_cabinets() {
        let tmp = std::env::temp_dir().join(format!("council-root-nocab-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let err = validate_council_root(tmp.to_str().unwrap()).unwrap_err();
        assert!(err.contains("cabinets"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_council_root_rejects_relative_and_tilde() {
        for bad in ["relative/path", "./here", "~/irin"] {
            let err = validate_council_root(bad).unwrap_err();
            assert!(err.contains("absolute"), "{bad}: {err}");
        }
    }

    #[test]
    fn validate_council_root_rejects_empty_and_whitespace() {
        for bad in ["", "   "] {
            assert!(validate_council_root(bad).is_err(), "{bad:?}");
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
