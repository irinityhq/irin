//! Council-rs path resolution and CORS origin helpers for the War Room desktop shell.

use std::path::{Path, PathBuf};

/// Default `--serve` port for the council sidecar.
pub const DEFAULT_SERVE_PORT: u16 = 8765;

/// Resolve the worktree-aware default Council port inherited by the desktop
/// launcher. Ordinary installs retain 8765.
pub fn default_serve_port() -> Result<u16, String> {
    match std::env::var("IRIN_COUNCIL_PORT") {
        Ok(raw) if !raw.trim().is_empty() => {
            let port = raw.trim().parse::<u16>().map_err(|_| {
                format!("IRIN_COUNCIL_PORT must be a non-zero TCP port (got {raw:?})")
            })?;
            validate_serve_port(port)?;
            Ok(port)
        }
        _ => Ok(DEFAULT_SERVE_PORT),
    }
}

/// Resolve the council-rs repository root.
///
/// Priority: non-empty `COUNCIL_RS_DIR` env → parent of `warroom-tauri/` derived from
/// `CARGO_MANIFEST_DIR` (`src-tauri` → `warroom-tauri` → repo root).
pub fn resolve_council_rs_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("COUNCIL_RS_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    council_rs_dir_from_manifest(env!("CARGO_MANIFEST_DIR"))
}

fn council_rs_dir_from_manifest(manifest_dir: &str) -> PathBuf {
    let manifest = Path::new(manifest_dir);
    manifest
        .parent()
        .and_then(|warroom_tauri| warroom_tauri.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest.to_path_buf())
}

/// Default release council binary under the resolved repo root.
///
/// On a standalone council-rs checkout the binary lands in `{root}/target/`.
/// In a mono-repo checkout council-rs is a workspace member, so cargo puts the
/// binary in the workspace root's `target/` instead — walk up until a directory
/// with both a `Cargo.toml` and a built `target/release/council` is found.
pub fn default_council_binary(council_rs_dir: &Path) -> PathBuf {
    let local = council_rs_dir.join("target/release/council");
    if local.is_file() {
        return local;
    }
    let mut dir = council_rs_dir.parent();
    while let Some(d) = dir {
        if d.join("Cargo.toml").is_file() {
            let workspace = d.join("target/release/council");
            if workspace.is_file() {
                return workspace;
            }
        }
        dir = d.parent();
    }
    local
}

pub fn default_council_binary_path() -> PathBuf {
    default_council_binary(&resolve_council_rs_dir())
}

/// Comma-separated `COUNCIL_CORS_ORIGINS` value for Tauri + browser dev ports.
pub fn build_cors_origins(port: u16) -> String {
    format!(
        "tauri://localhost,\
         https://tauri.localhost,\
         http://localhost:3000,\
         http://127.0.0.1:3000,\
         http://localhost:3010,\
         http://127.0.0.1:3010,\
         http://localhost:{port},\
         http://127.0.0.1:{port}"
    )
    .replace([' ', '\n'], "")
}

/// Validate a loopback Council port supplied by the embedded runtime config.
pub fn validate_serve_port(port: u16) -> Result<(), String> {
    if port != 0 {
        Ok(())
    } else {
        Err("War Room desktop council --serve port must be non-zero".to_string())
    }
}

/// Resolve the council release binary: only `{COUNCIL_RS_DIR}/target/release/council`.
///
/// Whitespace-only `explicit` is treated as absent (default path). The resolved path is
/// canonicalized and must match the allowed release binary location.
pub fn resolve_council_binary(explicit: Option<&str>) -> Result<PathBuf, String> {
    let root = resolve_council_rs_dir();
    let allowed = default_council_binary(&root);

    let candidate = match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => default_council_binary_path(),
    };

    if !candidate.is_file() {
        return Err(format!(
            "council binary not found at {}. Build with: cd {} && cargo build --release",
            candidate.display(),
            root.display()
        ));
    }

    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("failed to canonicalize council binary path: {e}"))?;

    let allowed_canonical = allowed
        .canonicalize()
        .map_err(|e| format!("failed to canonicalize allowed council binary: {e}"))?;

    if canonical != allowed_canonical {
        return Err(format!(
            "council binary must be the release build at {} (got {})",
            allowed_canonical.display(),
            canonical.display()
        ));
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn restore_council_rs_dir(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var("COUNCIL_RS_DIR", v),
            None => std::env::remove_var("COUNCIL_RS_DIR"),
        }
    }

    /// Minimal tree: `{root}/target/release/council` (empty file).
    fn write_release_council_at(root: &Path) -> PathBuf {
        let bin = default_council_binary(root);
        if let Some(parent) = bin.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&bin, b"").unwrap();
        bin
    }

    #[test]
    fn default_council_binary_falls_back_to_workspace_target() {
        let tmp = std::env::temp_dir().join(format!("council-ws-fallback-{}", std::process::id()));
        let member = tmp.join("council-rs");
        fs::create_dir_all(&member).unwrap();
        fs::write(tmp.join("Cargo.toml"), b"[workspace]\n").unwrap();
        let ws_bin = tmp.join("target/release/council");
        fs::create_dir_all(ws_bin.parent().unwrap()).unwrap();
        fs::write(&ws_bin, b"").unwrap();

        assert_eq!(default_council_binary(&member), ws_bin);

        // A binary in the member's own target/ still wins over the workspace one.
        let local = member.join("target/release/council");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, b"").unwrap();
        assert_eq!(default_council_binary(&member), local);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_council_rs_dir_from_manifest_parent_chain() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        std::env::remove_var("COUNCIL_RS_DIR");

        let manifest = env!("CARGO_MANIFEST_DIR");
        let expected = council_rs_dir_from_manifest(manifest);
        assert_eq!(resolve_council_rs_dir(), expected);
        assert!(
            resolve_council_rs_dir()
                .join("warroom-tauri")
                .join("src-tauri")
                .exists(),
            "expected council-rs root to contain warroom-tauri/src-tauri"
        );

        restore_council_rs_dir(prev);
    }

    #[test]
    fn resolve_council_rs_dir_honors_env_override() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        let tmp = std::env::temp_dir().join(format!("council-rs-dir-test-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("COUNCIL_RS_DIR", &tmp);
        assert_eq!(resolve_council_rs_dir(), tmp);
        let _ = fs::remove_dir_all(&tmp);
        restore_council_rs_dir(prev);
    }

    #[test]
    fn resolve_council_rs_dir_trims_whitespace_env() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        let tmp = std::env::temp_dir().join(format!("council-rs-dir-trim-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let padded = format!("  {}  ", tmp.display());
        std::env::set_var("COUNCIL_RS_DIR", &padded);
        assert_eq!(resolve_council_rs_dir(), tmp);
        let _ = fs::remove_dir_all(&tmp);
        restore_council_rs_dir(prev);
    }

    #[test]
    fn default_council_binary_under_target_release() {
        let root = PathBuf::from("/tmp/council-rs-test");
        assert_eq!(
            default_council_binary(&root),
            PathBuf::from("/tmp/council-rs-test/target/release/council")
        );
    }

    #[test]
    fn build_cors_origins_includes_tauri_and_dev_ports() {
        let origins = build_cors_origins(8765);
        assert!(origins.contains("tauri://localhost"));
        assert!(origins.contains("https://tauri.localhost"));
        assert!(!origins.contains("1420"));
        assert!(origins.contains("http://127.0.0.1:3000"));
        assert!(origins.contains("http://localhost:3010"));
        assert!(origins.contains("http://localhost:8765"));
        assert!(origins.contains("http://127.0.0.1:8765"));
        assert!(!origins.contains(' '));
    }

    #[test]
    fn validate_serve_port_accepts_isolated_worktree_ports() {
        assert!(validate_serve_port(DEFAULT_SERVE_PORT).is_ok());
        assert!(validate_serve_port(20_321).is_ok());
        assert!(validate_serve_port(0).is_err());
    }

    #[test]
    fn default_serve_port_honors_worktree_env() {
        let _guard = env_lock();
        let previous = std::env::var("IRIN_COUNCIL_PORT").ok();
        std::env::set_var("IRIN_COUNCIL_PORT", "20321");
        assert_eq!(default_serve_port().unwrap(), 20_321);
        std::env::set_var("IRIN_COUNCIL_PORT", "0");
        assert!(default_serve_port().is_err());
        match previous {
            Some(value) => std::env::set_var("IRIN_COUNCIL_PORT", value),
            None => std::env::remove_var("IRIN_COUNCIL_PORT"),
        }
    }

    #[test]
    fn build_cors_origins_includes_custom_port() {
        let origins = build_cors_origins(9999);
        assert!(origins.contains("http://localhost:9999"));
        assert!(origins.contains("http://127.0.0.1:9999"));
    }

    #[test]
    fn resolve_council_binary_errors_when_missing() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        let tmp = std::env::temp_dir().join(format!("council-bin-missing-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("COUNCIL_RS_DIR", &tmp);
        let err = resolve_council_binary(None).unwrap_err();
        assert!(err.contains("council binary not found"));
        assert!(err.contains("cargo build --release"));
        let _ = fs::remove_dir_all(&tmp);
        restore_council_rs_dir(prev);
    }

    #[test]
    fn resolve_council_binary_empty_explicit_uses_default() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        let tmp =
            std::env::temp_dir().join(format!("council-bin-empty-explicit-{}", std::process::id()));
        let bin = write_release_council_at(&tmp);
        std::env::set_var("COUNCIL_RS_DIR", &tmp);

        let got = resolve_council_binary(Some("   ")).unwrap();
        assert_eq!(got, bin.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&tmp);
        restore_council_rs_dir(prev);
    }

    #[test]
    fn resolve_council_binary_rejects_path_outside_release() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        let tmp = std::env::temp_dir().join(format!("council-bin-reject-{}", std::process::id()));
        write_release_council_at(&tmp);
        let rogue = tmp.join("rogue-council");
        fs::write(&rogue, b"").unwrap();
        std::env::set_var("COUNCIL_RS_DIR", &tmp);

        let err = resolve_council_binary(Some(rogue.to_str().unwrap())).unwrap_err();
        assert!(err.contains("council binary must be the release build"));

        let _ = fs::remove_dir_all(&tmp);
        restore_council_rs_dir(prev);
    }

    #[test]
    fn resolve_council_binary_accepts_release_path() {
        let _guard = env_lock();
        let prev = std::env::var("COUNCIL_RS_DIR").ok();
        let tmp = std::env::temp_dir().join(format!("council-bin-ok-{}", std::process::id()));
        let bin = write_release_council_at(&tmp);
        std::env::set_var("COUNCIL_RS_DIR", &tmp);

        let got = resolve_council_binary(Some(bin.to_str().unwrap())).unwrap();
        assert_eq!(got, bin.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&tmp);
        restore_council_rs_dir(prev);
    }
}
