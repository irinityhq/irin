//! DMG-owned Claude / Codex governed CLI adapter lifecycle.
//!
//! Host-side HTTP adapters bridge the Gateway Pack container to the operator's
//! installed `claude` / `codex` CLIs (OAuth session, no raw provider key).
//! Lifecycle is app-owned: start / health / restart / stop. Tokens live in the
//! Keychain and ride the per-spawn Compose process env only — never the public
//! env file, never logs.
//!
//! Implementation is native Rust inside the Tauri process so the installed app
//! does not require a separately installed Python/Node runtime. Missing CLI or
//! unauthenticated CLI leaves that route empty (Gateway fail-closed readiness);
//! War Room still starts. Direct CLI transport is independent and unchanged.

use super::keys::{random_hex, validate_env_value};
use crate::docker_cli::{
    run_command_timeout, run_command_timeout_input, ComposeEnv, DockerErrorKind,
};
use crate::keychain::{
    load_claude_proxy_token, load_codex_proxy_token, store_claude_proxy_token,
    store_codex_proxy_token, SecretStore,
};
use crate::private_config::gui_login_environment;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CLAUDE_ADAPTER_PORT: u16 = 9090;
pub const CODEX_ADAPTER_PORT: u16 = 9091;
/// Host-reachable URL form for Docker Desktop (`host.docker.internal`).
pub const CLAUDE_HOST_PROXY_URL: &str = "http://host.docker.internal:9090";
pub const CODEX_HOST_PROXY_URL: &str = "http://host.docker.internal:9091";
/// IPv4 all-interfaces bind host for owned adapters.
///
/// Contract for the macOS DMG + Gateway Pack path:
/// - Owned adapters bind `0.0.0.0` (AF_INET all-interfaces, not dual-stack).
/// - Compose injects `host.docker.internal:host-gateway` (IPv4) into the pack
///   container hosts file, so the container reaches the host over IPv4.
/// - Binding `127.0.0.1` alone makes `host.docker.internal` / host-gateway
///   unreachable from the container (Kimi F1).
/// - Dual-stack `::` is not required for this pack path: host-gateway forces
///   an IPv4 entry, matching the adapters' `0.0.0.0` bind. Python's
///   dual-stack helper only activates when `--bind` is an IPv6 form (`::`).
/// - Security: token is always required before listen (`ensure_one` refuses an
///   empty token), matching the non-loopback requires-token gate.
pub const ADAPTER_BIND_HOST: &str = "0.0.0.0";
const LOOPBACK_CLAUDE: &str = "http://127.0.0.1:9090";
const LOOPBACK_CODEX: &str = "http://127.0.0.1:9091";
const MAX_CONCURRENT_CLI: usize = 3;
/// Global bound on sockets/threads while a request is still being read.
/// Slow clients must not turn the all-interface adapter into an unbounded
/// thread/FD allocator.
pub const MAX_PREAUTH_CONNECTIONS: usize = 16;
/// One peer cannot fill [`MAX_PREAUTH_CONNECTIONS`]. Docker Gateway traffic
/// typically arrives as a single host-gateway IP; unauthenticated peers are
/// capped so they cannot starve that path for the full pre-auth read window.
pub const MAX_PREAUTH_PER_IP: usize = 2;
/// Incomplete / pre-auth request read budget. After the full HTTP request is
/// buffered the pre-auth slot is released; CLI work uses [`MAX_CONCURRENT_CLI`].
pub const PREAUTH_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Source proxy contract (`gateway/tools/proxy_limits.py`): 5-burst token bucket.
pub const RATE_LIMIT_CAPACITY: f64 = 5.0;
/// Source proxy contract: 10 requests per minute → tokens/sec.
pub const RATE_LIMIT_RATE_PER_SEC: f64 = 10.0 / 60.0;
/// Drop idle per-IP entries so the map cannot grow without bound.
const RATE_LIMIT_STALE_SECS: f64 = 120.0;
/// Hard cap on tracked client IPs (cleanup runs when exceeded).
const RATE_LIMIT_MAX_ENTRIES: usize = 1024;
const CLI_AUTH_ATTEMPTS: u32 = 3;
/// Source proxy contract: `claude-proxy.py` subprocess timeout.
pub const CLAUDE_CLI_TIMEOUT: Duration = Duration::from_secs(300);
/// Source proxy contract: `codex-proxy.py` subprocess timeout.
pub const CODEX_CLI_TIMEOUT: Duration = Duration::from_secs(600);
/// Preflight CLI presence (`--version`).
const CLI_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Preflight auth status (zero-spend).
const CLI_AUTH_TIMEOUT: Duration = Duration::from_secs(15);
/// Max HTTP header block (request-line + headers). Fail closed if exceeded.
pub const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
/// Max request body. Prompt-scale multi-turn JSON — not a silent 64 KiB truncate.
/// Gateways that need larger prompts must raise this deliberately.
pub const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Claude,
    Codex,
}

impl AdapterKind {
    pub fn port(self) -> u16 {
        match self {
            Self::Claude => CLAUDE_ADAPTER_PORT,
            Self::Codex => CODEX_ADAPTER_PORT,
        }
    }

    pub fn loopback_base(self) -> &'static str {
        match self {
            Self::Claude => LOOPBACK_CLAUDE,
            Self::Codex => LOOPBACK_CODEX,
        }
    }

    pub fn cli_bin(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn service_name(self) -> &'static str {
        match self {
            Self::Claude => "claude-proxy",
            Self::Codex => "codex-proxy",
        }
    }
}

/// Non-secret health snapshot for one adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterHealth {
    Ready,
    NotReady,
}

impl AdapterHealth {
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Non-secret lifecycle status (never includes tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliAdaptersStatus {
    pub claude: AdapterHealth,
    pub codex: AdapterHealth,
    pub claude_reason: AdapterNotReadyReason,
    pub codex_reason: AdapterNotReadyReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterNotReadyReason {
    None,
    CliMissing,
    CliUnauthenticated,
    PortBusyForeign,
    StartFailed,
    TokenMissing,
    NotStarted,
}

impl AdapterNotReadyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "ok",
            Self::CliMissing => "cli_missing",
            Self::CliUnauthenticated => "cli_unauthenticated",
            Self::PortBusyForeign => "port_busy_foreign",
            Self::StartFailed => "start_failed",
            Self::TokenMissing => "token_missing",
            Self::NotStarted => "not_started",
        }
    }
}

impl Default for CliAdaptersStatus {
    fn default() -> Self {
        Self {
            claude: AdapterHealth::NotReady,
            codex: AdapterHealth::NotReady,
            claude_reason: AdapterNotReadyReason::NotStarted,
            codex_reason: AdapterNotReadyReason::NotStarted,
        }
    }
}

/// Pure: Gateway may receive a proxy URL+token only when the adapter is healthy
/// and a matching token is present. Empty injection leaves the provider unready
/// (fail-closed) — never rewrites a governed seat to Direct.
pub fn decide_proxy_injection(adapter_ready: bool, token_present: bool) -> bool {
    adapter_ready && token_present
}

// --- Per-IP token bucket (Python proxy_limits.py parity) -------------------------

/// Deterministic token bucket. Clock is caller-supplied seconds for testability.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    rate: f64,
    tokens: f64,
    last_secs: f64,
}

impl TokenBucket {
    pub fn new(capacity: f64, rate: f64, now_secs: f64) -> Self {
        Self {
            capacity,
            rate,
            tokens: capacity,
            last_secs: now_secs,
        }
    }

    /// Source defaults: capacity 5, rate 10/min.
    #[cfg(test)]
    pub fn proxy_default(now_secs: f64) -> Self {
        Self::new(RATE_LIMIT_CAPACITY, RATE_LIMIT_RATE_PER_SEC, now_secs)
    }

    /// Allow `cost` tokens at `now_secs`. Refills from elapsed wall time.
    pub fn allow(&mut self, cost: f64, now_secs: f64) -> bool {
        let elapsed = (now_secs - self.last_secs).max(0.0);
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_secs = now_secs;
        if self.tokens + f64::EPSILON >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn capacity(&self) -> f64 {
        self.capacity
    }

    #[cfg(test)]
    pub fn rate(&self) -> f64 {
        self.rate
    }
}

/// Per-client-IP map of token buckets with stale-entry cleanup.
#[derive(Debug)]
pub struct IpRateLimitMap {
    capacity: f64,
    rate: f64,
    stale_after_secs: f64,
    max_entries: usize,
    /// ip → (bucket, last_seen_secs)
    buckets: HashMap<String, (TokenBucket, f64)>,
}

impl IpRateLimitMap {
    pub fn new(capacity: f64, rate: f64, stale_after_secs: f64, max_entries: usize) -> Self {
        Self {
            capacity,
            rate,
            stale_after_secs,
            max_entries,
            buckets: HashMap::new(),
        }
    }

    pub fn proxy_default() -> Self {
        Self::new(
            RATE_LIMIT_CAPACITY,
            RATE_LIMIT_RATE_PER_SEC,
            RATE_LIMIT_STALE_SECS,
            RATE_LIMIT_MAX_ENTRIES,
        )
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Drop entries not seen within `stale_after_secs` of `now_secs`.
    pub fn cleanup(&mut self, now_secs: f64) {
        let stale = self.stale_after_secs;
        self.buckets
            .retain(|_, (_, last_seen)| now_secs - *last_seen <= stale);
    }

    /// Allow one chat request from `ip` at `now_secs`. Cleans stale/over-cap first.
    pub fn allow(&mut self, ip: &str, now_secs: f64) -> bool {
        if self.buckets.len() >= self.max_entries {
            self.cleanup(now_secs);
        } else if self.buckets.len() > 16 {
            // Opportunistic cleanup on moderate size so idle IPs do not linger.
            self.cleanup(now_secs);
        }
        // If still at cap after cleanup, deny closed (no unbounded growth, no eviction of active).
        if self.buckets.len() >= self.max_entries && !self.buckets.contains_key(ip) {
            return false;
        }
        let cap = self.capacity;
        let rate = self.rate;
        let entry = self
            .buckets
            .entry(ip.to_string())
            .or_insert_with(|| (TokenBucket::new(cap, rate, now_secs), now_secs));
        entry.1 = now_secs;
        entry.0.allow(1.0, now_secs)
    }
}

fn wall_now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Pure: model allow-list for Claude CLI aliases (exact match only).
pub fn resolve_claude_model(model: &str) -> Option<&'static str> {
    match model {
        "claude-opus-4-8" => Some("claude-opus-4-8"),
        "claude-opus-4-7" | "claude-opus-4-6" | "claude-opus-4-5" | "opus" => Some("opus"),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "sonnet" => Some("sonnet"),
        "claude-haiku-4-5" | "haiku" => Some("haiku"),
        "claude-fable-5" | "fable" => Some("fable"),
        _ => None,
    }
}

/// Pure: model allow-list for Codex CLI `-m` args (exact match only).
pub fn resolve_codex_model(model: &str) -> Option<&'static str> {
    match model {
        "gpt-5.6-sol" => Some("gpt-5.6-sol"),
        "gpt-5.5" | "gpt" => Some("gpt-5.5"),
        "gpt-5.5-pro" => Some("gpt-5.5-pro"),
        "gpt-5.4" => Some("gpt-5.4"),
        "gpt-5.4-pro" => Some("gpt-5.4-pro"),
        "gpt-5.4-mini" => Some("gpt-5.4-mini"),
        "gpt-5.4-nano" => Some("gpt-5.4-nano"),
        "gpt-5.3-codex" => Some("gpt-5.3-codex"),
        _ => None,
    }
}

pub fn claude_model_ids() -> &'static [&'static str] {
    &[
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-fable-5",
        "opus",
        "sonnet",
        "haiku",
        "fable",
    ]
}

pub fn codex_model_ids() -> &'static [&'static str] {
    &[
        "gpt-5.6-sol",
        "gpt-5.5",
        "gpt-5.5-pro",
        "gpt-5.4",
        "gpt-5.4-pro",
        "gpt-5.4-mini",
        "gpt-5.4-nano",
        "gpt-5.3-codex",
        "gpt",
    ]
}

/// Constant-time compare for bearer tokens. Length mismatch returns false.
pub fn const_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Parse `X-Proxy-Auth` (optional `Bearer ` prefix). Empty expected token → allow.
pub fn check_proxy_auth(expected_token: &str, header_value: Option<&str>) -> bool {
    if expected_token.is_empty() {
        return true;
    }
    let presented = header_value.unwrap_or("").trim();
    let presented = presented
        .strip_prefix("Bearer ")
        .or_else(|| presented.strip_prefix("bearer "))
        .unwrap_or(presented)
        .trim();
    const_time_eq(presented, expected_token)
}

/// Mint or load Keychain-held proxy tokens. Never logs values.
pub fn ensure_proxy_tokens(store: &dyn SecretStore) -> Result<(String, String), String> {
    let claude = match load_claude_proxy_token(store)? {
        Some(t) => t,
        None => {
            let t = random_hex(32)?;
            store_claude_proxy_token(store, &t)?;
            t
        }
    };
    let codex = match load_codex_proxy_token(store)? {
        Some(t) => t,
        None => {
            let t = random_hex(32)?;
            store_codex_proxy_token(store, &t)?;
            t
        }
    };
    Ok((claude, codex))
}

/// Build the four proxy env keys for Compose. Ready adapters get host URL +
/// token; unready adapters get empty strings so Gateway readiness stays
/// fail-closed (`proxy_auth_unavailable` / empty base_url).
pub fn build_proxy_compose_pairs(
    status: &CliAdaptersStatus,
    claude_token: &str,
    codex_token: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::with_capacity(4);
    let claude_inject = decide_proxy_injection(status.claude.is_ready(), !claude_token.is_empty());
    let codex_inject = decide_proxy_injection(status.codex.is_ready(), !codex_token.is_empty());

    let claude_url = if claude_inject {
        CLAUDE_HOST_PROXY_URL
    } else {
        ""
    };
    let codex_url = if codex_inject {
        CODEX_HOST_PROXY_URL
    } else {
        ""
    };
    let claude_tok = if claude_inject { claude_token } else { "" };
    let codex_tok = if codex_inject { codex_token } else { "" };

    for (k, v) in [
        ("CLAUDE_PROXY_URL", claude_url),
        ("CODEX_PROXY_URL", codex_url),
        ("CLAUDE_PROXY_TOKEN", claude_tok),
        ("CODEX_PROXY_TOKEN", codex_tok),
    ] {
        validate_env_value(k, v)?;
        pairs.push((k.to_string(), v.to_string()));
    }
    Ok(pairs)
}

/// Inject proxy keys into a Compose env map (overwrites).
pub fn apply_proxy_compose_env(
    env: &mut ComposeEnv,
    status: &CliAdaptersStatus,
    claude_token: &str,
    codex_token: &str,
) -> Result<(), String> {
    for (k, v) in build_proxy_compose_pairs(status, claude_token, codex_token)? {
        env.insert(k, v);
    }
    Ok(())
}

/// Empty proxy slots for teardown (never load Keychain tokens onto stop path).
pub fn empty_proxy_compose_pairs() -> Vec<(String, String)> {
    [
        "CLAUDE_PROXY_URL",
        "CODEX_PROXY_URL",
        "CLAUDE_PROXY_TOKEN",
        "CODEX_PROXY_TOKEN",
    ]
    .into_iter()
    .map(|k| (k.to_string(), String::new()))
    .collect()
}

// --- Process-owned adapter state ------------------------------------------------

struct RunningAdapter {
    kind: AdapterKind,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

/// Why a new connection was refused before request threads ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreauthReject {
    /// Shared global pre-auth pool is full.
    GlobalLimit,
    /// This client IP already holds [`MAX_PREAUTH_PER_IP`] pre-auth slots.
    PerIpLimit,
}

impl PreauthReject {
    pub fn message(self) -> &'static str {
        match self {
            Self::GlobalLimit => "connection limit",
            Self::PerIpLimit => "per-ip connection limit",
        }
    }
}

/// Tracks global + per-IP pre-auth occupancy until the full request is buffered.
struct PreauthPool {
    global: AtomicUsize,
    per_ip: Mutex<HashMap<String, usize>>,
}

impl PreauthPool {
    fn new() -> Self {
        Self {
            global: AtomicUsize::new(0),
            per_ip: Mutex::new(HashMap::new()),
        }
    }

    /// Reserve one pre-auth slot for `client_ip`, or explain why not.
    fn try_acquire(self: &Arc<Self>, client_ip: &str) -> Result<PreauthConnectionGuard, PreauthReject> {
        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        let ip_count = map.get(client_ip).copied().unwrap_or(0);
        if ip_count >= MAX_PREAUTH_PER_IP {
            return Err(PreauthReject::PerIpLimit);
        }
        match self.global.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
            (active < MAX_PREAUTH_CONNECTIONS).then_some(active + 1)
        }) {
            Ok(_) => {
                *map.entry(client_ip.to_string()).or_insert(0) += 1;
                Ok(PreauthConnectionGuard {
                    pool: Arc::clone(self),
                    client_ip: client_ip.to_string(),
                })
            }
            Err(_) => Err(PreauthReject::GlobalLimit),
        }
    }

    fn release(&self, client_ip: &str) {
        self.global.fetch_sub(1, Ordering::SeqCst);
        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(client_ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(client_ip);
            }
        }
    }

    #[cfg(test)]
    fn global_active(&self) -> usize {
        self.global.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn ip_active(&self, client_ip: &str) -> usize {
        self.per_ip
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(client_ip)
            .copied()
            .unwrap_or(0)
    }
}

struct PreauthConnectionGuard {
    pool: Arc<PreauthPool>,
    client_ip: String,
}

impl Drop for PreauthConnectionGuard {
    fn drop(&mut self) {
        self.pool.release(&self.client_ip);
    }
}

struct AdapterState {
    claude: Option<RunningAdapter>,
    codex: Option<RunningAdapter>,
    last_status: CliAdaptersStatus,
}

impl AdapterState {
    fn new() -> Self {
        Self {
            claude: None,
            codex: None,
            last_status: CliAdaptersStatus::default(),
        }
    }
}

fn adapter_state() -> &'static Mutex<AdapterState> {
    static STATE: OnceLock<Mutex<AdapterState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(AdapterState::new()))
}

/// Last known non-secret status (for compose env construction).
pub fn current_status() -> CliAdaptersStatus {
    adapter_state()
        .lock()
        .map(|g| g.last_status)
        .unwrap_or_default()
}

/// Start app-owned adapters when CLIs are present and authenticated.
/// Missing CLI → NotReady (not an error). Never logs token values.
///
/// Loads proxy tokens from Keychain once via [`ensure_proxy_tokens`]. When the
/// caller already holds those tokens (e.g. FullStart resume building compose
/// env), use [`ensure_cli_adapters_with_tokens`] to avoid a second Keychain get
/// per account — each get can surface a macOS authorization dialog.
pub fn ensure_cli_adapters(store: &dyn SecretStore) -> CliAdaptersStatus {
    match ensure_proxy_tokens(store) {
        Ok((claude_tok, codex_tok)) => ensure_cli_adapters_with_tokens(&claude_tok, &codex_tok),
        Err(_) => {
            let status = CliAdaptersStatus {
                claude: AdapterHealth::NotReady,
                codex: AdapterHealth::NotReady,
                claude_reason: AdapterNotReadyReason::TokenMissing,
                codex_reason: AdapterNotReadyReason::TokenMissing,
            };
            if let Ok(mut g) = adapter_state().lock() {
                g.last_status = status;
            }
            status
        }
    }
}

/// Same as [`ensure_cli_adapters`] but uses already-loaded proxy tokens so the
/// Keychain is not re-entered for Claude/Codex accounts on this call.
///
/// Claude and Codex preflight/spawn run on parallel threads so cold launch pays
/// roughly the slower CLI once (~version+auth), not the sum of both serial
/// probes (each can take seconds under cold Node/Python starts).
pub fn ensure_cli_adapters_with_tokens(claude_tok: &str, codex_tok: &str) -> CliAdaptersStatus {
    let mut status = CliAdaptersStatus::default();
    {
        let mut g = match adapter_state().lock() {
            Ok(g) => g,
            Err(_) => {
                status.claude_reason = AdapterNotReadyReason::StartFailed;
                status.codex_reason = AdapterNotReadyReason::StartFailed;
                return status;
            }
        };
        // Drop dead children before re-evaluating.
        reap_dead(&mut g);

        // Disjoint field borrows: probe both adapters concurrently. Ports and
        // CLI binaries are independent; serial preflight was the ~30s stall.
        let AdapterState {
            claude,
            codex,
            last_status,
        } = &mut *g;
        let (c_result, x_result) = thread::scope(|s| {
            let claude_tok = claude_tok;
            let codex_tok = codex_tok;
            let c_handle =
                s.spawn(move || ensure_one(claude, AdapterKind::Claude, claude_tok));
            let x_handle = s.spawn(move || ensure_one(codex, AdapterKind::Codex, codex_tok));
            (
                c_handle
                    .join()
                    .unwrap_or((AdapterHealth::NotReady, AdapterNotReadyReason::StartFailed)),
                x_handle
                    .join()
                    .unwrap_or((AdapterHealth::NotReady, AdapterNotReadyReason::StartFailed)),
            )
        });
        let (c_health, c_reason) = c_result;
        let (x_health, x_reason) = x_result;
        status.claude = c_health;
        status.claude_reason = c_reason;
        status.codex = x_health;
        status.codex_reason = x_reason;
        *last_status = status;
    }
    status
}

/// Restart both adapters (stop then ensure).
/// Public lifecycle surface for pack recovery / future commands.
#[allow(dead_code)]
pub fn restart_cli_adapters(store: &dyn SecretStore) -> CliAdaptersStatus {
    stop_cli_adapters();
    ensure_cli_adapters(store)
}

/// Stop all app-owned adapter servers. Idempotent.
pub fn stop_cli_adapters() {
    let mut g = match adapter_state().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    stop_one(&mut g.claude);
    stop_one(&mut g.codex);
    g.last_status = CliAdaptersStatus {
        claude: AdapterHealth::NotReady,
        codex: AdapterHealth::NotReady,
        claude_reason: AdapterNotReadyReason::NotStarted,
        codex_reason: AdapterNotReadyReason::NotStarted,
    };
}

fn reap_dead(state: &mut AdapterState) {
    if let Some(r) = state.claude.as_ref() {
        if r.shutdown.load(Ordering::SeqCst)
            || r.join.as_ref().map(|j| j.is_finished()).unwrap_or(true)
        {
            stop_one(&mut state.claude);
        }
    }
    if let Some(r) = state.codex.as_ref() {
        if r.shutdown.load(Ordering::SeqCst)
            || r.join.as_ref().map(|j| j.is_finished()).unwrap_or(true)
        {
            stop_one(&mut state.codex);
        }
    }
}

fn stop_one(slot: &mut Option<RunningAdapter>) {
    if let Some(mut r) = slot.take() {
        r.shutdown.store(true, Ordering::SeqCst);
        // Unblock accept by connecting once.
        let _ = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", r.kind.port())
                .parse()
                .expect("port"),
            Duration::from_millis(100),
        )
        .map(|s| {
            let _ = s.shutdown(Shutdown::Both);
        });
        if let Some(j) = r.join.take() {
            let _ = j.join();
        }
    }
}

fn ensure_one(
    slot: &mut Option<RunningAdapter>,
    kind: AdapterKind,
    token: &str,
) -> (AdapterHealth, AdapterNotReadyReason) {
    if token.is_empty() {
        return (AdapterHealth::NotReady, AdapterNotReadyReason::TokenMissing);
    }
    // Already running and healthy.
    if let Some(r) = slot.as_ref() {
        if !r.shutdown.load(Ordering::SeqCst)
            && r.join.as_ref().map(|j| !j.is_finished()).unwrap_or(false)
            && probe_adapter_ready(kind, token)
        {
            return (AdapterHealth::Ready, AdapterNotReadyReason::None);
        }
        // Stale — replace.
        stop_one(slot);
    }

    // A listener not represented by our owned slot is foreign, regardless of
    // what it returns. Never send the real proxy token to probe or adopt it.
    if let Some(status) = occupied_port_status(kind.port()) {
        return status;
    }

    match cli_preflight(kind) {
        Ok(()) => {}
        Err(reason) => return (AdapterHealth::NotReady, reason),
    }

    match spawn_adapter_server(kind, token) {
        Ok(running) => {
            *slot = Some(running);
            // Wait briefly for /health + auth.
            for _ in 0..50 {
                if probe_adapter_ready(kind, token) {
                    return (AdapterHealth::Ready, AdapterNotReadyReason::None);
                }
                thread::sleep(Duration::from_millis(100));
            }
            stop_one(slot);
            (AdapterHealth::NotReady, AdapterNotReadyReason::StartFailed)
        }
        Err(_) => classify_spawn_failure(kind.port()),
    }
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().expect("port"),
        Duration::from_millis(150),
    )
    .is_ok()
}

fn occupied_port_status(port: u16) -> Option<(AdapterHealth, AdapterNotReadyReason)> {
    port_open(port).then_some((
        AdapterHealth::NotReady,
        AdapterNotReadyReason::PortBusyForeign,
    ))
}

/// Preserve the ownership rule across the check-then-bind race: if another
/// process occupies the port after the initial check but before our bind, the
/// failure is still a foreign-port conflict rather than a generic start error.
fn classify_spawn_failure(port: u16) -> (AdapterHealth, AdapterNotReadyReason) {
    occupied_port_status(port)
        .unwrap_or((AdapterHealth::NotReady, AdapterNotReadyReason::StartFailed))
}

/// Probe GET /v1/models with X-Proxy-Auth. Deterministic, no provider spend.
pub fn probe_adapter_ready(kind: AdapterKind, token: &str) -> bool {
    let url = format!("{}/v1/models", kind.loopback_base());
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(300))
        .timeout(Duration::from_secs(2))
        .build();
    let mut req = agent.get(&url);
    if !token.is_empty() {
        req = req.set("X-Proxy-Auth", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

/// Apply the same login-shell PATH authority used for packaged Council spawn
/// (`gui_login_environment`). Finder-launched apps do not inherit the operator
/// CLI PATH; without this, `claude`/`codex` resolve as missing despite valid
/// installs. Only PATH is applied — never proxy tokens or other secrets.
/// Values are never logged.
fn apply_gui_login_path(cmd: &mut Command) {
    for (k, v) in gui_login_environment() {
        if k == "PATH" {
            cmd.env("PATH", v);
            return;
        }
    }
}

fn cli_command(bin: &str) -> Command {
    let mut cmd = Command::new(bin);
    apply_gui_login_path(&mut cmd);
    cmd
}

fn cli_preflight(kind: AdapterKind) -> Result<(), AdapterNotReadyReason> {
    let mut ver = cli_command(kind.cli_bin());
    ver.arg("--version");
    match run_command_timeout(ver, CLI_VERSION_TIMEOUT) {
        Ok(o) if o.status.success() => {}
        _ => return Err(AdapterNotReadyReason::CliMissing),
    }
    for attempt in 0..CLI_AUTH_ATTEMPTS {
        if cli_authenticated(kind) {
            return Ok(());
        }
        if attempt + 1 < CLI_AUTH_ATTEMPTS {
            thread::sleep(Duration::from_millis(400));
        }
    }
    Err(AdapterNotReadyReason::CliUnauthenticated)
}

fn cli_authenticated(kind: AdapterKind) -> bool {
    match kind {
        AdapterKind::Claude => {
            let mut cmd = cli_command("claude");
            cmd.args(["auth", "status"]);
            match run_command_timeout(cmd, CLI_AUTH_TIMEOUT) {
                Ok(o) if o.status.success() => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| v.get("loggedIn").and_then(|x| x.as_bool()))
                        .unwrap_or(false)
                }
                _ => false,
            }
        }
        AdapterKind::Codex => {
            let mut cmd = cli_command("codex");
            cmd.args(["login", "status"]);
            matches!(
                run_command_timeout(cmd, CLI_AUTH_TIMEOUT),
                Ok(o) if o.status.success()
            )
        }
    }
}

/// Bind the adapter listen socket on [`ADAPTER_BIND_HOST`] (IPv4 all-interfaces).
/// Health probes and shutdown wakeups still use loopback URLs; those remain
/// reachable under an all-interfaces bind.
pub fn bind_adapter_listener(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind((ADAPTER_BIND_HOST, port)).map_err(|_| "bind failed".to_string())
}

fn spawn_adapter_server(kind: AdapterKind, token: &str) -> Result<RunningAdapter, String> {
    // Token is mandatory at ensure_one; refuse empty here as a second gate so a
    // non-loopback bind never starts tokenless even if a future caller drifts.
    if token.is_empty() {
        return Err("token required for non-loopback adapter bind".to_string());
    }
    let listener = bind_adapter_listener(kind.port())?;
    listener
        .set_nonblocking(false)
        .map_err(|_| "listener mode".to_string())?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_t = Arc::clone(&shutdown);
    let token_arc = Arc::new(token.to_string());
    let concurrent = Arc::new(AtomicUsize::new(0));
    let preauth_pool = Arc::new(PreauthPool::new());
    let rate_limiter = Arc::new(Mutex::new(IpRateLimitMap::proxy_default()));
    let join = thread::Builder::new()
        .name(format!("irin-{}-adapter", kind.service_name()))
        .spawn(move || {
            accept_loop(
                listener,
                kind,
                token_arc,
                shutdown_t,
                concurrent,
                preauth_pool,
                rate_limiter,
            );
        })
        .map_err(|_| "spawn thread failed".to_string())?;
    Ok(RunningAdapter {
        kind,
        shutdown,
        join: Some(join),
    })
}

fn accept_loop(
    listener: TcpListener,
    kind: AdapterKind,
    token: Arc<String>,
    shutdown: Arc<AtomicBool>,
    concurrent: Arc<AtomicUsize>,
    preauth_pool: Arc<PreauthPool>,
    rate_limiter: Arc<Mutex<IpRateLimitMap>>,
) {
    // Wake periodically via short accept timeout so shutdown is observed.
    let _ = listener.set_nonblocking(true);
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let client_ip = peer.ip().to_string();
                let guard = match preauth_pool.try_acquire(&client_ip) {
                    Ok(g) => g,
                    Err(reject) => {
                        let mut stream = stream;
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                        let _ = write_json(
                            &mut stream,
                            503,
                            &json!({"error":{"type":"busy","message":reject.message()}}),
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                };
                let tok = Arc::clone(&token);
                let conc = Arc::clone(&concurrent);
                let rl = Arc::clone(&rate_limiter);
                // On spawn failure the moved guard drops with the closure and
                // releases the reserved pre-auth slot.
                let _ = thread::Builder::new()
                    .name(format!("irin-{}-req", kind.service_name()))
                    .spawn(move || {
                        // The listener is nonblocking for shutdown polling; on
                        // platforms where accept inherits that flag, restore
                        // blocking request reads under the bounded pre-auth timeout.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(PREAUTH_READ_TIMEOUT));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                        handle_client(stream, kind, &tok, &conc, &rl, &client_ip, guard);
                    });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    kind: AdapterKind,
    token: &str,
    concurrent: &AtomicUsize,
    rate_limiter: &Mutex<IpRateLimitMap>,
    client_ip: &str,
    preauth: PreauthConnectionGuard,
) {
    let req = match read_complete_http_request(&mut stream) {
        Ok(v) => v,
        Err(RequestReadError::Closed | RequestReadError::Io) => return,
        Err(e) => {
            let (code, msg) = e.http_response();
            let _ = write_json(&mut stream, code, &json!({"error": msg}));
            return;
        }
    };
    // Full request is buffered — free the pre-auth slot before auth/rate-limit/
    // CLI work so incomplete peers cannot pin capacity for the CLI duration.
    drop(preauth);

    let auth_ok = check_proxy_auth(token, req.headers.get("x-proxy-auth").map(String::as_str));

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => {
            // Health probes never consume the chat per-IP budget (Python GET path).
            let _ = write_json(
                &mut stream,
                200,
                &json!({
                    "status": "ok",
                    "service": kind.service_name(),
                    "backend": format!("{}-cli", kind.cli_bin()),
                }),
            );
        }
        ("GET", "/v1/models") => {
            // Model probes never consume the chat per-IP budget (Python GET path).
            if !auth_ok {
                let _ = write_json(
                    &mut stream,
                    401,
                    &json!({"error":{"type":"unauthorized","message":"X-Proxy-Auth bearer required"}}),
                );
                return;
            }
            let ids = match kind {
                AdapterKind::Claude => claude_model_ids(),
                AdapterKind::Codex => codex_model_ids(),
            };
            let data: Vec<Value> = ids
                .iter()
                .map(|id| json!({"id": id, "object": "model"}))
                .collect();
            let _ = write_json(&mut stream, 200, &json!({"data": data}));
        }
        ("POST", "/v1/chat/completions")
        | ("POST", "/v1/messages")
        | ("POST", "/chat/completions")
        | ("POST", "/messages") => {
            if !auth_ok {
                let _ = write_json(
                    &mut stream,
                    401,
                    &json!({"error":{"type":"unauthorized","message":"X-Proxy-Auth bearer required"}}),
                );
                return;
            }
            // Per-IP token bucket before concurrency / CLI spend (Python POST path).
            let allowed = rate_limiter
                .lock()
                .map(|mut g| g.allow(client_ip, wall_now_secs()))
                .unwrap_or(false);
            if !allowed {
                let _ = write_json(
                    &mut stream,
                    429,
                    &json!({"error":{"type":"rate_limited","message":"per-IP rate limit exceeded"}}),
                );
                return;
            }
            let prev = concurrent.fetch_add(1, Ordering::SeqCst);
            if prev >= MAX_CONCURRENT_CLI {
                concurrent.fetch_sub(1, Ordering::SeqCst);
                let _ = write_json(
                    &mut stream,
                    429,
                    &json!({"error":{"type":"rate_limited","message":"concurrency limit"}}),
                );
                return;
            }
            let result = match serde_json::from_slice::<Value>(&req.body) {
                Ok(v) => dispatch_chat(kind, &v),
                Err(e) => {
                    json!({"error":{"message": format!("invalid JSON: {e}"), "type":"proxy_error"}})
                }
            };
            concurrent.fetch_sub(1, Ordering::SeqCst);
            let code = if result.pointer("/error/type").and_then(|t| t.as_str())
                == Some("invalid_request_error")
            {
                400
            } else if result.get("error").is_some() {
                502
            } else {
                200
            };
            let _ = write_json(&mut stream, code, &result);
        }
        _ => {
            let _ = write_json(&mut stream, 404, &json!({"error": "not found"}));
        }
    }
}

/// Complete HTTP/1.1 request after header + body assembly.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Failures while assembling a complete HTTP request from a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestReadError {
    Closed,
    Io,
    HeadersTooLarge,
    Malformed,
    MissingContentLength,
    BodyTooLarge,
    IncompleteBody,
}

impl RequestReadError {
    fn http_response(self) -> (u16, &'static str) {
        match self {
            Self::Closed | Self::Io => (400, "bad request"),
            Self::HeadersTooLarge => (431, "request headers too large"),
            Self::Malformed => (400, "malformed request"),
            Self::MissingContentLength => (411, "Content-Length required"),
            Self::BodyTooLarge => (413, "request body too large"),
            Self::IncompleteBody => (400, "incomplete body"),
        }
    }
}

/// Read a complete HTTP/1.1 request: full header block, then exactly
/// `Content-Length` body bytes (or zero for no-body methods). TCP may deliver
/// headers and body across multiple reads — never assume a single `read`.
///
/// Caps: [`MAX_REQUEST_HEADER_BYTES`] for the header block,
/// [`MAX_REQUEST_BODY_BYTES`] for the body (prompt-scale; not a silent truncate).
pub fn read_complete_http_request<R: Read>(
    stream: &mut R,
) -> Result<HttpRequest, RequestReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if buf.len() > MAX_REQUEST_HEADER_BYTES {
            return Err(RequestReadError::HeadersTooLarge);
        }
        if let Some(idx) = find_header_end(&buf) {
            break idx;
        }
        let n = stream.read(&mut tmp).map_err(|_| RequestReadError::Io)?;
        if n == 0 {
            return if buf.is_empty() {
                Err(RequestReadError::Closed)
            } else {
                Err(RequestReadError::Malformed)
            };
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_REQUEST_HEADER_BYTES {
            return Err(RequestReadError::HeadersTooLarge);
        }
    };

    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| RequestReadError::Malformed)?;
    let (method, path, headers) = parse_http_headers(head).ok_or(RequestReadError::Malformed)?;

    let body_start = header_end + 4; // skip \r\n\r\n
    let mut body = if body_start < buf.len() {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };

    let needs_body = matches!(method.as_str(), "POST" | "PUT" | "PATCH");
    if !needs_body {
        // Discard any unexpected trailer bytes; no body for GET/HEAD/…
        return Ok(HttpRequest {
            method,
            path,
            headers,
            body: Vec::new(),
        });
    }

    let len_str = headers
        .get("content-length")
        .ok_or(RequestReadError::MissingContentLength)?;
    let content_len: usize = len_str.parse().map_err(|_| RequestReadError::Malformed)?;
    if content_len > MAX_REQUEST_BODY_BYTES {
        return Err(RequestReadError::BodyTooLarge);
    }

    while body.len() < content_len {
        let remaining = content_len - body.len();
        let chunk = remaining.min(tmp.len());
        let n = stream
            .read(&mut tmp[..chunk])
            .map_err(|_| RequestReadError::Io)?;
        if n == 0 {
            return Err(RequestReadError::IncompleteBody);
        }
        body.extend_from_slice(&tmp[..n]);
    }
    // Exact length only — drop any overshoot (should not happen with Content-Length).
    body.truncate(content_len);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_http_headers(head: &str) -> Option<(String, String, HashMap<String, String>)> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some((method, path, headers))
}

/// Legacy header-only parse used by unit tests for auth header extraction.
#[cfg(test)]
fn parse_http_request(raw: &str) -> Option<(String, String, HashMap<String, String>, String)> {
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let (method, path, headers) = parse_http_headers(head)?;
    Some((method, path, headers, body.to_string()))
}

fn write_json(stream: &mut TcpStream, code: u16, body: &Value) -> std::io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&payload)?;
    let _ = stream.flush();
    Ok(())
}

fn dispatch_chat(kind: AdapterKind, body: &Value) -> Value {
    match kind {
        AdapterKind::Claude => {
            let (prompt, system, model) = extract_claude_prompt(body);
            if prompt.is_empty() {
                return json!({"error":{"message":"no prompt content found","type":"invalid_request_error"}});
            }
            let Some(model) = model else {
                return json!({
                    "error": {
                        "type": "invalid_request_error",
                        "message": "model not in allow-list",
                        "allowed_models": claude_model_ids(),
                    }
                });
            };
            let result = call_claude_cli(&prompt, system.as_deref(), model);
            to_openai_response(&result, model)
        }
        AdapterKind::Codex => {
            let (prompt, model, effort) = extract_codex_prompt(body);
            if prompt.is_empty() {
                return json!({"error":{"message":"no prompt content found","type":"invalid_request_error"}});
            }
            let Some(model) = model else {
                return json!({
                    "error": {
                        "type": "invalid_request_error",
                        "message": "model not in allow-list",
                        "allowed_models": codex_model_ids(),
                    }
                });
            };
            let result = call_codex_cli(&prompt, model, &effort);
            to_openai_response(&result, model)
        }
    }
}

/// Extract prompt/system/model from OpenAI or Anthropic-shaped bodies.
pub fn extract_claude_prompt(body: &Value) -> (String, Option<String>, Option<&'static str>) {
    let model_str = body.get("model").and_then(|m| m.as_str()).unwrap_or("opus");
    let model = resolve_claude_model(model_str);

    let mut system = String::new();
    if let Some(s) = body.get("system").and_then(|s| s.as_str()) {
        system = s.to_string();
    } else if let Some(arr) = body.get("system").and_then(|s| s.as_array()) {
        system = arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    let mut parts = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = message_text(msg.get("content"));
            match role {
                "system" if system.is_empty() => system = content,
                "user" => parts.push(content),
                "assistant" => parts.push(format!("[Previous assistant response: {content}]")),
                _ => {}
            }
        }
    }
    let prompt = parts.join("\n\n");
    let system = if system.is_empty() {
        None
    } else {
        Some(system)
    };
    (prompt, system, model)
}

pub fn extract_codex_prompt(body: &Value) -> (String, Option<&'static str>, String) {
    let model_str = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("gpt-5.5");
    let model = resolve_codex_model(model_str);
    let mut effort = body
        .get("reasoning_effort")
        .and_then(|e| e.as_str())
        .unwrap_or("medium")
        .to_string();
    if !matches!(effort.as_str(), "minimal" | "low" | "medium" | "high") {
        effort = "medium".into();
    }

    let mut system = String::new();
    let mut parts = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = message_text(msg.get("content"));
            match role {
                "system" => {
                    if system.is_empty() {
                        system = content;
                    } else {
                        system = format!("{system}\n\n{content}");
                    }
                }
                "user" => parts.push(content),
                "assistant" => parts.push(format!("[Previous assistant response: {content}]")),
                _ => {}
            }
        }
    }
    let prompt = if system.is_empty() {
        parts.join("\n\n")
    } else {
        format!("<system>\n{system}\n</system>\n\n{}", parts.join("\n"))
    };
    (prompt, model, effort)
}

fn message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| {
                let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("text");
                if ty == "text" {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

struct CliCallResult {
    text: Option<String>,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    error: Option<String>,
    latency_ms: u64,
}

fn call_claude_cli(prompt: &str, system: Option<&str>, model: &str) -> CliCallResult {
    let t0 = Instant::now();
    let mut cmd = cli_command("claude");
    cmd.args([
        "-p",
        "--model",
        model,
        "--output-format",
        "json",
        "--no-session-persistence",
    ]);
    if let Some(s) = system {
        if !s.is_empty() {
            cmd.args(["--system-prompt", s]);
        }
    }
    match run_command_timeout_input(cmd, CLAUDE_CLI_TIMEOUT, Some(prompt.as_bytes())) {
        Ok(out) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let short: String = stderr.chars().take(200).collect();
                return CliCallResult {
                    text: None,
                    model: model.into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    error: Some(format!("claude CLI exit: {short}")),
                    latency_ms,
                };
            }
            parse_claude_stdout(&out.stdout, model, latency_ms)
        }
        Err(e) if e.contains(DockerErrorKind::Timeout.as_str()) => CliCallResult {
            text: None,
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            error: Some("claude CLI timeout (300s)".into()),
            latency_ms: t0.elapsed().as_millis() as u64,
        },
        Err(e) => CliCallResult {
            text: None,
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            error: Some(format!("claude CLI: {e}")),
            latency_ms: t0.elapsed().as_millis() as u64,
        },
    }
}

fn parse_claude_stdout(stdout: &[u8], model: &str, latency_ms: u64) -> CliCallResult {
    let stdout = String::from_utf8_lossy(stdout);
    if let Ok(data) = serde_json::from_str::<Value>(&stdout) {
        if data.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            return CliCallResult {
                text: None,
                model: model.into(),
                input_tokens: 0,
                output_tokens: 0,
                error: Some(
                    data.get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                ),
                latency_ms,
            };
        }
        let usage = data.get("usage").cloned().unwrap_or(json!({}));
        return CliCallResult {
            text: Some(
                data.get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            model: data
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or(model)
                .to_string(),
            input_tokens: usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            error: None,
            latency_ms: data
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(latency_ms),
        };
    }
    CliCallResult {
        text: Some(stdout.trim().to_string()),
        model: format!("claude-cli-{model}"),
        input_tokens: 0,
        output_tokens: 0,
        error: None,
        latency_ms,
    }
}

fn call_codex_cli(prompt: &str, model: &str, effort: &str) -> CliCallResult {
    let t0 = Instant::now();
    let mut cmd = cli_command("codex");
    cmd.args([
        "exec",
        "--skip-git-repo-check",
        "--json",
        "-c",
        &format!("model_reasoning_effort={effort}"),
        "-m",
        model,
        "-",
    ]);
    match run_command_timeout_input(cmd, CODEX_CLI_TIMEOUT, Some(prompt.as_bytes())) {
        Ok(out) => {
            let latency_ms = t0.elapsed().as_millis() as u64;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let short: String = stderr.chars().take(400).collect();
                return CliCallResult {
                    text: None,
                    model: model.into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    error: Some(format!("codex CLI exit: {short}")),
                    latency_ms,
                };
            }
            parse_codex_stdout(&out.stdout, model, latency_ms)
        }
        Err(e) if e.contains(DockerErrorKind::Timeout.as_str()) => CliCallResult {
            text: None,
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            error: Some("codex CLI timeout (600s)".into()),
            latency_ms: t0.elapsed().as_millis() as u64,
        },
        Err(e) => CliCallResult {
            text: None,
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            error: Some(format!("codex CLI: {e}")),
            latency_ms: t0.elapsed().as_millis() as u64,
        },
    }
}

fn parse_codex_stdout(stdout: &[u8], model: &str, latency_ms: u64) -> CliCallResult {
    let mut text = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    for line in String::from_utf8_lossy(stdout).lines() {
        if !line.trim_start().starts_with('{') {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("item.completed") => {
                if let Some(item) = ev.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                        if let Some(chunk) = item.get("text").and_then(|t| t.as_str()) {
                            text.push_str(chunk);
                        }
                    }
                }
            }
            Some("turn.completed") => {
                if let Some(u) = ev.get("usage") {
                    input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let out_tok = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let reason = u
                        .get("reasoning_output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    output_tokens = out_tok + reason;
                }
            }
            _ => {}
        }
    }
    CliCallResult {
        text: Some(text),
        model: format!("codex-cli-{model}"),
        input_tokens,
        output_tokens,
        error: None,
        latency_ms,
    }
}

fn to_openai_response(result: &CliCallResult, model: &str) -> Value {
    if let Some(err) = &result.error {
        return json!({"error":{"message": err, "type":"proxy_error"}});
    }
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let id = format!(
        "chatcmpl-{:x}",
        created ^ result.latency_ms.wrapping_mul(0x9e37)
    );
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": if result.model.is_empty() { model } else { &result.model },
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": result.text.as_deref().unwrap_or(""),
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": result.input_tokens,
            "completion_tokens": result.output_tokens,
            "total_tokens": result.input_tokens + result.output_tokens,
        },
        "x_latency_ms": result.latency_ms,
    })
}

#[cfg(test)]
#[path = "cli_adapters_tests.rs"]
mod tests;
