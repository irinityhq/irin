//! Key material, atomic 0600 writes, and env-value injection guards.
//!
//! SECURITY-CRITICAL: move-only. Do not "improve" permissions, RNG, or validation.

use super::paths::{arm_keys_path, ensure_gateway_dir, ledger_key_path};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
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

pub(crate) fn ensure_ledger_key() -> Result<PathBuf, String> {
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

/// Ensure the Touch ID enrollment registry file exists so Compose can
/// bind-mount it as a FILE (a missing bind source becomes a directory, which
/// would make the sidecar's registry load fail in a confusing way).
///
/// The default contents are the empty array `[]`, which the sidecar's
/// `AttestKeyRegistry::parse` treats as a **fail-closed unloaded registry** —
/// the correct not-yet-enrolled state. Enabling the pack therefore never
/// creates arming capability; only a completed enrollment ceremony does.
pub fn ensure_arm_keys_file() -> Result<PathBuf, String> {
    ensure_gateway_dir()?;
    let path = arm_keys_path();
    if path.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        return Ok(path);
    }
    write_atomic_0600(&path, b"[]\n")?;
    Ok(path)
}

pub(crate) fn getrandom_fill(buf: &mut [u8]) -> Result<(), String> {
    use std::fs::File;
    use std::io::Read;
    let mut f = File::open("/dev/urandom").map_err(|e| format!("open urandom: {e}"))?;
    f.read_exact(buf).map_err(|e| format!("read urandom: {e}"))
}

pub(crate) fn random_hex(n_bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; n_bytes];
    getrandom_fill(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Reject CR/LF/NUL and other injection-prone characters in env values.
pub fn validate_env_value(key: &str, value: &str) -> Result<(), String> {
    if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return Err(format!("env value for {key} contains forbidden CR/LF/NUL"));
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
        if k.is_empty() || k.bytes().any(|b| !(b.is_ascii_alphanumeric() || b == b'_')) {
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

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
