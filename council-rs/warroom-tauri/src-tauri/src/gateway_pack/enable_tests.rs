use super::super::status::lifecycle_gen_test_lock;
use super::*;
use crate::docker_cli::DESKTOP_COMPOSE_PROJECT;
use crate::gateway_pack::paths::gateway_data_dir;
use crate::keychain::{MemorySecretStore, SecretStore};
use crate::private_config::test_env_lock;
use std::fs;
use std::sync::Mutex;

#[test]
fn project_constant() {
    assert_eq!(DESKTOP_COMPOSE_PROJECT, "irin-desktop-gateway");
    assert_eq!(crate::keychain::KEYCHAIN_SERVICE, "com.irinity.irin");
}

#[test]
fn explicit_enable_force_recreates_boot_time_configuration() {
    assert_eq!(
        COMPOSE_UP_ARGS,
        &["up", "-d", "--remove-orphans", "--force-recreate", "--wait"]
    );
}

#[test]
fn private_json_rejects_raw_key_detection() {
    let _g = test_env_lock();
    let prev = std::env::var("HOME").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-pj-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("Library/Application Support/com.irinity.irin")).unwrap();
    std::env::set_var("HOME", &tmp);
    let path = crate::private_config::private_config_path();
    fs::write(
        &path,
        r#"{"version":1,"install_id":"x","created_unix":1,"auth_token":"","via_gateway_default":false,"gateway_key_id":"k_abcdef12"}
"#,
    )
    .unwrap();
    assert!(assert_private_json_has_no_raw_key().is_ok());
    let bad_key = format!("gw_{}", "0".repeat(32));
    fs::write(
        &path,
        format!(
            r#"{{"version":1,"install_id":"x","created_unix":1,"gw":"{bad_key}"}}
"#
        ),
    )
    .unwrap();
    assert!(assert_private_json_has_no_raw_key().is_err());

    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn stop_without_installed_pack_emits_ordered_lifecycle_contract() {
    let _g = test_env_lock();
    let prev_home = std::env::var("HOME").ok();
    let prev_support = std::env::var("IRIN_APP_SUPPORT_ROOT").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-stop-stages-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);
    std::env::remove_var("IRIN_APP_SUPPORT_ROOT");

    let _g = lifecycle_gen_test_lock();
    let store = MemorySecretStore::default();
    stop_gateway_pack(&store).unwrap();

    let log = fs::read_to_string(gateway_data_dir().join("lifecycle.log")).unwrap();
    let begin = log.find(" stage=stop_begin detail=ok\n").unwrap();
    let lock = log.find(" stage=stop_lock detail=ok\n").unwrap();
    let config = log
        .find(" stage=stop_config detail=already_direct\n")
        .unwrap();
    let complete = log.find(" stage=stop_complete detail=ok\n").unwrap();
    assert!(begin < lock && lock < config && config < complete);
    assert!(!log.contains(" stage=stop_compose detail="));

    match prev_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match prev_support {
        Some(v) => std::env::set_var("IRIN_APP_SUPPORT_ROOT", v),
        None => std::env::remove_var("IRIN_APP_SUPPORT_ROOT"),
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn enable_loads_stable_keychain_accounts_once_per_flight() {
    let source = include_str!("enable.rs");
    assert_eq!(
        source
            .matches("load_launch_secrets(store, preloaded_pepper)")
            .count(),
        1
    );
    assert_eq!(
        source
            .matches("build_full_compose_env_with_launch_secrets(")
            .count(),
        3
    );
    assert!(!source.contains("build_full_compose_env("));
}

/// Records every Keychain get; foreign-port refusal must perform zero.
struct RecordingSecretStore {
    inner: MemorySecretStore,
    gets: Mutex<Vec<String>>,
}

impl crate::keychain::SecretStore for RecordingSecretStore {
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        self.inner.set_password(service, account, password)
    }

    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        self.gets.lock().unwrap().push(account.to_string());
        self.inner.get_password(service, account)
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        self.inner.delete_password(service, account)
    }
}

/// Runtime ordering proof: a foreign listener on 18080 is refused before ANY
/// Keychain account read — including the Docker-abort returns, whose
/// `gateway_pack_status_fresh` reads `GW_API_KEY`. The Docker probe panicking
/// proves the refusal also precedes the Docker branch entirely.
#[test]
fn enable_refuses_foreign_port_before_any_keychain_read() {
    let _g = test_env_lock();
    let prev_home = std::env::var("HOME").ok();
    let prev_support = std::env::var("IRIN_APP_SUPPORT_ROOT").ok();
    let tmp = std::env::temp_dir().join(format!(
        "gw-pack-foreign-port-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("HOME", &tmp);
    std::env::remove_var("IRIN_APP_SUPPORT_ROOT");

    let store = RecordingSecretStore {
        inner: MemorySecretStore::default(),
        gets: Mutex::new(Vec::new()),
    };
    let _g = lifecycle_gen_test_lock();
    let result = enable_gateway_pack_with_probes(
        &store,
        || Ok(true),
        || unreachable!("Docker probed before foreign-port refusal"),
    );

    let err = result.unwrap_err();
    assert!(err.contains("port 18080"), "unexpected error: {err}");
    assert!(
        store.gets.lock().unwrap().is_empty(),
        "foreign-port refusal must read no Keychain accounts, got {:?}",
        store.gets.lock().unwrap()
    );

    match prev_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match prev_support {
        Some(v) => std::env::set_var("IRIN_APP_SUPPORT_ROOT", v),
        None => std::env::remove_var("IRIN_APP_SUPPORT_ROOT"),
    }
    let _ = fs::remove_dir_all(&tmp);
}
