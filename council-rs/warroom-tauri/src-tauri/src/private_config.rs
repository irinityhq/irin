//! First-launch private configuration for the packaged desktop app.
//!
//! Lives under Application Support (or `$HOME/Library/Application Support/...`
//! when HOME is redirected for isolated testing). Never reads live IRIN
//! gateway.env, shell profiles, or operator secrets outside this app's tree.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_SUPPORT_DIR_NAME: &str = "com.sovereign.council.warroom";
const PRIVATE_CONFIG_FILE: &str = "private.json";
const CONFIG_VERSION: u32 = 1;

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
    format!("{:016x}{:016x}", h.finish(), unix_now().wrapping_mul(0x9e37))
}

/// Resolve Application Support directory for this app (HOME-redirectable).
pub fn app_support_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed)
                .join("Library")
                .join("Application Support")
                .join(APP_SUPPORT_DIR_NAME);
        }
    }
    PathBuf::from("/tmp").join(APP_SUPPORT_DIR_NAME)
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
    let raw = serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize private config: {e}"))?;
    if raw.contains("gw_") {
        // key ids are k_…; raw keys are gw_… — refuse any gw_ substring.
        return Err("refusing to write private.json containing gw_ material".to_string());
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("write private config tmp: {e}"))?;
        f.write_all(raw.as_bytes())
            .map_err(|e| format!("write private config tmp: {e}"))?;
        f.write_all(b"\n").map_err(|e| format!("write private config tmp: {e}"))?;
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
    CACHE
        .get_or_init(capture_interactive_login_env)
        .as_slice()
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

fn run_login_capture_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, ()> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::thread;

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
    let pid = child.id();
    let timed = Arc::new(Mutex::new(false));
    let timed2 = Arc::clone(&timed);
    let killer = thread::spawn(move || {
        thread::sleep(timeout);
        *timed2.lock().unwrap_or_else(|e| e.into_inner()) = true;
        #[cfg(unix)]
        {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            let _ = unsafe { kill(pid as i32, 15) };
            thread::sleep(std::time::Duration::from_millis(150));
            let _ = unsafe { kill(pid as i32, 9) };
        }
    });
    let status = child.wait().map_err(|_| ())?;
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    let _ = killer.join();
    if *timed.lock().unwrap_or_else(|e| e.into_inner()) && !status.success() {
        return Err(());
    }
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
        assert!(is_council_provider_env_key("GOOGLE_APPLICATION_CREDENTIALS"));
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
}
