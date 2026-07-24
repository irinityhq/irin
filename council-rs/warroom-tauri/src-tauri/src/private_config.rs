//! First-launch private configuration for the packaged desktop app.
//!
//! Lives under Application Support:
//! - Default: `$HOME/Library/Application Support/com.irinity.irin`
//! - Override: absolute path in `IRIN_APP_SUPPORT_ROOT` (test / portable-state only)
//!
//! First launch under the default location adopts operator state left by the
//! retired "Council War Room" directory (`com.sovereign.council.warroom`) as a
//! non-destructive one-time copy; the legacy directory is never deleted.
//!
//! `IRIN_APP_SUPPORT_ROOT` relocates **app data only** (private.json, gateway pack
//! tree, overlays). It must never change Keychain selection, login session, or
//! the operator search list — keep the real process `HOME` when Keychain is
//! required. Never reads live IRIN gateway.env, shell profiles, or operator
//! secrets outside this app's tree.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_SUPPORT_DIR_NAME: &str = "com.irinity.irin";
/// Legacy Application Support directory name from the retired "Council War
/// Room" product identity. Read-only source for first-launch adoption of
/// existing operator state; this app never writes or deletes here.
const LEGACY_APP_SUPPORT_DIR_NAME: &str = "com.sovereign.council.warroom";
const PRIVATE_CONFIG_FILE: &str = "private.json";
const CONFIG_VERSION: u32 = 1;

/// Absolute path override for this app's Application Support directory.
///
/// Test and portable-state only. Locates IRIN app data; does **not** select or
/// create a Keychain. Relative or empty values are ignored (fall through).
pub const APP_SUPPORT_ROOT_ENV: &str = "IRIN_APP_SUPPORT_ROOT";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateConfig {
    pub version: u32,
    /// Opaque install identity (not a secret; safe to log).
    pub install_id: String,
    pub created_unix: u64,
    /// Optional bearer for `council --serve`. Empty string = loopback single-operator
    /// (matches product default of no COUNCIL_AUTH_TOKEN).
    #[serde(default)]
    pub auth_token: String,
    /// Process-wide default; core War Room works with false (no Docker).
    /// Raw GW_API_KEY is never stored here — only in the macOS Keychain.
    #[serde(default)]
    pub via_gateway_default: bool,
    /// Non-secret Gateway client key id (`k_` + 8 hex), when provisioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_key_id: Option<String>,
    /// Non-secret pack version last enabled (for update reconciliation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_pack_version: Option<String>,
}

impl Default for PrivateConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            install_id: new_install_id(),
            created_unix: unix_now(),
            auth_token: String::new(),
            via_gateway_default: false,
            gateway_key_id: None,
            gateway_pack_version: None,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_install_id() -> String {
    // Prefer UUID-shaped randomness without adding a uuid crate dep on the shell.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::process::id().hash(&mut h);
    unix_now().hash(&mut h);
    format!(
        "{:016x}{:016x}",
        h.finish(),
        unix_now().wrapping_mul(0x9e37)
    )
}

/// Resolve Application Support directory for this app.
///
/// Precedence:
/// 1. `IRIN_APP_SUPPORT_ROOT` — absolute path to the app support dir itself
/// 2. `$HOME/Library/Application Support/<id>` — may first adopt legacy state
/// 3. `/tmp/<id>` last resort
///
/// The env override never affects Keychain APIs.
pub fn app_support_dir() -> PathBuf {
    if let Some(root) = validated_app_support_root_override() {
        return root;
    }
    if let Ok(home) = std::env::var("HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            let home_dir = PathBuf::from(trimmed);
            adopt_legacy_app_support_dir(&home_dir);
            return home_dir
                .join("Library")
                .join("Application Support")
                .join(APP_SUPPORT_DIR_NAME);
        }
    }
    PathBuf::from("/tmp").join(APP_SUPPORT_DIR_NAME)
}

/// Best-effort, non-destructive adoption of Application Support state written
/// by the legacy "Council War Room" build (`com.sovereign.council.warroom`).
///
/// Runs only for the default `$HOME/Library/Application Support` resolution —
/// an explicit `IRIN_APP_SUPPORT_ROOT` override never triggers it. The copy
/// happens only while the new directory does not exist yet (the idempotent
/// precondition), and is skipped while a legacy app process is running so two
/// writers never race. The legacy directory is never deleted. Failures
/// degrade to an ordinary fresh first-launch directory.
fn adopt_legacy_app_support_dir(home: &Path) {
    let base = home.join("Library").join("Application Support");
    let new_dir = base.join(APP_SUPPORT_DIR_NAME);
    let legacy_dir = base.join(LEGACY_APP_SUPPORT_DIR_NAME);
    if new_dir.exists() || !legacy_dir.is_dir() {
        return;
    }
    if legacy_app_process_running() {
        eprintln!(
            "legacy app process is running; skipping one-time Application Support \
             migration (retried on next launch)"
        );
        return;
    }
    if let Err(e) = copy_legacy_app_support_dir(&legacy_dir, &new_dir) {
        eprintln!("legacy Application Support migration failed: {e}; continuing with fresh state");
        // A partial copy must not satisfy the new-dir-exists precondition on
        // the next launch; remove only what this copy attempt just created.
        let _ = fs::remove_dir_all(&new_dir);
    }
}

/// Copy `legacy_dir` to `new_dir` when, and only when, `new_dir` does not yet
/// exist and `legacy_dir` does. Returns whether a copy happened. Split out
/// from `adopt_legacy_app_support_dir` so tests can drive it with temp dirs.
fn copy_legacy_app_support_dir(legacy_dir: &Path, new_dir: &Path) -> std::io::Result<bool> {
    if new_dir.exists() || !legacy_dir.is_dir() {
        return Ok(false);
    }
    copy_dir_recursive(legacy_dir, new_dir)?;
    Ok(true)
}

/// True when the legacy "Council War Room" app appears to be running.
fn legacy_app_process_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "Council War Room"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Parse and validate `IRIN_APP_SUPPORT_ROOT` (absolute, non-empty, no NUL).
/// Invalid overrides are ignored so a bad test env cannot silently misplace data
/// under a relative path.
pub fn validated_app_support_root_override() -> Option<PathBuf> {
    let raw = std::env::var(APP_SUPPORT_ROOT_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return None;
    }
    // Refuse "." / empty file name components that are not meaningful roots.
    if path.components().count() < 2 {
        return None;
    }
    Some(path)
}

pub fn private_config_path() -> PathBuf {
    app_support_dir().join(PRIVATE_CONFIG_FILE)
}

/// Load existing private config or create it on first launch.
///
/// Does not print secrets. Token file permissions best-effort 0600 on Unix.
pub fn load_or_create_private_config() -> Result<PrivateConfig, String> {
    let dir = app_support_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create app support dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }

    let path = private_config_path();
    if path.is_file() {
        let raw = fs::read_to_string(&path).map_err(|e| format!("read private config: {e}"))?;
        let cfg: PrivateConfig =
            serde_json::from_str(&raw).map_err(|e| format!("parse private config: {e}"))?;
        return Ok(cfg);
    }

    let cfg = PrivateConfig::default();
    write_private_config_at(&path, &cfg)?;
    Ok(cfg)
}

/// Atomically write private config (0600). Never include raw GW_API_KEY.
pub fn write_private_config_at(path: &Path, cfg: &PrivateConfig) -> Result<(), String> {
    // Defense in depth: refuse to serialize if key fields look like raw keys.
    if let Some(ref kid) = cfg.gateway_key_id {
        if kid.starts_with("gw_") {
            return Err("refusing to write raw-looking gateway key into private.json".to_string());
        }
    }
    let raw =
        serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize private config: {e}"))?;
    if raw.contains("gw_") {
        // key ids are k_…; raw keys are gw_… — refuse any gw_ substring.
        return Err("refusing to write private.json containing gw_ material".to_string());
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("write private config tmp: {e}"))?;
        f.write_all(raw.as_bytes())
            .map_err(|e| format!("write private config tmp: {e}"))?;
        f.write_all(b"\n")
            .map_err(|e| format!("write private config tmp: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|e| format!("finalize private config: {e}"))?;
    Ok(())
}

/// Operator-facing guidance when Gateway/Docker is unavailable.
#[cfg(test)]
pub fn gateway_missing_prereq_guidance() -> &'static str {
    "Gateway is optional. Core War Room works without Docker. \
     To enable governed routing on an installed release: install and open Docker Desktop, \
     wait until it is ready, then use Settings → Enable Gateway (app-owned pack). \
     Without Docker/Gateway, keep Direct routing enabled."
}

/// Writable council base-dir under Application Support (cabinets may be saved here).
pub fn writable_council_base_dir() -> PathBuf {
    app_support_dir().join("council-base")
}

/// Ensure Application Support has a writable copy of the bundled base-dir.
///
/// - First launch: full recursive copy of shipped assets.
/// - Later launches: seed any **missing** shipped files without clobbering
///   operator-edited YAMLs; never delete user cabinets.
pub fn ensure_writable_base_overlay(bundled_base: &Path) -> Result<PathBuf, String> {
    let dest = writable_council_base_dir();
    fs::create_dir_all(&dest).map_err(|e| format!("create writable base-dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o700));
    }

    let marker = dest.join(".overlay-seeded");
    if !marker.is_file() {
        copy_dir_recursive(bundled_base, &dest)
            .map_err(|e| format!("seed council-base overlay from bundle: {e}"))?;
        fs::write(&marker, b"1\n").map_err(|e| format!("write overlay marker: {e}"))?;
        return Ok(dest);
    }

    seed_missing(bundled_base, &dest).map_err(|e| format!("refresh overlay missing files: {e}"))?;
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
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn seed_missing(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            seed_missing(&from, &to)?;
        } else if ty.is_file() && !to.exists() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Env pairs so a Finder-launched app can still discover CLI providers.
///
/// Matches canonical IRIN runtime semantics (`scripts/irin-runtime.sh`):
/// - Capture the operator environment **once** via interactive login shell:
///   `/bin/zsh -lic 'printenv -0'` (same `-lic` shape as launchd serve).
/// - Import only **Council provider** variables (see `is_council_provider_env_key`).
/// - Never import IRIN-generated tokens, Watch/admin secrets, or Cloudflare tokens.
/// - Values are never logged.
///
/// Also expands PATH for common GUI gaps (Homebrew, cargo bin) after the login PATH.
pub fn gui_login_environment() -> Vec<(String, String)> {
    let captured = interactive_login_env_once();
    let mut out: Vec<(String, String)> = Vec::new();

    let path = expanded_path_for_gui(captured);
    out.push(("PATH".to_string(), path));

    for (key, value) in captured {
        if is_council_provider_env_key(key) && !value.trim().is_empty() {
            // Prefer a non-empty process env already set (tests / terminal launch).
            if let Ok(existing) = std::env::var(key) {
                if !existing.trim().is_empty() {
                    out.push((key.clone(), existing));
                    continue;
                }
            }
            out.push((key.clone(), value.clone()));
        }
    }
    out
}

/// True for env keys Council provider discovery may use from the login shell.
///
/// Aligns with IRIN `shell_owns_provider_setting` for provider credentials, while
/// **excluding** IRIN-generated / non-provider secrets that must not be pulled
/// into a packaged desktop sidecar.
pub fn is_council_provider_env_key(key: &str) -> bool {
    // Explicit denylist: never import these even if they match *_API_KEY.
    match key {
        "GW_API_KEY" // IRIN-generated Council→Gateway credential, not a provider key
        | "WATCH_ADMIN_TOKEN"
        | "COUNCIL_GATEWAY_TOKEN"
        | "BOOTSTRAP_TOKEN"
        | "AUTH_PEPPER"
        | "CLAUDE_PROXY_TOKEN"
        | "CODEX_PROXY_TOKEN"
        | "CLOUDFLARE_API_TOKEN"
        | "CLOUDFLARE_API_KEY" => return false,
        _ => {}
    }
    // IRIN shell_owns_provider_setting provider surface:
    if key.ends_with("_API_KEY") || key == "OPENAI_ADMIN_KEY" {
        return true;
    }
    matches!(
        key,
        "VERTEX_PROJECT"
            | "VERTEX_LOCATION"
            | "VERTEX_GEMINI_MODEL"
            | "GOOGLE_CLOUD_PROJECT"
            | "GOOGLE_CLOUD_LOCATION"
            | "GOOGLE_APPLICATION_CREDENTIALS"
    )
}

fn expanded_path_for_gui(login_pairs: &[(String, String)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let push_unique = |parts: &mut Vec<String>, p: &str| {
        let t = p.trim();
        if t.is_empty() {
            return;
        }
        if !parts.iter().any(|e| e == t) {
            parts.push(t.to_string());
        }
    };

    if let Some((_, login_path)) = login_pairs.iter().find(|(k, _)| k == "PATH") {
        for p in login_path.split(':') {
            push_unique(&mut parts, p);
        }
    }
    if let Ok(cur) = std::env::var("PATH") {
        for p in cur.split(':') {
            push_unique(&mut parts, p);
        }
    }
    for p in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        push_unique(&mut parts, p);
    }
    if let Ok(home) = std::env::var("HOME") {
        push_unique(&mut parts, &format!("{home}/.local/bin"));
        push_unique(&mut parts, &format!("{home}/.cargo/bin"));
    }
    parts.join(":")
}

/// One interactive-login capture per process (canonical IRIN: `zsh -lic`).
fn interactive_login_env_once() -> &'static [(String, String)] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<(String, String)>> = OnceLock::new();
    CACHE.get_or_init(capture_interactive_login_env).as_slice()
}

/// Capture environment via interactive login shell. Public for unit tests that
/// need a fresh capture under a redirected HOME (does not use the process cache).
///
/// Hard-bounded: a wedged interactive shell (slow gcloud hooks, hanging plugins)
/// must not block Council auto-start forever.
pub fn capture_interactive_login_env() -> Vec<(String, String)> {
    // Same shape as `scripts/irin-runtime.sh` launchd serve: /bin/zsh -lic …
    // macOS printenv has no -0; emit NUL-delimited KEY=VALUE via python3 (always
    // present on Apple silicon build hosts / operator Macs with CLT).
    let mut cmd = std::process::Command::new("/bin/zsh");
    cmd.args([
        "-lic",
        r#"python3 -c 'import os,sys
for k,v in os.environ.items():
    sys.stdout.buffer.write(k.encode()+b"="+v.encode()+b"\0")
'"#,
    ]);
    match run_login_capture_timeout(cmd, std::time::Duration::from_secs(8)) {
        Ok(output) if output.status.success() => return parse_printenv_null(&output.stdout),
        _ => {}
    }
    // Fallback: line-oriented env (API keys never contain newlines).
    let mut fallback = std::process::Command::new("/bin/zsh");
    fallback.args(["-lic", "/usr/bin/env"]);
    match run_login_capture_timeout(fallback, std::time::Duration::from_secs(5)) {
        Ok(fb) if fb.status.success() => parse_env_lines(&fb.stdout),
        _ => Vec::new(),
    }
}

/// Run `cmd` with a hard wall-clock timeout, owning the live child handle.
///
/// - Successful early exit cancels the timeout: no later signal is sent.
/// - On timeout, kill/reap **exactly** this child once (never a bare recycled PID).
/// - Does not arm a fire-and-forget sleeper that can outlive `wait`.
fn run_login_capture_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, ()> {
    use std::io::Read;
    use std::process::Stdio;
    use std::thread;
    use std::time::{Duration, Instant};

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|_| ())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_h = thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut r) = stdout {
            let _ = r.read_to_end(&mut b);
        }
        b
    });
    let err_h = thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_end(&mut b);
        }
        b
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Own the handle: kill then reap this child only.
                    let _ = child.kill();
                    let reaped = child.wait().map_err(|_| ())?;
                    let _ = out_h.join();
                    let _ = err_h.join();
                    let _ = reaped;
                    return Err(());
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_h.join();
                let _ = err_h.join();
                return Err(());
            }
        }
    };

    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn parse_env_lines(raw: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in raw.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(s) = std::str::from_utf8(line) else {
            continue;
        };
        let Some((k, v)) = s.split_once('=') else {
            continue;
        };
        if !k.is_empty() {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

fn parse_printenv_null(raw: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in raw.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(s) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((k, v)) = s.split_once('=') else {
            continue;
        };
        if k.is_empty() {
            continue;
        }
        out.push((k.to_string(), v.to_string()));
    }
    out
}

/// Process-wide lock for tests that mutate `HOME` (shared across modules).
#[cfg(test)]
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_is_idempotent_under_redirected_home() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "irin-private-cfg-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        let a = load_or_create_private_config().unwrap();
        let b = load_or_create_private_config().unwrap();
        assert_eq!(a.install_id, b.install_id);
        assert_eq!(a.version, CONFIG_VERSION);
        assert!(!a.via_gateway_default);
        assert!(private_config_path().is_file());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gateway_guidance_mentions_optional_and_docker() {
        let g = gateway_missing_prereq_guidance();
        assert!(g.contains("optional"));
        assert!(g.contains("Docker"));
        assert!(g.contains("Direct"));
    }

    #[test]
    fn writable_overlay_seeds_and_preserves_user_files() {
        let _g = test_env_lock();
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!(
            "irin-overlay-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("bundle/cabinets")).unwrap();
        fs::write(tmp.join("bundle/cabinets/shipped.yaml"), b"name: shipped\n").unwrap();
        fs::write(tmp.join("bundle/models.yaml"), b"models: {}\n").unwrap();
        std::env::set_var("HOME", tmp.join("home"));

        let dest = ensure_writable_base_overlay(&tmp.join("bundle")).unwrap();
        assert!(dest.join("cabinets/shipped.yaml").is_file());
        // Operator edit
        fs::write(dest.join("cabinets/shipped.yaml"), b"name: edited\n").unwrap();
        // New shipped file should seed; edited must stay
        fs::write(tmp.join("bundle/cabinets/newone.yaml"), b"name: new\n").unwrap();
        let dest2 = ensure_writable_base_overlay(&tmp.join("bundle")).unwrap();
        assert_eq!(
            fs::read_to_string(dest2.join("cabinets/shipped.yaml")).unwrap(),
            "name: edited\n"
        );
        assert!(dest2.join("cabinets/newone.yaml").is_file());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn expanded_path_includes_homebrew() {
        let p = expanded_path_for_gui(&[]);
        assert!(p.contains("/opt/homebrew/bin") || p.contains("/usr/local/bin"));
    }

    #[test]
    fn provider_key_whitelist_matches_council_surface() {
        assert!(is_council_provider_env_key("XAI_API_KEY"));
        assert!(is_council_provider_env_key("ANTHROPIC_API_KEY"));
        assert!(is_council_provider_env_key("OPENAI_API_KEY"));
        assert!(is_council_provider_env_key("OPENAI_ADMIN_KEY"));
        assert!(is_council_provider_env_key("VERTEX_PROJECT"));
        assert!(is_council_provider_env_key(
            "GOOGLE_APPLICATION_CREDENTIALS"
        ));
        assert!(is_council_provider_env_key("NVIDIA_API_KEY"));
    }

    #[test]
    fn provider_key_whitelist_excludes_irin_and_cloud_secrets() {
        assert!(!is_council_provider_env_key("GW_API_KEY"));
        assert!(!is_council_provider_env_key("WATCH_ADMIN_TOKEN"));
        assert!(!is_council_provider_env_key("COUNCIL_GATEWAY_TOKEN"));
        assert!(!is_council_provider_env_key("BOOTSTRAP_TOKEN"));
        assert!(!is_council_provider_env_key("AUTH_PEPPER"));
        assert!(!is_council_provider_env_key("CLOUDFLARE_API_TOKEN"));
        assert!(!is_council_provider_env_key("CLOUDFLARE_API_KEY"));
        assert!(!is_council_provider_env_key("CLAUDE_PROXY_TOKEN"));
        assert!(!is_council_provider_env_key("CODEX_PROXY_TOKEN"));
        assert!(!is_council_provider_env_key("PATH"));
        assert!(!is_council_provider_env_key("HOME"));
    }

    #[test]
    fn parse_printenv_null_splits_pairs() {
        let raw = b"PATH=/usr/bin\0XAI_API_KEY=marker\0WATCH_ADMIN_TOKEN=nope\0";
        let pairs = parse_printenv_null(raw);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[1], ("XAI_API_KEY".into(), "marker".into()));
    }

    #[test]
    fn app_support_root_override_takes_precedence_over_home() {
        let _g = test_env_lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_root = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        let tmp = std::env::temp_dir().join(format!(
            "irin-app-support-root-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let override_dir = tmp.join("portable-app-support");
        let decoy_home = tmp.join("decoy-home");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&override_dir).unwrap();
        fs::create_dir_all(&decoy_home).unwrap();
        std::env::set_var("HOME", &decoy_home);
        std::env::set_var(APP_SUPPORT_ROOT_ENV, &override_dir);

        assert_eq!(app_support_dir(), override_dir);
        assert!(validated_app_support_root_override().is_some());

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_root {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn app_support_root_rejects_relative_override() {
        let _g = test_env_lock();
        let prev_root = std::env::var(APP_SUPPORT_ROOT_ENV).ok();
        std::env::set_var(APP_SUPPORT_ROOT_ENV, "relative/not/absolute");
        assert!(validated_app_support_root_override().is_none());
        match prev_root {
            Some(v) => std::env::set_var(APP_SUPPORT_ROOT_ENV, v),
            None => std::env::remove_var(APP_SUPPORT_ROOT_ENV),
        }
    }

    #[test]
    fn legacy_app_support_copy_happens_only_when_new_absent() {
        let tmp = std::env::temp_dir().join(format!(
            "irin-legacy-support-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let legacy = tmp.join(LEGACY_APP_SUPPORT_DIR_NAME);
        let new = tmp.join(APP_SUPPORT_DIR_NAME);
        fs::create_dir_all(legacy.join("nested")).unwrap();
        fs::write(legacy.join("private.json"), b"{\"version\":1}\n").unwrap();
        fs::write(legacy.join("nested/state.txt"), b"operator state\n").unwrap();

        // Copy happens when the new directory is absent.
        assert!(copy_legacy_app_support_dir(&legacy, &new).unwrap());
        assert_eq!(
            fs::read_to_string(new.join("private.json")).unwrap(),
            "{\"version\":1}\n"
        );
        assert!(new.join("nested/state.txt").is_file());
        // Legacy tree is left intact (non-destructive).
        assert!(legacy.join("nested/state.txt").is_file());

        // No copy (and no clobber) when the new directory already exists.
        fs::write(new.join("operator.yaml"), b"name: operator\n").unwrap();
        fs::write(legacy.join("operator.yaml"), b"name: legacy\n").unwrap();
        assert!(!copy_legacy_app_support_dir(&legacy, &new).unwrap());
        assert_eq!(
            fs::read_to_string(new.join("operator.yaml")).unwrap(),
            "name: operator\n"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn login_capture_timeout_fast_exit_no_full_wait() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        let mut cmd = Command::new("/bin/echo");
        cmd.arg("ok");
        let start = Instant::now();
        let out = run_login_capture_timeout(cmd, Duration::from_secs(8)).expect("fast exit");
        assert!(out.status.success());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn login_capture_timeout_kills_slow_child_once() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let start = Instant::now();
        let res = run_login_capture_timeout(cmd, Duration::from_millis(200));
        assert!(res.is_err(), "slow child must time out");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout path must not wait full sleep"
        );
        // Child is reaped inside the helper; no delayed bare-PID kill remains.
    }
}
