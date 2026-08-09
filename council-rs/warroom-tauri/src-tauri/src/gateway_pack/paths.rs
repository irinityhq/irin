//! App-owned Gateway Pack paths and install marker checks.

use crate::paths::{bundled_base_dir, executable_dir};
use crate::private_config::app_support_dir;
use std::fs;
use std::path::PathBuf;

const GATEWAY_DIR_NAME: &str = "gateway";
const PUBLIC_ENV_NAME: &str = "compose.public.env";
const LEDGER_KEY_NAME: &str = "ledger_key";
const INSTALLED_MARKER: &str = "pack-installed.json";
const ARM_KEYS_NAME: &str = "arm_attest_keys.json";
const SENTINELS_DIR_NAME: &str = "sentinels";
const WATCH_INBOX_DIR_NAME: &str = "inbox";
const WATCH_PROFILE_FILE_NAME: &str = "sentinels.yaml";
pub(crate) const PACK_DIR_NAME: &str = "pack";

/// In-container path for the installed watch profile (when present).
pub const WATCH_PROFILE_CONTAINER_PATH: &str = "/var/lib/gateway/sentinels/sentinels.yaml";

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

/// Host path of the app-owned Touch ID enrollment registry (public credential
/// records only — credential ids, SEC1 public keys, labels, timestamps). It
/// lives beside the ledger key in the 0700 app gateway dir and is bind-mounted
/// read-only into both desktop-pack containers at [`ARM_KEYS_CONTAINER_PATH`].
/// The edge uses mount existence only as its desktop-only bridge signal; the
/// sidecar alone parses the registry and enforces attestation.
///
/// It is deliberately NOT inside the pack tree: that tree is hash-verified
/// against the install marker on every spawn, so an enrollment written there
/// would be treated as tampering and re-staged away.
pub fn arm_keys_path() -> PathBuf {
    gateway_data_dir().join(ARM_KEYS_NAME)
}

/// App-support directory for the installed watch profile (ro bind source).
/// Present `sentinels.yaml` inside this dir is the durable watch enable switch.
pub fn sentinels_dir() -> PathBuf {
    gateway_data_dir().join(SENTINELS_DIR_NAME)
}

/// Host path of the installed watch profile file (may be absent = disabled).
pub fn watch_profile_path() -> PathBuf {
    sentinels_dir().join(WATCH_PROFILE_FILE_NAME)
}

/// App-support inbox directory (rw bind source for file-inbox sentinel).
pub fn watch_inbox_dir() -> PathBuf {
    gateway_data_dir().join(WATCH_INBOX_DIR_NAME)
}

/// Ensure sentinels + inbox bind sources exist as directories before compose up.
/// Does **not** install a profile — presence of `sentinels.yaml` is the toggle.
pub fn ensure_watch_dirs() -> Result<(), String> {
    ensure_gateway_dir()?;
    for dir in [sentinels_dir(), watch_inbox_dir()] {
        if dir.is_file() {
            return Err(format!(
                "watch bind source must be a directory, found file: {}",
                dir.display()
            ));
        }
        fs::create_dir_all(&dir).map_err(|e| format!("create watch dir {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

/// Where the registry is mounted inside both desktop-pack containers. Also the
/// sidecar value of the admitted `GW_ARM_ATTEST_KEYS_PATH` pin.
pub const ARM_KEYS_CONTAINER_PATH: &str = "/run/secrets/arm_attest_keys.json";

pub fn installed_marker_path() -> PathBuf {
    gateway_data_dir().join(INSTALLED_MARKER)
}

/// Bundled pack root under app Resources (or debug-build test override).
/// Bundled assets alone do **not** mean installed — see [`is_pack_installed`].
///
/// `IRIN_GATEWAY_PACK_ROOT` is a unit-test escape hatch, never a production
/// input: it is honored only in debug builds (which covers `cargo test`).
/// Packaged release builds ignore it unconditionally — a production install
/// always uses its bundled Resources, so an environment-selected Compose
/// definition can never reach the Keychain/provider secret boundary.
pub fn bundled_pack_root() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        if let Ok(override_dir) = std::env::var("IRIN_GATEWAY_PACK_ROOT") {
            let p = PathBuf::from(override_dir.trim());
            if p.join("docker-compose.yml").is_file() {
                return Some(p);
            }
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
    let repo_pack =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../packaging/gateway-pack");
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

pub(crate) fn ensure_gateway_dir() -> Result<PathBuf, String> {
    let dir = gateway_data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create gateway dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}
