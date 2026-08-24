//! Allow-listed Tailscale CLI construction and JSON interpretation.
//!
//! Command construction and parsers are pure/testable. Live process execution
//! is isolated so unit tests never invoke the host binary.
//!
//! Tailscale 1.98 contract used by phone publication:
//! - `tailscale status --json`
//! - `tailscale serve status --json`
//! - `tailscale serve --bg --yes --https=<port> [--set-path=<path>] <target>`
//! - `tailscale serve --bg --yes --https=<port> [--set-path=<path>] off`
//!
//! IRIN publishes on a dedicated HTTPS port (default 8443) so other node Serve
//! handlers (for example Hermes root on 443) can coexist. Product code never
//! builds Funnel argv and never uses global `tailscale serve reset`.

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

/// Allow-listed absolute paths for the Tailscale CLI only.
pub const TAILSCALE_CLI_ALLOWLIST: &[&str] = &[
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
];

/// Default dedicated HTTPS Serve port for IRIN (Hermes may keep 443).
pub const DEFAULT_HTTPS_PORT: u16 = 8443;

/// Product default port constant used by pure argv builders and tests.
/// Prefer [`https_port`] when an operator override may apply.
#[cfg(test)]
pub const HTTPS_PORT: u16 = DEFAULT_HTTPS_PORT;

/// Environment override for the dedicated IRIN HTTPS Serve port.
pub const HTTPS_PORT_ENV: &str = "IRIN_TAILSCALE_HTTPS_PORT";

pub const COUNCIL_LOOPBACK: &str = "http://127.0.0.1:8765";
pub const LEGACY_GATEWAY_WATCH_TARGET: &str = "http://127.0.0.1:18080/watch";
pub const GATEWAY_HEALTH_TARGET: &str = "http://127.0.0.1:18080/health";

/// Resolve the HTTPS port IRIN should own. Invalid or empty overrides fall
/// back to [`DEFAULT_HTTPS_PORT`].
pub fn https_port() -> u16 {
    std::env::var(HTTPS_PORT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
        .unwrap_or(DEFAULT_HTTPS_PORT)
}

/// One Serve publication target. Only Serve paths — never Funnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeRoute {
    /// URL path prefix (`/` for root, `/watch`, `/health`).
    pub path: &'static str,
    /// Full loopback proxy target including optional path suffix.
    pub target: &'static str,
}

/// IRIN-owned publication table.
///
/// `/` always maps to Council. Gateway paths are included only when the pack
/// is enabled — Gateway is never published on any other listener.
pub fn irin_serve_routes(gateway_pack_enabled: bool) -> Vec<ServeRoute> {
    let mut routes = vec![ServeRoute {
        path: "/",
        target: COUNCIL_LOOPBACK,
    }];
    if gateway_pack_enabled {
        routes.push(ServeRoute {
            path: "/health",
            target: GATEWAY_HEALTH_TARGET,
        });
    }
    routes
}

/// Pure argv (without the binary) for `tailscale status --json`.
pub fn status_json_args() -> Vec<String> {
    vec!["status".into(), "--json".into()]
}

/// Pure argv for `tailscale serve status --json`.
pub fn serve_status_json_args() -> Vec<String> {
    vec!["serve".into(), "status".into(), "--json".into()]
}

/// Pure argv for one Serve route application on the IRIN HTTPS port.
///
/// Root uses no `--set-path`. Non-root uses `--set-path=<path>`.
/// Never emits `funnel`. Never touches other ports.
#[cfg(test)]
pub fn serve_apply_route_args(route: &ServeRoute) -> Vec<String> {
    serve_apply_route_args_for_port(route, HTTPS_PORT)
}

/// Apply argv for an explicit HTTPS port (tests and ownership restore).
pub fn serve_apply_route_args_for_port(route: &ServeRoute, port: u16) -> Vec<String> {
    let mut args = vec![
        "serve".into(),
        "--bg".into(),
        "--yes".into(),
        format!("--https={port}"),
    ];
    if route.path != "/" {
        args.push(format!("--set-path={}", route.path));
    }
    args.push(route.target.to_string());
    args
}

/// All apply argvs for the current product route table on the default port.
#[cfg(test)]
pub fn serve_apply_all_args(gateway_pack_enabled: bool) -> Vec<Vec<String>> {
    serve_apply_all_args_for_port(gateway_pack_enabled, HTTPS_PORT)
}

/// All apply argvs for an explicit HTTPS port.
pub fn serve_apply_all_args_for_port(gateway_pack_enabled: bool, port: u16) -> Vec<Vec<String>> {
    irin_serve_routes(gateway_pack_enabled)
        .iter()
        .map(|route| serve_apply_route_args_for_port(route, port))
        .collect()
}

/// Pure argv for removing one IRIN-owned Serve route via port-scoped `off`.
///
/// Product code must never use global `tailscale serve reset`.
#[cfg(test)]
pub fn serve_disable_route_args(route: &ServeRoute) -> Vec<String> {
    serve_disable_route_args_for_port(route, HTTPS_PORT)
}

/// Disable argv for an explicit HTTPS port.
pub fn serve_disable_route_args_for_port(route: &ServeRoute, port: u16) -> Vec<String> {
    let mut args = vec![
        "serve".into(),
        "--bg".into(),
        "--yes".into(),
        format!("--https={port}"),
    ];
    if route.path != "/" {
        args.push(format!("--set-path={}", route.path));
    }
    args.push("off".into());
    args
}

/// All disable argvs for the IRIN route table (non-root paths first, root last).
#[cfg(test)]
pub fn serve_disable_all_args(gateway_pack_enabled: bool) -> Vec<Vec<String>> {
    serve_disable_all_args_for_port(gateway_pack_enabled, HTTPS_PORT)
}

/// Disable argvs for an explicit HTTPS port (non-root first, root last).
pub fn serve_disable_all_args_for_port(gateway_pack_enabled: bool, port: u16) -> Vec<Vec<String>> {
    let mut routes = irin_serve_routes(gateway_pack_enabled);
    if gateway_pack_enabled {
        routes.push(ServeRoute {
            path: "/watch",
            target: LEGACY_GATEWAY_WATCH_TARGET,
        });
    }
    // Turn off nested paths before the root handler.
    routes.sort_by(|a, b| {
        let a_root = a.path == "/";
        let b_root = b.path == "/";
        a_root.cmp(&b_root).then_with(|| a.path.cmp(b.path))
    });
    routes
        .iter()
        .map(|route| serve_disable_route_args_for_port(route, port))
        .collect()
}

/// True when any argv token mentions funnel (case-insensitive). Fail-closed guard.
pub fn argv_contains_funnel(args: &[String]) -> bool {
    args.iter()
        .any(|a| a.to_ascii_lowercase().contains("funnel"))
}

/// True when argv performs a global serve reset (forbidden in product paths).
pub fn argv_contains_serve_reset(args: &[String]) -> bool {
    args.windows(2).any(|w| w[0] == "serve" && w[1] == "reset")
        || (args.len() == 2 && args[0] == "serve" && args[1] == "reset")
}

/// Install-guidance error when no allow-listed CLI is present.
fn tailscale_cli_not_found_error() -> String {
    "Tailscale CLI not found. Install Tailscale, then retry. Expected \
     /Applications/Tailscale.app/Contents/MacOS/Tailscale, \
     /opt/homebrew/bin/tailscale, or /usr/local/bin/tailscale."
        .to_string()
}

/// True when `path` is a regular file with at least one execute bit (Unix).
///
/// Non-Unix builds fall back to `is_file()` — this crate targets macOS product.
fn path_is_executable_cli(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match path.metadata() {
            Ok(meta) if meta.is_file() => meta.permissions().mode() & 0o111 != 0,
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Probe whether a CLI answers `status --json` with parseable node status.
///
/// Used during selection so a present-but-broken App wrapper (non-JSON
/// CLIError) does not win over a working Homebrew CLI (ProjectMem #0040).
fn cli_answers_status_json(bin: &Path) -> bool {
    let mut cmd = Command::new(bin);
    cmd.args(status_json_args());
    // Bound tightly: selection must not hang Settings open on a stuck wrapper.
    match run_command_timeout(cmd, Duration::from_secs(2)) {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            parse_status_json(raw.trim()).is_ok()
        }
        _ => false,
    }
}

/// Outcome of allow-list CLI selection (path + whether `status --json` passed).
///
/// `json_verified == false` means a present-but-unproved fallback. Callers that
/// process-cache must not store those (ProjectMem #0049).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleCliSelection {
    pub path: PathBuf,
    pub json_verified: bool,
}

/// Select a Tailscale CLI from an ordered candidate list.
///
/// **Policy (ProjectMem #0040 / Settings false-red):**
/// 1. Skip candidates that fail `is_present` (missing or non-executable).
/// 2. Prefer the first present candidate for which `answers_status_json` is
///    true (can answer `status --json` with parseable node status).
/// 3. If every present candidate fails the JSON probe, fall back to the first
///    present path so downstream errors stay path-bound (not "CLI not found").
///    That fallback is marked `json_verified: false`.
/// 4. If none are present, return install-guidance error text.
///
/// Production wires default allow-list order, executable presence, and a live
/// status-json probe via [`resolve_tailscale_cli`]. Unit tests inject paths and
/// predicates without touching the host layout.
pub fn resolve_tailscale_cli_from<'a, I, P, J>(
    candidates: I,
    is_present: P,
    answers_status_json: J,
) -> Result<TailscaleCliSelection, String>
where
    I: IntoIterator<Item = &'a Path>,
    P: Fn(&Path) -> bool,
    J: Fn(&Path) -> bool,
{
    let mut first_present: Option<PathBuf> = None;
    for candidate in candidates {
        if !is_present(candidate) {
            continue;
        }
        if first_present.is_none() {
            first_present = Some(candidate.to_path_buf());
        }
        if answers_status_json(candidate) {
            return Ok(TailscaleCliSelection {
                path: candidate.to_path_buf(),
                json_verified: true,
            });
        }
    }
    if let Some(path) = first_present {
        return Ok(TailscaleCliSelection {
            path,
            json_verified: false,
        });
    }
    Err(tailscale_cli_not_found_error())
}

/// Cache a selection only when it passed the status-json probe.
///
/// Unverified fallbacks are returned as-is without writing the cache so a
/// later poll can recover after a transient probe failure (ProjectMem #0049).
/// Verified paths use [`OnceLock::get_or_init`] so concurrent first-fill races
/// all observe the same winning path (not "set failed → return my local pick").
pub(crate) fn store_json_verified_cli(
    cache: &OnceLock<PathBuf>,
    selection: TailscaleCliSelection,
) -> PathBuf {
    if let Some(cached) = cache.get() {
        return cached.clone();
    }
    if selection.json_verified {
        return cache.get_or_init(|| selection.path.clone()).clone();
    }
    selection.path
}

/// Resolve an allow-listed Tailscale CLI binary for product use.
///
/// Candidate order matches [`TAILSCALE_CLI_ALLOWLIST`] (App bundle first, then
/// Homebrew, then `/usr/local`). Selection prefers the first executable entry
/// that answers `status --json` as parseable JSON so a broken App CLI wrapper
/// does not force Settings false-red while Homebrew works.
///
/// Only JSON-verified selections are process-cached (`OnceLock`). Present-but
/// unverified fallbacks and total misses are not cached, so a later poll can
/// recover after a transient probe failure or mid-session install (ProjectMem
/// #0049). A verified path sticks for the process lifetime.
pub fn resolve_tailscale_cli() -> Result<PathBuf, String> {
    static RESOLVED_OK: OnceLock<PathBuf> = OnceLock::new();
    if let Some(path) = RESOLVED_OK.get() {
        return Ok(path.clone());
    }
    let selection = resolve_tailscale_cli_from(
        TAILSCALE_CLI_ALLOWLIST.iter().map(|s| Path::new(*s)),
        path_is_executable_cli,
        cli_answers_status_json,
    )?;
    Ok(store_json_verified_cli(&RESOLVED_OK, selection))
}

/// Reject any path that is not on the allow-list.
pub fn validate_tailscale_cli_path(path: &Path) -> Result<(), String> {
    let s = path.to_string_lossy();
    if TAILSCALE_CLI_ALLOWLIST.iter().any(|a| *a == s) {
        Ok(())
    } else {
        Err(format!("tailscale CLI path not allow-listed: {s}"))
    }
}

/// Parsed `tailscale status --json` preflight fields (non-secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleNodeStatus {
    pub backend_state: String,
    pub dns_name: Option<String>,
    pub running: bool,
    pub logged_in: bool,
}

/// Truthful, non-secret classification when CLI stdout is not JSON.
///
/// Includes CLI path, exit code, byte count, and first-byte hex — never
/// content (node data is sensitive) and never the raw serde message operators
/// saw on the phone panel (`expected value at line 1 column 1`).
pub fn classify_non_json_stdout(
    kind: &str,
    raw: &str,
    cli_path: Option<&str>,
    exit_code: Option<i32>,
) -> String {
    let bytes = raw.len();
    let first_byte = raw
        .as_bytes()
        .first()
        .map(|b| format!("0x{b:02x}"))
        .unwrap_or_else(|| "none".to_string());
    let path = cli_path.unwrap_or("unknown");
    let exit = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    format!(
        "{kind}: non-JSON output (cli={path}, exit={exit}, bytes={bytes}, first_byte={first_byte})"
    )
}

/// Parse Tailscale node status JSON. Never returns tokens or keys.
pub fn parse_status_json(raw: &str) -> Result<TailscaleNodeStatus, String> {
    let v: Value = serde_json::from_str(raw)
        .map_err(|_| classify_non_json_stdout("status json", raw, None, None))?;
    let backend_state = v
        .get("BackendState")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let dns_name = v
        .pointer("/Self/DNSName")
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty());
    let running = backend_state.eq_ignore_ascii_case("Running");
    // NeedsLogin / NoState / Stopped are not logged in for product purposes.
    let logged_in = running
        || backend_state.eq_ignore_ascii_case("Starting")
        || (!backend_state.is_empty()
            && !backend_state.eq_ignore_ascii_case("NeedsLogin")
            && !backend_state.eq_ignore_ascii_case("NoState")
            && !backend_state.eq_ignore_ascii_case("Stopped"));
    // Stricter: NeedsLogin is explicitly not logged in.
    let logged_in = if backend_state.eq_ignore_ascii_case("NeedsLogin")
        || backend_state.eq_ignore_ascii_case("NoState")
    {
        false
    } else {
        logged_in
    };
    Ok(TailscaleNodeStatus {
        backend_state,
        dns_name,
        running,
        logged_in,
    })
}

/// One observed Serve handler (path → proxy target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedHandler {
    pub path: String,
    pub proxy: String,
}

/// Parsed `tailscale serve status --json` scoped to the IRIN HTTPS port.
///
/// Handlers on other ports (for example Hermes on 443) are intentionally
/// ignored so multi-port coexistence is normal rather than `foreign_unowned`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeStatusView {
    /// Handlers under Web keys for the IRIN HTTPS port only.
    pub handlers: Vec<ObservedHandler>,
    pub funnel_present: bool,
    /// True when a non-HTTPS TCP surface is present for the IRIN port.
    /// `TCP[port].HTTPS=true` is normal HTTPS Serve metadata and is not
    /// treated as unsupported.
    pub unsupported_surfaces_present: bool,
    /// True when the IRIN port has no handlers, no funnel, and no unsupported
    /// surface — other ports may still be published.
    pub empty: bool,
    /// Port this view was scoped to.
    pub https_port: u16,
}

/// Parse Serve status JSON for the product default HTTPS port.
#[cfg(test)]
pub fn parse_serve_status_json(raw: &str) -> Result<ServeStatusView, String> {
    parse_serve_status_json_for_port(raw, HTTPS_PORT)
}

/// Parse Serve status JSON scoped to one HTTPS port.
pub fn parse_serve_status_json_for_port(raw: &str, port: u16) -> Result<ServeStatusView, String> {
    let v: Value = serde_json::from_str(raw)
        .map_err(|_| classify_non_json_stdout("serve status json", raw, None, None))?;
    let funnel_present = detect_funnel_for_port(&v, port);
    let unsupported_surfaces_present = tcp_has_unsupported_for_port(&v, port);
    let mut handlers = Vec::new();
    if let Some(web) = v.get("Web").and_then(Value::as_object) {
        for (host_key, cfg) in web {
            if !web_key_matches_port(host_key, port) {
                continue;
            }
            if let Some(map) = cfg.get("Handlers").and_then(Value::as_object) {
                for (path, handler) in map {
                    let proxy = handler
                        .get("Proxy")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    handlers.push(ObservedHandler {
                        path: path.clone(),
                        proxy,
                    });
                }
            }
        }
    }
    handlers.sort_by(|a, b| a.path.cmp(&b.path));
    let empty = handlers.is_empty() && !funnel_present && !unsupported_surfaces_present;
    Ok(ServeStatusView {
        handlers,
        funnel_present,
        unsupported_surfaces_present,
        empty,
        https_port: port,
    })
}

/// True when a Serve Web key (e.g. `host:8443`) refers to `port`.
pub fn web_key_matches_port(host_key: &str, port: u16) -> bool {
    let key = host_key.trim();
    if let Some((host, port_str)) = key.rsplit_once(':') {
        if host.is_empty() {
            return false;
        }
        if let Ok(parsed) = port_str.parse::<u16>() {
            return parsed == port;
        }
    }
    // Bare host keys are treated as the HTTPS default (443) only.
    port == 443 && !key.is_empty() && !key.contains(':')
}

fn tcp_has_unsupported_for_port(v: &Value, port: u16) -> bool {
    let Some(tcp) = v.get("TCP").and_then(Value::as_object) else {
        return false;
    };
    let port_key = port.to_string();
    let Some(entry) = tcp.get(&port_key) else {
        // Other ports' TCP metadata is not IRIN's surface.
        return false;
    };
    // Classic HTTPS Serve records `{"HTTPS": true}` under TCP[port].
    match entry {
        Value::Object(map) => {
            if map.is_empty() {
                return false;
            }
            // Only HTTPS=true (and optionally null/empty siblings) is normal.
            map.iter().any(|(k, child)| {
                if k.eq_ignore_ascii_case("HTTPS") {
                    // true is normal; any other value is unexpected.
                    !matches!(child, Value::Bool(true))
                } else {
                    // Any non-HTTPS TCP field is unsupported for this product.
                    !value_is_empty(child)
                }
            })
        }
        Value::Bool(true) => false,
        Value::Null => false,
        other => !value_is_empty(other),
    }
}

fn value_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        Value::Number(_) => false,
    }
}

fn detect_funnel_for_port(v: &Value, port: u16) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(key, child)| {
            if key.to_ascii_lowercase().contains("funnel")
                && funnel_field_enabled_for_port(child, port)
            {
                return true;
            }
            detect_funnel_for_port(child, port)
        }),
        Value::Array(items) => items.iter().any(|item| detect_funnel_for_port(item, port)),
        _ => false,
    }
}

fn funnel_field_enabled_for_port(value: &Value, port: u16) -> bool {
    match value {
        // A boolean funnel field is global, so true applies to every selected
        // port and must fail closed.
        Value::Bool(enabled) => *enabled,
        Value::Object(scopes) => scopes.iter().any(|(scope, enabled)| {
            let scope_matches =
                web_key_matches_port(scope, port) || scope.parse::<u16>().ok() == Some(port);
            scope_matches && !value_is_empty(enabled)
        }),
        Value::Array(scopes) => scopes.iter().any(|scope| match scope {
            Value::String(scope) => {
                web_key_matches_port(scope, port) || scope.parse::<u16>().ok() == Some(port)
            }
            Value::Object(_) => funnel_field_enabled_for_port(scope, port),
            _ => false,
        }),
        Value::String(scope) => {
            web_key_matches_port(scope, port) || scope.parse::<u16>().ok() == Some(port)
        }
        Value::Null | Value::Number(_) => false,
    }
}

/// Build the copyable tailnet HTTPS URL when DNS name is known.
/// Port 443 omits the port suffix. Never includes a bearer token.
pub fn tailnet_https_url(dns_name: &str, https_port: u16) -> String {
    let host = dns_name.trim().trim_end_matches('.');
    if https_port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{https_port}")
    }
}

fn is_legacy_gateway_watch(handler: &ObservedHandler) -> bool {
    handler.path == "/watch"
        && normalize_proxy(&handler.proxy) == normalize_proxy(LEGACY_GATEWAY_WATCH_TARGET)
}

fn gateway_routes_expected(expected: &[ServeRoute]) -> bool {
    expected.iter().any(|route| {
        route.path == "/health"
            && normalize_proxy(route.target) == normalize_proxy(GATEWAY_HEALTH_TARGET)
    })
}

/// True when observed handlers match the expected IRIN route table.
///
/// Gateway-owned tables also tolerate the exact legacy `/watch` handler so an
/// older publication can be disabled or re-applied instead of becoming stuck.
pub fn handlers_match_routes(observed: &[ObservedHandler], expected: &[ServeRoute]) -> bool {
    let allow_legacy_watch = gateway_routes_expected(expected);
    let mut obs: Vec<_> = observed
        .iter()
        .filter(|handler| !(allow_legacy_watch && is_legacy_gateway_watch(handler)))
        .map(|handler| (handler.path.as_str(), normalize_proxy(&handler.proxy)))
        .collect();
    if obs.len() != expected.len() {
        return false;
    }
    obs.sort_by(|a, b| a.0.cmp(b.0));
    let mut exp: Vec<_> = expected
        .iter()
        .map(|route| (route.path, normalize_proxy(route.target)))
        .collect();
    exp.sort_by(|a, b| a.0.cmp(b.0));
    obs == exp
}

/// True when every observed handler belongs to the expected IRIN route table.
///
/// Used only to recover an interrupted partial apply. An empty or partial set
/// is safe to disable when durable ownership proves the expected route shape;
/// any extra path or changed target fails closed. The exact legacy `/watch`
/// handler is accepted only for a Gateway-owned table so teardown can remove it.
pub fn handlers_are_route_subset(observed: &[ObservedHandler], expected: &[ServeRoute]) -> bool {
    let allow_legacy_watch = gateway_routes_expected(expected);
    observed.iter().all(|handler| {
        (allow_legacy_watch && is_legacy_gateway_watch(handler))
            || expected.iter().any(|route| {
                handler.path == route.path
                    && normalize_proxy(&handler.proxy) == normalize_proxy(route.target)
            })
    })
}

fn normalize_proxy(p: &str) -> String {
    p.trim().trim_end_matches('/').to_string()
}

/// True when observed handlers on the IRIN port are a non-empty set that is not
/// IRIN-owned. Handlers on other ports are never considered here.
pub fn handlers_look_foreign(
    observed: &[ObservedHandler],
    expected: &[ServeRoute],
    ownership_enabled: bool,
) -> bool {
    if observed.is_empty() {
        return false;
    }
    if ownership_enabled && handlers_match_routes(observed, expected) {
        return false;
    }
    // Enabled but mismatched, or any handlers while we do not own publication.
    !ownership_enabled || !handlers_match_routes(observed, expected)
}

/// Run an allow-listed Tailscale command with a hard wall-clock bound.
/// Not used by unit tests.
pub fn run_tailscale(args: &[String], timeout: Duration) -> Result<Output, String> {
    let bin = resolve_tailscale_cli()?;
    run_tailscale_at(&bin, args, timeout)
}

/// Execute at a pre-resolved allow-listed path (one resolve per product call).
fn run_tailscale_at(bin: &Path, args: &[String], timeout: Duration) -> Result<Output, String> {
    validate_tailscale_cli_path(bin)?;
    if argv_contains_funnel(args) {
        return Err("refusing Tailscale argv that mentions funnel".to_string());
    }
    if argv_contains_serve_reset(args) {
        return Err("refusing global tailscale serve reset".to_string());
    }
    let mut cmd = Command::new(bin);
    cmd.args(args);
    run_command_timeout(cmd, timeout)
}

fn run_command_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn tailscale: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stdout {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err("tailscale command timed out".to_string());
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(format!("tailscale wait error: {e}"));
            }
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Convenience: run and return stdout (UTF-8 lossy) on success.
///
/// Empty stdout is a truthful host-boundary error (never handed to serde as
/// raw JSON). Logs selected CLI path, exit status, and stdout byte count.
/// Resolves the CLI once so logging and execution share the same path (and
/// uncached fallback does not double-probe within one call).
pub fn run_tailscale_stdout(args: &[String], timeout: Duration) -> Result<String, String> {
    let bin = resolve_tailscale_cli()?;
    let out = run_tailscale_at(&bin, args, timeout)?;
    let exit = out.status.code().unwrap_or(-1);
    let stdout_len = out.stdout.len();
    eprintln!(
        "[tailscale-cli] path={} exit={} stdout_bytes={} args={:?}",
        bin.display(),
        exit,
        stdout_len,
        args
    );
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let brief = stderr.lines().next().unwrap_or("command failed");
        return Err(format!("tailscale failed (exit {exit}): {brief}"));
    }
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let brief = stderr.lines().next().unwrap_or("(no stderr)");
        return Err(format!(
            "tailscale returned empty output (exit {exit}): {brief}"
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn route_table_council_only_without_gateway() {
        let routes = irin_serve_routes(false);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/");
        assert_eq!(routes[0].target, COUNCIL_LOOPBACK);
    }

    #[test]
    fn route_table_includes_gateway_paths_when_enabled() {
        let routes = irin_serve_routes(true);
        assert_eq!(routes.len(), 2);
        assert!(!routes.iter().any(|r| r.path == "/watch"));
        assert!(routes.iter().any(|r| r.path == "/health"));
        assert!(routes
            .iter()
            .all(|r| r.target.starts_with("http://127.0.0.1:")));
    }

    #[test]
    fn apply_argv_root_uses_dedicated_https_port_and_background() {
        let args = serve_apply_route_args(&ServeRoute {
            path: "/",
            target: COUNCIL_LOOPBACK,
        });
        assert_eq!(
            args,
            vec![
                "serve",
                "--bg",
                "--yes",
                "--https=8443",
                "http://127.0.0.1:8765"
            ]
        );
        assert!(!argv_contains_funnel(&args));
        assert!(!argv_contains_serve_reset(&args));
    }

    #[test]
    fn apply_argv_never_publishes_legacy_watch() {
        let all = serve_apply_all_args(true);
        assert_eq!(all.len(), 2);
        assert!(!all.iter().flatten().any(|arg| arg == "--set-path=/watch"));
        assert!(!all
            .iter()
            .flatten()
            .any(|arg| arg == LEGACY_GATEWAY_WATCH_TARGET));
    }

    #[test]
    fn disable_argv_uses_port_scoped_off_never_reset() {
        let root = serve_disable_route_args(&ServeRoute {
            path: "/",
            target: COUNCIL_LOOPBACK,
        });
        assert_eq!(root, vec!["serve", "--bg", "--yes", "--https=8443", "off"]);
        let legacy_watch = serve_disable_route_args(&ServeRoute {
            path: "/watch",
            target: LEGACY_GATEWAY_WATCH_TARGET,
        });
        assert_eq!(
            legacy_watch,
            vec![
                "serve",
                "--bg",
                "--yes",
                "--https=8443",
                "--set-path=/watch",
                "off"
            ]
        );
        assert!(serve_disable_all_args(true).contains(&legacy_watch));
        for args in serve_disable_all_args(true) {
            assert!(args.iter().any(|a| a == "off"));
            assert!(!argv_contains_serve_reset(&args));
            assert!(!argv_contains_funnel(&args));
        }
        // Nested paths are disabled before root.
        let all = serve_disable_all_args(true);
        assert!(all.len() >= 2);
        assert!(all.last().unwrap().ends_with(&["off".to_string()]));
        assert!(!all
            .last()
            .unwrap()
            .iter()
            .any(|a| a.starts_with("--set-path=")));
    }

    #[test]
    fn no_composed_apply_argv_contains_funnel_or_reset() {
        for enabled in [false, true] {
            for args in serve_apply_all_args(enabled) {
                assert!(!argv_contains_funnel(&args), "funnel leaked into {args:?}");
                assert!(
                    !argv_contains_serve_reset(&args),
                    "reset leaked into {args:?}"
                );
                assert!(args.iter().any(|a| a == "--https=8443"));
                assert!(args.iter().any(|a| a == "--bg"));
                assert!(args.iter().any(|a| a == "--yes"));
            }
            for args in serve_disable_all_args(enabled) {
                assert!(!argv_contains_funnel(&args));
                assert!(!argv_contains_serve_reset(&args));
            }
        }
        assert!(!argv_contains_funnel(&status_json_args()));
        assert!(!argv_contains_funnel(&serve_status_json_args()));
    }

    #[test]
    fn parse_status_running_with_dns() {
        let raw = r#"{"BackendState":"Running","Self":{"DNSName":"phone.example.ts.net."}}"#;
        let st = parse_status_json(raw).unwrap();
        assert!(st.running);
        assert!(st.logged_in);
        assert_eq!(st.dns_name.as_deref(), Some("phone.example.ts.net"));
    }

    #[test]
    fn parse_status_needs_login() {
        let raw = r#"{"BackendState":"NeedsLogin","Self":{}}"#;
        let st = parse_status_json(raw).unwrap();
        assert!(!st.running);
        assert!(!st.logged_in);
    }

    #[test]
    fn non_json_status_never_leaks_serde_or_content() {
        // Dave hit: non-empty non-JSON (first byte not '{'/JSON) used to surface
        // raw serde "expected value at line 1 column 1" on the phone panel.
        let raw = "tailscale: unexpected host message";
        let err = parse_status_json(raw).unwrap_err();
        assert!(err.starts_with("status json: non-JSON output"));
        assert!(err.contains("cli=unknown"));
        assert!(err.contains("exit=n/a"));
        assert!(err.contains("bytes=34"));
        assert!(err.contains("first_byte=0x74")); // 't'
        assert!(!err.contains("expected value"));
        assert!(!err.contains("line 1"));
        assert!(!err.contains("unexpected host"));
        assert!(!err.contains(raw));
    }

    #[test]
    fn non_json_serve_status_same_classification() {
        let raw = "NotLoggedIn";
        let err = parse_serve_status_json(raw).unwrap_err();
        assert!(err.starts_with("serve status json: non-JSON output"));
        assert!(err.contains("bytes=11"));
        assert!(err.contains("first_byte=0x4e")); // 'N'
        assert!(!err.contains("expected value"));
        assert!(!err.contains("NotLoggedIn"));
    }

    #[test]
    fn classify_non_json_empty_and_brace_prefix() {
        let empty = classify_non_json_stdout(
            "status json",
            "",
            Some("/opt/homebrew/bin/tailscale"),
            Some(0),
        );
        assert!(empty.contains("cli=/opt/homebrew/bin/tailscale"));
        assert!(empty.contains("exit=0"));
        assert!(empty.contains("bytes=0"));
        assert!(empty.contains("first_byte=none"));
        // JSON-looking but invalid still uses the non-serde path.
        let broken = parse_status_json("{not-json").unwrap_err();
        assert!(broken.contains("first_byte=0x7b"));
        assert!(!broken.contains("expected"));
    }

    #[test]
    fn multi_port_serve_accepts_hermes_443_and_scopes_irin_8443() {
        let raw = r#"{
          "TCP": {
            "443": {"HTTPS": true},
            "8443": {"HTTPS": true}
          },
          "Web": {
            "phone.example.ts.net:443": {
              "Handlers": {
                "/": {"Proxy": "http://127.0.0.1:8787"}
              }
            },
            "phone.example.ts.net:8443": {
              "Handlers": {
                "/": {"Proxy": "http://127.0.0.1:8765"}
              }
            }
          },
          "AllowFunnel": {}
        }"#;
        let view = parse_serve_status_json(raw).unwrap();
        assert_eq!(view.https_port, 8443);
        assert!(!view.funnel_present);
        assert!(!view.unsupported_surfaces_present);
        assert_eq!(view.handlers.len(), 1);
        assert_eq!(view.handlers[0].path, "/");
        assert_eq!(view.handlers[0].proxy, "http://127.0.0.1:8765");
        let expected = irin_serve_routes(false);
        assert!(handlers_match_routes(&view.handlers, &expected));
        assert!(!handlers_look_foreign(&view.handlers, &expected, true));
    }

    #[test]
    fn foreign_route_on_irin_port_is_detected() {
        let raw = r#"{
          "Web": {
            "phone.example.ts.net:443": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8787"}}
            },
            "phone.example.ts.net:8443": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:9999"}}
            }
          }
        }"#;
        let view = parse_serve_status_json(raw).unwrap();
        assert_eq!(view.handlers.len(), 1);
        assert!(handlers_look_foreign(
            &view.handlers,
            &irin_serve_routes(false),
            false
        ));
        assert!(handlers_look_foreign(
            &view.handlers,
            &irin_serve_routes(false),
            true
        ));
    }

    #[test]
    fn hermes_only_on_443_leaves_irin_port_empty() {
        let raw = r#"{
          "TCP": {"443": {"HTTPS": true}},
          "Web": {
            "phone.example.ts.net:443": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8787"}}
            }
          }
        }"#;
        let view = parse_serve_status_json(raw).unwrap();
        assert!(view.handlers.is_empty());
        assert!(view.empty);
        assert!(!view.unsupported_surfaces_present);
        assert!(!handlers_look_foreign(
            &view.handlers,
            &irin_serve_routes(false),
            false
        ));
    }

    #[test]
    fn parse_serve_status_handlers_and_funnel_on_irin_port() {
        let raw = r#"{
          "Web": {
            "phone.example.ts.net:8443": {
              "Handlers": {
                "/": {"Proxy": "http://127.0.0.1:8765"},
                "/watch": {"Proxy": "http://127.0.0.1:18080/watch"}
              }
            }
          },
          "AllowFunnel": {"phone.example.ts.net:8443": true}
        }"#;
        let view = parse_serve_status_json(raw).unwrap();
        assert!(view.funnel_present);
        assert_eq!(view.handlers.len(), 2);
        assert!(!view.unsupported_surfaces_present);
        assert!(!view.empty);
    }

    #[test]
    fn funnel_on_hermes_443_does_not_taint_irin_8443() {
        let raw = r#"{
          "Web": {
            "phone.example.ts.net:443": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}
            }
          },
          "AllowFunnel": {"phone.example.ts.net:443": true}
        }"#;
        let view = parse_serve_status_json_for_port(raw, 8443).unwrap();
        assert!(!view.funnel_present);
        assert!(view.empty);

        let hermes = parse_serve_status_json_for_port(raw, 443).unwrap();
        assert!(hermes.funnel_present);
    }

    #[test]
    fn parse_serve_status_empty_object() {
        let view = parse_serve_status_json("{}").unwrap();
        assert!(view.empty);
        assert!(!view.funnel_present);
        assert!(!view.unsupported_surfaces_present);
    }

    #[test]
    fn https_tcp_metadata_is_not_unsupported() {
        let view =
            parse_serve_status_json(r#"{"TCP":{"8443":{"HTTPS":true}},"Web":{},"AllowFunnel":{}}"#)
                .unwrap();
        assert!(!view.unsupported_surfaces_present);
        assert!(view.empty);
    }

    #[test]
    fn non_https_tcp_on_irin_port_is_unsupported() {
        let view =
            parse_serve_status_json(r#"{"TCP":{"8443":{"TCPForward":"127.0.0.1:22"}},"Web":{}}"#)
                .unwrap();
        assert!(view.unsupported_surfaces_present);
        assert!(!view.empty);
    }

    #[test]
    fn handlers_match_and_foreign_detection() {
        let expected = irin_serve_routes(false);
        let obs = vec![ObservedHandler {
            path: "/".into(),
            proxy: "http://127.0.0.1:8765".into(),
        }];
        assert!(handlers_match_routes(&obs, &expected));
        assert!(!handlers_look_foreign(&obs, &expected, true));
        assert!(handlers_look_foreign(&obs, &expected, false));

        let foreign = vec![ObservedHandler {
            path: "/".into(),
            proxy: "http://127.0.0.1:3010".into(),
        }];
        assert!(!handlers_match_routes(&foreign, &expected));
        assert!(handlers_look_foreign(&foreign, &expected, true));
    }

    #[test]
    fn interrupted_subset_accepts_owned_routes_only() {
        let expected = irin_serve_routes(true);
        let partial = vec![ObservedHandler {
            path: "/".into(),
            proxy: COUNCIL_LOOPBACK.into(),
        }];
        assert!(handlers_are_route_subset(&partial, &expected));
        let legacy_partial = vec![
            partial[0].clone(),
            ObservedHandler {
                path: "/watch".into(),
                proxy: LEGACY_GATEWAY_WATCH_TARGET.into(),
            },
        ];
        assert!(handlers_are_route_subset(&legacy_partial, &expected));
        assert!(handlers_are_route_subset(&[], &expected));
        assert!(!handlers_are_route_subset(
            &[ObservedHandler {
                path: "/foreign".into(),
                proxy: "http://127.0.0.1:9999".into(),
            }],
            &expected,
        ));
    }

    #[test]
    fn tailnet_url_includes_dedicated_https_port() {
        assert_eq!(
            tailnet_https_url("phone.example.ts.net.", 443),
            "https://phone.example.ts.net"
        );
        assert_eq!(
            tailnet_https_url("phone.example.ts.net", 8443),
            "https://phone.example.ts.net:8443"
        );
    }

    #[test]
    fn web_key_port_matching() {
        assert!(web_key_matches_port("phone.example.ts.net:8443", 8443));
        assert!(!web_key_matches_port("phone.example.ts.net:443", 8443));
        assert!(web_key_matches_port("phone.example.ts.net:443", 443));
        assert!(web_key_matches_port("phone.example.ts.net", 443));
        assert!(!web_key_matches_port("phone.example.ts.net", 8443));
    }

    #[test]
    fn validate_allowlist_rejects_arbitrary_path() {
        assert!(validate_tailscale_cli_path(Path::new("/bin/sh")).is_err());
        // Allow-list entry itself is accepted even if missing on disk for pure check.
        let listed = PathBuf::from(TAILSCALE_CLI_ALLOWLIST[1]);
        assert!(validate_tailscale_cli_path(&listed).is_ok());
    }

    #[test]
    fn default_allowlist_order_is_app_then_homebrew_then_usr_local() {
        assert_eq!(
            TAILSCALE_CLI_ALLOWLIST,
            &[
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                "/opt/homebrew/bin/tailscale",
                "/usr/local/bin/tailscale",
            ]
        );
    }

    #[test]
    fn resolve_first_present_json_capable_wins() {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        let brew = Path::new("/opt/homebrew/bin/tailscale");
        let candidates = [app, brew];
        let present = |p: &Path| p == app || p == brew;
        let answers = |p: &Path| p == app; // App answers JSON → wins despite later brew.
        let got = resolve_tailscale_cli_from(candidates, present, answers).unwrap();
        assert_eq!(got.path, app);
        assert!(got.json_verified);
    }

    #[test]
    fn resolve_skips_missing_and_non_executable() {
        let missing = Path::new("/tmp/irin-missing-tailscale");
        let non_exec = Path::new("/tmp/irin-non-exec-tailscale");
        let good = Path::new("/opt/homebrew/bin/tailscale");
        let candidates = [missing, non_exec, good];
        // Only `good` is present; non_exec is on the list but fails is_present.
        let present = |p: &Path| p == good;
        let answers = |_: &Path| true;
        let got = resolve_tailscale_cli_from(candidates, present, answers).unwrap();
        assert_eq!(got.path, good);
        assert!(got.json_verified);
    }

    /// #0040 regression: App CLI present but non-JSON (CLIError) while Homebrew
    /// answers status JSON → select Homebrew, not the App wrapper.
    #[test]
    fn resolve_prefers_homebrew_when_app_cli_fails_json_probe() {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        let brew = Path::new("/opt/homebrew/bin/tailscale");
        let candidates = [app, brew];
        let present = |p: &Path| p == app || p == brew;
        // Fingerprint of the live false-red: App wrapper returns English
        // CLIError (first_byte=0x54), not parseable status JSON.
        let answers = |p: &Path| p == brew;
        let got = resolve_tailscale_cli_from(candidates, present, answers).unwrap();
        assert_eq!(got.path, brew);
        assert!(got.json_verified);
    }

    #[test]
    fn resolve_falls_back_to_first_present_when_all_fail_json_probe() {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        let brew = Path::new("/opt/homebrew/bin/tailscale");
        let candidates = [app, brew];
        let present = |p: &Path| p == app || p == brew;
        let answers = |_: &Path| false;
        let got = resolve_tailscale_cli_from(candidates, present, answers).unwrap();
        assert_eq!(got.path, app);
        assert!(!got.json_verified);
    }

    /// #0049 regression: unverified fallback must not pin the cache; a later
    /// poll that sees JSON-capable brew must recover without process restart.
    #[test]
    fn unverified_fallback_not_cached_so_later_json_ok_recovers() {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        let brew = Path::new("/opt/homebrew/bin/tailscale");
        let candidates = [app, brew];
        let present = |p: &Path| p == app || p == brew;
        let cache = OnceLock::new();

        // Transient: every probe fails → first-present App, uncached.
        let fail = resolve_tailscale_cli_from(candidates, present, |_| false).unwrap();
        assert_eq!(fail.path, app);
        assert!(!fail.json_verified);
        let out1 = store_json_verified_cli(&cache, fail);
        assert_eq!(out1, app);
        assert!(
            cache.get().is_none(),
            "unverified fallback must not write OnceLock"
        );

        // Recover: brew answers JSON → verified, cache brew.
        let ok = resolve_tailscale_cli_from(candidates, present, |p| p == brew).unwrap();
        assert_eq!(ok.path, brew);
        assert!(ok.json_verified);
        let out2 = store_json_verified_cli(&cache, ok);
        assert_eq!(out2, brew);
        assert_eq!(cache.get().map(PathBuf::as_path), Some(brew));

        // Verified stickiness: later worse selection does not replace cache.
        let worse = resolve_tailscale_cli_from(candidates, present, |p| p == app).unwrap();
        assert!(worse.json_verified);
        assert_eq!(worse.path, app);
        let out3 = store_json_verified_cli(&cache, worse);
        assert_eq!(out3, brew);
        assert_eq!(cache.get().map(PathBuf::as_path), Some(brew));
    }

    /// Concurrent verified first-fill must not return the losing local pick.
    #[test]
    fn concurrent_verified_store_shares_once_lock_winner() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(OnceLock::new());
        let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        let brew = PathBuf::from("/opt/homebrew/bin/tailscale");
        let cache_a = Arc::clone(&cache);
        let cache_b = Arc::clone(&cache);
        let app_c = app.clone();
        let brew_c = brew.clone();
        let t1 = thread::spawn(move || {
            store_json_verified_cli(
                &cache_a,
                TailscaleCliSelection {
                    path: app_c,
                    json_verified: true,
                },
            )
        });
        let t2 = thread::spawn(move || {
            store_json_verified_cli(
                &cache_b,
                TailscaleCliSelection {
                    path: brew_c,
                    json_verified: true,
                },
            )
        });
        let r1 = t1.join().expect("thread1");
        let r2 = t2.join().expect("thread2");
        let winner = cache.get().expect("cache filled").clone();
        assert!(winner == app || winner == brew);
        assert_eq!(r1, winner);
        assert_eq!(r2, winner);
    }

    #[test]
    fn resolve_errors_when_no_candidate_present() {
        let app = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        let brew = Path::new("/opt/homebrew/bin/tailscale");
        let err = resolve_tailscale_cli_from([app, brew], |_| false, |_| true).unwrap_err();
        assert!(err.contains("Tailscale CLI not found"));
        assert!(err.contains("/opt/homebrew/bin/tailscale"));
    }

    #[test]
    fn status_json_probe_accepts_running_script_rejects_gui_error_script() {
        let dir = std::env::temp_dir().join(format!(
            "irin-ts-cli-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good-tailscale");
        std::fs::write(
            &good,
            "#!/bin/sh\necho '{\"BackendState\":\"Running\",\"Self\":{}}'\n",
        )
        .unwrap();
        let bad = dir.join("bad-tailscale");
        // Live false-red body shape: English CLIError, first byte 'T' (0x54).
        std::fs::write(
            &bad,
            "#!/bin/sh\necho 'The Tailscale GUI failed to start: (Tailscale.CLIError error 3.)'\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(
            cli_answers_status_json(&good),
            "running JSON script should pass status probe"
        );
        assert!(
            !cli_answers_status_json(&bad),
            "GUI CLIError script must fail status probe"
        );

        // End-to-end selection with real present/probe predicates on temp scripts.
        let non_exec = dir.join("non-exec-tailscale");
        std::fs::write(&non_exec, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let candidates = [non_exec.as_path(), bad.as_path(), good.as_path()];
        let got =
            resolve_tailscale_cli_from(candidates, path_is_executable_cli, cli_answers_status_json)
                .unwrap();
        assert_eq!(got.path, good);
        assert!(got.json_verified);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
