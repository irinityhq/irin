// ==========================================================================
// socket.rs — management UDS permission boundary.
//
// The sidecar's Unix Domain Socket carries the FULL management surface,
// including the arm/admin routes. nginx has NO /watch/admin/ location, so the
// file mode is the FIRST and (for non-arm callers) ONLY isolation boundary
// against other local processes. This module owns that boundary as a small,
// pure, fail-CLOSED config seam so the default and the parse/validate logic
// are unit-testable WITHOUT mutating process-global env (parallel-test safe,
// mirroring the `producer_gate_armed_from` / `LeaseOpts::from_env` precedent).
//
// SECURITY INVARIANT: a malformed SIDECAR_SOCKET_MODE / SIDECAR_SOCKET_GID
// MUST refuse startup. There is NO fallback to a looser mode (never 0o777).
// The tightened default is 0o660 (owner+group rw, world none).
// ==========================================================================

use std::fmt;

/// Default socket mode: owner + group read/write,
/// world none. Compose mode reaches the socket via a shared group (see
/// `SIDECAR_SOCKET_GID` + docker-compose.yml); host mode (same uid) needs only
/// the owner bit. The previous 0o777 (world-rwx) gave NO isolation and is gone.
pub const DEFAULT_SOCKET_MODE: u32 = 0o660;

/// Env var names (single source of truth so main.rs and tests agree).
pub const SIDECAR_SOCKET_MODE_VAR: &str = "SIDECAR_SOCKET_MODE";
pub const SIDECAR_SOCKET_GID_VAR: &str = "SIDECAR_SOCKET_GID";

/// Fail-closed config error. Carries the offending value so the operator gets
/// an actionable message at boot instead of a silent wrong-mode bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketConfigError {
    /// SIDECAR_SOCKET_MODE was not a valid octal permission string.
    BadMode { raw: String },
    /// SIDECAR_SOCKET_MODE parsed but set bits outside the 12-bit perm range
    /// (0o7777 — incl. setuid/setgid/sticky). Refused as almost-certainly a typo.
    ModeOutOfRange { value: u32 },
    /// SIDECAR_SOCKET_GID was set but not a valid non-negative integer gid.
    BadGid { raw: String },
}

impl fmt::Display for SocketConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocketConfigError::BadMode { raw } => write!(
                f,
                "{SIDECAR_SOCKET_MODE_VAR}={raw:?} is not a valid octal mode \
                 (e.g. \"0660\"); refusing to start — this is a security \
                 control and never falls back to a looser mode"
            ),
            SocketConfigError::ModeOutOfRange { value } => write!(
                f,
                "{SIDECAR_SOCKET_MODE_VAR} resolved to {value:#o} which sets bits \
                 outside the 0o7777 permission range; refusing to start"
            ),
            SocketConfigError::BadGid { raw } => write!(
                f,
                "{SIDECAR_SOCKET_GID_VAR}={raw:?} is not a valid gid (non-negative \
                 integer); refusing to start"
            ),
        }
    }
}

impl std::error::Error for SocketConfigError {}

/// Parse a socket mode string (octal, with or without a leading `0`/`0o`).
/// Fail-closed: any unparseable or out-of-range value is an error, NEVER a
/// fallback. Accepts e.g. "0660", "660", "0o660".
pub fn parse_socket_mode(raw: &str) -> Result<u32, SocketConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SocketConfigError::BadMode {
            raw: raw.to_string(),
        });
    }
    // Strip an optional "0o" prefix; a bare leading "0" is fine for from_str_radix.
    let digits = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
        .unwrap_or(trimmed);
    let value = u32::from_str_radix(digits, 8).map_err(|_| SocketConfigError::BadMode {
        raw: raw.to_string(),
    })?;
    // 0o7777 = setuid|setgid|sticky + rwxrwxrwx. Anything above is a typo
    // (e.g. decimal 660 read as octal would already be caught; this guards
    // a stray extra digit like "06600").
    if value > 0o7777 {
        return Err(SocketConfigError::ModeOutOfRange { value });
    }
    Ok(value)
}

/// Resolve the socket mode from an optional env value, applying the tightened
/// default when unset. `None` (var absent) → DEFAULT_SOCKET_MODE. `Some(_)` is
/// parsed strictly (fail-closed). This is the pure seam main.rs calls with
/// `std::env::var(..).ok().as_deref()`.
pub fn socket_mode_from_env(value: Option<&str>) -> Result<u32, SocketConfigError> {
    match value {
        None => Ok(DEFAULT_SOCKET_MODE),
        Some(raw) => parse_socket_mode(raw),
    }
}

/// Resolve the optional socket gid from an optional env value. `None` (var
/// absent) → `None` (do not chown — host mode / same-uid topology). An empty
/// string is also treated as unset (compose passes `${VAR:-}`). Otherwise the
/// value must parse as a non-negative integer gid; failure is fail-closed.
pub fn socket_gid_from_env(value: Option<&str>) -> Result<Option<u32>, SocketConfigError> {
    match value {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => {
            let gid = raw
                .trim()
                .parse::<u32>()
                .map_err(|_| SocketConfigError::BadGid {
                    raw: raw.to_string(),
                })?;
            Ok(Some(gid))
        }
    }
}

/// Apply the resolved permissions to an already-bound socket path: optionally
/// chown the group (owner left unchanged — the bind uid stays owner), then set
/// the mode. Ordering matters: chown CAN clear setuid/setgid bits on some
/// platforms, so we chown FIRST then chmod, guaranteeing the final mode is the
/// configured one. Returns io::Error on failure so the caller fails CLOSED
/// (refuses to serve a socket it could not lock down).
pub fn apply_socket_perms(
    path: &std::path::Path,
    mode: u32,
    gid: Option<u32>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(gid) = gid {
        // owner: None (keep bind uid as owner), group: Some(gid).
        std::os::unix::fs::chown(path, None, Some(gid))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_socket_mode_accepts_canonical_forms() {
        assert_eq!(parse_socket_mode("0660").unwrap(), 0o660);
        assert_eq!(parse_socket_mode("660").unwrap(), 0o660);
        assert_eq!(parse_socket_mode("0o660").unwrap(), 0o660);
        assert_eq!(parse_socket_mode("0600").unwrap(), 0o600);
        assert_eq!(parse_socket_mode("0700").unwrap(), 0o700);
        assert_eq!(parse_socket_mode("  0660 ").unwrap(), 0o660);
    }

    #[test]
    fn parse_socket_mode_rejects_garbage_fail_closed() {
        assert!(matches!(
            parse_socket_mode("rwx"),
            Err(SocketConfigError::BadMode { .. })
        ));
        assert!(matches!(
            parse_socket_mode("0o9z"),
            Err(SocketConfigError::BadMode { .. })
        ));
        assert!(matches!(
            parse_socket_mode(""),
            Err(SocketConfigError::BadMode { .. })
        ));
        // 8 and 9 are not octal digits.
        assert!(matches!(
            parse_socket_mode("0680"),
            Err(SocketConfigError::BadMode { .. })
        ));
    }

    #[test]
    fn parse_socket_mode_rejects_out_of_range() {
        // 0o6600 is a valid in-range mode (setgid + rw-r-----), so it must NOT
        // be rejected — only values beyond 0o7777 are out of range.
        assert_eq!(parse_socket_mode("06600").unwrap(), 0o6600);
        // 0o10000 sets a bit above the 12-bit permission range — runaway value.
        assert!(matches!(
            parse_socket_mode("010000"),
            Err(SocketConfigError::ModeOutOfRange { .. })
        ));
    }

    #[test]
    fn socket_mode_from_env_applies_tightened_default() {
        // Unset → tightened default 0o660 (NOT 0o777).
        assert_eq!(socket_mode_from_env(None).unwrap(), 0o660);
        assert_eq!(DEFAULT_SOCKET_MODE, 0o660);
        assert_ne!(socket_mode_from_env(None).unwrap(), 0o777);
    }

    #[test]
    fn socket_mode_from_env_honours_explicit_value() {
        assert_eq!(socket_mode_from_env(Some("0600")).unwrap(), 0o600);
    }

    #[test]
    fn socket_mode_from_env_invalid_refuses_no_fallback() {
        // A bad explicit value must error — it must NOT silently fall back to
        // the default or to a looser mode. This is the fail-closed invariant.
        let err = socket_mode_from_env(Some("notamode")).unwrap_err();
        assert!(matches!(err, SocketConfigError::BadMode { .. }));
    }

    #[test]
    fn socket_gid_from_env_absent_and_empty_are_none() {
        assert_eq!(socket_gid_from_env(None).unwrap(), None);
        assert_eq!(socket_gid_from_env(Some("")).unwrap(), None);
        assert_eq!(socket_gid_from_env(Some("   ")).unwrap(), None);
    }

    #[test]
    fn socket_gid_from_env_parses_valid() {
        assert_eq!(socket_gid_from_env(Some("65534")).unwrap(), Some(65534));
        assert_eq!(socket_gid_from_env(Some(" 101 ")).unwrap(), Some(101));
    }

    #[test]
    fn socket_gid_from_env_rejects_garbage_fail_closed() {
        assert!(matches!(
            socket_gid_from_env(Some("nobody")),
            Err(SocketConfigError::BadGid { .. })
        ));
        assert!(matches!(
            socket_gid_from_env(Some("-1")),
            Err(SocketConfigError::BadGid { .. })
        ));
    }
}
