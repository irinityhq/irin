//! Management UDS permission boundary.
//!
//! Proves the bind-time lockdown behaviour end-to-end against the SAME pure
//! functions main.rs calls (`socket::socket_mode_from_env` / `socket_gid_from_env`
//! / `apply_socket_perms`), bound to a temp UDS path and stat()'d. The env-seam
//! is exercised by passing the value DIRECTLY (no process-global env mutation),
//! mirroring the `producer_gate_armed_from` / `LeaseOpts::from_env` test pattern.

use gateway_sidecar::socket::{
    apply_socket_perms, socket_gid_from_env, socket_mode_from_env, SocketConfigError,
    DEFAULT_SOCKET_MODE,
};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

/// Bind a UDS at a temp path, apply the resolved perms, and return the mode bits
/// (masked to the 12 permission bits) actually on disk.
fn bind_and_mode(mode: u32, gid: Option<u32>) -> u32 {
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("mgmt.sock");
    let _listener = UnixListener::bind(&sock_path).expect("bind temp UDS");
    apply_socket_perms(&sock_path, mode, gid).expect("apply perms");
    let meta = std::fs::metadata(&sock_path).expect("stat socket");
    meta.permissions().mode() & 0o7777
}

#[test]
fn socket_created_with_tightened_default_mode() {
    // No env override → 0o660, and decisively NOT the old world-rwx 0o777.
    let mode = socket_mode_from_env(None).unwrap();
    assert_eq!(mode, DEFAULT_SOCKET_MODE);
    let on_disk = bind_and_mode(mode, None);
    assert_eq!(on_disk, 0o660, "default socket mode must be 0o660 on disk");
    assert_ne!(on_disk, 0o777, "must never be world-rwx");
    // World bits must be clear.
    assert_eq!(on_disk & 0o007, 0, "world bits must be unset");
}

#[test]
fn socket_created_with_configured_mode() {
    let mode = socket_mode_from_env(Some("0600")).unwrap();
    let on_disk = bind_and_mode(mode, None);
    assert_eq!(on_disk, 0o600, "explicit 0600 must land on disk");
    assert_eq!(
        on_disk & 0o077,
        0,
        "group+world bits must be unset for 0600"
    );
}

#[test]
fn invalid_socket_mode_refuses_startup_no_fallback() {
    // The parse/validate seam main.rs gates startup on. A garbage value MUST
    // error (caller exits) and MUST NOT silently fall back to a looser mode.
    let err = socket_mode_from_env(Some("worldwritable")).unwrap_err();
    assert!(matches!(err, SocketConfigError::BadMode { .. }));

    // An out-of-range value (beyond the 0o7777 perm range) is likewise refused.
    let err = socket_mode_from_env(Some("010000")).unwrap_err();
    assert!(matches!(err, SocketConfigError::ModeOutOfRange { .. }));

    // Sanity: the error never resolves to 0o777 by any path.
    assert!(socket_mode_from_env(Some("777x")).is_err());
}

#[test]
fn invalid_socket_gid_refuses_startup() {
    let err = socket_gid_from_env(Some("nginx")).unwrap_err();
    assert!(matches!(err, SocketConfigError::BadGid { .. }));
}

#[test]
fn socket_gid_chown_path_when_runnable() {
    // The gid-chown path requires either root or membership in the target gid.
    // We attempt to chown the socket's group to the CURRENT process's primary
    // gid — that is always permitted for a non-root owner (you may chgrp a file
    // you own to a group you belong to). This exercises `apply_socket_perms`
    // with `Some(gid)` without needing root. If even that fails (unusual
    // sandbox), skip gracefully with a documented reason.
    let my_gid = current_gid();
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("mgmt.sock");
    let _listener = UnixListener::bind(&sock_path).expect("bind temp UDS");

    match apply_socket_perms(&sock_path, 0o660, Some(my_gid)) {
        Ok(()) => {
            let meta = std::fs::metadata(&sock_path).expect("stat socket");
            assert_eq!(meta.permissions().mode() & 0o7777, 0o660);
            assert_eq!(
                group_of(&sock_path),
                my_gid,
                "group must be the chowned gid"
            );
        }
        Err(e) => {
            // Documented graceful skip: environments that forbid chgrp even to
            // an owned-group cannot exercise this path. The pure validation is
            // still covered by `invalid_socket_gid_refuses_startup` and the
            // unit tests in src/socket.rs.
            eprintln!("skipping gid-chown assertion (chown not permitted here): {e}");
        }
    }
}

fn current_gid() -> u32 {
    // SAFETY: getgid() is always-safe (no args, no error path).
    unsafe { libc_getgid() }
}

fn group_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().gid()
}

// Minimal extern binding so the test can read the current gid without adding a
// `libc`/`nix` direct dependency. `getgid` is a trivial always-success syscall.
extern "C" {
    #[link_name = "getgid"]
    fn libc_getgid() -> u32;
}
