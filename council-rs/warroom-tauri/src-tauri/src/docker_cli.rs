//! Strict Docker CLI resolution and allow-listed compose invocations.
//!
//! The renderer never supplies executable paths, project names, mount paths,
//! env payloads, or destructive volume targets. Only fixed constants are used.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Fixed Compose project name for the desktop Gateway Pack.
pub const DESKTOP_COMPOSE_PROJECT: &str = "irin-desktop-gateway";

/// Fixed Gateway loopback URL for the bundled Council child.
pub const DESKTOP_GATEWAY_URL: &str = "http://127.0.0.1:18080";

/// Fixed published host port (compose also pins 127.0.0.1:18080:8080).
#[allow(dead_code)]
pub const DESKTOP_GATEWAY_PORT: u16 = 18080;

/// Allow-listed absolute paths for the Docker CLI only.
pub const DOCKER_CLI_ALLOWLIST: &[&str] = &[
    "/usr/local/bin/docker",
    "/Applications/Docker.app/Contents/Resources/bin/docker",
];

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

/// Probe Docker CLI presence and daemon readiness (no secrets in output).
pub fn probe_docker_daemon() -> DockerDaemonState {
    let Ok(docker) = resolve_docker_cli() else {
        return DockerDaemonState::CliMissing;
    };
    let output = Command::new(&docker)
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(o) if o.status.success() => DockerDaemonState::Ready,
        _ => DockerDaemonState::DaemonDown,
    }
}

/// Run `docker <args…>` with the allow-listed binary. Never routes through a shell.
pub fn docker_command(args: &[&str]) -> Result<Output, String> {
    let docker = resolve_docker_cli()?;
    validate_docker_cli_path(&docker)?;
    Command::new(&docker)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to execute docker: {e}"))
}

/// Fixed-shape compose invocation for the desktop project only.
///
/// - `compose_file` and `env_file` must be absolute safe paths
/// - project is always `irin-desktop-gateway`
/// - extra args are an allow-listed subcommand set (up/down/ps/…)
pub fn compose_command(
    compose_file: &Path,
    env_file: Option<&Path>,
    extra: &[&str],
) -> Result<Output, String> {
    validate_compose_project(DESKTOP_COMPOSE_PROJECT)?;
    if !path_is_safe_argv(compose_file) {
        return Err("compose file path rejected".to_string());
    }
    if !compose_file.is_file() {
        return Err(format!(
            "compose file missing: {}",
            compose_file.display()
        ));
    }
    if let Some(ef) = env_file {
        if !path_is_safe_argv(ef) {
            return Err("env file path rejected".to_string());
        }
        if !ef.is_file() {
            return Err(format!("env file missing: {}", ef.display()));
        }
    }

    // Allow-list first token of extra (subcommand).
    let sub = extra.first().copied().unwrap_or("");
    if !COMPOSE_SUBCOMMAND_ALLOWLIST.contains(&sub) {
        return Err(format!("compose subcommand not allow-listed: {sub}"));
    }
    for a in extra.iter().skip(1) {
        if a.starts_with('-') {
            if !COMPOSE_FLAG_ALLOWLIST.iter().any(|f| a == f || a.starts_with(&format!("{f}="))) {
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
                    return Err(format!("compose flag not allow-listed: {a}"));
                }
            }
        } else if a.contains("..") || a.contains('$') || a.contains('`') {
            return Err(format!("compose arg rejected: {a}"));
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to execute docker compose: {e}"))
}

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

/// Format command failure without leaking secret-bearing env file contents.
pub fn format_cmd_failure(action: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Scrub common secret-looking tokens from docker noise (best-effort).
    let scrub = |s: &str| -> String {
        let mut out = s.to_string();
        for pat in ["gw_", "BOOTSTRAP_TOKEN=", "AUTH_PEPPER=", "GW_API_KEY="] {
            if out.contains(pat) {
                out = out.replace(pat, "<redacted>");
            }
        }
        if out.len() > 800 {
            out.truncate(800);
            out.push_str("…");
        }
        out
    };
    format!(
        "{action} failed (status {:?}): {} {}",
        output.status.code(),
        scrub(&stderr),
        scrub(&stdout)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
        // compose_command always uses DESKTOP_COMPOSE_PROJECT; foreign names cannot be passed.
        assert!(validate_compose_project("gateway").is_err());
    }

    #[test]
    fn compose_rejects_bad_subcommand() {
        let tmp = std::env::temp_dir().join(format!("gw-compose-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let compose = tmp.join("docker-compose.yml");
        fs::write(&compose, b"name: irin-desktop-gateway\nservices: {}\n").unwrap();
        let err = compose_command(&compose, None, &["exec", "sh"]).unwrap_err();
        assert!(err.contains("not allow-listed"), "{err}");
        let _ = fs::remove_dir_all(&tmp);
    }
}
