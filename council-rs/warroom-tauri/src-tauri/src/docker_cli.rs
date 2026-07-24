//! Strict Docker CLI resolution and allow-listed compose invocations.
//!
//! The renderer never supplies executable paths, project names, mount paths,
//! env payloads, or destructive volume targets. Only fixed constants are used.
//! Every native Docker/Compose call is hard wall-clock bounded and killable.
//!
//! Spawn env is layered on every call: ambient secrets are scrubbed, the
//! validated pack env is applied, and Watch/admin surfaces are forced
//! disarmed last. Compose variable precedence ranks the process environment
//! above `--env-file`, so ambient parent values must never be the source for
//! interpolated pack variables.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

/// Fixed Compose project name for the desktop Gateway Pack.
pub const DESKTOP_COMPOSE_PROJECT: &str = "irin-desktop-gateway";

/// Fixed Gateway loopback URL for the bundled Council child.
pub const DESKTOP_GATEWAY_URL: &str = "http://127.0.0.1:18080";

/// Fixed published host port (compose also pins 127.0.0.1:18080:8080).
#[allow(dead_code)]
pub const DESKTOP_GATEWAY_PORT: u16 = 18080;

/// Default hard timeout for Docker CLI calls (including daemon probes).
pub const DOCKER_CMD_TIMEOUT: Duration = Duration::from_secs(45);

/// Longer timeout for `compose up --wait` style operations.
pub const DOCKER_COMPOSE_UP_TIMEOUT: Duration = Duration::from_secs(180);

/// Allow-listed absolute paths for the Docker CLI only.
pub const DOCKER_CLI_ALLOWLIST: &[&str] = &[
    "/usr/local/bin/docker",
    "/Applications/Docker.app/Contents/Resources/bin/docker",
];

/// Docker Desktop ships Compose as a CLI plugin under this directory.
/// `$HOME/.docker/cli-plugins` normally contains symlinks; when HOME is
/// isolated (packaged UI smoke) or the operator has no plugin links, the CLI
/// reports `unknown command: docker compose` unless we inject
/// `cliPluginsExtraDirs` via a managed `DOCKER_CONFIG`.
pub const DOCKER_DESKTOP_CLI_PLUGINS: &str =
    "/Applications/Docker.app/Contents/Resources/cli-plugins";

/// Bounded, non-secret failure categories returned to the UI/logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerErrorKind {
    Timeout,
    PathRejected,
    SubcommandRejected,
    SpawnFailed,
    CommandFailed,
    ImageDigestMismatch,
}

impl DockerErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "docker_timeout",
            Self::PathRejected => "docker_path_rejected",
            Self::SubcommandRejected => "docker_subcommand_rejected",
            Self::SpawnFailed => "docker_spawn_failed",
            Self::CommandFailed => "docker_command_failed",
            Self::ImageDigestMismatch => "docker_image_digest_mismatch",
        }
    }

    pub fn operator_message(self) -> &'static str {
        match self {
            Self::Timeout => {
                "Docker command timed out and was terminated. Retry once Docker is responsive."
            }
            Self::PathRejected => "Internal path rejected by the Docker allow-list.",
            Self::SubcommandRejected => "Compose subcommand rejected by the allow-list.",
            Self::SpawnFailed => "Failed to start the Docker CLI process.",
            Self::CommandFailed => "Docker command failed (details redacted).",
            Self::ImageDigestMismatch => {
                "Local image does not match the Gateway Pack manifest digest."
            }
        }
    }
}

/// Resolve the first existing allow-listed Docker CLI binary.
pub fn resolve_docker_cli() -> Result<PathBuf, String> {
    for candidate in DOCKER_CLI_ALLOWLIST {
        let p = Path::new(candidate);
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
    }
    Err("Docker CLI not found. Install Docker Desktop, then retry. \
         Expected /usr/local/bin/docker or \
         /Applications/Docker.app/Contents/Resources/bin/docker."
        .to_string())
}

/// Reject any path that is not on the allow-list (defense in depth).
pub fn validate_docker_cli_path(path: &Path) -> Result<(), String> {
    let s = path.to_string_lossy();
    if DOCKER_CLI_ALLOWLIST.iter().any(|a| *a == s) {
        Ok(())
    } else {
        Err(format!("docker CLI path not allow-listed: {s}"))
    }
}

/// Validate compose project name is exactly the fixed desktop project.
pub fn validate_compose_project(project: &str) -> Result<(), String> {
    if project == DESKTOP_COMPOSE_PROJECT {
        Ok(())
    } else {
        Err(format!(
            "refusing project {project:?}; only {DESKTOP_COMPOSE_PROJECT} is allowed"
        ))
    }
}

/// True when the path looks like a shell-metacharacter injection risk for argv.
pub fn path_is_safe_argv(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s.is_empty() || s.contains('\0') {
        return false;
    }
    // Absolute paths only; no shell metacharacters (we never pass through a shell,
    // but reject odd paths so mounts stay predictable).
    if !s.starts_with('/') {
        return false;
    }
    !s.chars()
        .any(|c| matches!(c, '$' | '`' | '\n' | '\r' | ';'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerDaemonState {
    CliMissing,
    DaemonDown,
    Ready,
}

/// Run a process with a hard wall-clock timeout. On timeout the child is killed.
/// Never returns stdout/stderr to callers that need redaction — use
/// [`format_cmd_failure`] which only emits fixed categories.
///
/// Owns the live [`std::process::Child`]: successful early exit returns
/// immediately with **no** delayed signal and no join of a full-timeout sleeper
/// (a prior sleeper+join pattern forced every successful `compose up --wait`
/// to wait the full 180s before Enable could provision Keychain keys).
pub fn run_command_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    use std::time::Instant;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{}: {}", DockerErrorKind::SpawnFailed.as_str(), e))?;

    // Capture stdout/stderr on background threads so full pipes cannot deadlock.
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
                    let reaped = child.wait().map_err(|e| {
                        format!(
                            "{}: wait after kill failed: {e}",
                            DockerErrorKind::SpawnFailed.as_str()
                        )
                    })?;
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    let _ = reaped;
                    return Err(DockerErrorKind::Timeout.as_str().to_string());
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(format!(
                    "{}: wait failed: {e}",
                    DockerErrorKind::SpawnFailed.as_str()
                ));
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

/// Probe Docker CLI presence and daemon readiness (no secrets in output).
/// Hard-bounded — a wedged `docker info` cannot hang the app forever.
pub fn probe_docker_daemon() -> DockerDaemonState {
    let Ok(docker) = resolve_docker_cli() else {
        return DockerDaemonState::CliMissing;
    };
    let cmd = build_docker_command(&docker, &["info", "--format", "{{.ServerVersion}}"]);
    match run_command_timeout(cmd, Duration::from_secs(12)) {
        Ok(o) if o.status.success() => DockerDaemonState::Ready,
        Ok(_) => DockerDaemonState::DaemonDown,
        Err(e) if e.contains(DockerErrorKind::Timeout.as_str()) => DockerDaemonState::DaemonDown,
        Err(_) => DockerDaemonState::DaemonDown,
    }
}

/// Optional extra environment for compose (secrets / provider keys / pins).
/// Values never appear in argv and are not returned in error messages.
/// Applied after the ambient secret scrub and before the disarmed-surface
/// force, so validated pack values win over the parent environment but can
/// never arm a surface the pack contract requires off.
pub type ComposeEnv = HashMap<String, String>;

/// Known Compose plugin directories to inject via `cliPluginsExtraDirs`.
pub fn docker_cli_plugin_extra_dirs() -> Vec<&'static str> {
    let mut out = Vec::new();
    for dir in [
        DOCKER_DESKTOP_CLI_PLUGINS,
        "/usr/local/lib/docker/cli-plugins",
        "/usr/lib/docker/cli-plugins",
    ] {
        if Path::new(dir).is_dir() {
            out.push(dir);
        }
    }
    out
}

/// Managed Docker CLI config directory (non-secret). Contains only
/// `cliPluginsExtraDirs` plugin path hints so Compose plugin resolution is
/// deterministic. It never reads or merges the operator's
/// `~/.docker/config.json`: the production pack images are public, so no
/// registry auth is needed here. When registry authentication is actually
/// required, the normal HOME-based Docker config is used instead; this managed
/// config exists only for deterministic plugin resolution. Never logs config
/// contents.
///
/// Lives under `app_support_dir()` so `IRIN_APP_SUPPORT_ROOT` isolation applies
/// without remapping `HOME` (Keychain stays on the operator login keychain).
pub fn ensure_managed_docker_config_dir() -> Result<PathBuf, String> {
    let dir = crate::private_config::app_support_dir()
        .join("gateway")
        .join(".docker-cli");
    fs::create_dir_all(&dir).map_err(|e| format!("create managed docker config dir: {e}"))?;

    let extra = docker_cli_plugin_extra_dirs();
    let mut base = serde_json::Map::new();
    if !extra.is_empty() {
        base.insert(
            "cliPluginsExtraDirs".into(),
            serde_json::Value::Array(
                extra
                    .iter()
                    .map(|d| serde_json::Value::String((*d).to_string()))
                    .collect(),
            ),
        );
    }
    let body = serde_json::Value::Object(base).to_string();
    let path = dir.join("config.json");
    // Atomic-ish write without logging contents. Tolerate a concurrent
    // removal of the app-support tree (parallel tests, external cleaners):
    // recreate the dir and retry the write+rename once.
    let tmp = dir.join(format!(".config.{}.tmp", std::process::id()));
    let attempt = || -> Result<(), String> {
        fs::create_dir_all(&dir).map_err(|e| format!("create managed docker config dir: {e}"))?;
        fs::write(&tmp, body.as_bytes())
            .map_err(|e| format!("write managed docker config: {e}"))?;
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("rename managed docker config: {e}")
        })
    };
    attempt().or_else(|first_err| {
        if first_err.contains("No such file or directory") {
            attempt()
        } else {
            Err(first_err)
        }
    })?;
    // The managed config carries no credentials, but never leave it at
    // umask-dependent permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod managed docker dir: {e}"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod managed docker config: {e}"))?;
    }
    Ok(dir)
}

/// Inject env so every Docker/Compose spawn resolves plugins deterministically.
///
/// Always select the managed pack config (`DOCKER_CONFIG=<managed dir>`) for
/// pack spawns. The ambient operator `~/.docker` state and any parent
/// `DOCKER_CONFIG` (possibly polluted by harness/debug shells and lacking
/// plugin hints) are never honored. The managed config carries no registry
/// credentials — only `cliPluginsExtraDirs` — while the plugin binaries
/// themselves still resolve from the operator's Docker Desktop install. That
/// is accepted: Docker Desktop already runs as the operator, and the
/// documented boundary does not defend against a compromised host. Never logs
/// config contents.
fn apply_docker_cli_env(cmd: &mut Command) {
    if let Ok(dir) = ensure_managed_docker_config_dir() {
        cmd.env("DOCKER_CONFIG", dir);
    } else {
        // Never inherit a polluted ambient DOCKER_CONFIG from the parent env.
        cmd.env_remove("DOCKER_CONFIG");
    }
}

/// Compose-interpolated secret keys that must never be inherited from the
/// parent process environment. An explicit empty `cmd.env` overrides the
/// inherited value; the pack's validated per-spawn env (allow-listed
/// `extra_env`) is applied after this scrub and wins. Compose variable
/// precedence ranks the process environment above `--env-file`, so without
/// this scrub an ambient provider key or bootstrap token would silently
/// reach the pack containers.
pub const AMBIENT_SCRUB_ENV_KEYS: &[&str] = &[
    "XAI_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "NVIDIA_API_KEY",
    "AUTH_PEPPER",
    "BOOTSTRAP_TOKEN",
];

/// Watch/admin surfaces the desktop pack contract requires disarmed. Forced
/// on every Docker/Compose spawn AFTER any caller-supplied env, so neither
/// the parent environment nor an internal caller can arm them
/// (`WATCH_PRODUCER_ENABLED=false` alone never neutralized admin APIs gated
/// on `WATCH_ADMIN_TOKEN`). Values match the pack compose contract.
pub const FORCED_DISARM_ENV: &[(&str, &str)] = &[
    ("WATCH_ADMIN_TOKEN", ""),
    ("COUNCIL_GATEWAY_TOKEN", ""),
    ("WATCH_PRODUCER_ENABLED", "false"),
    ("WATCH_DISPATCHER_ENABLED", "false"),
];

/// Scrub ambient copies of compose-interpolated secrets (see
/// [`AMBIENT_SCRUB_ENV_KEYS`]). Runs before the validated pack env.
fn apply_ambient_scrub_env(cmd: &mut Command) {
    for key in AMBIENT_SCRUB_ENV_KEYS {
        cmd.env(key, "");
    }
}

/// Force disarmed Watch/admin surfaces (see [`FORCED_DISARM_ENV`]). Runs
/// last on every spawn so no layer can re-arm them.
fn apply_forced_disarm_env(cmd: &mut Command) {
    for (key, value) in FORCED_DISARM_ENV {
        cmd.env(key, value);
    }
}

/// Run `docker <args…>` with the allow-listed binary. Never routes through a shell.
pub fn docker_command(args: &[&str]) -> Result<Output, String> {
    docker_command_timeout(args, DOCKER_CMD_TIMEOUT)
}

pub fn docker_command_timeout(args: &[&str], timeout: Duration) -> Result<Output, String> {
    let docker = resolve_docker_cli()?;
    validate_docker_cli_path(&docker)?;
    run_command_timeout(build_docker_command(&docker, args), timeout)
}

/// Build `docker <args…>` with the layered spawn env: managed Docker config,
/// ambient secret scrub, then the disarmed-surface force. Never routes
/// through a shell; the plain Docker path has no caller-supplied env channel.
fn build_docker_command(docker: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(docker);
    cmd.args(args);
    apply_docker_cli_env(&mut cmd);
    apply_ambient_scrub_env(&mut cmd);
    apply_forced_disarm_env(&mut cmd);
    cmd
}

/// Fixed-shape compose invocation for the desktop project only.
///
/// - `compose_file` and `env_file` must be absolute safe paths
/// - project is always `irin-desktop-gateway`
/// - extra args are an allow-listed subcommand set (up/down/ps/…)
/// - `extra_env` is a strict allow-list of env keys (never logged)
#[cfg(test)]
pub fn compose_command(
    compose_file: &Path,
    env_file: Option<&Path>,
    extra: &[&str],
) -> Result<Output, String> {
    compose_command_with_env(compose_file, env_file, extra, None, DOCKER_CMD_TIMEOUT)
}

pub fn compose_command_with_env(
    compose_file: &Path,
    env_file: Option<&Path>,
    extra: &[&str],
    extra_env: Option<&ComposeEnv>,
    timeout: Duration,
) -> Result<Output, String> {
    validate_compose_invocation(compose_file, env_file, extra, extra_env)?;
    let docker = resolve_docker_cli()?;
    validate_docker_cli_path(&docker)?;
    let cmd = build_compose_command(&docker, compose_file, env_file, extra, extra_env);
    run_command_timeout(cmd, timeout)
}

/// Validation for the fixed-shape compose invocation. Kept ahead of Docker
/// CLI resolution so rejections never depend on a local Docker install.
fn validate_compose_invocation(
    compose_file: &Path,
    env_file: Option<&Path>,
    extra: &[&str],
    extra_env: Option<&ComposeEnv>,
) -> Result<(), String> {
    validate_compose_project(DESKTOP_COMPOSE_PROJECT)?;
    if !path_is_safe_argv(compose_file) {
        return Err(DockerErrorKind::PathRejected.operator_message().to_string());
    }
    if !compose_file.is_file() {
        return Err(format!("compose file missing: {}", compose_file.display()));
    }
    if let Some(ef) = env_file {
        if !path_is_safe_argv(ef) {
            return Err(DockerErrorKind::PathRejected.operator_message().to_string());
        }
        if !ef.is_file() {
            return Err(format!("env file missing: {}", ef.display()));
        }
    }

    // Allow-list first token of extra (subcommand).
    let sub = extra.first().copied().unwrap_or("");
    if !COMPOSE_SUBCOMMAND_ALLOWLIST.contains(&sub) {
        return Err(format!(
            "{}: {sub}",
            DockerErrorKind::SubcommandRejected.as_str()
        ));
    }
    for a in extra.iter().skip(1) {
        if a.starts_with('-') {
            if !COMPOSE_FLAG_ALLOWLIST
                .iter()
                .any(|f| a == f || a.starts_with(&format!("{f}=")))
            {
                // Allow only known flags; values for -p etc. are not taken from caller.
                if !a.starts_with("--timeout")
                    && *a != "-d"
                    && *a != "--detach"
                    && *a != "--remove-orphans"
                    && *a != "-v"
                    && *a != "--volumes"
                    && *a != "--wait"
                    && *a != "--force-recreate"
                {
                    return Err(format!(
                        "{}: {a}",
                        DockerErrorKind::SubcommandRejected.as_str()
                    ));
                }
            }
        } else if a.contains("..") || a.contains('$') || a.contains('`') {
            return Err(format!(
                "{}: rejected arg",
                DockerErrorKind::SubcommandRejected.as_str()
            ));
        }
    }

    if let Some(env) = extra_env {
        for key in env.keys() {
            if !COMPOSE_ENV_KEY_ALLOWLIST.contains(&key.as_str()) {
                return Err(format!(
                    "{}: env key not allow-listed",
                    DockerErrorKind::SubcommandRejected.as_str()
                ));
            }
        }
    }
    Ok(())
}

/// Build the compose command with the layered spawn env:
///
/// 1. managed Docker config + ambient secret scrub (ambient parent values
///    must never be the source for interpolated pack variables),
/// 2. the validated pack env (allow-listed keys only) — explicit `cmd.env`
///    beats both the inherited parent value and the `--env-file` pins under
///    Compose variable precedence,
/// 3. the disarmed-surface force, last, so no layer can arm Watch/admin.
///
/// Assumes [`validate_compose_invocation`] already passed.
fn build_compose_command(
    docker: &Path,
    compose_file: &Path,
    env_file: Option<&Path>,
    extra: &[&str],
    extra_env: Option<&ComposeEnv>,
) -> Command {
    let mut cmd = Command::new(docker);
    apply_docker_cli_env(&mut cmd);
    apply_ambient_scrub_env(&mut cmd);
    cmd.arg("compose")
        .arg("-p")
        .arg(DESKTOP_COMPOSE_PROJECT)
        .arg("-f")
        .arg(compose_file);
    if let Some(ef) = env_file {
        cmd.arg("--env-file").arg(ef);
    }
    cmd.args(extra);
    if let Some(env) = extra_env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    apply_forced_disarm_env(&mut cmd);
    cmd
}

/// Env keys allowed on the compose process (secrets + non-secret image/path pins).
/// Values are never logged or returned in error strings.
pub const COMPOSE_ENV_KEY_ALLOWLIST: &[&str] = &[
    "IRIN_GATEWAY_IMAGE",
    "IRIN_SIDECAR_IMAGE",
    "IRIN_DESKTOP_PACK_ROOT",
    "IRIN_DESKTOP_LEDGER_KEY",
    "AUTH_PEPPER",
    "BOOTSTRAP_TOKEN",
    "XAI_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "NVIDIA_API_KEY",
    "GATEWAY_DURABLE",
    "GATEWAY_AUTH_FAIL_CLOSED",
    "SIDECAR_SOCKET_MODE",
    "SIDECAR_SOCKET_GID",
    "GW_ENABLE_COUNCIL_ENDPOINT",
    "COUNCIL_BASE_URL",
    "GW_ENABLE_STREAMING",
    "GW_ENABLE_BATCH",
    "GATEWAY_BASE_URL",
    "WATCH_PRODUCER_ENABLED",
    "WATCH_DISPATCHER_ENABLED",
    "WATCH_CANARY_TENANT",
    "DAILY_SPEND_CAP_USD",
    "WATCH_MAX_FANOUT_COST_USD",
    "COUNCIL_GATEWAY_KEY_ID",
];

const COMPOSE_SUBCOMMAND_ALLOWLIST: &[&str] = &[
    "up", "down", "ps", "config", "pull", "images", "stop", "start", "ls",
];

const COMPOSE_FLAG_ALLOWLIST: &[&str] = &[
    "-d",
    "--detach",
    "--remove-orphans",
    "-v",
    "--volumes",
    "--wait",
    "--force-recreate",
    "--timeout",
    "services",
];

/// Redact whole secret-looking values from untrusted process text.
///
/// Unlike prefix-only replace (`gw_` → redacted), this removes the entire
/// credential token so suffixes cannot leak through the renderer or logs.
pub fn redact_process_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len().min(512));
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // gw_ + 32 hex
        if i + 35 <= bytes.len()
            && &bytes[i..i + 3] == b"gw_"
            && bytes[i + 3..i + 35].iter().all(|c| c.is_ascii_hexdigit())
        {
            out.push_str("<redacted:gw_key>");
            i += 35;
            continue;
        }
        // sk-… openai-ish
        if i + 3 <= bytes.len() && &bytes[i..i + 3] == b"sk-" {
            let mut j = i + 3;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-' || bytes[j] == b'_')
            {
                j += 1;
            }
            if j - i >= 12 {
                out.push_str("<redacted:api_key>");
                i = j;
                continue;
            }
        }
        // KEY=value patterns for known secret keys (consume whole value token)
        if let Some((key, rest_start)) = match_secret_assignment(bytes, i) {
            out.push_str(key);
            out.push_str("=<redacted>");
            // skip value until whitespace/quote end
            let mut j = rest_start;
            if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                let q = bytes[j];
                j += 1;
                while j < bytes.len() && bytes[j] != q && bytes[j] != b'\n' && bytes[j] != b'\r' {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == q {
                    j += 1;
                }
            } else {
                while j < bytes.len()
                    && !matches!(
                        bytes[j],
                        b' ' | b'\t' | b'\n' | b'\r' | b';' | b',' | b')' | b']'
                    )
                {
                    j += 1;
                }
            }
            i = j;
            continue;
        }
        // Long hex blobs that look like pepper/bootstrap (32+ hex as standalone token)
        if bytes[i].is_ascii_hexdigit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j - i >= 32
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (j == bytes.len() || !bytes[j].is_ascii_alphanumeric())
            {
                out.push_str("<redacted:hex>");
                i = j;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    if out.len() > 400 {
        out.truncate(400);
        out.push('…');
    }
    out
}

fn match_secret_assignment(bytes: &[u8], i: usize) -> Option<(&'static str, usize)> {
    const KEYS: &[&str] = &[
        "BOOTSTRAP_TOKEN=",
        "AUTH_PEPPER=",
        "GW_API_KEY=",
        "WATCH_ADMIN_TOKEN=",
        "COUNCIL_GATEWAY_TOKEN=",
        "XAI_API_KEY=",
        "OPENAI_API_KEY=",
        "ANTHROPIC_API_KEY=",
        "NVIDIA_API_KEY=",
        "admin_key=",
        "raw_key=",
        "Authorization: Bearer ",
        "Authorization:Bearer ",
    ];
    for key in KEYS {
        let kb = key.as_bytes();
        if i + kb.len() <= bytes.len() && &bytes[i..i + kb.len()] == kb {
            // Return key without trailing separator for display consistency
            let display = key.trim_end_matches('=').trim_end_matches(' ');
            return Some((display, i + kb.len()));
        }
    }
    None
}

/// Format command failure without leaking secret-bearing env file contents or
/// Docker stdout/stderr. Prefer fixed categories; only attach a heavily redacted
/// status code when present.
pub fn format_cmd_failure(action: &str, output: &Output) -> String {
    let code = output.status.code().unwrap_or(-1);
    // Intentionally do not forward raw stdout/stderr. Redact any accidental attach.
    let _ = redact_process_text(&String::from_utf8_lossy(&output.stderr));
    let _ = redact_process_text(&String::from_utf8_lossy(&output.stdout));
    format!(
        "{action} failed (status {code}; category={})",
        DockerErrorKind::CommandFailed.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn project_name_is_fixed() {
        assert_eq!(DESKTOP_COMPOSE_PROJECT, "irin-desktop-gateway");
        assert!(validate_compose_project(DESKTOP_COMPOSE_PROJECT).is_ok());
        assert!(validate_compose_project("gateway").is_err());
        assert!(validate_compose_project("irin-desktop-gateway-evil").is_err());
    }

    #[test]
    fn docker_allowlist_is_strict() {
        assert!(validate_docker_cli_path(Path::new("/usr/local/bin/docker")).is_ok());
        assert!(validate_docker_cli_path(Path::new(
            "/Applications/Docker.app/Contents/Resources/bin/docker"
        ))
        .is_ok());
        assert!(validate_docker_cli_path(Path::new("/opt/homebrew/bin/docker")).is_err());
        assert!(validate_docker_cli_path(Path::new("/bin/sh")).is_err());
    }

    #[test]
    fn path_safety_rejects_metachar() {
        assert!(path_is_safe_argv(Path::new("/tmp/ok/compose.yml")));
        assert!(!path_is_safe_argv(Path::new("relative.yml")));
        assert!(!path_is_safe_argv(Path::new("/tmp/$(evil)")));
        assert!(!path_is_safe_argv(Path::new("/tmp/a;rm")));
    }

    #[test]
    fn compose_rejects_foreign_project_via_validate() {
        assert!(validate_compose_project("gateway").is_err());
    }

    #[test]
    fn compose_rejects_bad_subcommand() {
        let tmp = std::env::temp_dir().join(format!("gw-compose-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let compose = tmp.join("docker-compose.yml");
        fs::write(&compose, b"name: irin-desktop-gateway\nservices: {}\n").unwrap();
        let err = compose_command(&compose, None, &["exec", "sh"]).unwrap_err();
        assert!(
            err.contains("not allow-listed") || err.contains("subcommand"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn redactor_removes_whole_gw_key_not_prefix_only() {
        let secret = format!("gw_{}", "a".repeat(32));
        let noisy = format!("docker error: key={secret} trailing");
        let r = redact_process_text(&noisy);
        assert!(!r.contains(&secret), "raw key leaked: {r}");
        assert!(!r.contains(&"a".repeat(32)), "hex suffix leaked: {r}");
        assert!(r.contains("<redacted:gw_key>"), "{r}");
    }

    #[test]
    fn redactor_removes_bootstrap_assignment_value() {
        let token = "f".repeat(64);
        let noisy = format!("BOOTSTRAP_TOKEN={token} failed");
        let r = redact_process_text(&noisy);
        assert!(!r.contains(&token), "bootstrap leaked: {r}");
        assert!(r.contains("BOOTSTRAP_TOKEN=<redacted>"), "{r}");
    }

    #[test]
    fn redactor_adversarial_prefix_replace_is_insufficient_case() {
        // The old bug: replace "gw_" only leaves the hex suffix.
        let secret = format!("gw_{}", "b".repeat(32));
        let naive = secret.replace("gw_", "<redacted>");
        assert!(
            naive.contains(&"b".repeat(32)),
            "sanity: naive replace still has suffix"
        );
        let r = redact_process_text(&secret);
        assert!(!r.contains(&"b".repeat(32)));
        assert_eq!(r, "<redacted:gw_key>");
    }

    #[test]
    fn format_cmd_failure_never_includes_secret_stdout() {
        let secret = format!("gw_{}", "c".repeat(32));
        let output = Output {
            status: ExitStatus::from_raw(1 << 8),
            stdout: format!("leaked {secret}").into_bytes(),
            stderr: format!("BOOTSTRAP_TOKEN={}", "d".repeat(64)).into_bytes(),
        };
        let msg = format_cmd_failure("up", &output);
        assert!(!msg.contains(&secret));
        assert!(!msg.contains(&"d".repeat(64)));
        assert!(!msg.contains("BOOTSTRAP_TOKEN="));
        assert!(msg.contains("docker_command_failed") || msg.contains("failed"));
    }

    #[test]
    fn run_command_timeout_kills_sleep() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let err = run_command_timeout(cmd, Duration::from_millis(400)).unwrap_err();
        assert!(
            err.contains(DockerErrorKind::Timeout.as_str()) || err.contains("timeout"),
            "{err}"
        );
    }

    #[test]
    fn run_command_timeout_fast_exit_no_full_wait() {
        use std::time::Instant;
        let mut cmd = Command::new("/bin/echo");
        cmd.arg("ok");
        let start = Instant::now();
        let out = run_command_timeout(cmd, Duration::from_secs(60)).expect("fast exit");
        assert!(out.status.success());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not join a full-timeout sleeper after success"
        );
    }

    #[test]
    fn compose_env_allowlist_rejects_unknown_key() {
        let tmp = std::env::temp_dir().join(format!("gw-compose-env-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let compose = tmp.join("docker-compose.yml");
        fs::write(&compose, b"name: irin-desktop-gateway\nservices: {}\n").unwrap();
        let mut env = ComposeEnv::new();
        env.insert("EVIL_KEY".into(), "x".into());
        let err =
            compose_command_with_env(&compose, None, &["ps"], Some(&env), Duration::from_secs(5))
                .unwrap_err();
        assert!(err.contains("not allow-listed"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn managed_docker_config_is_minimal_plugin_hints_only() {
        // Only meaningful on Docker Desktop hosts; still validates writer shape.
        if !Path::new(DOCKER_DESKTOP_CLI_PLUGINS).is_dir() {
            return;
        }
        let extras = docker_cli_plugin_extra_dirs();
        assert!(
            extras.contains(&DOCKER_DESKTOP_CLI_PLUGINS),
            "expected Desktop plugin dir in extras: {extras:?}"
        );
        let dir = ensure_managed_docker_config_dir().expect("managed config dir");
        let raw = fs::read_to_string(dir.join("config.json")).expect("config.json");
        assert!(
            raw.contains("cliPluginsExtraDirs"),
            "managed config must declare plugin dirs"
        );
        assert!(
            raw.contains("cli-plugins"),
            "managed config must point at plugin path"
        );
        // Minimal contract: plugin hints only. Operator registry auth,
        // credential helpers, and context pins are never merged in.
        let value: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let object = value.as_object().expect("config object");
        assert!(
            object.keys().all(|k| k == "cliPluginsExtraDirs"),
            "managed config must contain only cliPluginsExtraDirs: {object:?}"
        );
        for key in ["auths", "credsStore", "credHelpers", "currentContext"] {
            assert!(!raw.contains(key), "managed config must not carry {key}");
        }
        // Never embed obvious secret key material in the managed config writer path.
        assert!(!raw.contains("gw_"));
        assert!(!raw.contains("AUTH_PEPPER"));
    }

    /// Collect explicitly-set spawn env overrides. `get_envs` does not list
    /// the inherited parent env, which is exactly the layer under test: an
    /// explicit `cmd.env` is what overrides inheritance on spawn.
    fn spawn_env(cmd: &Command) -> HashMap<String, String> {
        cmd.get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    fn temp_compose_file(tag: &str) -> PathBuf {
        let tmp =
            std::env::temp_dir().join(format!("gw-compose-spawn-{tag}-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let compose = tmp.join("docker-compose.yml");
        fs::write(&compose, b"name: irin-desktop-gateway\nservices: {}\n").unwrap();
        compose
    }

    /// Redirect the managed Docker config write into a temp dir; restores on drop.
    struct SupportRootGuard {
        prev: Option<String>,
        tmp: PathBuf,
    }

    impl SupportRootGuard {
        fn new(tag: &str) -> Self {
            let prev = std::env::var("IRIN_APP_SUPPORT_ROOT").ok();
            let tmp =
                std::env::temp_dir().join(format!("gw-spawn-support-{tag}-{}", std::process::id()));
            let _ = fs::create_dir_all(&tmp);
            std::env::set_var("IRIN_APP_SUPPORT_ROOT", &tmp);
            Self { prev, tmp }
        }
    }

    impl Drop for SupportRootGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("IRIN_APP_SUPPORT_ROOT", v),
                None => std::env::remove_var("IRIN_APP_SUPPORT_ROOT"),
            }
            let _ = fs::remove_dir_all(&self.tmp);
        }
    }

    struct EnvDecoys(&'static [&'static str]);

    impl EnvDecoys {
        fn plant(pairs: &'static [(&'static str, &'static str)]) -> Self {
            for (k, v) in pairs {
                std::env::set_var(k, v);
            }
            Self(
                // SAFETY-free: keys are 'static literals from the caller.
                Box::leak(
                    pairs
                        .iter()
                        .map(|(k, _)| *k)
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                ),
            )
        }
    }

    impl Drop for EnvDecoys {
        fn drop(&mut self) {
            for k in self.0 {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn compose_spawn_forces_pack_pins_over_ambient_decoys() {
        let _g = crate::private_config::test_env_lock();
        let _root = SupportRootGuard::new("pins");
        let hex = "a".repeat(64);
        let pinned_gateway = format!("ghcr.io/irin/gateway@sha256:{hex}");
        let pinned_sidecar = format!("ghcr.io/irin/sidecar@sha256:{hex}");
        // Ambient parent values that Compose precedence would otherwise let
        // beat the --env-file pins.
        let _decoys = EnvDecoys::plant(&[
            ("IRIN_GATEWAY_IMAGE", "evil.example/swapped-gateway:latest"),
            ("IRIN_SIDECAR_IMAGE", "evil.example/swapped-sidecar:latest"),
            ("IRIN_DESKTOP_PACK_ROOT", "/tmp/evil-pack"),
            ("IRIN_DESKTOP_LEDGER_KEY", "/tmp/evil-ledger"),
            ("GATEWAY_AUTH_FAIL_CLOSED", "false"),
        ]);

        let compose = temp_compose_file("pins");
        let mut pins = ComposeEnv::new();
        pins.insert("IRIN_GATEWAY_IMAGE".into(), pinned_gateway.clone());
        pins.insert("IRIN_SIDECAR_IMAGE".into(), pinned_sidecar.clone());
        pins.insert("IRIN_DESKTOP_PACK_ROOT".into(), "/app/pack".into());
        pins.insert("IRIN_DESKTOP_LEDGER_KEY".into(), "/app/ledger".into());
        pins.insert("GATEWAY_AUTH_FAIL_CLOSED".into(), "true".into());

        let cmd = build_compose_command(
            Path::new("/usr/local/bin/docker"),
            &compose,
            None,
            &["config"],
            Some(&pins),
        );
        let env = spawn_env(&cmd);
        assert_eq!(env.get("IRIN_GATEWAY_IMAGE"), Some(&pinned_gateway));
        assert_eq!(env.get("IRIN_SIDECAR_IMAGE"), Some(&pinned_sidecar));
        assert_eq!(
            env.get("IRIN_DESKTOP_PACK_ROOT").map(String::as_str),
            Some("/app/pack")
        );
        assert_eq!(
            env.get("IRIN_DESKTOP_LEDGER_KEY").map(String::as_str),
            Some("/app/ledger")
        );
        assert_eq!(
            env.get("GATEWAY_AUTH_FAIL_CLOSED").map(String::as_str),
            Some("true")
        );
        let _ = fs::remove_dir_all(compose.parent().unwrap());
    }

    #[test]
    fn compose_spawn_disarms_dangerous_keys_and_scrubs_ambient_secrets() {
        let _g = crate::private_config::test_env_lock();
        let _root = SupportRootGuard::new("disarm");
        let _decoys = EnvDecoys::plant(&[
            ("WATCH_ADMIN_TOKEN", "ambient-admin-token"),
            ("COUNCIL_GATEWAY_TOKEN", "ambient-council-token"),
            ("WATCH_PRODUCER_ENABLED", "true"),
            ("WATCH_DISPATCHER_ENABLED", "true"),
            ("XAI_API_KEY", "ambient-provider-key"),
            ("AUTH_PEPPER", "ambient-pepper"),
        ]);
        let compose = temp_compose_file("disarm");

        // Validated pack channel values win over ambient for provider keys…
        let mut extra = ComposeEnv::new();
        extra.insert("XAI_API_KEY".into(), "validated-provider-key".into());
        // …but even an allow-listed caller value can never arm Watch surfaces.
        extra.insert("WATCH_PRODUCER_ENABLED".into(), "true".into());
        let cmd = build_compose_command(
            Path::new("/usr/local/bin/docker"),
            &compose,
            None,
            &["config"],
            Some(&extra),
        );
        let env = spawn_env(&cmd);
        assert_eq!(
            env.get("XAI_API_KEY").map(String::as_str),
            Some("validated-provider-key")
        );
        assert_eq!(env.get("WATCH_ADMIN_TOKEN").map(String::as_str), Some(""));
        assert_eq!(
            env.get("COUNCIL_GATEWAY_TOKEN").map(String::as_str),
            Some("")
        );
        assert_eq!(
            env.get("WATCH_PRODUCER_ENABLED").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            env.get("WATCH_DISPATCHER_ENABLED").map(String::as_str),
            Some("false")
        );

        // Without a pack channel value, ambient secrets are scrubbed to empty.
        let cmd2 = build_compose_command(
            Path::new("/usr/local/bin/docker"),
            &compose,
            None,
            &["config"],
            None,
        );
        let env2 = spawn_env(&cmd2);
        assert_eq!(env2.get("XAI_API_KEY").map(String::as_str), Some(""));
        assert_eq!(env2.get("AUTH_PEPPER").map(String::as_str), Some(""));
        assert_eq!(env2.get("WATCH_ADMIN_TOKEN").map(String::as_str), Some(""));
        assert_eq!(
            env2.get("WATCH_PRODUCER_ENABLED").map(String::as_str),
            Some("false")
        );
        let _ = fs::remove_dir_all(compose.parent().unwrap());
    }

    #[test]
    fn plain_docker_spawn_is_scrubbed_and_disarmed() {
        let _g = crate::private_config::test_env_lock();
        let _root = SupportRootGuard::new("plain");
        let _decoys = EnvDecoys::plant(&[
            ("WATCH_ADMIN_TOKEN", "ambient-admin-token"),
            ("WATCH_PRODUCER_ENABLED", "true"),
            ("OPENAI_API_KEY", "ambient-provider-key"),
        ]);
        let cmd = build_docker_command(
            Path::new("/usr/local/bin/docker"),
            &["info", "--format", "{{.ServerVersion}}"],
        );
        let env = spawn_env(&cmd);
        assert_eq!(env.get("WATCH_ADMIN_TOKEN").map(String::as_str), Some(""));
        assert_eq!(
            env.get("COUNCIL_GATEWAY_TOKEN").map(String::as_str),
            Some("")
        );
        assert_eq!(
            env.get("WATCH_PRODUCER_ENABLED").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            env.get("WATCH_DISPATCHER_ENABLED").map(String::as_str),
            Some("false")
        );
        assert_eq!(env.get("OPENAI_API_KEY").map(String::as_str), Some(""));
    }
}
