use super::*;
use crate::gateway_pack::paths::{ensure_gateway_dir, gateway_data_dir, public_env_path};
use crate::private_config::test_env_lock;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn public_env_rejects_crlf_and_duplicates() {
    assert!(validate_env_value("K", "ok").is_ok());
    assert!(validate_env_value("K", "bad\nvalue").is_err());
    assert!(validate_env_value("K", "bad\rvalue").is_err());
    let dup = serialize_public_env(&[("A".into(), "1".into()), ("A".into(), "2".into())]);
    assert!(dup.is_err());
    let body = serialize_public_env(&[
        (
            "IRIN_GATEWAY_IMAGE".into(),
            "n@sha256:".to_string() + &"a".repeat(64),
        ),
        ("WATCH_PRODUCER_ENABLED".into(), "false".into()),
    ])
    .unwrap();
    assert!(body.contains("WATCH_PRODUCER_ENABLED=false"));
    assert!(!body.contains("AUTH_PEPPER"));
    assert!(!body.contains("XAI_API_KEY"));
}

#[test]
fn gateway_dir_permissions_and_atomic_files() {
    let _g = test_env_lock();
    let prev = std::env::var("HOME").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);

    let dir = ensure_gateway_dir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "gateway dir mode {mode:o}");
    }
    let ledger = ensure_ledger_key().unwrap();
    assert_eq!(fs::metadata(&ledger).unwrap().len(), 32);

    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    let _ = fs::remove_dir_all(&tmp);
}
