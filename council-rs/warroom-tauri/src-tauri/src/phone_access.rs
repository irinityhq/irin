//! App-owned private phone publication (Tailscale Serve only).
//!
//! Ownership, interrupted-operation state, and non-secret status live under the
//! existing Application Support root. Funnel is unconstructable and rejected
//! by status verification. Bearer tokens never appear in status or URLs.
//!
//! IRIN publishes on a dedicated HTTPS port (default 8443). Handlers on other
//! ports, including Hermes root on 443, coexist and are never classified as
//! foreign. Disable removes only IRIN-owned handlers on that port via
//! port-scoped `off` — never global `tailscale serve reset`.
//!
//! Concurrent enable/disable is serialized. Live Tailscale invocations are
//! behind a small runner so unit tests never execute the host binary.

use crate::private_config::app_support_dir;
use crate::tailscale_cli::{
    handlers_are_route_subset, handlers_look_foreign, handlers_match_routes, https_port,
    irin_serve_routes, parse_serve_status_json_for_port, parse_status_json,
    serve_apply_all_args_for_port, serve_disable_all_args_for_port, serve_status_json_args,
    status_json_args, tailnet_https_url, ServeStatusView, TailscaleNodeStatus, DEFAULT_HTTPS_PORT,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Global phone-access lifecycle lock — enable/disable must not interleave.
static PHONE_LOCK: Mutex<()> = Mutex::new(());

const PHONE_DIR_NAME: &str = "phone-access";
const OWNERSHIP_FILE: &str = "ownership.json";
const PRIOR_STATUS_FILE: &str = "prior-serve-status.json";
const OWNERSHIP_OWNER: &str = "irin-desktop-phone-access";
const OWNERSHIP_VERSION: u32 = 1;

pub const TAILSCALE_CMD_TIMEOUT: Duration = Duration::from_secs(20);

/// Distinct phone-access states for the operator surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneAccessState {
    Off,
    Starting,
    Ready,
    PublishedButBackendDown,
    TailscaleUnavailable,
    NotLoggedIn,
    ForeignUnowned,
    FunnelPresent,
    InterruptedChange,
    Stopping,
    CommandError,
}

impl PhoneAccessState {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::PublishedButBackendDown => "published_but_backend_down",
            Self::TailscaleUnavailable => "tailscale_unavailable",
            Self::NotLoggedIn => "not_logged_in",
            Self::ForeignUnowned => "foreign_unowned",
            Self::FunnelPresent => "funnel_present",
            Self::InterruptedChange => "interrupted_change",
            Self::Stopping => "stopping",
            Self::CommandError => "command_error",
        }
    }
}

/// Non-secret phone access status returned to the renderer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhoneAccessStatus {
    pub state: PhoneAccessState,
    pub message: String,
    /// Copyable tailnet HTTPS URL when deterministically available.
    pub tailnet_url: Option<String>,
    pub enabled: bool,
    pub ownership: String,
    pub interrupted: bool,
    pub gateway_routes: bool,
    pub funnel_present: bool,
}

impl PhoneAccessStatus {
    fn base(state: PhoneAccessState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            tailnet_url: None,
            enabled: false,
            ownership: "none".into(),
            interrupted: false,
            gateway_routes: false,
            funnel_present: false,
        }
    }
}

/// Durable non-secret ownership record under Application Support.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnershipRecord {
    pub version: u32,
    pub owner: String,
    pub enabled: bool,
    pub interrupted: bool,
    pub gateway_routes: bool,
    /// HTTPS port this ownership covers. Defaults to the product dedicated port.
    #[serde(default = "default_ownership_https_port")]
    pub https_port: u16,
    #[serde(default)]
    pub updated_unix: u64,
}

fn default_ownership_https_port() -> u16 {
    DEFAULT_HTTPS_PORT
}

impl Default for OwnershipRecord {
    fn default() -> Self {
        Self {
            version: OWNERSHIP_VERSION,
            owner: OWNERSHIP_OWNER.to_string(),
            enabled: false,
            interrupted: false,
            gateway_routes: false,
            https_port: DEFAULT_HTTPS_PORT,
            updated_unix: 0,
        }
    }
}

/// Inputs for pure phone status classification (no I/O).
#[derive(Debug, Clone)]
pub struct PhoneStatusInputs {
    pub ownership: OwnershipRecord,
    pub node: Option<TailscaleNodeStatus>,
    pub serve: Option<ServeStatusView>,
    pub node_error: Option<String>,
    pub serve_error: Option<String>,
    pub council_backend_ready: bool,
    pub gateway_pack_enabled: bool,
}

fn ownership_port(ownership: &OwnershipRecord) -> u16 {
    if ownership.https_port > 0 {
        ownership.https_port
    } else {
        DEFAULT_HTTPS_PORT
    }
}

/// Pure status classification used by live status and unit tests.
pub fn classify_phone_status(input: PhoneStatusInputs) -> PhoneAccessStatus {
    let mut status = PhoneAccessStatus::base(PhoneAccessState::Off, "");
    status.enabled = input.ownership.enabled;
    status.interrupted = input.ownership.interrupted;
    status.gateway_routes = input.ownership.gateway_routes;
    status.ownership = if input.ownership.enabled {
        OWNERSHIP_OWNER.to_string()
    } else {
        "none".to_string()
    };
    let port = ownership_port(&input.ownership);

    if input.ownership.interrupted {
        status.state = PhoneAccessState::InterruptedChange;
        status.message =
            "A previous phone-access change was interrupted. Disable or re-enable to recover."
                .into();
        return status;
    }

    if let Some(err) = input.node_error.as_ref() {
        if err.contains("not found") || err.contains("not allow-listed") {
            status.state = PhoneAccessState::TailscaleUnavailable;
            status.message = err.clone();
            return status;
        }
        status.state = PhoneAccessState::CommandError;
        status.message = err.clone();
        return status;
    }

    let Some(node) = input.node.as_ref() else {
        status.state = PhoneAccessState::TailscaleUnavailable;
        status.message = "Tailscale status unavailable".into();
        return status;
    };

    if !node.logged_in {
        status.state = PhoneAccessState::NotLoggedIn;
        status.message = format!(
            "Tailscale is not logged in (BackendState={}). Connect Tailscale, then retry.",
            node.backend_state
        );
        return status;
    }

    if !node.running {
        status.state = PhoneAccessState::TailscaleUnavailable;
        status.message = format!(
            "Tailscale is not running (BackendState={}).",
            node.backend_state
        );
        return status;
    }

    if let Some(err) = input.serve_error.as_ref() {
        status.state = PhoneAccessState::CommandError;
        status.message = err.clone();
        return status;
    }

    let Some(serve) = input.serve.as_ref() else {
        status.state = PhoneAccessState::CommandError;
        status.message = "Serve status unavailable".into();
        return status;
    };

    status.funnel_present = serve.funnel_present;
    if serve.funnel_present {
        status.state = PhoneAccessState::FunnelPresent;
        status.message =
            "Tailscale Funnel is present. IRIN never enables Funnel; clear Funnel before phone access."
                .into();
        // Still surface URL if known so the operator can diagnose.
        if let Some(dns) = node.dns_name.as_deref() {
            status.tailnet_url = Some(tailnet_https_url(dns, port));
        }
        return status;
    }
    if serve.unsupported_surfaces_present {
        status.state = PhoneAccessState::ForeignUnowned;
        status.message =
            "Tailscale Serve has a non-HTTPS TCP surface on the IRIN port this app does not own."
                .into();
        return status;
    }

    let expected_if_owned = if input.ownership.enabled {
        irin_serve_routes(input.ownership.gateway_routes)
    } else {
        Vec::new()
    };

    if !input.ownership.enabled {
        // Only handlers on the IRIN port count. Other ports (Hermes on 443) coexist.
        if !serve.handlers.is_empty()
            && handlers_look_foreign(&serve.handlers, &expected_if_owned, false)
        {
            status.state = PhoneAccessState::ForeignUnowned;
            status.message =
                "Tailscale Serve has configuration on the IRIN HTTPS port this app does not own. Disable foreign routes on that port or leave phone access off."
                    .into();
            return status;
        }
        status.state = PhoneAccessState::Off;
        status.message = "Phone access is off".into();
        return status;
    }

    // Enabled: require exact IRIN route table on the owned port.
    if serve.handlers.is_empty() {
        status.state = PhoneAccessState::CommandError;
        status.message =
            "Phone access is marked enabled but no Serve handlers are configured on the IRIN port."
                .into();
        return status;
    }

    if !handlers_match_routes(&serve.handlers, &expected_if_owned) {
        // Mismatch while we claim ownership — treat as foreign/unowned for safety.
        status.state = PhoneAccessState::ForeignUnowned;
        status.message =
            "Serve handlers on the IRIN port do not match the IRIN-owned route table. Disable phone access to remove IRIN routes."
                .into();
        return status;
    }

    if let Some(dns) = node.dns_name.as_deref() {
        status.tailnet_url = Some(tailnet_https_url(dns, port));
    }

    if input.council_backend_ready {
        status.state = PhoneAccessState::Ready;
        status.message = match status.tailnet_url.as_deref() {
            Some(url) => format!("Phone access ready at {url}"),
            None => "Phone access published; tailnet DNS name not yet available".into(),
        };
    } else {
        status.state = PhoneAccessState::PublishedButBackendDown;
        status.message = match status.tailnet_url.as_deref() {
            Some(url) => {
                format!("Phone access published at {url}, but the Council backend is not ready")
            }
            None => "Phone access published, but the Council backend is not ready".into(),
        };
    }

    let _ = input.gateway_pack_enabled;
    status
}

pub fn phone_data_dir() -> PathBuf {
    app_support_dir().join(PHONE_DIR_NAME)
}

pub fn ownership_path() -> PathBuf {
    phone_data_dir().join(OWNERSHIP_FILE)
}

pub fn prior_status_path() -> PathBuf {
    phone_data_dir().join(PRIOR_STATUS_FILE)
}

fn ensure_phone_dir() -> Result<PathBuf, String> {
    let dir = phone_data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create phone-access dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "atomic write path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let name = path
        .file_name()
        .ok_or_else(|| "atomic write path has no file name".to_string())?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create tmp: {e}"))?;
        f.write_all(bytes).map_err(|e| format!("write tmp: {e}"))?;
        f.sync_all().map_err(|e| format!("sync tmp: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|e| format!("rename tmp: {e}"))?;
    Ok(())
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load_ownership() -> OwnershipRecord {
    let path = ownership_path();
    let Ok(bytes) = fs::read(&path) else {
        return OwnershipRecord::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_ownership(rec: &OwnershipRecord) -> Result<(), String> {
    ensure_phone_dir()?;
    let mut rec = rec.clone();
    rec.version = OWNERSHIP_VERSION;
    rec.owner = OWNERSHIP_OWNER.to_string();
    if rec.https_port == 0 {
        rec.https_port = DEFAULT_HTTPS_PORT;
    }
    rec.updated_unix = unix_now();
    let bytes = serde_json::to_vec_pretty(&rec).map_err(|e| format!("serialize ownership: {e}"))?;
    write_atomic_0600(&ownership_path(), &bytes)
}

/// Trait for Tailscale execution so tests inject pure fixtures.
pub trait TailscaleRunner {
    fn run_stdout(&self, args: &[String]) -> Result<String, String>;
}

/// Live runner — executes the allow-listed host binary.
pub struct LiveTailscaleRunner;

impl TailscaleRunner for LiveTailscaleRunner {
    fn run_stdout(&self, args: &[String]) -> Result<String, String> {
        crate::tailscale_cli::run_tailscale_stdout(args, TAILSCALE_CMD_TIMEOUT)
    }
}

fn parse_serve_for_port(raw: &str, port: u16) -> Result<ServeStatusView, String> {
    parse_serve_status_json_for_port(raw, port)
}

/// Read and parse one Tailscale status projection, retrying once only when a
/// successful command returned malformed output. Status reads are idempotent;
/// a second malformed response still fails closed with the original error.
fn read_status_with_one_parse_retry<T>(
    runner: &dyn TailscaleRunner,
    args: &[String],
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    let raw = runner.run_stdout(args)?;
    match parse(&raw) {
        Ok(status) => Ok(status),
        Err(first_error) => {
            let retry = runner.run_stdout(args)?;
            parse(&retry).map_err(|_| first_error)
        }
    }
}

/// Read non-secret phone status. The native owner supplies an authenticated,
/// source-identity-matched Council readiness result; a bare open TCP port is
/// never sufficient because phone publication must not expose an unrelated
/// listener that happens to occupy the Council port.
pub fn phone_access_status(
    runner: &dyn TailscaleRunner,
    gateway_pack_enabled: bool,
    council_backend_ready: bool,
) -> PhoneAccessStatus {
    let ownership = load_ownership();
    let port = if ownership.enabled || ownership.interrupted {
        ownership_port(&ownership)
    } else {
        https_port()
    };
    let node_args = status_json_args();
    let (node, node_error) =
        match read_status_with_one_parse_retry(runner, &node_args, parse_status_json) {
            Ok(node) => (Some(node), None),
            Err(error) => (None, Some(error)),
        };
    let serve_args = serve_status_json_args();
    let (serve, serve_error) = match read_status_with_one_parse_retry(runner, &serve_args, |raw| {
        parse_serve_for_port(raw, port)
    }) {
        Ok(serve) => (Some(serve), None),
        Err(error) => (None, Some(error)),
    };
    classify_phone_status(PhoneStatusInputs {
        ownership,
        node,
        serve,
        node_error,
        serve_error,
        council_backend_ready,
        gateway_pack_enabled,
    })
}

/// Enable phone publication: snapshot prior config, apply IRIN routes, verify.
pub fn phone_access_enable(
    runner: &dyn TailscaleRunner,
    gateway_pack_enabled: bool,
    council_backend_ready: bool,
) -> Result<PhoneAccessStatus, String> {
    phone_access_enable_on_port(
        runner,
        gateway_pack_enabled,
        council_backend_ready,
        https_port(),
    )
}

/// Enable on an explicit selected HTTPS port (tests inject the port to avoid
/// process-wide env races; product code uses [`phone_access_enable`]).
pub fn phone_access_enable_on_port(
    runner: &dyn TailscaleRunner,
    gateway_pack_enabled: bool,
    council_backend_ready: bool,
    selected_port: u16,
) -> Result<PhoneAccessStatus, String> {
    if !council_backend_ready {
        return Err(
            "Council is not authenticated-ready on the bundled build; phone access was not changed"
                .to_string(),
        );
    }
    if selected_port == 0 {
        return Err("selected HTTPS port must be non-zero".to_string());
    }
    let _guard = PHONE_LOCK
        .lock()
        .map_err(|_| "phone access lifecycle lock poisoned".to_string())?;

    ensure_phone_dir()?;

    // Preflight node status.
    let status_raw = runner
        .run_stdout(&status_json_args())
        .map_err(|e| format!("tailscale status: {e}"))?;
    let node = parse_status_json(&status_raw)?;
    if !node.logged_in {
        return Ok(classify_phone_status(PhoneStatusInputs {
            ownership: load_ownership(),
            node: Some(node),
            serve: None,
            node_error: None,
            serve_error: None,
            council_backend_ready,
            gateway_pack_enabled,
        }));
    }
    if !node.running {
        return Ok(classify_phone_status(PhoneStatusInputs {
            ownership: load_ownership(),
            node: Some(node),
            serve: None,
            node_error: None,
            serve_error: None,
            council_backend_ready,
            gateway_pack_enabled,
        }));
    }

    let serve_raw = runner
        .run_stdout(&serve_status_json_args())
        .map_err(|e| format!("serve status: {e}"))?;
    let target_serve = parse_serve_for_port(&serve_raw, selected_port)?;
    let mut ownership_now = load_ownership();
    // Owned only for the selected port. A durable record on a different port
    // does not grant ownership of the target — preflight that port as unowned.
    let already_owns_selected =
        ownership_now.enabled && ownership_port(&ownership_now) == selected_port;
    match enable_preflight_gate(&node, &target_serve, already_owns_selected) {
        EnableGate::Proceed => {}
        EnableGate::RejectNotLoggedIn
        | EnableGate::RejectNotRunning
        | EnableGate::RejectFunnel
        | EnableGate::RejectForeign => {
            return Ok(classify_phone_status(PhoneStatusInputs {
                ownership: ownership_now,
                node: Some(node),
                serve: Some(target_serve),
                node_error: None,
                serve_error: None,
                council_backend_ready,
                gateway_pack_enabled,
            }));
        }
    }

    if ownership_now.enabled {
        let owned_port = ownership_port(&ownership_now);
        let owned_routes = irin_serve_routes(ownership_now.gateway_routes);
        let owned_serve = if already_owns_selected {
            target_serve.clone()
        } else {
            parse_serve_for_port(&serve_raw, owned_port)?
        };
        if owned_serve.funnel_present
            || owned_serve.unsupported_surfaces_present
            || !handlers_match_routes(&owned_serve.handlers, &owned_routes)
        {
            return Ok(classify_phone_status(PhoneStatusInputs {
                ownership: ownership_now,
                node: Some(node),
                serve: Some(owned_serve),
                node_error: None,
                serve_error: None,
                council_backend_ready,
                gateway_pack_enabled,
            }));
        }
        if already_owns_selected && ownership_now.gateway_routes == gateway_pack_enabled {
            return Ok(phone_access_status(
                runner,
                gateway_pack_enabled,
                council_backend_ready,
            ));
        }

        if !already_owns_selected {
            // Port migration: target was preflighted as unowned. Fail closed if
            // it is not empty before we disable the old owned port.
            if !target_serve.empty {
                return Ok(classify_phone_status(PhoneStatusInputs {
                    ownership: ownership_now,
                    node: Some(node),
                    serve: Some(target_serve),
                    node_error: None,
                    serve_error: None,
                    council_backend_ready,
                    gateway_pack_enabled,
                }));
            }
            // Snapshot/prove the migration target is empty before any mutation.
            write_atomic_0600(prior_status_path().as_path(), serve_raw.as_bytes())?;
            verify_prior_irin_port_empty(selected_port)?;
        } else {
            // Same-port reconfigure only from a proven exact owned state.
            verify_prior_irin_port_empty(owned_port)?;
        }

        ownership_now.interrupted = true;
        save_ownership(&ownership_now)?;
        disable_owned_and_verify_empty(runner, ownership_now.gateway_routes, owned_port)?;
        ownership_now.enabled = false;
    } else {
        // Preflight admitted only an empty selected port (other ports may exist).
        write_atomic_0600(prior_status_path().as_path(), serve_raw.as_bytes())?;
    }

    let mut ownership = ownership_now;
    ownership.interrupted = true;
    ownership.gateway_routes = gateway_pack_enabled;
    ownership.enabled = false;
    ownership.https_port = selected_port;
    save_ownership(&ownership)?;

    // Apply IRIN routes on the selected port only.
    for args in serve_apply_all_args_for_port(gateway_pack_enabled, selected_port) {
        runner
            .run_stdout(&args)
            .map_err(|e| format!("serve apply: {e}"))?;
    }

    // Verify: re-read serve status; reject funnel; require exact routes on port.
    let serve_raw = runner
        .run_stdout(&serve_status_json_args())
        .map_err(|e| format!("serve status verify: {e}"))?;
    let serve = parse_serve_for_port(&serve_raw, selected_port)?;
    if serve.funnel_present {
        ownership.interrupted = true;
        ownership.enabled = false;
        save_ownership(&ownership)?;
        return Err(
            "Serve verification found Funnel after apply; phone access left interrupted for recovery"
                .to_string(),
        );
    }
    let expected = irin_serve_routes(gateway_pack_enabled);
    if !handlers_match_routes(&serve.handlers, &expected) {
        ownership.interrupted = true;
        ownership.enabled = false;
        save_ownership(&ownership)?;
        return Err(
            "Serve verification did not observe the IRIN-owned route table; left interrupted"
                .to_string(),
        );
    }

    ownership.enabled = true;
    ownership.interrupted = false;
    ownership.gateway_routes = gateway_pack_enabled;
    ownership.https_port = selected_port;
    save_ownership(&ownership)?;

    Ok(phone_access_status(
        runner,
        gateway_pack_enabled,
        council_backend_ready,
    ))
}

/// Disable phone publication by turning off only IRIN-owned handlers on the
/// owned HTTPS port. Never uses global `serve reset`.
pub fn phone_access_disable(
    runner: &dyn TailscaleRunner,
    gateway_pack_enabled: bool,
    council_backend_ready: bool,
) -> Result<PhoneAccessStatus, String> {
    let _guard = PHONE_LOCK
        .lock()
        .map_err(|_| "phone access lifecycle lock poisoned".to_string())?;

    let mut ownership = load_ownership();
    let prior = prior_status_path();
    match plan_restore(&ownership, prior.is_file()) {
        RestorePlan::FailClosedNoSnapshot => {
            ownership.interrupted = true;
            save_ownership(&ownership)?;
            return Err(
                "No pre-enable Serve status found; refusing blind disable. Restore Tailscale Serve on the IRIN port manually, then clear phone-access ownership."
                    .to_string(),
            );
        }
        RestorePlan::DisableOwnedPort => {
            let port = ownership_port(&ownership);
            verify_prior_irin_port_empty(port)?;
            let current_raw = runner
                .run_stdout(&serve_status_json_args())
                .map_err(|e| format!("serve status before disable: {e}"))?;
            let current = parse_serve_for_port(&current_raw, port)?;
            let expected = irin_serve_routes(ownership.gateway_routes);
            let exact = handlers_match_routes(&current.handlers, &expected);
            let recoverable_partial =
                ownership.interrupted && handlers_are_route_subset(&current.handlers, &expected);
            if current.funnel_present
                || current.unsupported_surfaces_present
                || (!exact && !recoverable_partial)
            {
                ownership.interrupted = true;
                save_ownership(&ownership)?;
                return Err(
                    "Current Serve configuration on the IRIN port is not exclusively IRIN-owned; refusing disable"
                        .to_string(),
                );
            }
            ownership.interrupted = true;
            save_ownership(&ownership)?;
            disable_owned_and_verify_empty(runner, ownership.gateway_routes, port)?;
        }
        RestorePlan::AlreadyOff => {}
    }

    ownership.enabled = false;
    ownership.interrupted = false;
    ownership.gateway_routes = false;
    ownership.https_port = DEFAULT_HTTPS_PORT;
    save_ownership(&ownership)?;

    // Keep pre-enable status for audit; next unowned enable refreshes it.
    Ok(phone_access_status(
        runner,
        gateway_pack_enabled,
        council_backend_ready,
    ))
}

/// Pure restore decision: whether disable may clear IRIN-owned routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestorePlan {
    /// Disable IRIN-owned handlers on the owned port via port-scoped `off`.
    DisableOwnedPort,
    /// Nothing was enabled and no snapshot — just clear interrupted flag.
    AlreadyOff,
    /// Enabled without snapshot — fail closed (no blind disable).
    FailClosedNoSnapshot,
}

pub fn plan_restore(ownership: &OwnershipRecord, prior_exists: bool) -> RestorePlan {
    if ownership.enabled || ownership.interrupted {
        if prior_exists {
            RestorePlan::DisableOwnedPort
        } else if ownership.enabled {
            RestorePlan::FailClosedNoSnapshot
        } else {
            // Interrupted but no snapshot and not enabled: clear local flag only.
            RestorePlan::AlreadyOff
        }
    } else {
        RestorePlan::AlreadyOff
    }
}

fn verify_prior_irin_port_empty(port: u16) -> Result<(), String> {
    let raw = fs::read_to_string(prior_status_path())
        .map_err(|e| format!("read pre-enable Serve status: {e}"))?;
    let prior = parse_serve_for_port(&raw, port)
        .map_err(|e| format!("parse pre-enable Serve status: {e}"))?;
    if !prior.empty {
        return Err(
            "Pre-enable Serve status was not empty on the IRIN HTTPS port; refusing destructive disable"
                .to_string(),
        );
    }
    Ok(())
}

fn disable_owned_and_verify_empty(
    runner: &dyn TailscaleRunner,
    gateway_routes: bool,
    port: u16,
) -> Result<(), String> {
    for args in serve_disable_all_args_for_port(gateway_routes, port) {
        runner
            .run_stdout(&args)
            .map_err(|e| format!("serve disable: {e}"))?;
    }
    let raw = runner
        .run_stdout(&serve_status_json_args())
        .map_err(|e| format!("serve status after disable: {e}"))?;
    let status = parse_serve_for_port(&raw, port)?;
    if !status.handlers.is_empty() {
        return Err(
            "Serve disable did not clear IRIN handlers on the dedicated HTTPS port".to_string(),
        );
    }
    if status.funnel_present {
        return Err("Serve disable left Funnel present on the IRIN port".to_string());
    }
    Ok(())
}

/// Pure enable preflight gate before mutating Serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnableGate {
    Proceed,
    RejectNotLoggedIn,
    RejectNotRunning,
    RejectFunnel,
    RejectForeign,
}

/// Pure preflight gate before mutating Serve on the *selected* port.
///
/// `already_owns_selected` is true only when durable ownership is enabled AND
/// the owned HTTPS port equals the selected port. Ownership of another port
/// must not suppress foreign rejection on the selected port.
pub fn enable_preflight_gate(
    node: &TailscaleNodeStatus,
    serve: &ServeStatusView,
    already_owns_selected: bool,
) -> EnableGate {
    if !node.logged_in {
        return EnableGate::RejectNotLoggedIn;
    }
    if !node.running {
        return EnableGate::RejectNotRunning;
    }
    if serve.funnel_present {
        return EnableGate::RejectFunnel;
    }
    if serve.unsupported_surfaces_present {
        return EnableGate::RejectForeign;
    }
    // When we do not already own the selected port, refuse foreign handlers on
    // that port only. Handlers on other ports (Hermes 443) are not foreign.
    if !already_owns_selected
        && !serve.handlers.is_empty()
        && handlers_look_foreign(&serve.handlers, &[], false)
    {
        return EnableGate::RejectForeign;
    }
    EnableGate::Proceed
}

/// Expected proxy target for a path after enable (test helper surface).
#[cfg(test)]
fn expected_proxy_for_path(path: &str, gateway_pack_enabled: bool) -> Option<&'static str> {
    irin_serve_routes(gateway_pack_enabled)
        .into_iter()
        .find(|r| r.path == path)
        .map(|r| r.target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_config::{test_env_lock, APP_SUPPORT_ROOT_ENV};
    use crate::tailscale_cli::{ObservedHandler, ServeStatusView, TailscaleNodeStatus};

    fn running_node() -> TailscaleNodeStatus {
        TailscaleNodeStatus {
            backend_state: "Running".into(),
            dns_name: Some("phone.example.ts.net".into()),
            running: true,
            logged_in: true,
        }
    }

    fn owned_serve(gateway: bool) -> ServeStatusView {
        let handlers = irin_serve_routes(gateway)
            .into_iter()
            .map(|r| ObservedHandler {
                path: r.path.to_string(),
                proxy: r.target.to_string(),
            })
            .collect();
        ServeStatusView {
            handlers,
            funnel_present: false,
            unsupported_surfaces_present: false,
            empty: false,
            https_port: DEFAULT_HTTPS_PORT,
        }
    }

    fn empty_serve() -> ServeStatusView {
        ServeStatusView {
            handlers: vec![],
            funnel_present: false,
            unsupported_surfaces_present: false,
            empty: true,
            https_port: DEFAULT_HTTPS_PORT,
        }
    }

    #[test]
    fn classify_off_when_disabled_and_empty_serve() {
        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: Some(running_node()),
            serve: Some(empty_serve()),
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::Off);
        assert!(status.tailnet_url.is_none());
    }

    #[test]
    fn classify_ready_with_copyable_url_including_port() {
        let ownership = OwnershipRecord {
            enabled: true,
            gateway_routes: false,
            https_port: DEFAULT_HTTPS_PORT,
            ..OwnershipRecord::default()
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership,
            node: Some(running_node()),
            serve: Some(owned_serve(false)),
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::Ready);
        assert_eq!(
            status.tailnet_url.as_deref(),
            Some("https://phone.example.ts.net:8443")
        );
        assert!(!status.message.contains("Bearer"));
        assert!(!status.message.contains("token"));
    }

    #[test]
    fn classify_multi_port_hermes_coexists_when_disabled() {
        // Serve view is already port-scoped: empty IRIN port means Off even if
        // Hermes exists on 443 in the raw JSON (parser strips other ports).
        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: Some(running_node()),
            serve: Some(empty_serve()),
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::Off);
    }

    #[test]
    fn classify_foreign_on_irin_port_when_disabled() {
        let serve = ServeStatusView {
            handlers: vec![ObservedHandler {
                path: "/".into(),
                proxy: "http://127.0.0.1:9999".into(),
            }],
            funnel_present: false,
            unsupported_surfaces_present: false,
            empty: false,
            https_port: DEFAULT_HTTPS_PORT,
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: Some(running_node()),
            serve: Some(serve),
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::ForeignUnowned);
    }

    #[test]
    fn classify_published_but_backend_down() {
        let ownership = OwnershipRecord {
            enabled: true,
            ..OwnershipRecord::default()
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership,
            node: Some(running_node()),
            serve: Some(owned_serve(false)),
            node_error: None,
            serve_error: None,
            council_backend_ready: false,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::PublishedButBackendDown);
    }

    #[test]
    fn classify_not_logged_in_and_unavailable() {
        let node = TailscaleNodeStatus {
            backend_state: "NeedsLogin".into(),
            dns_name: None,
            running: false,
            logged_in: false,
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: Some(node),
            serve: None,
            node_error: None,
            serve_error: None,
            council_backend_ready: false,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::NotLoggedIn);

        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: None,
            serve: None,
            node_error: Some("Tailscale CLI not found".into()),
            serve_error: None,
            council_backend_ready: false,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::TailscaleUnavailable);
    }

    #[test]
    fn classify_funnel_and_foreign_and_interrupted() {
        let serve = ServeStatusView {
            handlers: vec![],
            funnel_present: true,
            unsupported_surfaces_present: false,
            empty: false,
            https_port: DEFAULT_HTTPS_PORT,
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: Some(running_node()),
            serve: Some(serve),
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::FunnelPresent);

        let serve = ServeStatusView {
            handlers: vec![ObservedHandler {
                path: "/".into(),
                proxy: "http://127.0.0.1:3010".into(),
            }],
            funnel_present: false,
            unsupported_surfaces_present: false,
            empty: false,
            https_port: DEFAULT_HTTPS_PORT,
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership: OwnershipRecord::default(),
            node: Some(running_node()),
            serve: Some(serve),
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::ForeignUnowned);

        let ownership = OwnershipRecord {
            interrupted: true,
            ..OwnershipRecord::default()
        };
        let status = classify_phone_status(PhoneStatusInputs {
            ownership,
            node: Some(running_node()),
            serve: None,
            node_error: None,
            serve_error: None,
            council_backend_ready: true,
            gateway_pack_enabled: false,
        });
        assert_eq!(status.state, PhoneAccessState::InterruptedChange);
    }

    #[test]
    fn restore_plan_fail_closed_without_snapshot() {
        let ownership = OwnershipRecord {
            enabled: true,
            ..OwnershipRecord::default()
        };
        assert_eq!(
            plan_restore(&ownership, false),
            RestorePlan::FailClosedNoSnapshot
        );
        assert_eq!(
            plan_restore(&ownership, true),
            RestorePlan::DisableOwnedPort
        );
        assert_eq!(
            plan_restore(&OwnershipRecord::default(), false),
            RestorePlan::AlreadyOff
        );
    }

    #[test]
    fn enable_preflight_rejects_funnel_and_foreign_on_irin_port_only() {
        let node = running_node();
        let funnel = ServeStatusView {
            handlers: vec![],
            funnel_present: true,
            unsupported_surfaces_present: false,
            empty: false,
            https_port: DEFAULT_HTTPS_PORT,
        };
        assert_eq!(
            enable_preflight_gate(&node, &funnel, false),
            EnableGate::RejectFunnel
        );
        let foreign = ServeStatusView {
            handlers: vec![ObservedHandler {
                path: "/".into(),
                proxy: "http://127.0.0.1:9999".into(),
            }],
            funnel_present: false,
            unsupported_surfaces_present: false,
            empty: false,
            https_port: DEFAULT_HTTPS_PORT,
        };
        assert_eq!(
            enable_preflight_gate(&node, &foreign, false),
            EnableGate::RejectForeign
        );
        // Empty IRIN port (Hermes may live on 443 in raw JSON) proceeds.
        let empty = empty_serve();
        assert_eq!(
            enable_preflight_gate(&node, &empty, false),
            EnableGate::Proceed
        );
    }

    #[test]
    fn ownership_roundtrip_under_app_support_override() {
        let _g = test_env_lock();
        let tmp =
            std::env::temp_dir().join(format!("phone-own-{}-{}", std::process::id(), unix_now()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);
        let rec = OwnershipRecord {
            enabled: true,
            gateway_routes: true,
            https_port: 8443,
            ..OwnershipRecord::default()
        };
        save_ownership(&rec).unwrap();
        let loaded = load_ownership();
        assert!(loaded.enabled);
        assert!(loaded.gateway_routes);
        assert_eq!(loaded.owner, OWNERSHIP_OWNER);
        assert_eq!(loaded.https_port, 8443);
        assert!(prior_status_path()
            .parent()
            .unwrap()
            .ends_with(PHONE_DIR_NAME));
        write_atomic_0600(prior_status_path().as_path(), b"{}").unwrap();
        assert!(prior_status_path().is_file());
        match prev {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn expected_routes_never_expose_gateway_listener() {
        for gw in [false, true] {
            for r in irin_serve_routes(gw) {
                assert!(
                    r.target.starts_with("http://127.0.0.1:"),
                    "non-loopback target {}",
                    r.target
                );
                assert!(!r.target.contains(":0"));
            }
        }
        assert_eq!(
            expected_proxy_for_path("/", false),
            Some("http://127.0.0.1:8765")
        );
        assert_eq!(expected_proxy_for_path("/watch", false), None);
        assert_eq!(
            expected_proxy_for_path("/watch", true),
            Some("http://127.0.0.1:18080/watch")
        );
    }

    /// Fake runner for enable/disable pure sequencing without a host binary.
    struct ScriptedRunner {
        responses: Mutex<Vec<(String, Result<String, String>)>>,
        log: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn new(responses: Vec<(String, Result<String, String>)>) -> Self {
            Self {
                responses: Mutex::new(responses),
                log: Mutex::new(Vec::new()),
            }
        }
    }

    impl TailscaleRunner for ScriptedRunner {
        fn run_stdout(&self, args: &[String]) -> Result<String, String> {
            self.log.lock().unwrap().push(args.to_vec());
            let key = args.join(" ");
            let mut responses = self.responses.lock().unwrap();
            if let Some(idx) = responses
                .iter()
                .position(|(k, _)| key.starts_with(k) || k == &key)
            {
                let (_, result) = responses.remove(idx);
                return result;
            }
            // Prefix match on first tokens.
            if let Some(idx) = responses.iter().position(|(k, _)| {
                let parts: Vec<&str> = k.split_whitespace().collect();
                args.iter().zip(parts.iter()).all(|(a, p)| a == *p) && args.len() >= parts.len()
            }) {
                let (_, result) = responses.remove(idx);
                return result;
            }
            Err(format!("no scripted response for: {key}"))
        }
    }

    #[test]
    fn malformed_status_output_recovers_on_one_retry() {
        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(String::new())),
            (
                "status --json".into(),
                Ok(
                    r#"{"BackendState":"Running","Self":{"DNSName":"node.example.ts.net."}}"#
                        .into(),
                ),
            ),
        ]);
        let args = status_json_args();

        let node = read_status_with_one_parse_retry(&runner, &args, parse_status_json).unwrap();

        assert!(node.running);
        assert_eq!(node.dns_name.as_deref(), Some("node.example.ts.net"));
        assert_eq!(runner.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn repeated_malformed_status_output_still_fails_closed() {
        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(String::new())),
            ("status --json".into(), Ok(String::new())),
        ]);
        let args = status_json_args();

        let error =
            read_status_with_one_parse_retry(&runner, &args, parse_status_json).unwrap_err();

        assert!(error.starts_with("status json:"));
        assert_eq!(runner.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn phone_access_status_recovers_from_one_malformed_node_sample() {
        let _g = test_env_lock();
        let tmp = std::env::temp_dir().join(format!(
            "phone-status-retry-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);

        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(String::new())),
            (
                "status --json".into(),
                Ok(
                    r#"{"BackendState":"Running","Self":{"DNSName":"node.example.ts.net."}}"#
                        .into(),
                ),
            ),
            ("serve status --json".into(), Ok(r#"{"Web":{}}"#.into())),
        ]);

        let status = phone_access_status(&runner, false, true);

        assert_eq!(status.state, PhoneAccessState::Off);
        assert_eq!(status.message, "Phone access is off");
        assert_eq!(runner.log.lock().unwrap().len(), 3);

        match prev {
            Some(value) => std::env::set_var(APP_SUPPORT_ROOT_ENV, value),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn enable_refuses_before_tailscale_when_council_is_unverified() {
        let runner = ScriptedRunner::new(Vec::new());
        let err = phone_access_enable(&runner, false, false).unwrap_err();
        assert!(err.contains("not authenticated-ready"), "{err}");
        assert!(runner.log.lock().unwrap().is_empty());
    }

    #[test]
    fn enable_disable_scripted_uses_8443_and_port_scoped_off() {
        let _g = test_env_lock();
        let tmp =
            std::env::temp_dir().join(format!("phone-flow-{}-{}", std::process::id(), unix_now()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);

        let status_json =
            r#"{"BackendState":"Running","Self":{"DNSName":"phone.example.ts.net."}}"#;
        // Prior multi-port: Hermes on 443, IRIN port empty.
        let prior_serve = r#"{
          "TCP":{"443":{"HTTPS":true}},
          "Web":{"phone.example.ts.net:443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:8787"}
          }}}
        }"#;
        let owned_serve_json = r#"{
          "TCP":{"443":{"HTTPS":true},"8443":{"HTTPS":true}},
          "Web":{
            "phone.example.ts.net:443":{"Handlers":{
              "/":{"Proxy":"http://127.0.0.1:8787"}
            }},
            "phone.example.ts.net:8443":{"Handlers":{
              "/":{"Proxy":"http://127.0.0.1:8765"}
            }}
          }
        }"#;
        // enable sequence: status, multi-port prior, apply root on 8443, verify,
        // then phone_access_status: status + Serve status
        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(prior_serve.into())),
            (
                "serve --bg --yes --https=8443 http://127.0.0.1:8765".into(),
                Ok(String::new()),
            ),
            ("serve status --json".into(), Ok(owned_serve_json.into())),
            // final status sample inside phone_access_status
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(owned_serve_json.into())),
        ]);

        let st = phone_access_enable(&runner, false, true).unwrap();
        assert_eq!(st.state, PhoneAccessState::Ready);
        assert_eq!(
            st.tailnet_url.as_deref(),
            Some("https://phone.example.ts.net:8443")
        );
        assert!(prior_status_path().is_file());
        assert!(load_ownership().enabled);
        assert_eq!(load_ownership().https_port, 8443);

        let enable_log = runner.log.lock().unwrap().clone();
        assert!(enable_log
            .iter()
            .any(|args| args.iter().any(|a| a == "--https=8443")));
        assert!(!enable_log
            .iter()
            .any(|args| args.windows(2).any(|w| w[0] == "serve" && w[1] == "reset")));

        // disable: prove current ownership, port-scoped off, verify IRIN empty.
        let after_off = prior_serve; // Hermes remains; IRIN port empty
        let runner = ScriptedRunner::new(vec![
            ("serve status --json".into(), Ok(owned_serve_json.into())),
            (
                "serve --bg --yes --https=8443 off".into(),
                Ok(String::new()),
            ),
            ("serve status --json".into(), Ok(after_off.into())),
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(after_off.into())),
        ]);
        let st = phone_access_disable(&runner, false, false).unwrap();
        assert_eq!(st.state, PhoneAccessState::Off);
        assert!(!load_ownership().enabled);
        assert!(!load_ownership().interrupted);

        let disable_log = runner.log.lock().unwrap().clone();
        assert!(disable_log.iter().any(
            |args| args.iter().any(|a| a == "off") && args.iter().any(|a| a == "--https=8443")
        ));
        assert!(!disable_log
            .iter()
            .any(|args| args.windows(2).any(|w| w[0] == "serve" && w[1] == "reset")));

        for args in serve_apply_all_args_for_port(true, 8443) {
            assert!(!args.iter().any(|a| a.to_lowercase().contains("funnel")));
            assert!(!args.windows(2).any(|w| w[0] == "serve" && w[1] == "reset"));
        }

        match prev {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn enable_preflight_proceeds_with_hermes_on_other_port() {
        let _g = test_env_lock();
        let tmp = std::env::temp_dir().join(format!(
            "phone-hermes-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);

        let status_json =
            r#"{"BackendState":"Running","Self":{"DNSName":"phone.example.ts.net."}}"#;
        let hermes_only = r#"{
          "Web":{"phone.example.ts.net:443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:8787"}
          }}}
        }"#;
        let owned = r#"{
          "Web":{
            "phone.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}},
            "phone.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}}
          }
        }"#;
        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(hermes_only.into())),
            (
                "serve --bg --yes --https=8443 http://127.0.0.1:8765".into(),
                Ok(String::new()),
            ),
            ("serve status --json".into(), Ok(owned.into())),
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(owned.into())),
        ]);
        let st = phone_access_enable(&runner, false, true).unwrap();
        assert_eq!(st.state, PhoneAccessState::Ready);

        match prev {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn enable_rejects_foreign_on_irin_port() {
        let _g = test_env_lock();
        let tmp = std::env::temp_dir().join(format!(
            "phone-foreign-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);

        let status_json =
            r#"{"BackendState":"Running","Self":{"DNSName":"phone.example.ts.net."}}"#;
        let foreign = r#"{
          "Web":{"phone.example.ts.net:8443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:9999"}
          }}}
        }"#;
        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(foreign.into())),
        ]);
        let st = phone_access_enable_on_port(&runner, false, true, 8443).unwrap();
        assert_eq!(st.state, PhoneAccessState::ForeignUnowned);
        assert!(!load_ownership().enabled);
        // No apply argv should have run.
        assert!(!runner
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|args| args.iter().any(|a| a.starts_with("--https="))));

        match prev {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn enable_rejects_foreign_on_migration_target_without_touching_old_port() {
        let _g = test_env_lock();
        let tmp = std::env::temp_dir().join(format!(
            "phone-migrate-foreign-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);

        // Durable ownership on 8443; selected migration target is 9443 with a
        // foreign root. Must fail closed without off on 8443 or apply on 9443.
        let rec = OwnershipRecord {
            enabled: true,
            https_port: 8443,
            gateway_routes: false,
            ..OwnershipRecord::default()
        };
        save_ownership(&rec).unwrap();
        write_atomic_0600(
            prior_status_path().as_path(),
            br#"{"Web":{},"TCP":{},"AllowFunnel":{}}"#,
        )
        .unwrap();

        let status_json =
            r#"{"BackendState":"Running","Self":{"DNSName":"phone.example.ts.net."}}"#;
        let multi = r#"{
          "TCP":{"8443":{"HTTPS":true},"9443":{"HTTPS":true}},
          "Web":{
            "phone.example.ts.net:8443":{"Handlers":{
              "/":{"Proxy":"http://127.0.0.1:8765"}
            }},
            "phone.example.ts.net:9443":{"Handlers":{
              "/":{"Proxy":"http://127.0.0.1:9999"}
            }}
          }
        }"#;
        let runner = ScriptedRunner::new(vec![
            ("status --json".into(), Ok(status_json.into())),
            ("serve status --json".into(), Ok(multi.into())),
        ]);
        let st = phone_access_enable_on_port(&runner, false, true, 9443).unwrap();
        assert_eq!(st.state, PhoneAccessState::ForeignUnowned);
        assert!(load_ownership().enabled);
        assert_eq!(load_ownership().https_port, 8443);
        assert!(!load_ownership().interrupted);

        let log = runner.log.lock().unwrap().clone();
        assert!(
            !log.iter().any(|args| {
                args.iter().any(|a| a == "off")
                    || args
                        .iter()
                        .any(|a| a == "--https=8443" && args.iter().any(|b| b == "off"))
                    || args.iter().any(|a| a == "--https=9443")
            }),
            "migration foreign reject must not disable old port or apply new port: {log:?}"
        );
        // Only status probes — never serve apply/disable.
        assert!(log.iter().all(|args| {
            args.starts_with(&["status".to_string(), "--json".to_string()])
                || args.starts_with(&[
                    "serve".to_string(),
                    "status".to_string(),
                    "--json".to_string(),
                ])
        }));

        match prev {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disable_fail_closed_without_snapshot_when_enabled() {
        let _g = test_env_lock();
        let tmp =
            std::env::temp_dir().join(format!("phone-fail-{}-{}", std::process::id(), unix_now()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &tmp);

        let rec = OwnershipRecord {
            enabled: true,
            ..OwnershipRecord::default()
        };
        save_ownership(&rec).unwrap();
        assert!(load_ownership().enabled);
        // no pre-enable status file
        let runner = ScriptedRunner::new(vec![]);
        let err = phone_access_disable(&runner, false, false).unwrap_err();
        assert!(err.contains("No pre-enable Serve status"), "{err}");
        assert!(load_ownership().interrupted);

        match prev {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
