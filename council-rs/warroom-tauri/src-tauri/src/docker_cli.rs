//! Strict Docker CLI resolution and allow-listed compose invocations.
//!
//! The renderer never supplies executable paths, project names, mount paths,
//! env payloads, or destructive volume targets. Only fixed constants are used.
//! Every native Docker/Compose call is hard wall-clock bounded and killable.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
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

/// Bounded, non-secret failure categories returned to the UI/logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerErrorKind {
    CliMissing,
    DaemonDown,
    Timeout,
    PathRejected,
    SubcommandRejected,
    SpawnFailed,
    CommandFailed,
    ImageMissing,
    ImageDigestMismatch,
    ComposeFailed,
    ProbeFailed,
}

impl DockerErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CliMissing => "docker_cli_missing",
            Self::DaemonDown => "docker_daemon_down",
            Self::Timeout => "docker_timeout",
            Self::PathRejected => "docker_path_rejected",
            Self::SubcommandRejected => "docker_subcommand_rejected",
            Self::SpawnFailed => "docker_spawn_failed",
            Self::CommandFailed => "docker_command_failed",
            Self::ImageMissing => "docker_image_missing",
            Self::ImageDigestMismatch => "docker_image_digest_mismatch",
            Self::ComposeFailed => "docker_compose_failed",
            Self::ProbeFailed => "docker_probe_failed",
        }
    }

    pub fn operator_message(self) -> &'static str {
        match self {
            Self::CliMissing => {
                "Docker CLI not found. Install Docker Desktop, then retry."
            }
            Self::DaemonDown => {
                "Docker Desktop is installed but the daemon is not ready."
            }
            Self::Timeout => {
                "Docker command timed out and was terminated. Retry once Docker is responsive."
            }
            Self::PathRejected => "Internal path rejected by the Docker allow-list.",
            Self::SubcommandRejected => "Compose subcommand rejected by the allow-list.",
            Self::SpawnFailed => "Failed to start the Docker CLI process.",
            Self::CommandFailed => "Docker command failed (details redacted).",
            Self::ImageMissing => "Required Gateway Pack image is not present locally.",
            Self::ImageDigestMismatch => {
                "Local image does not match the Gateway Pack manifest digest."
            }
            Self::ComposeFailed => "Docker Compose operation failed (details redacted).",
            Self::ProbeFailed => "Docker daemon probe failed (details redacted).",
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
    Err(
        "Docker CLI not found. Install Docker Desktop, then retry. \
         Expected /usr/local/bin/docker or \
         /Applications/Docker.app/Contents/Resources/bin/docker."
            .to_string(),
    )
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
    !s.chars().any(|c| matches!(c, '$' | '`' | '\n' | '\r' | ';'))
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
pub fn run_command_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
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

    let child_slot: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(Some(child.id())));
    let child_slot_watch = Arc::clone(&child_slot);
    let timed_out = Arc::new(Mutex::new(false));
    let timed_out_watch = Arc::clone(&timed_out);
    let killer = thread::spawn(move || {
        thread::sleep(timeout);
        let mut flag = timed_out_watch.lock().unwrap_or_else(|e| e.into_inner());
        *flag = true;
        drop(flag);
        if let Ok(mut slot) = child_slot_watch.lock() {
            if let Some(pid) = slot.take() {
                // Best-effort kill of the timed-out child.
                #[cfg(unix)]
                {
                    let _ = unsafe { libc_kill(pid as i32, 15) }; // SIGTERM
                    thread::sleep(Duration::from_millis(200));
                    let _ = unsafe { libc_kill(pid as i32, 9) }; // SIGKILL
                }
                #[cfg(not(unix))]
                {
                    let _ = pid;
                }
            }
        }
    });

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            if let Ok(mut slot) = child_slot.lock() {
                slot.take();
            }
            let _ = killer.join();
            return Err(format!("{}: wait failed: {e}", DockerErrorKind::SpawnFailed.as_str()));
        }
    };

    // Prevent killer from SIGKILLing a recycled PID after wait returns.
    if let Ok(mut slot) = child_slot.lock() {
        slot.take();
    }

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    let _ = killer.join();

    let was_timeout = timed_out
        .lock()
        .map(|g| *g)
        .unwrap_or(false);
    if was_timeout && !status.success() {
        return Err(DockerErrorKind::Timeout.as_str().to_string());
    }

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    // libc is available via std on macOS through the system; use raw syscall-ish kill.
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

/// Probe Docker CLI presence and daemon readiness (no secrets in output).
/// Hard-bounded — a wedged `docker info` cannot hang the app forever.
pub fn probe_docker_daemon() -> DockerDaemonState {
    let Ok(docker) = resolve_docker_cli() else {
        return DockerDaemonState::CliMissing;
    };
    let mut cmd = Command::new(&docker);
    cmd.args(["info", "--format", "{{.ServerVersion}}"]);
    match run_command_timeout(cmd, Duration::from_secs(12)) {
        Ok(o) if o.status.success() => DockerDaemonState::Ready,
        Ok(_) => DockerDaemonState::DaemonDown,
        Err(e) if e.contains(DockerErrorKind::Timeout.as_str()) => DockerDaemonState::DaemonDown,
        Err(_) => DockerDaemonState::DaemonDown,
    }
}

/// Optional extra environment for compose (secrets / provider keys). Values never
/// appear in argv and are not returned in error messages.
pub type ComposeEnv = HashMap<String, String>;

/// Run `docker <args…>` with the allow-listed binary. Never routes through a shell.
pub fn docker_command(args: &[&str]) -> Result<Output, String> {
    docker_command_timeout(args, DOCKER_CMD_TIMEOUT)
}

pub fn docker_command_timeout(args: &[&str], timeout: Duration) -> Result<Output, String> {
    let docker = resolve_docker_cli()?;
    validate_docker_cli_path(&docker)?;
    let mut cmd = Command::new(&docker);
    cmd.args(args);
    run_command_timeout(cmd, timeout)
}

/// Fixed-shape compose invocation for the desktop project only.
///
/// - `compose_file` and `env_file` must be absolute safe paths
/// - project is always `irin-desktop-gateway`
/// - extra args are an allow-listed subcommand set (up/down/ps/…)
/// - `extra_env` is a strict allow-list of env keys (never logged)
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
    validate_compose_project(DESKTOP_COMPOSE_PROJECT)?;
    if !path_is_safe_argv(compose_file) {
        return Err(DockerErrorKind::PathRejected.operator_message().to_string());
    }
    if !compose_file.is_file() {
        return Err(format!(
            "compose file missing: {}",
            compose_file.display()
        ));
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

    let docker = resolve_docker_cli()?;
    validate_docker_cli_path(&docker)?;

    let mut cmd = Command::new(&docker);
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
    run_command_timeout(cmd, timeout)
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
            && bytes[i + 3..i + 35]
                .iter()
                .all(|c| c.is_ascii_hexdigit())
        {
            out.push_str("<redacted:gw_key>");
            i += 35;
            continue;
        }
        // sk-… openai-ish
        if i + 3 <= bytes.len() && &bytes[i..i + 3] == b"sk-" {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-' || bytes[j] == b'_') {
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
                    && !matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r' | b';' | b',' | b')' | b']')
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

/// Operator-safe error string for timeout/daemon cases.
pub fn format_docker_error(kind: DockerErrorKind, action: &str) -> String {
    format!("{action}: {} ({})", kind.operator_message(), kind.as_str())
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
        assert!(err.contains("not allow-listed") || err.contains("subcommand"), "{err}");
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
    fn compose_env_allowlist_rejects_unknown_key() {
        let tmp = std::env::temp_dir().join(format!("gw-compose-env-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let compose = tmp.join("docker-compose.yml");
        fs::write(&compose, b"name: irin-desktop-gateway\nservices: {}\n").unwrap();
        let mut env = ComposeEnv::new();
        env.insert("EVIL_KEY".into(), "x".into());
        let err = compose_command_with_env(
            &compose,
            None,
            &["ps"],
            Some(&env),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.contains("not allow-listed"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }
}
