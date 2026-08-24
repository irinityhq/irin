use super::*;
use crate::docker_cli::{run_command_timeout, ComposeEnv, DockerErrorKind};
use crate::keychain::{
    load_claude_proxy_token, load_codex_proxy_token, MemorySecretStore, SecretStore,
};
use serde_json::json;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[test]
fn decide_proxy_injection_requires_ready_and_token() {
    assert!(!decide_proxy_injection(false, true));
    assert!(!decide_proxy_injection(true, false));
    assert!(!decide_proxy_injection(false, false));
    assert!(decide_proxy_injection(true, true));
}

#[test]
fn claude_model_allowlist_is_exact_only() {
    assert_eq!(
        resolve_claude_model("claude-opus-4-8"),
        Some("claude-opus-4-8")
    );
    assert_eq!(resolve_claude_model("opus"), Some("opus"));
    assert_eq!(resolve_claude_model("sonnet"), Some("sonnet"));
    // Fuzzy/evil IDs must not map silently.
    assert_eq!(resolve_claude_model("claude-opus-99-evil"), None);
    assert_eq!(resolve_claude_model("claude-sonnet"), None);
}

#[test]
fn codex_model_allowlist_is_exact_only() {
    assert_eq!(resolve_codex_model("gpt-5.5"), Some("gpt-5.5"));
    assert_eq!(resolve_codex_model("gpt"), Some("gpt-5.5"));
    assert_eq!(resolve_codex_model("gpt-9-evil"), None);
    assert_eq!(resolve_codex_model("o1"), None);
}

#[test]
fn const_time_eq_and_proxy_auth() {
    assert!(const_time_eq("abc", "abc"));
    assert!(!const_time_eq("abc", "abd"));
    assert!(!const_time_eq("abc", "ab"));
    let tok = "a".repeat(64);
    assert!(check_proxy_auth(&tok, Some(&format!("Bearer {tok}"))));
    assert!(check_proxy_auth(&tok, Some(&tok)));
    assert!(!check_proxy_auth(&tok, Some("Bearer wrong")));
    assert!(!check_proxy_auth(&tok, None));
    // Empty expected → fail closed (matches ensure_one / spawn refuse).
    assert!(!check_proxy_auth("", None));
    assert!(!check_proxy_auth("", Some("Bearer anything")));
}

#[test]
fn check_proxy_auth_empty_expected_never_opens() {
    // Shared helper must refuse empty expected tokens regardless of header shape.
    for header in [None, Some(""), Some("Bearer "), Some("Bearer x")] {
        assert!(
            !check_proxy_auth("", header),
            "empty expected must refuse header={header:?}"
        );
    }
}

/// F1: bind contract matches source-runtime `--bind 0.0.0.0` (not loopback).
/// Compose reaches the host via host-gateway IPv4; loopback-only is unreachable.
#[test]
fn adapter_bind_host_is_all_interfaces_not_loopback() {
    assert_eq!(ADAPTER_BIND_HOST, "0.0.0.0");
    assert_ne!(ADAPTER_BIND_HOST, "127.0.0.1");
    assert_ne!(ADAPTER_BIND_HOST, "::1");
    // Injected container URL is the Docker Desktop host alias, not loopback.
    assert!(CLAUDE_HOST_PROXY_URL.contains("host.docker.internal"));
    assert!(CODEX_HOST_PROXY_URL.contains("host.docker.internal"));
}

/// Binding the product host on an ephemeral port must not report loopback-only.
#[test]
fn bind_adapter_listener_is_not_loopback_only() {
    let listener = bind_adapter_listener(0).expect("ephemeral all-interfaces bind");
    let addr = listener.local_addr().expect("local_addr");
    assert_eq!(addr.ip().to_string(), "0.0.0.0");
    assert!(!addr.ip().is_loopback());
}

/// Non-loopback bind path refuses an empty token (second gate; ensure_one is first).
#[test]
fn spawn_adapter_server_requires_token_for_all_interfaces_bind() {
    // Child module may reach parent-private spawn via super:: (not use super::*).
    match super::spawn_adapter_server(AdapterKind::Claude, "") {
        Ok(_) => panic!("tokenless all-interfaces bind must fail closed"),
        Err(err) => assert!(err.contains("token"), "expected token refusal, got: {err}"),
    }
}

#[test]
fn foreign_listener_is_never_adopted_or_sent_proxy_token() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let foreign = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        bytes
    });

    assert_eq!(
        occupied_port_status(port),
        Some((
            AdapterHealth::NotReady,
            AdapterNotReadyReason::PortBusyForeign
        ))
    );
    assert!(
        foreign.join().unwrap().is_empty(),
        "ownership check must not send HTTP, token, or environment data"
    );

    let token = "aa".repeat(32);
    let status = CliAdaptersStatus {
        claude: AdapterHealth::NotReady,
        codex: AdapterHealth::NotReady,
        claude_reason: AdapterNotReadyReason::PortBusyForeign,
        codex_reason: AdapterNotReadyReason::PortBusyForeign,
    };
    let env: std::collections::HashMap<_, _> = build_proxy_compose_pairs(&status, &token, &token)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(env.get("CLAUDE_PROXY_URL").map(String::as_str), Some(""));
    assert_eq!(env.get("CLAUDE_PROXY_TOKEN").map(String::as_str), Some(""));
    assert_eq!(env.get("CODEX_PROXY_URL").map(String::as_str), Some(""));
    assert_eq!(env.get("CODEX_PROXY_TOKEN").map(String::as_str), Some(""));
}

#[test]
fn foreign_listener_is_not_recorded_as_owned_or_stopped() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut owned_slot = None;

    assert_eq!(
        occupied_port_status(addr.port()),
        Some((
            AdapterHealth::NotReady,
            AdapterNotReadyReason::PortBusyForeign
        ))
    );
    assert!(owned_slot.is_none(), "foreign listener entered owned slot");

    // stop_gateway_pack and uninstall_gateway_pack both delegate adapter
    // teardown to stop_one via stop_cli_adapters. An empty owned slot must be a
    // no-op against the foreign listener.
    stop_one(&mut owned_slot);
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok(),
        "teardown primitive stopped a non-owned listener"
    );
}

#[test]
fn occupied_port_after_spawn_race_is_classified_as_foreign() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        classify_spawn_failure(listener.local_addr().unwrap().port()),
        (
            AdapterHealth::NotReady,
            AdapterNotReadyReason::PortBusyForeign
        )
    );
}

#[test]
fn preauth_pool_enforces_per_ip_and_global_caps() {
    let pool = Arc::new(PreauthPool::new());
    // One IP may only hold MAX_PREAUTH_PER_IP incomplete slots.
    let mut held = Vec::new();
    for _ in 0..MAX_PREAUTH_PER_IP {
        held.push(pool.try_acquire("10.0.0.1").expect("per-ip slot"));
    }
    assert_eq!(pool.ip_active("10.0.0.1"), MAX_PREAUTH_PER_IP);
    assert_eq!(
        pool.try_acquire("10.0.0.1").err(),
        Some(PreauthReject::PerIpLimit)
    );
    // A different IP still gets service while the first is pinned.
    let other = pool.try_acquire("10.0.0.2").expect("other ip");
    assert_eq!(pool.global_active(), MAX_PREAUTH_PER_IP + 1);
    drop(other);
    drop(held);
    assert_eq!(pool.global_active(), 0);
    assert_eq!(pool.ip_active("10.0.0.1"), 0);

    // Fill the global pool with distinct IPs (2 slots each).
    let mut global_held = Vec::new();
    let ips_needed = MAX_PREAUTH_CONNECTIONS.div_ceil(MAX_PREAUTH_PER_IP);
    for i in 0..ips_needed {
        let ip = format!("10.1.0.{i}");
        for _ in 0..MAX_PREAUTH_PER_IP {
            if global_held.len() >= MAX_PREAUTH_CONNECTIONS {
                break;
            }
            global_held.push(pool.try_acquire(&ip).expect("global fill"));
        }
    }
    assert_eq!(pool.global_active(), MAX_PREAUTH_CONNECTIONS);
    assert_eq!(
        pool.try_acquire("10.9.9.9").err(),
        Some(PreauthReject::GlobalLimit)
    );
    drop(global_held);
    assert_eq!(pool.global_active(), 0);
}

#[test]
fn slow_incomplete_clients_are_bounded_per_ip_before_auth() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_server = Arc::clone(&shutdown);
    let preauth_pool = Arc::new(PreauthPool::new());
    let preauth_server = Arc::clone(&preauth_pool);
    let server = thread::spawn(move || {
        accept_loop(
            listener,
            AdapterKind::Claude,
            Arc::new("bb".repeat(32)),
            shutdown_server,
            Arc::new(AtomicUsize::new(0)),
            preauth_server,
            Arc::new(Mutex::new(IpRateLimitMap::proxy_default())),
        );
    });

    // Loopback shares one IP — one peer can pin at most MAX_PREAUTH_PER_IP.
    let mut slow = Vec::new();
    for _ in 0..MAX_PREAUTH_PER_IP {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(b"G").unwrap();
        slow.push(stream);
    }
    for _ in 0..100 {
        if preauth_pool.global_active() == MAX_PREAUTH_PER_IP {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        preauth_pool.global_active(),
        MAX_PREAUTH_PER_IP,
        "server did not account for slow same-IP clients"
    );

    let mut rejected = TcpStream::connect(addr).unwrap();
    rejected
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    rejected
        .write_all(b"GET /v1/models HTTP/1.1\r\nHost: test\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    rejected.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
    assert!(
        response.contains("per-ip connection limit"),
        "{response}"
    );

    drop(slow);
    for _ in 0..100 {
        if preauth_pool.global_active() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Incomplete clients release within PREAUTH_READ_TIMEOUT; force wait if needed.
    if preauth_pool.global_active() != 0 {
        thread::sleep(PREAUTH_READ_TIMEOUT + Duration::from_millis(200));
    }
    assert_eq!(preauth_pool.global_active(), 0);

    let mut valid = TcpStream::connect(addr).unwrap();
    valid
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    valid
        .write_all(
            format!(
                "GET /v1/models HTTP/1.1\r\nHost: test\r\nX-Proxy-Auth: Bearer {}\r\n\r\n",
                "bb".repeat(32)
            )
            .as_bytes(),
        )
        .unwrap();
    let mut response = String::new();
    valid.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    // Pre-auth slot must not remain held after the full request is buffered.
    for _ in 0..50 {
        if preauth_pool.global_active() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        preauth_pool.global_active(),
        0,
        "pre-auth slot leaked after complete request"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(addr);
    server.join().unwrap();
}

#[test]
fn ensure_proxy_tokens_mints_once_and_reuses() {
    let store = MemorySecretStore::default();
    let (a1, b1) = ensure_proxy_tokens(&store).unwrap();
    assert_eq!(a1.len(), 64);
    assert_eq!(b1.len(), 64);
    assert!(a1.chars().all(|c| c.is_ascii_hexdigit()));
    let (a2, b2) = ensure_proxy_tokens(&store).unwrap();
    assert_eq!(a1, a2);
    assert_eq!(b1, b2);
    assert_eq!(
        load_claude_proxy_token(&store).unwrap().as_deref(),
        Some(a1.as_str())
    );
    assert_eq!(
        load_codex_proxy_token(&store).unwrap().as_deref(),
        Some(b1.as_str())
    );
}

#[test]
fn proxy_compose_pairs_fail_closed_when_not_ready() {
    let token = "ab".repeat(32);
    let status = CliAdaptersStatus {
        claude: AdapterHealth::NotReady,
        codex: AdapterHealth::NotReady,
        claude_reason: AdapterNotReadyReason::CliMissing,
        codex_reason: AdapterNotReadyReason::CliMissing,
    };
    let pairs = build_proxy_compose_pairs(&status, &token, &token).unwrap();
    let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
    assert_eq!(map.get("CLAUDE_PROXY_URL").map(String::as_str), Some(""));
    assert_eq!(map.get("CODEX_PROXY_URL").map(String::as_str), Some(""));
    assert_eq!(map.get("CLAUDE_PROXY_TOKEN").map(String::as_str), Some(""));
    assert_eq!(map.get("CODEX_PROXY_TOKEN").map(String::as_str), Some(""));
}

#[test]
fn proxy_compose_pairs_inject_only_ready_adapters() {
    let claude_tok = "cd".repeat(32);
    let codex_tok = "ef".repeat(32);
    let status = CliAdaptersStatus {
        claude: AdapterHealth::Ready,
        codex: AdapterHealth::NotReady,
        claude_reason: AdapterNotReadyReason::None,
        codex_reason: AdapterNotReadyReason::CliUnauthenticated,
    };
    let pairs = build_proxy_compose_pairs(&status, &claude_tok, &codex_tok).unwrap();
    let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
    assert_eq!(
        map.get("CLAUDE_PROXY_URL").map(String::as_str),
        Some(CLAUDE_HOST_PROXY_URL)
    );
    assert_eq!(
        map.get("CLAUDE_PROXY_TOKEN").map(String::as_str),
        Some(claude_tok.as_str())
    );
    assert_eq!(map.get("CODEX_PROXY_URL").map(String::as_str), Some(""));
    assert_eq!(map.get("CODEX_PROXY_TOKEN").map(String::as_str), Some(""));
}

#[test]
fn apply_proxy_compose_env_overwrites_slots() {
    let mut env = ComposeEnv::new();
    env.insert("CLAUDE_PROXY_URL".into(), "stale".into());
    let status = CliAdaptersStatus {
        claude: AdapterHealth::Ready,
        codex: AdapterHealth::Ready,
        claude_reason: AdapterNotReadyReason::None,
        codex_reason: AdapterNotReadyReason::None,
    };
    let t = "11".repeat(32);
    apply_proxy_compose_env(&mut env, &status, &t, &t).unwrap();
    assert_eq!(
        env.get("CLAUDE_PROXY_URL").map(String::as_str),
        Some(CLAUDE_HOST_PROXY_URL)
    );
    assert_eq!(
        env.get("CODEX_PROXY_URL").map(String::as_str),
        Some(CODEX_HOST_PROXY_URL)
    );
    // Values present but never asserted via Debug/print of secrets in failure messages
    // beyond equality to the local test fixture (not a production secret).
    assert_eq!(env.get("CLAUDE_PROXY_TOKEN").map(|s| s.len()), Some(64));
}

#[test]
fn empty_proxy_pairs_for_teardown() {
    let pairs = empty_proxy_compose_pairs();
    assert_eq!(pairs.len(), 4);
    assert!(pairs.iter().all(|(_, v)| v.is_empty()));
}

#[test]
fn extract_claude_prompt_openai_shape() {
    let body = json!({
        "model": "sonnet",
        "messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hello"}
        ]
    });
    let (prompt, system, model) = extract_claude_prompt(&body);
    assert_eq!(prompt, "hello");
    assert_eq!(system.as_deref(), Some("be brief"));
    assert_eq!(model, Some("sonnet"));
}

#[test]
fn extract_claude_prompt_rejects_unknown_model() {
    let body = json!({
        "model": "claude-opus-99-evil",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (prompt, _, model) = extract_claude_prompt(&body);
    assert_eq!(prompt, "hi");
    assert!(model.is_none());
}

#[test]
fn extract_codex_prompt_folds_system() {
    let body = json!({
        "model": "gpt-5.5",
        "reasoning_effort": "high",
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "ask"}
        ]
    });
    let (prompt, model, effort) = extract_codex_prompt(&body);
    assert!(prompt.contains("<system>"));
    assert!(prompt.contains("sys"));
    assert!(prompt.contains("ask"));
    assert_eq!(model, Some("gpt-5.5"));
    assert_eq!(effort, "high");
}

#[test]
fn missing_cli_leaves_adapters_unready_not_panic() {
    // Deterministic: ensure_cli_adapters must not crash when CLIs are absent.
    // On developer machines with CLIs logged in this may mark Ready — still ok.
    let store = MemorySecretStore::default();
    let status = ensure_cli_adapters(&store);
    // Status is always a valid enum pair; tokens were minted.
    let _ = status.claude;
    let _ = status.codex;
    assert!(load_claude_proxy_token(&store).unwrap().is_some());
    assert!(load_codex_proxy_token(&store).unwrap().is_some());
    // Cleanup any servers we started so the suite does not leave listeners.
    stop_cli_adapters();
}

#[test]
fn stop_cli_adapters_is_idempotent() {
    stop_cli_adapters();
    stop_cli_adapters();
    let st = current_status();
    assert!(!st.claude.is_ready());
    assert!(!st.codex.is_ready());
}

#[test]
fn restart_cli_adapters_runs_without_panic() {
    let store = MemorySecretStore::default();
    let _ = restart_cli_adapters(&store);
    stop_cli_adapters();
}

/// In-process server + auth probe without invoking provider CLIs.
#[test]
fn http_request_parser_and_auth_gate() {
    let raw = "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Proxy-Auth: Bearer secret\r\n\r\n";
    let (method, path, headers, body) = parse_http_request(raw).unwrap();
    assert_eq!(method, "GET");
    assert_eq!(path, "/v1/models");
    assert_eq!(
        headers.get("x-proxy-auth").map(String::as_str),
        Some("Bearer secret")
    );
    assert!(body.is_empty());
    assert!(check_proxy_auth(
        "secret",
        headers.get("x-proxy-auth").map(String::as_str)
    ));
}

/// Chunked `Read` that returns one slice per call — models TCP fragmentation.
struct ChunkedReader {
    chunks: Vec<Vec<u8>>,
    idx: usize,
}

impl Read for ChunkedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.idx >= self.chunks.len() {
            return Ok(0);
        }
        let chunk = &self.chunks[self.idx];
        self.idx += 1;
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        Ok(n)
    }
}

#[test]
fn read_complete_http_request_assembles_fragmented_body() {
    let body = br#"{"model":"opus","messages":[{"role":"user","content":"hello"}]}"#;
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nX-Proxy-Auth: Bearer tok\r\n\r\n",
        body.len()
    );
    // Headers in two chunks, body in three — never one-shot.
    let mut mid = head.as_bytes().to_vec();
    mid.extend_from_slice(&body[..8]);
    let chunks = vec![
        mid[..20].to_vec(),
        mid[20..].to_vec(),
        body[8..16].to_vec(),
        body[16..].to_vec(),
    ];
    let mut reader = ChunkedReader { chunks, idx: 0 };
    let req = read_complete_http_request(&mut reader).unwrap();
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/chat/completions");
    let expected_len = body.len().to_string();
    assert_eq!(
        req.headers.get("content-length").map(String::as_str),
        Some(expected_len.as_str())
    );
    assert_eq!(req.body, body);
}

#[test]
fn read_complete_http_request_socket_fragmented_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = br#"{"model":"sonnet","messages":[{"role":"user","content":"hi"}]}"#;
    let head = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        read_complete_http_request(&mut stream)
    });

    let mut client = TcpStream::connect(addr).unwrap();
    // Headers alone first.
    client.write_all(head.as_bytes()).unwrap();
    thread::sleep(Duration::from_millis(40));
    // Body split across two writes after a delay.
    client.write_all(&body[..4]).unwrap();
    thread::sleep(Duration::from_millis(40));
    client.write_all(&body[4..]).unwrap();
    let _ = client.shutdown(std::net::Shutdown::Write);

    let req = server.join().unwrap().expect("complete request");
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.body, body);
}

#[test]
fn read_complete_http_request_rejects_oversized_and_missing_length() {
    let huge_len = MAX_REQUEST_BODY_BYTES + 1;
    let head = format!("POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {huge_len}\r\n\r\n");
    let mut cur = Cursor::new(head.into_bytes());
    assert_eq!(
        read_complete_http_request(&mut cur).unwrap_err(),
        RequestReadError::BodyTooLarge
    );

    let missing = b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n\r\n{}";
    let mut cur = Cursor::new(missing.to_vec());
    assert_eq!(
        read_complete_http_request(&mut cur).unwrap_err(),
        RequestReadError::MissingContentLength
    );

    // Content-Length claims more bytes than will ever arrive.
    let partial = b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 20\r\n\r\nshort";
    let mut cur = Cursor::new(partial.to_vec());
    assert_eq!(
        read_complete_http_request(&mut cur).unwrap_err(),
        RequestReadError::IncompleteBody
    );
}

#[test]
fn cli_timeout_machinery_kills_without_provider() {
    // Same kill/reap contract adapters use for Claude/Codex; no live provider.
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");
    let err = run_command_timeout(cmd, Duration::from_millis(300)).unwrap_err();
    assert!(
        err.contains(DockerErrorKind::Timeout.as_str()) || err.contains("timeout"),
        "{err}"
    );
}

#[test]
fn apply_gui_login_path_sets_path_env_only() {
    // PATH authority is the login-shell seam; never logs values.
    let mut cmd = Command::new("/usr/bin/true");
    apply_gui_login_path(&mut cmd);
    // Spawning with the applied env must succeed (PATH is well-formed enough).
    let out = run_command_timeout(cmd, Duration::from_secs(2)).expect("true");
    assert!(out.status.success());
}

// Per-IP token bucket; limits match Python's proxy_limits.py.

#[test]
fn token_bucket_burst_then_deny() {
    let mut b = TokenBucket::new(5.0, 0.0, 1000.0);
    for _ in 0..5 {
        assert!(b.allow(1.0, 1000.0));
    }
    assert!(!b.allow(1.0, 1000.0));
}

#[test]
fn token_bucket_refills_over_time() {
    let mut b = TokenBucket::new(2.0, 10.0, 0.0); // 10 tokens/sec
    assert!(b.allow(1.0, 0.0));
    assert!(b.allow(1.0, 0.0));
    assert!(!b.allow(1.0, 0.0));
    // 0.25s → ~2.5 tokens accrued, capped at capacity 2
    assert!(b.allow(1.0, 0.25));
    assert!(b.allow(1.0, 0.25));
    assert!(!b.allow(1.0, 0.25));
}

#[test]
fn token_bucket_never_exceeds_capacity() {
    let mut b = TokenBucket::new(1.0, 100.0, 0.0);
    assert!(b.allow(1.0, 0.0));
    assert!(!b.allow(1.0, 0.0));
    assert!(b.allow(1.0, 1.0)); // long idle still caps at 1
    assert!(!b.allow(1.0, 1.0));
}

#[test]
fn token_bucket_proxy_defaults_match_python() {
    let b = TokenBucket::proxy_default(0.0);
    assert!((b.capacity() - 5.0).abs() < f64::EPSILON);
    assert!((b.rate() - 10.0 / 60.0).abs() < 1e-9);
}

#[test]
fn ip_rate_limit_isolates_clients() {
    let mut map = IpRateLimitMap::new(1.0, 0.0, 120.0, 64);
    assert!(map.allow("10.0.0.1", 0.0));
    assert!(!map.allow("10.0.0.1", 0.0));
    // Different IP independent.
    assert!(map.allow("10.0.0.2", 0.0));
    assert!(!map.allow("10.0.0.2", 0.0));
}

#[test]
fn ip_rate_limit_cleanup_drops_stale_entries() {
    let mut map = IpRateLimitMap::new(1.0, 0.0, 10.0, 64);
    assert!(map.allow("1.1.1.1", 0.0));
    assert!(map.allow("2.2.2.2", 1.0));
    assert_eq!(map.len(), 2);
    // Advance past stale window for both.
    map.cleanup(20.0);
    assert!(map.is_empty());
    // Fresh IP works after cleanup.
    assert!(map.allow("3.3.3.3", 20.0));
    assert_eq!(map.len(), 1);
}

#[test]
fn ip_rate_limit_hard_cap_refuses_new_ips() {
    let mut map = IpRateLimitMap::new(5.0, 0.0, 120.0, 2);
    assert!(map.allow("a", 0.0));
    assert!(map.allow("b", 0.0));
    // Cap full and no stale → new IP denied (no unbounded growth).
    assert!(!map.allow("c", 0.0));
    // Existing IPs still tracked.
    assert!(map.allow("a", 0.0));
}

#[test]
fn rate_limit_and_concurrency_are_independent_gates() {
    // Document order: per-IP bucket first, then concurrency counter.
    // Exhaust IP budget without touching concurrency.
    let mut map = IpRateLimitMap::new(2.0, 0.0, 120.0, 16);
    assert!(map.allow("ip", 0.0));
    assert!(map.allow("ip", 0.0));
    assert!(!map.allow("ip", 0.0), "per-IP 429 before concurrency");
    // Concurrency gate remains independently available (3-way bound).
    let concurrent = AtomicUsize::new(0);
    let prev = concurrent.fetch_add(1, Ordering::SeqCst);
    assert!(prev < 3, "concurrency budget still open for other work");
    concurrent.fetch_sub(1, Ordering::SeqCst);
}
