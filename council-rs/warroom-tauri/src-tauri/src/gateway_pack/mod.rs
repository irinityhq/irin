//! Installed-release Gateway Pack lifecycle.
//!
//! Privileged native boundary: Docker CLI allow-list, fixed compose project,
//! app-owned paths, Keychain-held GW_API_KEY + AUTH_PEPPER. Renderer only
//! receives non-secret status and triggers fixed workflows.
//!
//! Concurrent lifecycle commands are serialized. Authenticated-ready requires
//! both Gateway auth proof and an owned Council child in the requested route.

pub mod manifest;

use crate::docker_cli::{
    compose_command_with_env, docker_command, format_cmd_failure, path_is_safe_argv,
    probe_docker_daemon, resolve_docker_cli, ComposeEnv, DockerDaemonState, DockerErrorKind,
    DESKTOP_COMPOSE_PROJECT, DESKTOP_GATEWAY_URL, DOCKER_CMD_TIMEOUT, DOCKER_COMPOSE_UP_TIMEOUT,
};
use crate::keychain::{
    delete_all_gateway_pack_secrets, gw_api_key_present, is_valid_gw_raw_key, load_auth_pepper,
    load_gw_api_key, store_auth_pepper, store_gw_api_key, KeychainSecretStore, SecretStore,
};
use crate::paths::{bundled_base_dir, executable_dir};
use crate::private_config::{
    app_support_dir, gui_login_environment, load_or_create_private_config, write_private_config_at,
};
use manifest::{
    image_config_id_matches_ref, load_manifest, repo_digests_match_ref, validate_manifest,
    ImageRef, ManifestMode, ValidatedManifest,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Global lifecycle lock — enable/disable/stop/uninstall must not interleave.
static LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

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
    /// True when Gateway is up, key authenticates, and Council is expected governed.
    pub council_governed: bool,
    /// Distinct from authenticated: URL field present / pack project known.
    pub gateway_url_configured: bool,
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
            council_governed: false,
            gateway_url_configured: true, // fixed loopback URL is always the pack target
            support_matrix_summary: SUPPORT_MATRIX_SUMMARY.to_string(),
        }
    }
}

pub const SUPPORT_MATRIX_SUMMARY: &str = "\
v0.1: API-key providers (xAI/OpenAI/Anthropic/NVIDIA) when present in login env; \
Vertex Direct-only (no gcloud mount); Claude/Codex CLI proxies not supported; \
Watch producer/dispatcher forced off.";

const GATEWAY_DIR_NAME: &str = "gateway";
const PUBLIC_ENV_NAME: &str = "compose.public.env";
const LEDGER_KEY_NAME: &str = "ledger_key";
const INSTALLED_MARKER: &str = "pack-installed.json";
const PACK_DIR_NAME: &str = "pack";

/// Fixed Application Support gateway directory (0700).
pub fn gateway_data_dir() -> PathBuf {
    app_support_dir().join(GATEWAY_DIR_NAME)
}

pub fn public_env_path() -> PathBuf {
    gateway_data_dir().join(PUBLIC_ENV_NAME)
}

/// Legacy path — never write secrets here; removed on install/uninstall.
pub fn runtime_env_path() -> PathBuf {
    gateway_data_dir().join("runtime.env")
}

pub fn ledger_key_path() -> PathBuf {
    gateway_data_dir().join(LEDGER_KEY_NAME)
}

pub fn installed_marker_path() -> PathBuf {
    gateway_data_dir().join(INSTALLED_MARKER)
}

/// Bundled pack root under app Resources (or test override).
/// Bundled assets alone do **not** mean installed — see [`is_pack_installed`].
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
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("gateway-pack");
    if dev.join("docker-compose.yml").is_file() {
        return Some(dev);
    }
    let repo_pack = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packaging/gateway-pack");
    if repo_pack.join("docker-compose.yml").is_file() {
        return Some(repo_pack.canonicalize().unwrap_or(repo_pack));
    }
    let _ = bundled_base_dir();
    None
}

/// True only when Application Support has a validated install marker + pack root.
pub fn is_pack_installed() -> bool {
    installed_marker_path().is_file()
        && gateway_data_dir()
            .join(PACK_DIR_NAME)
            .join("docker-compose.yml")
            .is_file()
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
    Ok(path)
}

fn getrandom_fill(buf: &mut [u8]) -> Result<(), String> {
    use std::fs::File;
    use std::io::Read;
    let mut f = File::open("/dev/urandom").map_err(|e| format!("open urandom: {e}"))?;
    f.read_exact(buf).map_err(|e| format!("read urandom: {e}"))
}

fn random_hex(n_bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; n_bytes];
    getrandom_fill(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Reject CR/LF/NUL and other injection-prone characters in env values.
pub fn validate_env_value(key: &str, value: &str) -> Result<(), String> {
    if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return Err(format!(
            "env value for {key} contains forbidden CR/LF/NUL"
        ));
    }
    if value.contains('\0') {
        return Err(format!("env value for {key} contains NUL"));
    }
    Ok(())
}

/// Serialize a public (non-secret) compose env file. Keys unique; values validated.
pub fn serialize_public_env(pairs: &[(String, String)]) -> Result<String, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut body = String::new();
    for (k, v) in pairs {
        if !seen.insert(k.clone()) {
            return Err(format!("duplicate env key refused: {k}"));
        }
        if k.is_empty()
            || k.bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
        {
            return Err(format!("invalid env key: {k}"));
        }
        validate_env_value(k, v)?;
        // Quote values that need it (spaces); otherwise plain KEY=value.
        if v.chars()
            .any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '\'')
        {
            let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
            body.push_str(&format!("{k}=\"{escaped}\"\n"));
        } else {
            body.push_str(k);
            body.push('=');
            body.push_str(v);
            body.push('\n');
        }
    }
    Ok(body)
}

fn write_public_compose_env(
    pack_root: &Path,
    ledger: &Path,
    gateway_image: &ImageRef,
    sidecar_image: &ImageRef,
    key_id: Option<&str>,
) -> Result<PathBuf, String> {
    if !path_is_safe_argv(pack_root) || !path_is_safe_argv(ledger) {
        return Err("pack root or ledger path rejected".to_string());
    }
    ensure_gateway_dir()?;
    let mut pairs = vec![
        (
            "IRIN_GATEWAY_IMAGE".into(),
            gateway_image.as_str().to_string(),
        ),
        (
            "IRIN_SIDECAR_IMAGE".into(),
            sidecar_image.as_str().to_string(),
        ),
        (
            "IRIN_DESKTOP_PACK_ROOT".into(),
            pack_root.display().to_string(),
        ),
        (
            "IRIN_DESKTOP_LEDGER_KEY".into(),
            ledger.display().to_string(),
        ),
        ("GATEWAY_DURABLE".into(), "1".into()),
        ("GATEWAY_AUTH_FAIL_CLOSED".into(), "true".into()),
        ("SIDECAR_SOCKET_MODE".into(), "0660".into()),
        ("SIDECAR_SOCKET_GID".into(), "9999".into()),
        ("GW_ENABLE_COUNCIL_ENDPOINT".into(), "0".into()),
        (
            "COUNCIL_BASE_URL".into(),
            "http://host.docker.internal:8765".into(),
        ),
        ("WATCH_PRODUCER_ENABLED".into(), "false".into()),
        ("WATCH_DISPATCHER_ENABLED".into(), "false".into()),
        ("WATCH_CANARY_TENANT".into(), "canary".into()),
        ("DAILY_SPEND_CAP_USD".into(), "25".into()),
        ("WATCH_MAX_FANOUT_COST_USD".into(), "2.50".into()),
        // Surfaces disabled — never generate WATCH_ADMIN_TOKEN / COUNCIL_GATEWAY_TOKEN.
        ("BOOTSTRAP_TOKEN".into(), "".into()),
    ];
    if let Some(kid) = key_id {
        if !kid.is_empty() {
            validate_env_value("COUNCIL_GATEWAY_KEY_ID", kid)?;
            pairs.push(("COUNCIL_GATEWAY_KEY_ID".into(), kid.to_string()));
        }
    }
    let body = serialize_public_env(&pairs)?;
    let path = public_env_path();
    write_atomic_0600(&path, body.as_bytes())?;
    // Scrub any legacy secret-bearing runtime.env if present.
    let legacy = runtime_env_path();
    if legacy.is_file() {
        let _ = fs::remove_file(&legacy);
    }
    Ok(path)
}

/// Build process env for compose: secrets + providers. Never written to disk.
fn build_compose_secret_env(
    store: &dyn SecretStore,
    bootstrap: Option<&str>,
) -> Result<ComposeEnv, String> {
    let mut env = ComposeEnv::new();
    let pepper = match load_auth_pepper(store).map_err(|e| format!("keychain load pepper: {e}"))? {
        Some(p) => p,
        None => {
            let p = random_hex(32)?;
            store_auth_pepper(store, &p).map_err(|e| format!("keychain store pepper: {e}"))?;
            p
        }
    };
    validate_env_value("AUTH_PEPPER", &pepper)?;
    env.insert("AUTH_PEPPER".into(), pepper);

    if let Some(bs) = bootstrap {
        validate_env_value("BOOTSTRAP_TOKEN", bs)?;
        env.insert("BOOTSTRAP_TOKEN".into(), bs.to_string());
    } else {
        env.insert("BOOTSTRAP_TOKEN".into(), String::new());
    }

    // Provider keys from login/process only — never persisted to app env file.
    // Skip gui_login_environment when IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV=1 (tests).
    let login = if std::env::var_os("IRIN_GATEWAY_PACK_SKIP_LOGIN_ENV").is_some() {
        Vec::new()
    } else {
        gui_login_environment()
    };
    for key in ["XAI_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "NVIDIA_API_KEY"] {
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
            .unwrap_or_default();
        // Provider keys are optional for pack start. Skip any value that fails
        // injection validation (CR/LF/NUL) rather than aborting Enable — pack
        // auth still works; only that provider route is empty.
        let safe = if val.is_empty() {
            String::new()
        } else if validate_env_value(key, &val).is_ok() {
            val
        } else {
            String::new()
        };
        env.insert(key.to_string(), safe);
    }
    Ok(env)
}

/// Stage bundled pack assets to a sibling temp dir, validate, then atomically swap.
/// Preserves operator data (ledger, Keychain, volumes) outside the pack/ tree.
pub fn install_pack_files() -> Result<PathBuf, String> {
    let src = bundled_pack_root().ok_or_else(|| {
        "Gateway Pack is not bundled in this app. Rebuild the DMG with stage-gateway-pack.sh."
            .to_string()
    })?;
    let gw = ensure_gateway_dir()?;
    let final_dest = gw.join(PACK_DIR_NAME);
    let stage = gw.join(format!(
        ".pack-stage-{}.tmp",
        std::process::id()
    ));
    let backup = gw.join(format!(".pack-backup-{}.tmp", std::process::id()));

    // Clean leftover stage dirs.
    let _ = fs::remove_dir_all(&stage);
    let _ = fs::remove_dir_all(&backup);

    copy_dir_recursive(&src, &stage).map_err(|e| format!("stage pack files: {e}"))?;

    // Validate complete assets before swap.
    let compose = stage.join("docker-compose.yml");
    let manifest_path = stage.join("image-manifest.json");
    if !compose.is_file() {
        let _ = fs::remove_dir_all(&stage);
        return Err("staged pack missing docker-compose.yml".to_string());
    }
    if !manifest_path.is_file() {
        let _ = fs::remove_dir_all(&stage);
        return Err("staged pack missing image-manifest.json".to_string());
    }
    let validated = {
        let m = load_manifest(&manifest_path).map_err(|e| {
            let _ = fs::remove_dir_all(&stage);
            e
        })?;
        validate_manifest(&m).map_err(|e| {
            let _ = fs::remove_dir_all(&stage);
            e
        })?
    };
    // Require nginx + conf + lua for a complete pack.
    for rel in ["nginx.conf", "conf", "lua"] {
        let p = stage.join(rel);
        if !p.exists() {
            let _ = fs::remove_dir_all(&stage);
            return Err(format!("staged pack missing {rel}"));
        }
    }

    // Atomic swap: final -> backup, stage -> final, drop backup.
    if final_dest.exists() {
        fs::rename(&final_dest, &backup).map_err(|e| {
            let _ = fs::remove_dir_all(&stage);
            format!("pack swap backup failed: {e}")
        })?;
    }
    if let Err(e) = fs::rename(&stage, &final_dest) {
        // Roll back.
        if backup.exists() {
            let _ = fs::rename(&backup, &final_dest);
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(format!("pack swap failed: {e}"));
    }
    let _ = fs::remove_dir_all(&backup);

    let marker = serde_json::json!({
        "installed": true,
        "pack_version": validated.pack_version,
        "manifest_mode": validated.mode.as_str(),
        "project": DESKTOP_COMPOSE_PROJECT,
        "source_sha": validated.source_sha,
    });
    write_atomic_0600(
        &installed_marker_path(),
        format!("{marker}\n").as_bytes(),
    )?;
    Ok(final_dest)
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

/// Installed pack root only — never falls back to bundled Resources as "installed".
pub fn installed_pack_root() -> Option<PathBuf> {
    if !is_pack_installed() {
        return None;
    }
    let p = gateway_data_dir().join(PACK_DIR_NAME);
    if p.join("docker-compose.yml").is_file() {
        Some(p)
    } else {
        None
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
    match v.mode {
        ManifestMode::LocalDev => verify_images_local_dev(v),
        ManifestMode::Production => verify_images_production(v),
    }
}

fn verify_images_local_dev(v: &ValidatedManifest) -> Result<(), String> {
    for (label, image_ref) in [("gateway", &v.gateway), ("sidecar", &v.sidecar)] {
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
                if !image_config_id_matches_ref(&id, image_ref) {
                    // Also accept local_tags / image_ids from manifest.
                    if let Some(expected_id) = v.local_image_ids.get(label) {
                        if image_config_id_matches_ref(&id, image_ref)
                            || id == *expected_id
                            || id.strip_prefix("sha256:") == expected_id.strip_prefix("sha256:")
                        {
                            continue;
                        }
                    }
                    return Err(format!(
                        "{label}: {}",
                        DockerErrorKind::ImageDigestMismatch.operator_message()
                    ));
                }
            }
            Ok(o) => {
                let id_ref = format!("sha256:{}", image_ref.digest_hex());
                let out2 = docker_command(&["image", "inspect", "--format", "{{.Id}}", &id_ref]);
                match out2 {
                    Ok(o2) if o2.status.success() => {
                        let id = String::from_utf8_lossy(&o2.stdout).trim().to_string();
                        if !image_config_id_matches_ref(&id, image_ref) {
                            return Err(format!(
                                "{label}: {}",
                                DockerErrorKind::ImageDigestMismatch.operator_message()
                            ));
                        }
                    }
                    _ => {
                        return Err(format!(
                            "{label} image not present for local-dev. \
                             Run scripts/build-gateway-pack-dev-images.sh. {}",
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

fn verify_images_production(v: &ValidatedManifest) -> Result<(), String> {
    for (label, image_ref) in [("gateway", &v.gateway), ("sidecar", &v.sidecar)] {
        // Pull/resolve exact name@sha256 registry digest.
        let pull = docker_command(&["pull", image_ref.digest_ref()]);
        match pull {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return Err(format!(
                    "{label} production pull failed. {}",
                    format_cmd_failure("image pull", &o)
                ));
            }
            Err(e) => return Err(e),
        }

        let out = docker_command(&[
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            image_ref.digest_ref(),
        ]);
        match out {
            Ok(o) if o.status.success() => {
                let raw = String::from_utf8_lossy(&o.stdout);
                // RepoDigests JSON array → join as lines for matcher.
                let digests = raw
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .replace('"', "")
                    .replace(',', "\n");
                if !repo_digests_match_ref(&digests, image_ref) {
                    // Also try newline format from Go template alternative.
                    let out2 = docker_command(&[
                        "image",
                        "inspect",
                        "--format",
                        "{{range .RepoDigests}}{{println .}}{{end}}",
                        image_ref.digest_ref(),
                    ]);
                    let digests2 = out2
                        .ok()
                        .map(|o2| String::from_utf8_lossy(&o2.stdout).to_string())
                        .unwrap_or_default();
                    if !repo_digests_match_ref(&digests2, image_ref)
                        && !repo_digests_match_ref(&raw, image_ref)
                    {
                        return Err(format!(
                            "{label}: production RepoDigests do not contain expected registry digest \
                             (config Id matching is not accepted in production mode)"
                        ));
                    }
                }
            }
            Ok(o) => {
                return Err(format!(
                    "{label}: {}",
                    format_cmd_failure("image inspect RepoDigests", &o)
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn compose_file(pack_root: &Path) -> PathBuf {
    pack_root.join("docker-compose.yml")
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
        Err(_) => true,
    }
}

/// Provision Council service-role client via real admin API. Raw key → Keychain only.
/// `bootstrap` is held only in memory / compose process env for this call.
fn provision_council_client(store: &dyn SecretStore, bootstrap: &str) -> Result<String, String> {
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
        return Err("provision response key_id has invalid shape".to_string());
    }

    store_gw_api_key(store, raw_key)?;
    let mut cfg = load_or_create_private_config()?;
    cfg.gateway_key_id = Some(key_id.to_string());
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;

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
            if is_pack_installed() {
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
            // Bundled resources may exist; still not_installed until marker.
            st.state = GatewayPackState::NotInstalled;
            if bundled_pack_root().is_some() {
                st.message = "Gateway Pack assets are bundled but not installed. Use Enable Gateway to install into Application Support.".into();
            }
            return st;
        }
    };

    if let Ok(v) = load_validated_manifest(&pack_root) {
        st.pack_version = Some(v.pack_version.clone());
        st.manifest_mode = Some(v.mode.as_str().to_string());
    }

    let running = desktop_project_running();
    let health = gateway_health_ok();

    let key = load_gw_api_key(store).ok().flatten();
    let authenticated = key.as_ref().map(|k| models_authenticated(k)).unwrap_or(false);
    st.authenticated = authenticated;

    // Council governed: enabled flag + authenticated + pack healthy.
    // Exact child PID proof is supplied by the enable path; status is best-effort.
    st.council_governed = st.enabled && authenticated && health && running;

    if !running {
        if cfg.as_ref().map(|c| c.via_gateway_default) == Some(true) {
            st.state = GatewayPackState::Degraded;
            st.message = "Gateway was enabled but the pack is not running. Start the pack or Disable Gateway for Direct mode.".into();
            st.council_governed = false;
        } else if is_pack_installed() {
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
        st.council_governed = false;
        return st;
    }

    if authenticated && st.enabled {
        st.state = GatewayPackState::AuthenticatedReady;
        st.message = "Gateway Pack is authenticated and ready for governed proceedings.".into();
        st.council_governed = true;
    } else if authenticated && !st.enabled {
        st.state = GatewayPackState::Disabled;
        st.message = "Gateway is up with a stored key, but governed mode is disabled (Direct).".into();
        st.council_governed = false;
    } else if key.is_some() {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway is up but the stored client key failed /v1/models. Re-run Enable Gateway to re-provision.".into();
        st.council_governed = false;
    } else {
        st.state = GatewayPackState::Degraded;
        st.message = "Gateway is up but no client key is in Keychain. Run Enable Gateway to provision.".into();
        st.council_governed = false;
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

fn compose_up(
    compose: &Path,
    env_path: &Path,
    secret_env: &ComposeEnv,
) -> Result<(), String> {
    let up = compose_command_with_env(
        compose,
        Some(env_path),
        &["up", "-d", "--remove-orphans", "--wait"],
        Some(secret_env),
        DOCKER_COMPOSE_UP_TIMEOUT,
    )?;
    if up.status.success() {
        return Ok(());
    }
    let up2 = compose_command_with_env(
        compose,
        Some(env_path),
        &["up", "-d", "--remove-orphans", "--force-recreate"],
        Some(secret_env),
        DOCKER_COMPOSE_UP_TIMEOUT,
    )?;
    if !up2.status.success() {
        return Err(format_cmd_failure("gateway pack up", &up2));
    }
    Ok(())
}

/// Append a single non-secret lifecycle stage line for operator/smoke diagnosis.
/// Never logs values, keys, paths that may contain credentials, or command output.
fn lifecycle_stage(stage: &str, detail: &str) {
    let dir = gateway_data_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("lifecycle.log");
    let line = format!(
        "{} stage={} detail={}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        stage,
        detail
    );
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Full enable workflow. Returns ready only when Gateway auth is proven.
/// Caller (lib.rs) must restart Council into governed mode and treat restart
/// failure as overall failure (not ready).
pub fn enable_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;
    lifecycle_stage("enable_begin", "ok");

    match probe_docker_daemon() {
        DockerDaemonState::CliMissing => {
            lifecycle_stage("enable_abort", "docker_cli_missing");
            return Ok(gateway_pack_status(store));
        }
        DockerDaemonState::DaemonDown => {
            lifecycle_stage("enable_abort", "docker_daemon_down");
            return Ok(gateway_pack_status(store));
        }
        DockerDaemonState::Ready => {}
    }
    let _ = resolve_docker_cli()?;
    lifecycle_stage("docker_ready", "ok");

    let pack_root = install_pack_files().map_err(|e| {
        lifecycle_stage("install_pack", "error");
        e
    })?;
    lifecycle_stage("install_pack", "ok");
    let validated = load_validated_manifest(&pack_root).map_err(|e| {
        lifecycle_stage("manifest", "error");
        e
    })?;
    verify_images_present(&validated).map_err(|e| {
        lifecycle_stage("verify_images", "error");
        e
    })?;
    lifecycle_stage("verify_images", "ok");

    let ledger = ensure_ledger_key().map_err(|e| {
        lifecycle_stage("ledger", "error");
        e
    })?;
    lifecycle_stage("ledger", "ok");
    let existing_key_id = load_or_create_private_config()?.gateway_key_id;
    let env_path = write_public_compose_env(
        &pack_root,
        &ledger,
        &validated.gateway,
        &validated.sidecar,
        existing_key_id.as_deref(),
    )
    .map_err(|e| {
        lifecycle_stage("public_env", "error");
        e
    })?;
    lifecycle_stage("public_env", "ok");

    if port_busy_by_foreign_gateway()? {
        lifecycle_stage("port_check", "foreign_busy");
        return Err(
            "port 18080 is in use by a process outside irin-desktop-gateway; \
             stop the foreign Gateway or free the port. The desktop pack will not replace it."
                .to_string(),
        );
    }
    lifecycle_stage("port_check", "ok");

    let compose = compose_file(&pack_root);

    // Reuse existing Keychain key if still valid after start; else provision with bootstrap.
    let existing = load_gw_api_key(store).map_err(|e| {
        lifecycle_stage("keychain_load", "error");
        e
    })?;
    lifecycle_stage(
        "keychain_load",
        if existing.is_some() {
            "present"
        } else {
            "absent"
        },
    );
    let need_provision = match existing.as_ref() {
        Some(k) => {
            // Start without bootstrap first if we might already be provisioned.
            let secret_env = build_compose_secret_env(store, None).map_err(|e| {
                lifecycle_stage("secret_env", "error");
                e
            })?;
            lifecycle_stage("secret_env", "ok");
            compose_up(&compose, &env_path, &secret_env).map_err(|e| {
                lifecycle_stage("compose_up_existing", "error");
                e
            })?;
            lifecycle_stage("compose_up_existing", "ok");
            wait_control_plane().map_err(|e| {
                lifecycle_stage("wait_control_plane", "error");
                e
            })?;
            lifecycle_stage("wait_control_plane", "ok");
            !models_authenticated(k)
        }
        None => true,
    };
    lifecycle_stage(
        "need_provision",
        if need_provision { "true" } else { "false" },
    );

    let key_id = if need_provision {
        // Generate bootstrap only for provisioning.
        let bootstrap = random_hex(32)?;
        let secret_env = build_compose_secret_env(store, Some(&bootstrap)).map_err(|e| {
            // Fixed non-secret categories only — never log the error body if it
            // could include env material. Classify known prefixes.
            let cat = if e.contains("keychain") {
                "keychain_error"
            } else if e.contains("env value") || e.contains("forbidden") {
                "env_validate_error"
            } else {
                "secret_env_error"
            };
            lifecycle_stage("secret_env_bootstrap", cat);
            e
        })?;
        lifecycle_stage("secret_env_bootstrap", "ok");
        compose_up(&compose, &env_path, &secret_env).map_err(|e| {
            lifecycle_stage("compose_up_bootstrap", "error");
            e
        })?;
        lifecycle_stage("compose_up_bootstrap", "ok");
        wait_control_plane().map_err(|e| {
            lifecycle_stage("wait_control_plane_bootstrap", "error");
            e
        })?;
        lifecycle_stage("wait_control_plane_bootstrap", "ok");
        if !models_fail_closed_without_key() {
            lifecycle_stage("models_fail_closed", "error");
            return Err("gateway /v1/models did not fail closed without a client key".to_string());
        }
        lifecycle_stage("models_fail_closed", "ok");
        let kid = provision_council_client(store, &bootstrap).map_err(|e| {
            lifecycle_stage("provision", "error");
            e
        })?;
        lifecycle_stage("provision", "ok");
        // Blank bootstrap and recreate sidecar without it.
        let secret_env_blank = build_compose_secret_env(store, None).map_err(|e| {
            lifecycle_stage("secret_env_blank", "error");
            e
        })?;
        write_public_compose_env(
            &pack_root,
            &ledger,
            &validated.gateway,
            &validated.sidecar,
            Some(&kid),
        )?;
        compose_up(&compose, &env_path, &secret_env_blank).map_err(|e| {
            lifecycle_stage("compose_up_blank", "error");
            e
        })?;
        wait_control_plane().map_err(|e| {
            lifecycle_stage("wait_control_plane_blank", "error");
            e
        })?;
        kid
    } else {
        if !models_fail_closed_without_key() {
            lifecycle_stage("models_fail_closed", "error");
            return Err("gateway /v1/models did not fail closed without a client key".to_string());
        }
        existing_key_id.unwrap_or_else(|| "existing".into())
    };

    // Confirm auth after provision path.
    let key = load_gw_api_key(store)?
        .ok_or_else(|| "GW_API_KEY missing from Keychain after enable".to_string())?;
    if !models_authenticated(&key) {
        lifecycle_stage("models_auth", "error");
        return Err("Gateway client key failed /v1/models after enable".to_string());
    }
    lifecycle_stage("models_auth", "ok");

    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = true;
    cfg.gateway_key_id = Some(key_id);
    cfg.gateway_pack_version = Some(validated.pack_version.clone());
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    assert_private_json_has_no_raw_key()?;
    lifecycle_stage("enable_complete", "authenticated");

    let mut st = gateway_pack_status(store);
    // Not fully ready until Council restart succeeds — lib marks council_governed.
    if st.authenticated && st.enabled {
        st.state = GatewayPackState::AuthenticatedReady;
        st.message = "Gateway Pack authenticated. Council restart required for governed mode.".into();
    }
    Ok(st)
}

fn wait_control_plane() -> Result<(), String> {
    for _ in 0..60 {
        if gateway_health_ok() && admin_surface_ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(
        "gateway pack started but authenticated control plane is not ready \
         (/health + /admin/keys not accepting requests)"
            .to_string(),
    )
}

fn assert_private_json_has_no_raw_key() -> Result<(), String> {
    let path = crate::private_config::private_config_path();
    if !path.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
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
    if gateway_health_ok() {
        return Ok(true);
    }
    use std::net::TcpStream;
    match TcpStream::connect_timeout(
        &"127.0.0.1:18080".parse().unwrap(),
        Duration::from_millis(200),
    ) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Disable governed mode: flip private config. Does not delete pack data/Keychain.
pub fn disable_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;
    let mut cfg = load_or_create_private_config()?;
    cfg.via_gateway_default = false;
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;
    let _ = store;
    Ok(gateway_pack_status(store))
}

/// Stop desktop compose project only after Direct mode is recorded.
/// Refuses if still enabled (via_gateway_default) — caller must disable first,
/// or we auto-disable then stop so Council is not left governed against a dead Gateway.
pub fn stop_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    lifecycle_stage("stop_begin", "ok");
    let _guard = match LIFECYCLE_LOCK.lock() {
        Ok(guard) => guard,
        Err(_) => {
            lifecycle_stage("stop_lock", "error");
            return Err("gateway pack lifecycle lock poisoned".to_string());
        }
    };
    lifecycle_stage("stop_lock", "ok");

    let mut cfg = match load_or_create_private_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            lifecycle_stage("stop_config", "error");
            return Err(err);
        }
    };
    if cfg.via_gateway_default {
        // Switch to Direct first so we never leave enabled Council against stopped Gateway.
        cfg.via_gateway_default = false;
        if let Err(err) =
            write_private_config_at(&crate::private_config::private_config_path(), &cfg)
        {
            lifecycle_stage("stop_config", "error");
            return Err(err);
        }
    }
    lifecycle_stage("stop_config", "direct");

    if let Some(pack_root) = installed_pack_root() {
        let compose = compose_file(&pack_root);
        if compose.is_file() {
            let env = public_env_path();
            let env_arg = env.is_file().then_some(env.as_path());
            let secret_env = build_compose_secret_env(store, None).unwrap_or_default();
            lifecycle_stage("stop_compose", "begin");
            let out = match compose_command_with_env(
                &compose,
                env_arg,
                &["stop"],
                Some(&secret_env),
                DOCKER_CMD_TIMEOUT,
            ) {
                Ok(out) => out,
                Err(err) => {
                    lifecycle_stage("stop_compose", "error");
                    return Err(err);
                }
            };
            if !out.status.success() {
                lifecycle_stage("stop_compose", "nonzero");
                lifecycle_stage("stop_down", "begin");
                let out2 = match compose_command_with_env(
                    &compose,
                    env_arg,
                    &["down", "--remove-orphans"],
                    Some(&secret_env),
                    DOCKER_CMD_TIMEOUT,
                ) {
                    Ok(out) => out,
                    Err(err) => {
                        lifecycle_stage("stop_down", "error");
                        return Err(err);
                    }
                };
                if !out2.status.success() {
                    lifecycle_stage("stop_down", "nonzero");
                    return Err(format_cmd_failure("gateway pack stop", &out2));
                }
                lifecycle_stage("stop_down", "ok");
            } else {
                lifecycle_stage("stop_compose", "ok");
            }
        }
    }
    let status = gateway_pack_status(store);
    lifecycle_stage("stop_complete", "ok");
    Ok(status)
}

/// Destructive uninstall: only irin-desktop-gateway project + app-owned gateway dir + Keychain items.
pub fn uninstall_gateway_pack(store: &dyn SecretStore) -> Result<GatewayPackStatus, String> {
    let _guard = LIFECYCLE_LOCK
        .lock()
        .map_err(|_| "gateway pack lifecycle lock poisoned".to_string())?;

    if let Some(pack_root) = installed_pack_root() {
        let compose = compose_file(&pack_root);
        if compose.is_file() {
            let env = public_env_path();
            let env_arg = env.is_file().then_some(env.as_path());
            let secret_env = build_compose_secret_env(store, None).unwrap_or_default();
            let out = compose_command_with_env(
                &compose,
                env_arg,
                &["down", "--volumes", "--remove-orphans"],
                Some(&secret_env),
                DOCKER_CMD_TIMEOUT,
            )?;
            if !out.status.success() {
                return Err(format_cmd_failure("gateway pack uninstall down", &out));
            }
        }
    } else if let Some(pack_root) = {
        // Best-effort down even if marker missing but pack dir exists.
        let p = gateway_data_dir().join(PACK_DIR_NAME);
        p.join("docker-compose.yml").is_file().then_some(p)
    } {
        let compose = compose_file(&pack_root);
        let secret_env = build_compose_secret_env(store, None).unwrap_or_default();
        let _ = compose_command_with_env(
            &compose,
            None,
            &["down", "--volumes", "--remove-orphans"],
            Some(&secret_env),
            DOCKER_CMD_TIMEOUT,
        );
    }

    let _ = delete_all_gateway_pack_secrets(store);
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

#[allow(dead_code)]
pub fn default_secret_store() -> KeychainSecretStore {
    KeychainSecretStore
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GatewayChildEnv {
    pub api_key: String,
    pub gateway_url: String,
}

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

/// Mark status after Council restart proof (called from lib.rs).
pub fn status_with_council_route(
    store: &dyn SecretStore,
    council_governed: bool,
    council_direct: bool,
) -> GatewayPackStatus {
    let mut st = gateway_pack_status(store);
    if council_governed && st.authenticated && st.enabled && gateway_health_ok() {
        st.state = GatewayPackState::AuthenticatedReady;
        st.council_governed = true;
        st.message =
            "Gateway Pack is authenticated and Council is governed.".into();
    } else if council_direct && !st.enabled {
        st.council_governed = false;
        if st.state == GatewayPackState::AuthenticatedReady {
            st.state = GatewayPackState::Disabled;
        }
    } else if st.enabled && st.authenticated && !council_governed {
        st.state = GatewayPackState::Degraded;
        st.council_governed = false;
        st.message =
            "Gateway is authenticated but Council did not enter governed mode.".into();
    }
    let _ = gw_api_key_present(store);
    st
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
    fn public_env_rejects_crlf_and_duplicates() {
        assert!(validate_env_value("K", "ok").is_ok());
        assert!(validate_env_value("K", "bad\nvalue").is_err());
        assert!(validate_env_value("K", "bad\rvalue").is_err());
        let dup = serialize_public_env(&[
            ("A".into(), "1".into()),
            ("A".into(), "2".into()),
        ]);
        assert!(dup.is_err());
        let body = serialize_public_env(&[
            ("IRIN_GATEWAY_IMAGE".into(), "n@sha256:".to_string() + &"a".repeat(64)),
            ("WATCH_PRODUCER_ENABLED".into(), "false".into()),
        ])
        .unwrap();
        assert!(body.contains("WATCH_PRODUCER_ENABLED=false"));
        assert!(!body.contains("AUTH_PEPPER"));
        assert!(!body.contains("XAI_API_KEY"));
    }

    #[test]
    fn bundled_alone_is_not_installed() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "gw-pack-notinst-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);
        assert!(!is_pack_installed());
        assert!(installed_pack_root().is_none());
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_swap_replaces_stale_and_writes_marker() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let prev_pack = std::env::var("IRIN_GATEWAY_PACK_ROOT").ok();
        let tmp = std::env::temp_dir().join(format!(
            "gw-pack-swap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        let bundle = tmp.join("bundle");
        fs::create_dir_all(bundle.join("conf")).unwrap();
        fs::create_dir_all(bundle.join("lua")).unwrap();
        fs::write(bundle.join("docker-compose.yml"), b"name: irin-desktop-gateway\n").unwrap();
        fs::write(bundle.join("nginx.conf"), b"# nginx\n").unwrap();
        let hex = "a".repeat(64);
        let manifest = format!(
            r#"{{
  "schema_version": 1,
  "mode": "local-dev",
  "pack_version": "0.1.0-a",
  "source_sha": "abc",
  "source_dirty": false,
  "images": {{
    "gateway": "irin-desktop/gateway@sha256:{hex}",
    "sidecar": "irin-desktop/sidecar@sha256:{hex}"
  }},
  "watch_invariants": {{
    "WATCH_PRODUCER_ENABLED": false,
    "WATCH_DISPATCHER_ENABLED": false
  }}
}}"#
        );
        fs::write(bundle.join("image-manifest.json"), manifest.as_bytes()).unwrap();
        std::env::set_var("IRIN_GATEWAY_PACK_ROOT", &bundle);

        let dest = install_pack_files().unwrap();
        assert!(dest.join("docker-compose.yml").is_file());
        assert!(is_pack_installed());
        // Stale file then update with new manifest version.
        fs::write(dest.join("stale.txt"), b"old").unwrap();
        let manifest_b = manifest.replace("0.1.0-a", "0.1.0-b");
        fs::write(bundle.join("image-manifest.json"), manifest_b.as_bytes()).unwrap();
        let dest2 = install_pack_files().unwrap();
        assert!(!dest2.join("stale.txt").exists(), "stale file survived swap");
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(installed_marker_path()).unwrap()).unwrap();
        assert_eq!(marker["pack_version"], "0.1.0-b");

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_pack {
            Some(v) => std::env::set_var("IRIN_GATEWAY_PACK_ROOT", v),
            None => std::env::remove_var("IRIN_GATEWAY_PACK_ROOT"),
        }
        let _ = fs::remove_dir_all(&tmp);
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
        let bad_key = format!("gw_{}", "0".repeat(32));
        fs::write(
            &path,
            format!(
                r#"{{"version":1,"install_id":"x","created_unix":1,"gw":"{bad_key}"}}
"#
            ),
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
        assert!(!st.council_governed);
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn secret_env_never_in_public_file_shape() {
        let body = serialize_public_env(&[
            ("IRIN_GATEWAY_IMAGE".into(), "x".into()),
            ("BOOTSTRAP_TOKEN".into(), "".into()),
        ])
        .unwrap();
        assert!(!body.contains("AUTH_PEPPER="));
        assert!(!body.contains("WATCH_ADMIN_TOKEN"));
        assert!(!body.contains("COUNCIL_GATEWAY_TOKEN"));
        // Empty bootstrap is ok in public file (blanked).
        assert!(body.contains("BOOTSTRAP_TOKEN="));
    }
}
