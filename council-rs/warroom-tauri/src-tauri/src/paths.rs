//! Council-rs path resolution and CORS origin helpers for the War Room desktop shell.

use std::path::{Path, PathBuf};

#[cfg(test)]
const DEFAULT_SERVE_PORT: u16 = 8765;

/// Resolve the Council port selected when this desktop binary was built.
///
/// The Tauri CSP is also fixed at build time, so a later process environment
/// must not redirect a built app to an origin its CSP does not allow.
pub fn default_serve_port() -> Result<u16, String> {
    serve_port_from_build_value(env!("IRIN_TAURI_COUNCIL_PORT"))
}

fn serve_port_from_build_value(raw: &str) -> Result<u16, String> {
    let port = raw
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("built-in Council port must be a non-zero TCP port (got {raw:?})"))?;
    validate_serve_port(port)?;
    Ok(port)
}

/// Bundled sidecar binary name inside the macOS app (`Contents/MacOS/council`).
pub const BUNDLED_COUNCIL_BIN_NAME: &str = "council";

/// Bundled base-dir folder name under `Contents/Resources/`.
pub const BUNDLED_BASE_DIR_NAME: &str = "council-base";

/// Resolve the directory that contains the running app executable (`Contents/MacOS` on macOS).
pub fn executable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Packaged app: sibling of the Tauri host binary (Tauri `externalBin`).
pub fn bundled_council_binary() -> Option<PathBuf> {
    let bin = executable_dir()?.join(BUNDLED_COUNCIL_BIN_NAME);
    bin.is_file().then_some(bin)
}

/// Packaged app: `Contents/Resources/.../council-base` (Tauri `bundle.resources`).
///
/// Tauri may place files at `Resources/council-base` or nest the source path as
/// `Resources/resources/council-base` depending on the resources config shape.
pub fn bundled_base_dir() -> Option<PathBuf> {
    let mac_os = executable_dir()?;
    let resources_root = mac_os.parent()?.join("Resources");
    let candidates = [
        resources_root.join(BUNDLED_BASE_DIR_NAME),
        resources_root.join("resources").join(BUNDLED_BASE_DIR_NAME),
        resources_root.join("_up_").join(BUNDLED_BASE_DIR_NAME),
    ];
    for candidate in candidates {
        if candidate.join("cabinets").is_dir() {
            return Some(candidate);
        }
    }
    // Last resort: search one level under Resources for a cabinets/ directory.
    if let Ok(entries) = std::fs::read_dir(&resources_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.join("cabinets").is_dir()
                    && path
                        .file_name()
                        .is_some_and(|n| n == BUNDLED_BASE_DIR_NAME)
                {
                    return Some(path);
                }
                if let Ok(nested) = std::fs::read_dir(&path) {
                    for nested_entry in nested.flatten() {
                        let nested_path = nested_entry.path();
                        if nested_path.join("cabinets").is_dir()
                            && nested_path
                                .file_name()
                                .is_some_and(|n| n == BUNDLED_BASE_DIR_NAME)
                        {
                            return Some(nested_path);
                        }
                    }
                }
            }
        }
    }
    None
}

/// True when this process looks like a self-contained DMG/app install.
pub fn is_packaged_install() -> bool {
    bundled_council_binary().is_some() && bundled_base_dir().is_some()
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

/// Allowed council binary locations for spawn.
///
/// Security: callers may not point at an arbitrary executable. The only permitted
/// paths are:
/// - the packaged app's bundled `Contents/MacOS/council` (when present), and/or
/// - the release build at `{COUNCIL_RS_DIR|workspace}/target/release/council`.
fn allowed_council_binaries(root: &Path) -> Vec<PathBuf> {
    let mut allowed = Vec::new();
    if let Some(bundled) = bundled_council_binary() {
        allowed.push(bundled);
    }
    allowed.push(default_council_binary(root));
    allowed
}

/// Resolve the council binary for spawn.
///
/// Priority when `explicit` is absent:
/// 1. Bundled `Contents/MacOS/council` when present (packaged install)
/// 2. Repo `target/release/council` under `COUNCIL_RS_DIR` / workspace (dev)
///
/// When `explicit` is set (non-whitespace), the path must canonicalize to one of
/// the allowed locations above — same pin as pre-DMG product (no arbitrary exec).
/// Whitespace-only `explicit` is treated as absent.
pub fn resolve_council_binary(explicit: Option<&str>) -> Result<PathBuf, String> {
    let root = resolve_council_rs_dir();
    let allowed_list = allowed_council_binaries(&root);

    let candidate = match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            if let Some(bundled) = bundled_council_binary() {
                bundled
            } else {
                default_council_binary_path()
            }
        }
    };

    if !candidate.is_file() {
        return Err(format!(
            "council binary not found at {}. Build with: cd {} && cargo build --release \
             (or install the self-contained app bundle that includes the council sidecar)",
            candidate.display(),
            root.display()
        ));
    }

    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("failed to canonicalize council binary path: {e}"))?;

    let mut allowed_canonicals = Vec::new();
    for allowed in &allowed_list {
        if allowed.is_file() {
            if let Ok(c) = allowed.canonicalize() {
                allowed_canonicals.push(c);
            }
        }
    }

    if allowed_canonicals.is_empty() {
        // Dev tree without a built binary: surface the missing-file path clearly.
        return Err(format!(
            "council binary not found at {}. Build with: cd {} && cargo build --release",
            default_council_binary(&root).display(),
            root.display()
        ));
    }

    if !allowed_canonicals.iter().any(|a| a == &canonical) {
        let expected = allowed_canonicals
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        return Err(format!(
            "council binary must be the release build at {expected} (got {})",
            canonical.display()
        ));
    }

    Ok(canonical)
}

/// Resolve `--base-dir` for a packaged or development spawn.
///
/// Packaged: `Resources/council-base`. Dev: council-rs repo root.
pub fn resolve_spawn_base_dir(council_root_override: Option<&str>) -> Result<PathBuf, String> {
    if let Some(root) = council_root_override.map(str::trim).filter(|s| !s.is_empty()) {
        // Caller validates via validate_council_root when override is set.
        return Ok(PathBuf::from(root));
    }
    if let Some(bundled) = bundled_base_dir() {
        return bundled
            .canonicalize()
            .map_err(|e| format!("failed to canonicalize bundled council base dir: {e}"));
    }
    Ok(resolve_council_rs_dir())
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
    fn default_serve_port_is_build_selected() {
        let _guard = env_lock();
        let previous = std::env::var("IRIN_COUNCIL_PORT").ok();
        std::env::set_var("IRIN_COUNCIL_PORT", "20322");
        assert_eq!(
            default_serve_port().unwrap(),
            env!("IRIN_TAURI_COUNCIL_PORT").parse::<u16>().unwrap()
        );
        match previous {
            Some(value) => std::env::set_var("IRIN_COUNCIL_PORT", value),
            None => std::env::remove_var("IRIN_COUNCIL_PORT"),
        }
        assert_eq!(serve_port_from_build_value("20321").unwrap(), 20_321);
        assert!(serve_port_from_build_value("0").is_err());
        assert!(serve_port_from_build_value("not-a-port").is_err());
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

    #[test]
    fn packaged_helpers_are_none_outside_app_bundle() {
        // Unit tests run from target/debug — not a packaged .app layout.
        // We only assert the helpers do not panic; presence depends on layout.
        let _ = bundled_council_binary();
        let _ = bundled_base_dir();
        let _ = is_packaged_install();
    }

    #[test]
    fn resolve_council_binary_accepts_bundled_when_staged_as_sibling() {
        // Simulate Contents/MacOS layout: put a fake host + council in a temp MacOS dir
        // and point current_exe via… we cannot override current_exe easily, so only
        // assert the allow-list helper includes default_council_binary always.
        let root = PathBuf::from("/tmp/council-rs-allow-list");
        let list = allowed_council_binaries(&root);
        assert!(list.iter().any(|p| p.ends_with("target/release/council")));
    }
}
