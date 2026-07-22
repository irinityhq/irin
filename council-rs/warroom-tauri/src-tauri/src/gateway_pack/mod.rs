//! Installed-release Gateway Pack lifecycle.
//!
//! Privileged native boundary: Docker CLI allow-list, fixed compose project,
//! app-owned paths, Keychain-held GW_API_KEY. Renderer only receives non-secret
//! status and triggers fixed workflows.

pub mod manifest;

use crate::docker_cli::{
    compose_command, docker_command, format_cmd_failure, path_is_safe_argv, probe_docker_daemon,
    resolve_docker_cli, DockerDaemonState, DESKTOP_COMPOSE_PROJECT, DESKTOP_GATEWAY_URL,
};
use crate::keychain::{
    delete_gw_api_key, is_valid_gw_raw_key, load_gw_api_key, store_gw_api_key, KeychainSecretStore,
    SecretStore,
};
use crate::paths::{bundled_base_dir, executable_dir};
use crate::private_config::{
    app_support_dir, gui_login_environment, load_or_create_private_config, write_private_config_at,
};
use manifest::{
    image_id_matches_ref, load_manifest, validate_manifest, ImageRef, ValidatedManifest,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Truthful operator-facing pack states. Never label a bare URL as ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPackState {
    NotInstalled,
    DockerMissing,
    DockerDaemonDown,
    Installing,
    InstalledStopped,
    Starting,
    AuthenticatedReady,
    Degraded,
    Disabled,
}

impl GatewayPackState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::DockerMissing => "docker_missing",
            Self::DockerDaemonDown => "docker_daemon_down",
            Self::Installing => "installing",
            Self::InstalledStopped => "installed_stopped",
            Self::Starting => "starting",
            Self::AuthenticatedReady => "authenticated_ready",
            Self::Degraded => "degraded",
            Self::Disabled => "disabled",
        }
    }

    /// Governed proceedings may start only in this state.
    pub fn allows_governed(self) -> bool {
        matches!(self, Self::AuthenticatedReady)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayPackStatus {
    pub state: GatewayPackState,
    pub message: String,
    pub pack_version: Option<String>,
    pub manifest_mode: Option<String>,
    pub gateway_url: String,
    pub project: String,
    pub key_id: Option<String>,
    pub enabled: bool,
    pub docker: String,
    pub watch_producer_enabled: bool,
    pub watch_dispatcher_enabled: bool,
    pub authenticated: bool,
    pub support_matrix_summary: String,
}

impl GatewayPackStatus {
    fn base(state: GatewayPackState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            pack_version: None,
            manifest_mode: None,
            gateway_url: DESKTOP_GATEWAY_URL.to_string(),
            project: DESKTOP_COMPOSE_PROJECT.to_string(),
            key_id: None,
            enabled: false,
            docker: "unknown".into(),
            watch_producer_enabled: false,
            watch_dispatcher_enabled: false,
            authenticated: false,
            support_matrix_summary: SUPPORT_MATRIX_SUMMARY.to_string(),
        }
    }
}

pub const SUPPORT_MATRIX_SUMMARY: &str = "\
v0.1: API-key providers (xAI/OpenAI/Anthropic/NVIDIA) when present in login env; \
Vertex Direct-only (no gcloud mount); Claude/Codex CLI proxies not supported; \
Watch producer/dispatcher forced off.";

const GATEWAY_DIR_NAME: &str = "gateway";
const RUNTIME_ENV_NAME: &str = "runtime.env";
const LEDGER_KEY_NAME: &str = "ledger_key";
const INSTALLED_MARKER: &str = "pack-installed.json";

/// Fixed Application Support gateway directory (0700).
pub fn gateway_data_dir() -> PathBuf {
    app_support_dir().join(GATEWAY_DIR_NAME)
}

pub fn runtime_env_path() -> PathBuf {
    gateway_data_dir().join(RUNTIME_ENV_NAME)
}

pub fn ledger_key_path() -> PathBuf {
    gateway_data_dir().join(LEDGER_KEY_NAME)
}

pub fn installed_marker_path() -> PathBuf {
    gateway_data_dir().join(INSTALLED_MARKER)
}

/// Bundled pack root under app Resources (or test override).
pub fn bundled_pack_root() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var("IRIN_GATEWAY_PACK_ROOT") {
        let p = PathBuf::from(override_dir.trim());
        if p.join("docker-compose.yml").is_file() {
            return Some(p);
        }
    }
    let mac_os = executable_dir()?;
    let resources = mac_os.parent()?.join("Resources");
    let candidates = [
        resources.join("gateway-pack"),
        resources.join("resources").join("gateway-pack"),
    ];
    for c in candidates {
        if c.join("docker-compose.yml").is_file() {
            return Some(c);
        }
    }
    // Dev: packaging source or staged resources next to tauri.
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("gateway-pack");
    if dev.join("docker-compose.yml").is_file() {
        return Some(dev);
    }
    let repo_pack = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packaging/gateway-pack");
    if repo_pack.join("docker-compose.yml").is_file() {
        // Incomplete without conf/lua — only used for path unit tests.
        return Some(repo_pack.canonicalize().unwrap_or(repo_pack));
    }
    let _ = bundled_base_dir(); // silence unused in some cfgs
    None
}

fn ensure_gateway_dir() -> Result<PathBuf, String> {
    let dir = gateway_data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create gateway dir: {e}"))?;
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
        f.sync_all().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename {} -> {}: {e}", tmp.display(), path.display())
    })?;
    Ok(())
}

fn ensure_ledger_key() -> Result<PathBuf, String> {
    ensure_gateway_dir()?;
    let path = ledger_key_path();
    if path.is_file() {
        let meta = fs::metadata(&path).map_err(|e| format!("ledger meta: {e}"))?;
        if meta.len() != 32 {
            return Err("existing desktop ledger key must be exactly 32 bytes".to_string());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        return Ok(path);
    }
    let mut bytes = [0u8; 32];
    getrandom_fill(&mut bytes)?;
    write_atomic_0600(&path, &bytes)?;
    // Never log the seed.
    Ok(path)
}

fn getrandom_fill(buf: &mut [u8]) -> Result<(), String> {
    use std::fs::File;
    use std::io::Read;
    // Prefer OS random device; no extra crate.
    let mut f = File::open("/dev/urandom").map_err(|e| format!("open urandom: {e}"))?;
    f.read_exact(buf).map_err(|e| format!("read urandom: {e}"))
}

fn random_hex(n_bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; n_bytes];
    getrandom_fill(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Provider keys that may be forwarded into the pack env file (API-key routes only).
const PACK_PROVIDER_KEYS: &[&str] = &[
    "XAI_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "NVIDIA_API_KEY",
];

fn write_runtime_env(
    pack_root: &Path,
    ledger: &Path,
    gateway_image: &ImageRef,
    sidecar_image: &ImageRef,
    regenerate_bootstrap: bool,
) -> Result<PathBuf, String> {
    if !path_is_safe_argv(pack_root) || !path_is_safe_argv(ledger) {
        return Err("pack root or ledger path rejected".to_string());
    }
    ensure_gateway_dir()?;
    let path = runtime_env_path();

    // Preserve existing secrets when present unless regenerate is requested.
    let existing = if path.is_file() {
        parse_env_file(&path)
    } else {
        Default::default()
    };

    let auth_pepper = existing
        .get("AUTH_PEPPER")
        .filter(|v| !v.is_empty() && !v.starts_with("__GENERATED"))
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| random_hex(32))?;
    let bootstrap = if regenerate_bootstrap {
        random_hex(32)?
    } else {
        existing
            .get("BOOTSTRAP_TOKEN")
            .filter(|v| !v.is_empty() && !v.starts_with("__GENERATED"))
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| random_hex(32))?
    };
    let watch_admin = existing
        .get("WATCH_ADMIN_TOKEN")
        .filter(|v| !v.is_empty() && !v.starts_with("__GENERATED"))
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| random_hex(32))?;
    let council_token = existing
        .get("COUNCIL_GATEWAY_TOKEN")
        .filter(|v| !v.is_empty() && !v.starts_with("__GENERATED"))
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| random_hex(32))?;

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("IRIN_GATEWAY_IMAGE={}", gateway_image.as_str()));
    lines.push(format!("IRIN_SIDECAR_IMAGE={}", sidecar_image.as_str()));
    lines.push(format!(
        "IRIN_DESKTOP_PACK_ROOT={}",
        pack_root.display()
    ));
    lines.push(format!("IRIN_DESKTOP_LEDGER_KEY={}", ledger.display()));
    lines.push(format!("AUTH_PEPPER={auth_pepper}"));
    lines.push(format!("BOOTSTRAP_TOKEN={bootstrap}"));
    lines.push(format!("WATCH_ADMIN_TOKEN={watch_admin}"));
    lines.push(format!("COUNCIL_GATEWAY_TOKEN={council_token}"));
    lines.push("GATEWAY_DURABLE=1".into());
    lines.push("GATEWAY_AUTH_FAIL_CLOSED=true".into());
    lines.push("SIDECAR_SOCKET_MODE=0660".into());
    lines.push("SIDECAR_SOCKET_GID=9999".into());
    lines.push("GW_ENABLE_COUNCIL_ENDPOINT=0".into());
    lines.push("COUNCIL_BASE_URL=http://host.docker.internal:8765".into());
    lines.push("WATCH_PRODUCER_ENABLED=false".into());
    lines.push("WATCH_DISPATCHER_ENABLED=false".into());
    lines.push("WATCH_CANARY_TENANT=canary".into());
    lines.push("DAILY_SPEND_CAP_USD=25".into());
    lines.push("WATCH_MAX_FANOUT_COST_USD=2.50".into());

    // Provider API keys from login env / process — never Vertex/gcloud paths.
    let login = gui_login_environment();
    for key in PACK_PROVIDER_KEYS {
        let val = std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                login
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.trim().is_empty())
            })
            .or_else(|| existing.get(*key).cloned())
            .unwrap_or_default();
        // Values go only to the 0600 file; never logged.
        lines.push(format!("{key}={val}"));
    }

    if let Some(kid) = existing.get("COUNCIL_GATEWAY_KEY_ID") {
        if !kid.is_empty() {
            lines.push(format!("COUNCIL_GATEWAY_KEY_ID={kid}"));
        }
    }

    let mut body = lines.join("\n");
    body.push('\n');
    write_atomic_0600(&path, body.as_bytes())?;
    Ok(path)
}

fn parse_env_file(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let Ok(raw) = fs::read_to_string(path) else {
        return map;
    };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn upsert_env_value(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let mut map = if path.is_file() {
        parse_env_file(path)
    } else {
        Default::default()
    };
    map.insert(key.to_string(), value.to_string());
    let mut body = String::new();
    for (k, v) in map {
        body.push_str(&k);
        body.push('=');
        body.push_str(&v);
        body.push('\n');
    }
    write_atomic_0600(path, body.as_bytes())
}

fn read_env_value(path: &Path, key: &str) -> Option<String> {
    parse_env_file(path).get(key).cloned()
}

/// Copy bundled pack assets into Application Support (durable across updates).
pub fn install_pack_files() -> Result<PathBuf, String> {
    let src = bundled_pack_root().ok_or_else(|| {
        "Gateway Pack is not bundled in this app. Rebuild the DMG with stage-gateway-pack.sh."
            .to_string()
    })?;
    let dest = ensure_gateway_dir()?.join("pack");
    copy_dir_recursive(&src, &dest).map_err(|e| format!("install pack files: {e}"))?;
    // Marker with pack version from manifest when present.
    let manifest_path = dest.join("image-manifest.json");
    let version = if manifest_path.is_file() {
        load_manifest(&manifest_path)
            .ok()
            .map(|m| m.pack_version)
            .unwrap_or_else(|| "unknown".into())
    } else {
        "unknown".into()
    };
    let marker = serde_json::json!({
        "installed": true,
        "pack_version": version,
        "project": DESKTOP_COMPOSE_PROJECT,
    });
    write_atomic_0600(
        &installed_marker_path(),
        format!("{marker}\n").as_bytes(),
    )?;
    Ok(dest)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

pub fn installed_pack_root() -> Option<PathBuf> {
    let p = gateway_data_dir().join("pack");
    if p.join("docker-compose.yml").is_file() {
        Some(p)
    } else {
        bundled_pack_root()
    }
}

fn load_validated_manifest(pack_root: &Path) -> Result<ValidatedManifest, String> {
    let path = pack_root.join("image-manifest.json");
    if !path.is_file() {
        return Err("image-manifest.json missing from Gateway Pack".to_string());
    }
    let m = load_manifest(&path)?;
    validate_manifest(&m)
}

fn verify_images_present(v: &ValidatedManifest) -> Result<(), String> {
    for (label, image_ref) in [("gateway", &v.gateway), ("sidecar", &v.sidecar)] {
        // Prefer resolve by digest form; fall back to image id inspect via local tags.
        let out = docker_command(&[
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            image_ref.as_str(),
        ]);
        match out {
            Ok(o) if o.status.success() => {
                let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !image_id_matches_ref(&id, image_ref) {
                    return Err(format!(
                        "{label} image id does not match manifest digest (resolved id redacted length {})",
                        id.len()
                    ));
                }
            }
            Ok(o) => {
                // Try bare sha256:id form (local builds).
                let id_ref = format!("sha256:{}", image_ref.digest_hex());
                let out2 = docker_command(&["image", "inspect", "--format", "{{.Id}}", &id_ref]);
                match out2 {
                    Ok(o2) if o2.status.success() => {
                        let id = String::from_utf8_lossy(&o2.stdout).trim().to_string();
                        if !image_id_matches_ref(&id, image_ref) {
                            return Err(format!("{label} image digest mismatch"));
                        }
                    }
                    _ => {
                        return Err(format!(
                            "{label} image not present locally for {}. \
                             Run scripts/build-gateway-pack-dev-images.sh (local-dev) \
                             or load published digest-pinned images. {}",
                            image_ref.as_str(),
                            format_cmd_failure("image inspect", &o)
                        ));
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn compose_file(pack_root: &Path) -> PathBuf {
    pack_root.join("docker-compose.yml")
}

fn assert_watch_off_in_env_file(path: &Path) -> Result<(), String> {
    let map = parse_env_file(path);
    if map.get("WATCH_PRODUCER_ENABLED").map(String::as_str) != Some("false") {
        return Err("runtime env must set WATCH_PRODUCER_ENABLED=false".to_string());
    }
    if map.get("WATCH_DISPATCHER_ENABLED").map(String::as_str) != Some("false") {
        return Err("runtime env must set WATCH_DISPATCHER_ENABLED=false".to_string());
    }
    Ok(())
}

fn http_get_status(url: &str, bearer: Option<&str>) -> Result<(u16, String), String> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(10));
    if let Some(token) = bearer {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.into_string().unwrap_or_default();
            Ok((status, body))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((code, body))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

fn gateway_health_ok() -> bool {
    matches!(
        http_get_status(&format!("{DESKTOP_GATEWAY_URL}/health"), None),
        Ok((200, _))
    )
}

/// True when the sidecar management surface answers (not 502/connection error).
/// Empty/invalid body yields 4xx — that still proves the path is live.
fn admin_surface_ready() -> bool {
    match ureq::post(&format!("{DESKTOP_GATEWAY_URL}/admin/keys"))
        .timeout(Duration::from_secs(3))
        .set("Content-Type", "application/json")
        .send_string("{}")
    {
        Ok(resp) => {
            let s = resp.status();
            s != 502 && s != 503 && s != 504
        }
        Err(ureq::Error::Status(code, _)) => code != 502 && code != 503 && code != 504,
        Err(_) => false,
    }
}

fn models_authenticated(raw_key: &str) -> bool {
    matches!(
        http_get_status(
            &format!("{DESKTOP_GATEWAY_URL}/v1/models"),
            Some(raw_key)
        ),
        Ok((200, _))
    )
}

fn models_fail_closed_without_key() -> bool {
    match http_get_status(&format!("{DESKTOP_GATEWAY_URL}/v1/models"), None) {
        Ok((code, _)) => code == 401 || code == 403,
        Err(_) => true, // unreachable treated as fail-closed for pre-start
    }
}

/// Provision Council service-role client via real admin API. Raw key → Keychain only.
fn provision_council_client(store: &dyn SecretStore) -> Result<String, String> {
    let env_path = runtime_env_path();
    let bootstrap = read_env_value(&env_path, "BOOTSTRAP_TOKEN")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "BOOTSTRAP_TOKEN missing from pack runtime env".to_string())?;

    // Body via ureq JSON — secrets never appear in process argv.
    let body = serde_json::json!({
        "budget_key": "desktop-council",
        "tier": "default",
        "rpm": 600,
        "service_role": "council",
        "admin_key": bootstrap,
    });
    let resp = ureq::post(&format!("{DESKTOP_GATEWAY_URL}/admin/keys"))
        .timeout(Duration::from_secs(15))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("provision request failed: {e}"))?;
    if resp.status() != 200 {
        return Err(format!(
            "provision rejected with HTTP {}",
            resp.status()
        ));
    }
    let value: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("provision response json: {e}"))?;
    let raw_key = value
        .get("raw_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "provision response missing raw_key".to_string())?;
    if !is_valid_gw_raw_key(raw_key) {
        return Err("provision response raw_key has invalid shape".to_string());
    }
    let key_id = value
        .get("key_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "provision response missing key_id".to_string())?;
    if !key_id.starts_with("k_") || key_id.len() != 10 {
        // k_ + 8 hex — len 10
        return Err("provision response key_id has invalid shape".to_string());
    }

    store_gw_api_key(store, raw_key)?;
    // Persist non-secret key id only.
    upsert_env_value(&env_path, "COUNCIL_GATEWAY_KEY_ID", key_id)?;
    let mut cfg = load_or_create_private_config()?;
    cfg.gateway_key_id = Some(key_id.to_string());
    cfg.gateway_pack_version = cfg.gateway_pack_version.clone();
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;

    // raw_key dropped at end of scope; never returned to frontend.
    let _ = raw_key;
    Ok(key_id.to_string())
}

/// Compute truthful pack status without starting anything.
pub fn gateway_pack_status(store: &dyn SecretStore) -> GatewayPackStatus {
    let mut st = GatewayPackStatus::base(
        GatewayPackState::NotInstalled,
        "Gateway Pack is not installed. Core War Room works in Direct mode without Docker.",
    );

    let cfg = load_or_create_private_config().ok();
    if let Some(ref c) = cfg {
        st.enabled = c.via_gateway_default;
        st.key_id = c.gateway_key_id.clone();
        st.pack_version = c.gateway_pack_version.clone();
    }

    match probe_docker_daemon() {
        DockerDaemonState::CliMissing => {
            st.state = GatewayPackState::DockerMissing;
            st.docker = "cli_missing".into();
            st.message = "Docker CLI not found. Install Docker Desktop to use the optional Gateway Pack. Core War Room stays healthy in Direct mode.".into();
            return st;
        }
        DockerDaemonState::DaemonDown => {
            st.state = GatewayPackState::DockerDaemonDown;
            st.docker = "daemon_down".into();
            st.message = "Docker Desktop is installed but the daemon is not running. Open Docker Desktop, wait until it is ready, then retry. Core War Room stays healthy in Direct mode.".into();
            // Still note if pack files exist.
            if installed_marker_path().is_file() {
                st.message.push_str(" Pack files are present on disk.");
            }
            return st;
        }
        DockerDaemonState::Ready => {
            st.docker = "ready".into();
        }
    }

    let pack_root = match installed_pack_root() {
        Some(p) => p,
        None => {
            st.state = GatewayPackState::NotInstalled;
            return st;
        }
    };

    if let Ok(v) = load_validated_manifest(&pack_root) {
        st.pack_version = Some(v.pack_version.clone());
        st.manifest_mode = Some(v.mode.as_str().to_string());
    }

    // Is the desktop project running?
    let running = desktop_project_running();
    let health = gateway_health_ok();

    let key = load_gw_api_key(store).ok().flatten();
    let authenticated = key.as_ref().map(|k| models_authenticated(k)).unwrap_or(false);
    st.authenticated = authenticated;

    if !running {
        if cfg.as_ref().map(|c| c.via_gateway_default) == Some(true) {
            st.state = GatewayPackState::Degraded;
            st.message = "Gateway was enabled but the pack is not running. Start the pack or Disable Gateway for Direct mode.".into();
        } else if installed_marker_path().is_file() || pack_root.join("docker-compose.yml").is_file()
        {
            st.state = if cfg.as_ref().map(|c| c.via_gateway_default) == Some(false) {
                GatewayPackState::Disabled
            } else {
                GatewayPackState::InstalledStopped
            };
            st.message = "Gateway Pack is installed and stopped. Enable Gateway to start, provision, and authenticate.".into();
        } else {
            st.state = GatewayPackState::NotInstalled;
        }
        return st;
    }

    if !health {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway containers are up but /health failed. Check Docker logs for irin-desktop-gateway.".into();
        return st;
    }

    if authenticated {
        st.state = GatewayPackState::AuthenticatedReady;
        st.message = "Gateway Pack is authenticated and ready for governed proceedings.".into();
    } else if key.is_some() {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway is up but the stored client key failed /v1/models. Re-run Enable Gateway to re-provision.".into();
    } else {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway is up but no client key is in Keychain. Run Enable Gateway to provision.".into();
    }
    st
}

fn desktop_project_running() -> bool {
    let out = docker_command(&[
        "compose",
        "-p",
        DESKTOP_COMPOSE_PROJECT,
        "ps",
        "--status",
        "running",
        "-q",
    ]);
    match out {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => false,
    }
}

/// Full enable workflow: install files → start pack → provision → mark enabled.
/// Returns non-secret status. Never returns raw secrets.
pub fn enable_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    match probe_docker_daemon() {
        DockerDaemonState::CliMissing => {
            return Ok(gateway_pack_status(store));
        }
        DockerDaemonState::DaemonDown => {
            return Ok(gateway_pack_status(store));
        }
        DockerDaemonState::Ready => {}
    }
    let _ = resolve_docker_cli()?;

    let pack_root = install_pack_files()?;
    let validated = load_validated_manifest(&pack_root)?;
    verify_images_present(&validated)?;

    let ledger = ensure_ledger_key()?;
    let env_path = write_runtime_env(
        &pack_root,
        &ledger,
        &validated.gateway,
        &validated.sidecar,
        false,
    )?;
    assert_watch_off_in_env_file(&env_path)?;

    // Refuse if something else owns :18080 without our project.
    if port_busy_by_foreign_gateway()? {
        return Err(
            "port 18080 is in use by a process outside irin-desktop-gateway; \
             stop the foreign Gateway or free the port. The desktop pack will not replace it."
                .to_string(),
        );
    }

    let compose = compose_file(&pack_root);
    let up = compose_command(
        &compose,
        Some(&env_path),
        &["up", "-d", "--remove-orphans", "--wait"],
    )?;
    if !up.status.success() {
        // --wait may not exist on older compose; retry without it.
        let up2 = compose_command(&compose, Some(&env_path), &["up", "-d", "--remove-orphans"])?;
        if !up2.status.success() {
            return Err(format_cmd_failure("gateway pack up", &up2));
        }
    }

    // Wait for edge health and sidecar-backed admin surface (not mere URL reachability).
    let mut ready = false;
    for _ in 0..60 {
        if gateway_health_ok() && admin_surface_ready() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !ready {
        return Err(
            "gateway pack started but authenticated control plane is not ready \
             (/health + /admin/keys not accepting requests)"
                .to_string(),
        );
    }

    // Fail-closed: unauthenticated /v1/models must not succeed.
    if !models_fail_closed_without_key() {
        return Err("gateway /v1/models did not fail closed without a client key".to_string());
    }

    // Reuse existing Keychain key if still valid.
    let existing = load_gw_api_key(store)?;
    let key_id = if let Some(ref k) = existing {
        if models_authenticated(k) {
            load_or_create_private_config()?
                .gateway_key_id
                .unwrap_or_else(|| "existing".into())
        } else {
            provision_council_client(store)?
        }
    } else {
        provision_council_client(store)?
    };

    // Mark enabled (non-secret).
    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = true;
    cfg.gateway_key_id = Some(key_id);
    cfg.gateway_pack_version = Some(validated.pack_version.clone());
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;

    // Assert private.json has no raw key material.
    assert_private_json_has_no_raw_key()?;

    Ok(gateway_pack_status(store))
}

fn assert_private_json_has_no_raw_key() -> Result<(), String> {
    let path = crate::private_config::private_config_path();
    if !path.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    if raw.contains("gw_") && raw.contains("raw") {
        return Err("private.json appears to contain raw key material".to_string());
    }
    // Stricter: no gw_ + 32 hex
    if raw.contains("gw_") {
        for part in raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if is_valid_gw_raw_key(part) {
                return Err("private.json must never contain the raw GW_API_KEY".to_string());
            }
        }
    }
    Ok(())
}

fn port_busy_by_foreign_gateway() -> Result<bool, String> {
    if desktop_project_running() {
        return Ok(false);
    }
    // If health answers but our project is not running, something else owns the port.
    if gateway_health_ok() {
        return Ok(true);
    }
    // TCP connect without HTTP.
    use std::net::TcpStream;
    match TcpStream::connect_timeout(
        &"127.0.0.1:18080".parse().unwrap(),
        Duration::from_millis(200),
    ) {
        Ok(_) => Ok(true), // something listens, not our project
        Err(_) => Ok(false),
    }
}

/// Disable governed mode: flip private config, do not delete pack data/Keychain.
pub fn disable_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = false;
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    let _ = store; // key retained for re-enable
    Ok(gateway_pack_status(store))
}

/// Stop desktop compose project only. Never targets foreign projects.
pub fn stop_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    if let Some(pack_root) = installed_pack_root() {
        let compose = compose_file(&pack_root);
        if compose.is_file() {
            let env = runtime_env_path();
            let env_arg = env.is_file().then_some(env.as_path());
            let out = compose_command(&compose, env_arg, &["stop"])?;
            if !out.status.success() {
                // Fallback: docker compose -p down without volumes
                let out2 = compose_command(&compose, env_arg, &["down", "--remove-orphans"])?;
                if !out2.status.success() {
                    return Err(format_cmd_failure("gateway pack stop", &out2));
                }
            }
        }
    }
    Ok(gateway_pack_status(store))
}

/// Destructive uninstall: only irin-desktop-gateway project + app-owned gateway dir + Keychain item.
pub fn uninstall_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    if let Some(pack_root) = installed_pack_root() {
        let compose = compose_file(&pack_root);
        if compose.is_file() {
            let env = runtime_env_path();
            let env_arg = env.is_file().then_some(env.as_path());
            // down -v only for our project (compose_command pins -p).
            let out = compose_command(
                &compose,
                env_arg,
                &["down", "--volumes", "--remove-orphans"],
            )?;
            if !out.status.success() {
                return Err(format_cmd_failure("gateway pack uninstall down", &out));
            }
        }
    }
    let _ = delete_gw_api_key(store);
    let dir = gateway_data_dir();
    if dir.is_dir() {
        fs::remove_dir_all(&dir).map_err(|e| format!("remove gateway data dir: {e}"))?;
    }
    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = false;
    cfg.gateway_key_id = None;
    cfg.gateway_pack_version = None;
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    Ok(gateway_pack_status(store))
}

/// Default Keychain-backed store for production commands.
#[allow(dead_code)]
pub fn default_secret_store() -> KeychainSecretStore {
    KeychainSecretStore
}

/// Child env injection when Council should use Gateway.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GatewayChildEnv {
    pub api_key: String,
    pub gateway_url: String,
}

/// Load Keychain key for child injection only when pack is authenticated-ready and enabled.
#[allow(dead_code)]
pub fn gateway_child_env_if_ready(store: &dyn SecretStore) -> Result<Option<GatewayChildEnv>, String> {
    let st = gateway_pack_status(store);
    if !st.enabled || st.state != GatewayPackState::AuthenticatedReady {
        return Ok(None);
    }
    let key = load_gw_api_key(store)?
        .ok_or_else(|| "GW_API_KEY missing from Keychain".to_string())?;
    Ok(Some(GatewayChildEnv {
        api_key: key,
        gateway_url: DESKTOP_GATEWAY_URL.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemorySecretStore;
    use crate::private_config::test_env_lock;

    #[test]
    fn state_ready_allows_governed_only() {
        assert!(GatewayPackState::AuthenticatedReady.allows_governed());
        assert!(!GatewayPackState::InstalledStopped.allows_governed());
        assert!(!GatewayPackState::DockerMissing.allows_governed());
        assert!(!GatewayPackState::Degraded.allows_governed());
    }

    #[test]
    fn project_constant() {
        assert_eq!(DESKTOP_COMPOSE_PROJECT, "irin-desktop-gateway");
        assert_eq!(
            crate::keychain::KEYCHAIN_SERVICE,
            "com.sovereign.council.warroom"
        );
    }

    #[test]
    fn gateway_dir_permissions_and_atomic_files() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "gw-pack-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        let dir = ensure_gateway_dir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "gateway dir mode {mode:o}");
        }
        let ledger = ensure_ledger_key().unwrap();
        assert_eq!(fs::metadata(&ledger).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&ledger).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn private_json_rejects_raw_key_detection() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "gw-pack-pj-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("Library/Application Support/com.sovereign.council.warroom"))
            .unwrap();
        std::env::set_var("HOME", &tmp);
        let path = crate::private_config::private_config_path();
        fs::write(
            &path,
            r#"{"version":1,"install_id":"x","created_unix":1,"auth_token":"","via_gateway_default":false,"gateway_key_id":"k_abcdef12"}
"#,
        )
        .unwrap();
        assert!(assert_private_json_has_no_raw_key().is_ok());
        fs::write(
            &path,
            r#"{"version":1,"install_id":"x","created_unix":1,"gw":"gw_0123456789abcdef0123456789abcdef"}
"#,
        )
        .unwrap();
        assert!(assert_private_json_has_no_raw_key().is_err());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn status_docker_missing_is_actionable() {
        // When docker is missing, status should not claim ready.
        // We cannot force CLI missing if docker is installed on this machine;
        // instead unit-check the state enum contract.
        let st = GatewayPackStatus::base(
            GatewayPackState::DockerMissing,
            "Docker CLI not found",
        );
        assert!(!st.state.allows_governed());
        assert!(!st.authenticated);
    }

    #[test]
    fn memory_store_status_without_pack() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "gw-pack-st-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);
        let store = MemorySecretStore::default();
        let st = gateway_pack_status(&store);
        assert!(
            matches!(
                st.state,
                GatewayPackState::NotInstalled
                    | GatewayPackState::DockerMissing
                    | GatewayPackState::DockerDaemonDown
                    | GatewayPackState::InstalledStopped
                    | GatewayPackState::Disabled
            ),
            "unexpected {:?}",
            st.state
        );
        assert!(!st.state.allows_governed());
        assert!(!st.authenticated);
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
